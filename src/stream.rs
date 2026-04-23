//! Async VCL TCP stream — the connected-session wrapper.
//!
//! `VclStream` wraps a non-blocking VCL session handle and
//! provides async `read` / `write` methods that integrate with
//! Tokio via the VCL epoll reactor.
//!
//! Also implements `tokio::io::AsyncRead + AsyncWrite` so anything
//! that speaks tokio's IO traits — `tokio-rustls`, `hyper`, `axum` —
//! can sit directly on top of a VCL session with no kernel socket
//! underneath. The poll-based implementation stores a pending
//! reactor-wait future inline on the stream for the rare
//! `WouldBlock` case.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::error::{Result, VclError};
use crate::reactor::VclReactor;
use crate::session::SessionHandle;

type ReactorWait = Pin<Box<dyn Future<Output = Result<()>> + Send>>;

pub struct VclStream {
    handle: SessionHandle,
    reactor: VclReactor,
    /// When a non-blocking read returns WouldBlock, we stash a
    /// reactor.wait_readable() future here so the next poll_read can
    /// resume it. Cleared on readiness.
    pending_readable: Option<ReactorWait>,
    pending_writable: Option<ReactorWait>,
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

        Ok(VclStream {
            handle,
            reactor,
            pending_readable: None,
            pending_writable: None,
        })
    }

    /// Wrap an already-accepted session handle.
    pub(crate) fn from_accepted(handle: SessionHandle, reactor: VclReactor) -> Self {
        handle.set_nodelay().ok();
        VclStream {
            handle,
            reactor,
            pending_readable: None,
            pending_writable: None,
        }
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

// ---- tokio::io::AsyncRead / AsyncWrite ----
//
// These let `tokio-rustls`, `hyper`, `axum`, and anything else that
// speaks tokio's IO traits sit on top of a VCL session directly. The
// pattern: try the non-blocking syscall first; on WouldBlock, box up
// a `reactor.wait_*(timeout)` future inline on the stream so the next
// poll can resume it. On ready, clear the stored future and retry
// the syscall.
//
// We convert VclError -> io::Error lossily (losing the rc value
// since tokio's API is io::Error-based). Closed → io::ErrorKind::
// UnexpectedEof to match tokio conventions (0-byte read = EOF).

fn vcl_to_io(e: VclError) -> io::Error {
    match e {
        VclError::Closed => io::Error::new(io::ErrorKind::UnexpectedEof, "VCL session closed"),
        VclError::WouldBlock => io::Error::new(io::ErrorKind::WouldBlock, "VCL would block"),
        other => io::Error::new(io::ErrorKind::Other, other.to_string()),
    }
}

impl AsyncRead for VclStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        loop {
            // If we had a pending reactor wait, poll it to completion first.
            if let Some(fut) = this.pending_readable.as_mut() {
                match fut.as_mut().poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(())) => {
                        this.pending_readable = None;
                        // Fall through and retry the read.
                    }
                    Poll::Ready(Err(e)) => {
                        this.pending_readable = None;
                        return Poll::Ready(Err(vcl_to_io(e)));
                    }
                }
            }

            let unfilled = buf.initialize_unfilled();
            match this.handle.read(unfilled) {
                Ok(0) => {
                    // VCL's read() treats 0 as Closed internally (see
                    // handle.read). If we see a raw 0 here it's still
                    // EOF — advance nothing + return Ok to signal EOF.
                    return Poll::Ready(Ok(()));
                }
                Ok(n) => {
                    buf.advance(n);
                    return Poll::Ready(Ok(()));
                }
                Err(VclError::WouldBlock) => {
                    let reactor = this.reactor.clone();
                    let handle_id = this.handle.0;
                    this.pending_readable = Some(Box::pin(async move {
                        reactor
                            .wait_readable(handle_id, Duration::from_secs(300))
                            .await
                    }));
                    // Loop around to poll the newly-armed future.
                }
                Err(VclError::Closed) => {
                    return Poll::Ready(Ok(())); // EOF: buf unchanged
                }
                Err(e) => return Poll::Ready(Err(vcl_to_io(e))),
            }
        }
    }
}

impl AsyncWrite for VclStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        loop {
            if let Some(fut) = this.pending_writable.as_mut() {
                match fut.as_mut().poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(())) => {
                        this.pending_writable = None;
                    }
                    Poll::Ready(Err(e)) => {
                        this.pending_writable = None;
                        return Poll::Ready(Err(vcl_to_io(e)));
                    }
                }
            }

            match this.handle.write(buf) {
                Ok(n) => return Poll::Ready(Ok(n)),
                Err(VclError::WouldBlock) => {
                    let reactor = this.reactor.clone();
                    let handle_id = this.handle.0;
                    this.pending_writable = Some(Box::pin(async move {
                        reactor
                            .wait_writable(handle_id, Duration::from_secs(300))
                            .await
                    }));
                }
                Err(e) => return Poll::Ready(Err(vcl_to_io(e))),
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // VCL has no userspace buffer; bytes handed to write() are
        // either in VPP's TX FIFO already or returned as WouldBlock.
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        // Best-effort shutdown via vppcom_session_shutdown(SHUT_WR).
        let rc = unsafe {
            crate::ffi::vppcom_session_shutdown(this.handle.0, libc::SHUT_WR as i32)
        };
        if rc < 0 {
            return Poll::Ready(Err(vcl_to_io(VclError::from_rc(rc))));
        }
        Poll::Ready(Ok(()))
    }
}
