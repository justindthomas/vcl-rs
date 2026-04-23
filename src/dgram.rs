//! Async VCL datagram (UDP) socket.
//!
//! VCL's UDP path creates one session per local endpoint. After `bind`
//! + `listen`, every incoming datagram is delivered to that session,
//! and we read them one at a time with `vppcom_session_recvfrom` — the
//! endpoint structure tells us who sent it. Outbound datagrams go via
//! `vppcom_session_sendto` with an explicit peer endpoint.
//!
//! This mirrors `tokio::net::UdpSocket`'s shape, so callers can write
//! ordinary async-Rust loops:
//!
//! ```rust,no_run
//! # async fn example() -> vcl_rs::error::Result<()> {
//! use vcl_rs::{VclApp, VclReactor, VclDgramSocket};
//! let _app = VclApp::init("imp-dnsd")?;
//! let reactor = VclReactor::new()?;
//! let sock = VclDgramSocket::bind("0.0.0.0:53".parse().unwrap(), reactor.clone())?;
//!
//! let mut buf = [0u8; 4096];
//! let (n, peer) = sock.recv_from(&mut buf).await?;
//! sock.send_to(&buf[..n], peer).await?;
//! # Ok(())
//! # }
//! ```

use std::net::SocketAddr;
use std::time::Duration;

use crate::error::{Result, VclError};
use crate::reactor::VclReactor;
use crate::session::SessionHandle;

pub struct VclDgramSocket {
    handle: SessionHandle,
    reactor: VclReactor,
}

impl VclDgramSocket {
    /// Bind a UDP socket to `addr` through VPP's session layer.
    ///
    /// For VCL, UDP receive requires the session to transition into a
    /// listening state via `vppcom_session_listen`; the backlog value
    /// is ignored for datagram protocols. This matches what the VCL
    /// UDP echo examples do.
    pub fn bind(addr: SocketAddr, reactor: VclReactor) -> Result<Self> {
        unsafe {
            crate::ffi::vppcom_worker_register();
        }
        let handle = SessionHandle::create_udp(true)?;
        handle.bind(addr)?;
        handle.listen(0)?; // backlog ignored for UDP
        reactor.register(handle.0)?;
        tracing::info!(%addr, "VCL dgram bound");
        Ok(Self { handle, reactor })
    }

    /// Bind to any address — used for client-side UDP sockets that
    /// only need source-port randomisation and don't care about the
    /// local address (recursor upstream queries).
    pub fn bind_ephemeral_v4(reactor: VclReactor) -> Result<Self> {
        Self::bind("0.0.0.0:0".parse().unwrap(), reactor)
    }

    pub fn bind_ephemeral_v6(reactor: VclReactor) -> Result<Self> {
        Self::bind("[::]:0".parse().unwrap(), reactor)
    }

    /// Receive one datagram into `buf`. Awaits until one is available.
    /// Returns (bytes_copied, peer_addr).
    pub async fn recv_from(&self, buf: &mut [u8]) -> Result<(usize, SocketAddr)> {
        loop {
            match self.handle.recvfrom(buf) {
                Ok(pair) => return Ok(pair),
                Err(VclError::WouldBlock) => {
                    self.reactor
                        .wait_readable(self.handle.0, Duration::from_secs(300))
                        .await?;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Send a datagram to `dst`. If the TX FIFO is full, awaits until
    /// there's space — but UDP sends rarely block, so this is a fast
    /// path in practice.
    pub async fn send_to(&self, buf: &[u8], dst: SocketAddr) -> Result<usize> {
        loop {
            match self.handle.sendto(buf, dst) {
                Ok(n) => return Ok(n),
                Err(VclError::WouldBlock) => {
                    self.reactor
                        .wait_writable(self.handle.0, Duration::from_secs(30))
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
