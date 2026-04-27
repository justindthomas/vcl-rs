//! VCL application lifecycle.
//!
//! A `VclApp` must be created once per process before any sessions
//! can be opened. It calls `vppcom_app_create` which connects to
//! VPP's session layer via the app-socket-api, sets up shared
//! memory segments for FIFOs, and creates worker-0.
//!
//! Configuration is read from `VCL_CONFIG` env var (defaults to
//! `/etc/vpp/vcl.conf`). The config tells VCL where VPP's
//! app-socket-api socket lives.
//!
//! Drop calls `vppcom_app_destroy`.

use std::cell::Cell;
use std::ffi::CString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier, Mutex};

use crate::error::{Result, VclError};
use crate::ffi;

static INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Serializes `vppcom_worker_register` calls. libvppcom 25.10's
/// worker-pool growth is not thread-safe: when it grows the
/// `vcm->workers` vector it frees the old buffer and writes a new
/// pointer, but other threads in `vppcom_session_create` (and friends)
/// load `vcm->workers` once and dereference it later — racing the
/// realloc gets them a freed pointer and a GP fault. Holding this
/// mutex across the FFI call removes the register-vs-register race.
/// The register-vs-session race is killed separately by `prewarm`
/// (below) — once the workers vector reaches its steady-state size,
/// no further registrations happen and there's nothing to race with.
static REGISTER_LOCK: Mutex<()> = Mutex::new(());

thread_local! {
    /// Per-thread "already registered" flag. Both
    /// `tokio::runtime::Builder::on_thread_start` and the defensive
    /// calls inside `query_udp_sync` / `query_tcp_dns_sync` go through
    /// `register_worker_thread`, so a thread can hit this path several
    /// times. After the first call libvppcom returns the existing
    /// worker index — but we still want to skip the FFI hop and the
    /// mutex acquisition on subsequent calls.
    static REGISTERED: Cell<bool> = const { Cell::new(false) };
}

pub struct VclApp {
    _private: (),
}

impl VclApp {
    /// Initialize VCL. Must be called exactly once per process.
    /// Reads config from `VCL_CONFIG` env var or `/etc/vpp/vcl.conf`.
    pub fn init(app_name: &str) -> Result<Self> {
        if INITIALIZED.swap(true, Ordering::SeqCst) {
            return Err(VclError::Api("VCL already initialized".into(), -1));
        }
        let c_name =
            CString::new(app_name).map_err(|_| VclError::Api("invalid app name".into(), -1))?;
        let rc = unsafe { ffi::vppcom_app_create(c_name.as_ptr()) };
        if rc < 0 {
            INITIALIZED.store(false, Ordering::SeqCst);
            return Err(VclError::from_rc(rc));
        }
        tracing::info!(app_name, "VCL application created");
        Ok(VclApp { _private: () })
    }

    /// Get the message-queue eventfd for Tokio integration.
    /// Register this FD with `tokio::io::unix::AsyncFd` to wake
    /// when VCL sessions have events ready.
    pub fn mq_epoll_fd(&self) -> i32 {
        unsafe { ffi::vppcom_mq_epoll_fd() }
    }
}

/// Register the current thread as a VCL worker.
///
/// VCL uses a worker-per-thread model — every thread that touches
/// a VCL session (via `recv`, `send`, `connect`, etc.) must first
/// have allocated its own worker context. The main thread that
/// calls `VclApp::init` is registered implicitly as worker-0; any
/// other thread must call this before making a VCL call or
/// `libvppcom` will SEGV inside `vppcom_session_*`.
///
/// Idempotent per-thread (a thread-local flag short-circuits repeat
/// calls) and serialized across threads (a process-wide mutex wraps
/// the FFI call so two threads can't race the worker-pool realloc
/// inside libvppcom — see `REGISTER_LOCK` for the gory details).
///
/// On failure (`-17` from `vppcom_worker_register`, almost always
/// VPP-side fifo-segment exhaustion when a previous app instance
/// crashed and left memfd-backed segments stranded), this calls
/// `vppcom_worker_unregister` to reclaim the local-pool slot that
/// `vcl_worker_alloc_and_init` already grabbed before the VPP
/// register failed. Without that the local pool fills up across
/// retries even though no usable workers exist.
pub fn register_worker_thread() {
    REGISTERED.with(|r| {
        if r.get() {
            return;
        }
        let _guard = REGISTER_LOCK.lock().unwrap();
        let rc = unsafe { ffi::vppcom_worker_register() };
        let idx = unsafe { ffi::vppcom_worker_index() };
        let name = std::thread::current()
            .name()
            .unwrap_or("<unnamed>")
            .to_owned();
        if idx < 0 {
            // Reclaim the local vcl_worker pool slot the failed
            // register already took. Per VPP source review: on the
            // server-fail path `vppcom.c` returns without calling
            // `vcl_worker_free`, leaving a hole. Subsequent
            // registrations from other threads might bypass it
            // because `pool_get` always returns the lowest free
            // index, but if we keep retrying on the same thread
            // we'd burn through the pool. Best-effort cleanup; we
            // ignore the unregister return code.
            unsafe {
                ffi::vppcom_worker_unregister();
            }
            tracing::warn!(
                thread.name = %name,
                register_rc = rc,
                "VCL worker registration failed — thread cannot use VCL sessions"
            );
        } else {
            tracing::debug!(
                thread.name = %name,
                worker_idx = idx,
                "VCL worker registered"
            );
        }
        r.set(true);
    });
}

/// Pre-register `n` workers on the tokio blocking pool, synchronously
/// from the caller's task. Each spawned blocking task calls
/// `register_worker_thread` and then waits on a barrier so tokio is
/// forced to materialize `n` distinct threads (otherwise it could run
/// tasks back-to-back on a single thread and only one worker would
/// register).
///
/// Caller responsibilities:
///   1. `VclApp::init` must already have run.
///   2. The tokio runtime's `max_blocking_threads` must be ≥ `n`,
///      and ideally equal to `n` so runtime growth never spawns a
///      fresh thread that would re-trigger registration.
///   3. Call this *before* any VCL session ops are kicked off — the
///      whole point is to drain libvppcom's worker-pool growth into
///      a quiet startup window.
pub async fn prewarm(n: usize) -> Result<()> {
    if n == 0 {
        return Ok(());
    }
    let barrier = Arc::new(Barrier::new(n));
    let mut handles = Vec::with_capacity(n);
    for _ in 0..n {
        let b = barrier.clone();
        handles.push(tokio::task::spawn_blocking(move || {
            register_worker_thread();
            b.wait();
        }));
    }
    for h in handles {
        h.await
            .map_err(|e| VclError::Api(format!("prewarm join: {e}"), -1))?;
    }
    tracing::info!(workers = n, "VCL workers pre-warmed");
    Ok(())
}

impl Drop for VclApp {
    fn drop(&mut self) {
        unsafe {
            ffi::vppcom_app_destroy();
        }
        INITIALIZED.store(false, Ordering::SeqCst);
        tracing::info!("VCL application destroyed");
    }
}
