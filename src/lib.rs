//! Async Rust wrapper for VPP's VCL (VPP Communications Library).
//!
//! Provides TCP client and server sessions that run through VPP's
//! built-in TCP stack, bypassing the kernel entirely. No linux-cp
//! TAP interfaces needed.
//!
//! ## Architecture
//!
//! ```text
//! imp-bgpd ──► VclStream ──► VCL (libvppcom.so)
//!                               │
//!                    shared-memory FIFOs
//!                               │
//!                          VPP session layer ──► VPP TCP stack ──► wire
//! ```
//!
//! ## Usage
//!
//! ```rust,no_run
//! use vcl_rs::{VclApp, VclReactor, VclListener, VclStream};
//!
//! # async fn example() -> vcl_rs::error::Result<()> {
//! // Initialize VCL (reads /etc/vpp/vcl.conf).
//! let _app = VclApp::init("imp-bgpd")?;
//! let reactor = VclReactor::new()?;
//!
//! // Server: listen for BGP connections.
//! let listener = VclListener::bind("0.0.0.0:179".parse().unwrap(), reactor.clone())?;
//! let (stream, peer_addr) = listener.accept().await?;
//!
//! // Client: connect to a BGP peer.
//! let stream = VclStream::connect(
//!     "10.0.0.1:179".parse().unwrap(),
//!     None,
//!     std::time::Duration::from_secs(30),
//!     reactor.clone(),
//! ).await?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Tokio Integration
//!
//! VCL's message-queue eventfd (`vppcom_mq_epoll_fd`) is wrapped
//! in `tokio::io::unix::AsyncFd`. When VPP signals that a session
//! has events (data ready, connection accepted, write space
//! available), the Tokio runtime wakes the appropriate `wait_*`
//! future in the reactor.
//!
//! ## Configuration
//!
//! VCL reads its config from `VCL_CONFIG` env var (defaults to
//! `/etc/vpp/vcl.conf`). Minimal config:
//!
//! ```text
//! vcl {
//!   app-socket-api /run/vpp/app_ns_sockets/default
//!   use-mq-eventfd
//! }
//! ```
//!
//! VPP must have the session layer enabled:
//!
//! ```text
//! session {
//!   evt_qs_memfd_seg
//!   use-app-socket-api
//! }
//! ```

pub mod ffi;

pub mod app;
pub mod dgram;
pub mod error;
pub mod listener;
pub mod reactor;
pub mod session; // pub for endpoint_from_addr used by VclTransport's I/O thread
pub mod stream;

pub use app::VclApp;
pub use dgram::VclDgramSocket;
pub use listener::VclListener;
pub use reactor::VclReactor;
pub use stream::VclStream;
