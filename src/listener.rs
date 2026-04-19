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
        unsafe {
            crate::ffi::vppcom_worker_register();
        }
        let handle = SessionHandle::create_tcp(true)?;
        handle.bind(addr)?;
        handle.listen(128)?;
        reactor.register(handle.0)?;
        tracing::info!(%addr, "VCL listener bound");
        Ok(VclListener { handle, reactor })
    }

    /// Accept the next incoming connection. Blocks (async) until
    /// a connection arrives.
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
                    self.reactor
                        .wait_readable(self.handle.0, Duration::from_secs(300))
                        .await?;
                }
                Err(e) => return Err(e),
            }
        }
    }

    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.handle.local_addr().ok()
    }
}
