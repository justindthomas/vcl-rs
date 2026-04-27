//! Async VCL TCP listener — bind + listen + accept loop.

use std::net::SocketAddr;
use std::time::Duration;

use crate::error::Result;
use crate::reactor::VclReactor;
use crate::session::SessionHandle;
use crate::stream::VclStream;

pub struct VclListener {
    handle: SessionHandle,
    reactor: VclReactor,
}

impl VclListener {
    /// Bind and listen on `addr` through VPP's TCP stack.
    pub fn bind(addr: SocketAddr, reactor: VclReactor) -> Result<Self> {
        crate::app::register_worker_thread();
        let handle = SessionHandle::create_tcp(true)?;
        handle.bind(addr)?;
        handle.listen(128)?;
        // Drain MQ so VPP's async `bound_handler` runs; if VPP
        // rejects the bind (e.g., port already listened on by
        // another app instance) we surface that as a sync Err
        // here rather than letting the caller proceed with a
        // half-broken session. See dgram.rs for full rationale.
        crate::dgram::verify_bind_or_err(handle.0, addr)?;
        reactor.register(handle.0)?;
        tracing::info!(%addr, "VCL listener bound");
        Ok(VclListener { handle, reactor })
    }

    /// Accept the next incoming connection. Blocks (async) until
    /// a connection arrives.
    /// Accept the next inbound TCP connection. Awaits until one
    /// arrives. Like `VclDgramSocket::recv_from`, the 5-minute
    /// internal `wait_readable` timeout is swallowed and re-armed —
    /// `accept` semantically blocks for the next connection regardless
    /// of how long the listener has been idle.
    pub async fn accept(&self) -> Result<(VclStream, SocketAddr)> {
        loop {
            match self.handle.accept() {
                Ok((client_handle, peer_addr)) => {
                    self.reactor.register(client_handle.0)?;
                    let stream =
                        VclStream::from_accepted(client_handle, self.reactor.clone());
                    return Ok((stream, peer_addr));
                }
                Err(crate::error::VclError::WouldBlock) => {
                    match self
                        .reactor
                        .wait_readable(self.handle.0, Duration::from_secs(300))
                        .await
                    {
                        Ok(()) | Err(crate::error::VclError::Timeout) => {}
                        Err(e) => return Err(e),
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }

    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.handle.local_addr().ok()
    }
}

impl Drop for VclListener {
    fn drop(&mut self) {
        self.reactor.deregister(self.handle.0);
    }
}
