//! Async VCL TCP stream — the connected-session wrapper.
//!
//! `VclStream` wraps a non-blocking VCL session handle and
//! provides async `read` / `write` methods that integrate with
//! Tokio via the VCL epoll reactor.

use std::net::SocketAddr;
use std::time::Duration;

use crate::error::{Result, VclError};
use crate::reactor::VclReactor;
use crate::session::SessionHandle;

pub struct VclStream {
    handle: SessionHandle,
    reactor: VclReactor,
}

impl VclStream {
    /// Connect to a remote address through VPP's TCP stack.
    pub async fn connect(
        addr: SocketAddr,
        source: Option<SocketAddr>,
        timeout: Duration,
        reactor: VclReactor,
    ) -> Result<Self> {
        // VCL uses a worker-per-thread model. If this task is
        // running on a Tokio worker thread that hasn't registered
        // with VCL yet, session_create will segfault. Register
        // (idempotent — returns existing index if already done).
        unsafe {
            crate::ffi::vppcom_worker_register();
        }
        let handle = SessionHandle::create_tcp(true)?;
        handle.set_nodelay().ok();

        if let Some(src) = source {
            handle.bind(src)?;
        }

        // VCL connect on a non-blocking session returns immediately
        // (or EINPROGRESS). We register for EPOLLOUT to detect
        // completion.
        // For the initial connect, use a blocking session so we
        // don't need cross-worker epoll. VCL's non-blocking
        // connect + epoll requires all operations on the same
        // worker thread, which conflicts with Tokio's task
        // scheduling. We spawn_blocking the connect to avoid
        // blocking the Tokio runtime.
        //
        // After connect completes, we register with the reactor
        // for async read/write.
        let sh = handle.0;
        let connect_result = tokio::task::spawn_blocking(move || {
            unsafe { crate::ffi::vppcom_worker_register(); }
            // Create a blocking session for the connect.
            let blocking_sh = unsafe {
                crate::ffi::vppcom_session_create(crate::ffi::VPPCOM_PROTO_TCP, 0)
            };
            if blocking_sh < 0 {
                return Err(VclError::from_rc(blocking_sh));
            }
            let mut ep = crate::session::endpoint_from_addr(addr);
            let rc = unsafe {
                crate::ffi::vppcom_session_connect(blocking_sh as u32, &mut ep)
            };
            if rc < 0 {
                unsafe { crate::ffi::vppcom_session_close(blocking_sh as u32); }
                return Err(VclError::from_rc(rc));
            }
            Ok(blocking_sh as u32)
        })
        .await
        .map_err(|e| VclError::Api(format!("spawn_blocking join: {}", e), -1))??;

        // Replace the non-blocking handle with the connected one.
        // Forget the original (don't close it — it was never connected).
        std::mem::forget(handle);
        let handle = SessionHandle(connect_result);
        handle.set_nodelay().ok();

        Ok(VclStream { handle, reactor })
    }

    /// Wrap an already-accepted session handle.
    pub(crate) fn from_accepted(handle: SessionHandle, reactor: VclReactor) -> Self {
        handle.set_nodelay().ok();
        VclStream { handle, reactor }
    }

    /// Read up to `buf.len()` bytes. Returns the number of bytes
    /// read, or `VclError::Closed` on EOF.
    pub async fn read(&self, buf: &mut [u8]) -> Result<usize> {
        loop {
            match self.handle.read(buf) {
                Ok(0) => return Err(VclError::Closed),
                Ok(n) => return Ok(n),
                Err(VclError::WouldBlock) => {
                    self.reactor
                        .wait_readable(self.handle.0, Duration::from_secs(300))
                        .await?;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Read exactly `buf.len()` bytes. Returns `Closed` if the
    /// peer disconnects before all bytes arrive.
    pub async fn read_exact(&self, buf: &mut [u8]) -> Result<()> {
        let mut pos = 0;
        while pos < buf.len() {
            let n = self.read(&mut buf[pos..]).await?;
            pos += n;
        }
        Ok(())
    }

    /// Write all bytes from `buf`. May require multiple underlying
    /// writes if the TX FIFO is full.
    pub async fn write_all(&self, buf: &[u8]) -> Result<()> {
        let mut pos = 0;
        while pos < buf.len() {
            match self.handle.write(&buf[pos..]) {
                Ok(n) => pos += n,
                Err(VclError::WouldBlock) => {
                    self.reactor
                        .wait_writable(self.handle.0, Duration::from_secs(300))
                        .await?;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.handle.local_addr().ok()
    }

    pub fn peer_addr(&self) -> Option<SocketAddr> {
        self.handle.peer_addr().ok()
    }

    /// Session handle for reactor registration.
    pub(crate) fn handle_id(&self) -> u32 {
        self.handle.0
    }
}
