//! Tokio integration for VCL sessions.
//!
//! VCL exposes a message-queue eventfd via `vppcom_mq_epoll_fd()`
//! that becomes readable whenever any VCL session has pending
//! events. We wrap this in a `tokio::io::unix::AsyncFd` so the
//! Tokio runtime wakes us when VCL has work to do.
//!
//! The reactor owns a VCL epoll set (`vppcom_epoll_create`).
//! Sessions are registered with `vppcom_epoll_ctl` and we drain
//! events with `vppcom_epoll_wait(timeout=0)` (non-blocking)
//! whenever the MQ fd fires.
//!
//! Waiters register via `wait_readable` / `wait_writable` and are
//! woken by a `tokio::sync::Notify` per session.

use std::collections::HashMap;
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::unix::AsyncFd;
use tokio::io::Interest;
use tokio::sync::Notify;

use crate::error::{Result, VclError};
use crate::ffi;

struct ReactorInner {
    /// VCL epoll handle (from vppcom_epoll_create).
    vep: u32,
    /// Per-session notification. When a VCL epoll event fires for
    /// a session, we notify its waiter.
    waiters: HashMap<u32, Arc<Notify>>,
}

#[derive(Clone)]
pub struct VclReactor {
    inner: Arc<Mutex<ReactorInner>>,
    /// The MQ eventfd wrapped in AsyncFd for Tokio integration.
    /// Shared via Arc so clones can spawn the poll loop once.
    mq_fd: Arc<AsyncFd<MqFd>>,
}

/// Newtype so we can impl `AsRawFd` for the MQ file descriptor.
struct MqFd(RawFd);

impl AsRawFd for MqFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

impl VclReactor {
    /// Create a new reactor. Call once after `VclApp::init`.
    pub fn new() -> Result<Self> {
        let vep = unsafe { ffi::vppcom_epoll_create() };
        if vep < 0 {
            return Err(VclError::from_rc(vep));
        }
        let mq_raw = unsafe { ffi::vppcom_mq_epoll_fd() };
        if mq_raw < 0 {
            return Err(VclError::Api(
                "vppcom_mq_epoll_fd returned negative".into(),
                mq_raw,
            ));
        }
        let async_fd = AsyncFd::with_interest(MqFd(mq_raw), Interest::READABLE)
            .map_err(|e| VclError::Io(e))?;

        let reactor = VclReactor {
            inner: Arc::new(Mutex::new(ReactorInner {
                vep: vep as u32,
                waiters: HashMap::new(),
            })),
            mq_fd: Arc::new(async_fd),
        };
        Ok(reactor)
    }

    /// Register a session handle with the VCL epoll set.
    /// Watches for read + write + edge-triggered.
    pub fn register(&self, session_handle: u32) -> Result<()> {
        let inner = self.inner.lock().unwrap();
        let mut ev = ffi::epoll_event {
            events: ffi::EPOLLIN | ffi::EPOLLOUT | ffi::EPOLLET,
            data: session_handle as u64,
        };
        let rc = unsafe {
            ffi::vppcom_epoll_ctl(
                inner.vep,
                ffi::EPOLL_CTL_ADD,
                session_handle,
                &mut ev,
            )
        };
        if rc < 0 {
            return Err(VclError::from_rc(rc));
        }
        Ok(())
    }

    /// Deregister a session from the epoll set.
    pub fn deregister(&self, session_handle: u32) {
        let mut inner = self.inner.lock().unwrap();
        inner.waiters.remove(&session_handle);
        unsafe {
            ffi::vppcom_epoll_ctl(
                inner.vep,
                ffi::EPOLL_CTL_DEL,
                session_handle,
                std::ptr::null_mut(),
            );
        }
    }

    /// Wait until the given session is readable (has data or a
    /// new connection to accept). Integrates with Tokio by waiting
    /// on the MQ eventfd, then draining VCL epoll events.
    pub async fn wait_readable(&self, session_handle: u32, timeout: Duration) -> Result<()> {
        self.wait_event(session_handle, ffi::EPOLLIN, timeout).await
    }

    /// Wait until the given session is writable (TX FIFO has space
    /// or a connect() completed).
    pub async fn wait_writable(&self, session_handle: u32, timeout: Duration) -> Result<()> {
        self.wait_event(session_handle, ffi::EPOLLOUT, timeout).await
    }

    async fn wait_event(&self, session_handle: u32, _event_mask: u32, timeout: Duration) -> Result<()> {
        let notify = {
            let mut inner = self.inner.lock().unwrap();
            inner
                .waiters
                .entry(session_handle)
                .or_insert_with(|| Arc::new(Notify::new()))
                .clone()
        };

        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            // Wait for the MQ fd to become readable (VCL has events).
            let mut ready = tokio::time::timeout_at(
                deadline,
                self.mq_fd.readable(),
            )
            .await
            .map_err(|_| VclError::Timeout)?
            .map_err(|e| VclError::Io(e))?;

            // Drain VCL events and notify waiters.
            self.drain_events();

            // Clear the AsyncFd readiness so we re-arm for next time.
            ready.clear_ready();

            // Check if our session was notified. Use a zero-duration
            // check — if the notification arrived, proceed; if not,
            // loop and wait for the next MQ event.
            if tokio::time::timeout(Duration::from_millis(0), notify.notified())
                .await
                .is_ok()
            {
                return Ok(());
            }
        }
    }

    /// Non-blocking drain of all pending VCL epoll events. Wakes
    /// the per-session Notify for each session that had an event.
    fn drain_events(&self) {
        let inner = self.inner.lock().unwrap();
        let mut events = [ffi::epoll_event { events: 0, data: 0 }; 64];
        loop {
            let n = unsafe {
                ffi::vppcom_epoll_wait(
                    inner.vep,
                    events.as_mut_ptr(),
                    events.len() as i32,
                    0.0, // non-blocking
                )
            };
            if n <= 0 {
                break;
            }
            for i in 0..n as usize {
                let sh = events[i].data as u32;
                if let Some(notify) = inner.waiters.get(&sh) {
                    notify.notify_one();
                }
            }
            if (n as usize) < events.len() {
                break;
            }
        }
    }
}
