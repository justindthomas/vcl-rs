//! UDP echo server over VPP's session layer.
//!
//! Binds `0.0.0.0:5353` through VCL, then echoes every incoming
//! datagram back to the sender. Useful for validating the VclDgramSocket
//! wiring end-to-end without pulling in a full DNS stack.
//!
//! Prereq: VPP running with session layer enabled + /etc/vpp/vcl.conf
//! readable by this process (see crate docs).
//!
//! Run with: `cargo run --example udp_echo`.

use std::net::SocketAddr;

use vcl_rs::{VclApp, VclDgramSocket, VclReactor};

#[tokio::main]
async fn main() -> vcl_rs::error::Result<()> {
    tracing_subscriber::fmt().init();

    let _app = VclApp::init("vcl-rs-udp-echo")?;
    let reactor = VclReactor::new()?;

    let bind_addr: SocketAddr = std::env::args()
        .nth(1)
        .as_deref()
        .unwrap_or("0.0.0.0:5353")
        .parse()
        .expect("invalid bind address");

    let sock = VclDgramSocket::bind(bind_addr, reactor.clone())?;
    tracing::info!(addr = %bind_addr, "UDP echo ready");

    let mut buf = vec![0u8; 65535];
    loop {
        let (n, peer) = sock.recv_from(&mut buf).await?;
        tracing::debug!(%peer, bytes = n, "echo");
        let _ = sock.send_to(&buf[..n], peer).await?;
    }
}
