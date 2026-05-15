//! VCL application lifecycle (VLS — VCL Locked Sessions).
//!
//! A `VclApp` must be created once per process before any sessions
//! can be opened. It calls `vls_app_create` which is the VLS entry
//! point — internally that runs `vppcom_app_create` AND installs
//! the VLS process-wide locks plus the per-thread auto-register
//! hooks. After init, every VLS op called from any thread
//! auto-registers that thread (via libvppcom's `vls_mt_detect`)
//! and takes the VLS lock for the duration of the op. The result
//! is thread-agnostic sessions: a `VclStream` created on thread A
//! can be read/written from thread B without the GP-faults that
//! plagued the pre-VLS code path.
//!
//! Configuration is read from `VCL_CONFIG` env var (defaults to
//! `/etc/vpp/vcl.conf`). The config tells VCL where VPP's
//! app-socket-api socket lives. `use-mq-eventfd` is still required
//! for the reactor's tokio integration.
//!
//! Drop calls `vppcom_app_destroy`. (VLS doesn't expose its own
//! teardown — `vls_app_exit` is an atexit handler libvppcom
//! installs internally.)

use std::ffi::CString;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::{Result, VclError};
use crate::ffi;

static INITIALIZED: AtomicBool = AtomicBool::new(false);

pub struct VclApp {
    _private: (),
}

impl VclApp {
    /// Initialize VCL via VLS. Must be called exactly once per
    /// process. Reads config from `VCL_CONFIG` env var or
    /// `/etc/vpp/vcl.conf`.
    pub fn init(app_name: &str) -> Result<Self> {
        if INITIALIZED.swap(true, Ordering::SeqCst) {
            return Err(VclError::Api("VCL already initialized".into(), -1));
        }
        let c_name =
            CString::new(app_name).map_err(|_| VclError::Api("invalid app name".into(), -1))?;
        let rc = unsafe { ffi::vls_app_create(c_name.as_ptr()) };
        if rc < 0 {
            INITIALIZED.store(false, Ordering::SeqCst);
            return Err(VclError::from_rc(rc));
        }
        let use_eventfd = unsafe { ffi::vls_use_eventfd() } != 0;
        let mt_wrk = unsafe { ffi::vls_mt_wrk_supported() } != 0;
        if !use_eventfd {
            return Err(VclError::Api(
                "VLS requires use-mq-eventfd in vcl.conf for tokio integration".into(),
                -1,
            ));
        }
        tracing::info!(app_name, mt_wrk_supported = mt_wrk, "VLS application created");
        Ok(VclApp { _private: () })
    }

    /// Get the message-queue eventfd for Tokio integration.
    /// Register this FD with `tokio::io::unix::AsyncFd` to wake
    /// when VCL sessions have events ready.
    pub fn mq_epoll_fd(&self) -> i32 {
        unsafe { ffi::vppcom_mq_epoll_fd() }
    }
}

/// Register the current thread with VLS.
///
/// Under classic libvppcom this was load-bearing — every thread
/// that touched a session had to call `vppcom_worker_register`
/// first or libvppcom would GP-fault on the TLS `__vcl_worker_index`.
/// Under VLS the picture is simpler: every VLS op auto-registers
/// the calling thread the first time it runs (see `vls_mt_detect`
/// in `vcl_locked.c`), so explicit calls are only needed before
/// you touch a non-VLS function on a fresh thread. The one we
/// actually care about is `vppcom_mq_epoll_fd`, which the reactor
/// fetches on its construction thread — call this before that fetch.
///
/// Kept as a public function (instead of being deleted entirely)
/// because the reactor and downstream callers still need a way to
/// "warm up" a thread's libvppcom TLS without first issuing a real
/// session op. Idempotent.
pub fn register_worker_thread() {
    unsafe {
        ffi::vls_register_vcl_worker();
    }
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
