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

use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

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
        crate::app::register_worker_thread();
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
    ///
    /// VCL in VPP 25.10 does NOT auto-assign a non-zero source port
    /// when bound to `:0` — outgoing packets go out with source port
    /// 0, which real upstreams (1.1.1.1, 8.8.8.8) silently drop. So
    /// we pick a random port in the Linux ephemeral range ourselves
    /// and retry a few times on the rare collision.
    pub fn bind_ephemeral_v4(reactor: VclReactor) -> Result<Self> {
        Self::bind_random_ephemeral("0.0.0.0", reactor)
    }

    pub fn bind_ephemeral_v6(reactor: VclReactor) -> Result<Self> {
        Self::bind_random_ephemeral("::", reactor)
    }

    fn bind_random_ephemeral(ip_str: &str, reactor: VclReactor) -> Result<Self> {
        use rand::Rng;
        // Linux's default local-port range is 32768–60999; we use a
        // slightly tighter range to avoid any well-known services and
        // keep plenty of headroom.
        const LOW: u16 = 32768;
        const HIGH: u16 = 60999;
        let mut rng = rand::thread_rng();
        let mut last_err: Option<VclError> = None;
        for _ in 0..8 {
            let port: u16 = rng.gen_range(LOW..=HIGH);
            let addr: SocketAddr = format!("{ip_str}:{port}")
                .parse()
                .map_err(|e: std::net::AddrParseError| {
                    VclError::Api(format!("ephemeral addr parse: {e}"), -1)
                })?;
            match Self::bind(addr, reactor.clone()) {
                Ok(sock) => return Ok(sock),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| {
            VclError::Api("exhausted ephemeral bind retries".into(), -1)
        }))
    }

    /// Receive one datagram into `buf`. Awaits until one is available.
    /// Returns (bytes_copied, peer_addr).
    ///
    /// `wait_readable`'s 5-minute internal timeout fires if no datagram
    /// arrives in that window; we silently re-arm and keep waiting.
    /// `recv_from` semantically blocks until data arrives, so a quiet
    /// listener should never surface a "timeout" error to the caller.
    pub async fn recv_from(&self, buf: &mut [u8]) -> Result<(usize, SocketAddr)> {
        loop {
            match self.handle.recvfrom(buf) {
                Ok(pair) => return Ok(pair),
                Err(VclError::WouldBlock) => {
                    match self
                        .reactor
                        .wait_readable(self.handle.0, Duration::from_secs(300))
                        .await
                    {
                        Ok(()) | Err(VclError::Timeout) => {} // re-arm
                        Err(e) => return Err(e),
                    }
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

/// One-shot synchronous TCP DNS query on the caller's thread: open
/// a fresh TCP session to `peer`, send `query` with the RFC 1035
/// 2-byte length prefix, read the response, close.
///
/// Same design as `query_udp_sync` — intended for `spawn_blocking`
/// so the whole connection lifecycle (including the VCL cleanup in
/// drop) runs off the tokio main thread. Returns the raw DNS body
/// without the length prefix.
pub fn query_tcp_dns_sync(
    peer: SocketAddr,
    source: Option<std::net::IpAddr>,
    query: &[u8],
    timeout: Duration,
) -> Result<Vec<u8>> {
    crate::app::register_worker_thread();

    let handle = SessionHandle::create_tcp(true)?;
    // Bind a source-IP-with-ephemeral-port BEFORE connect when the
    // caller specified one. Connect() will then preserve it. Without
    // this VPP picks (or fails to pick) a source via FIB lookup —
    // which works on most setups but emits packets with src=0.0.0.0
    // on some (multi-interface, no default-route-to-peer, etc.).
    if let Some(ip) = source {
        if ip.is_ipv4() != peer.is_ipv4() {
            return Err(VclError::Api(
                format!("source IP family ({ip}) doesn't match peer ({peer})"),
                -1,
            ));
        }
        use rand::Rng;
        const LOW: u16 = 32768;
        const HIGH: u16 = 60999;
        let mut rng = rand::thread_rng();
        let mut bound = false;
        for _ in 0..8 {
            let port: u16 = rng.gen_range(LOW..=HIGH);
            if handle.bind(SocketAddr::new(ip, port)).is_ok() {
                bound = true;
                break;
            }
        }
        if !bound {
            return Err(VclError::Api(
                "ephemeral source bind exhausted".into(),
                -1,
            ));
        }
    }
    // Non-blocking VCL TCP connect returns WouldBlock immediately
    // (the session is in CONNECT state, SYN sent, waiting on SYN+ACK).
    // That's normal — the write loop below polls until the session
    // becomes writable. Treat WouldBlock here as "in progress",
    // bubble up anything else.
    match handle.connect(peer) {
        Ok(()) | Err(VclError::WouldBlock) => {}
        Err(e) => return Err(e),
    }

    let deadline = Instant::now() + timeout;

    // Wait for connect to complete — non-blocking connect returns
    // immediately but the session isn't writable until the SYN+ACK
    // lands. Poll `write` to detect readiness.
    let mut framed = Vec::with_capacity(2 + query.len());
    framed.extend_from_slice(&(query.len() as u16).to_be_bytes());
    framed.extend_from_slice(query);

    let mut sent = 0;
    while sent < framed.len() {
        match handle.write(&framed[sent..]) {
            Ok(n) => sent += n,
            // WouldBlock = TX FIFO full; NotConnected = TCP handshake
            // hasn't completed yet (session is in CONNECT state, not
            // CONNECTED). Both are transient — sleep and retry.
            Err(VclError::WouldBlock) | Err(VclError::NotConnected) => {
                if Instant::now() >= deadline {
                    return Err(VclError::Timeout);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => return Err(e),
        }
    }

    // Read the 2-byte length prefix.
    let mut lenbuf = [0u8; 2];
    let mut got = 0;
    while got < 2 {
        if Instant::now() >= deadline {
            return Err(VclError::Timeout);
        }
        match handle.read(&mut lenbuf[got..]) {
            Ok(0) => return Err(VclError::Closed),
            Ok(n) => got += n,
            // NotConnected during read shouldn't happen post-write
            // (the session must've been CONNECTED to send), but
            // belt-and-suspenders: VPP can briefly report it during
            // close-state transitions.
            Err(VclError::WouldBlock) | Err(VclError::NotConnected) => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => return Err(e),
        }
    }
    let len = u16::from_be_bytes(lenbuf) as usize;
    if len == 0 {
        return Err(VclError::Api("zero-length DNS response".into(), -1));
    }

    // Read the body.
    let mut resp = vec![0u8; len];
    let mut filled = 0;
    while filled < len {
        if Instant::now() >= deadline {
            return Err(VclError::Timeout);
        }
        match handle.read(&mut resp[filled..]) {
            Ok(0) => return Err(VclError::Closed),
            Ok(n) => filled += n,
            Err(VclError::WouldBlock) | Err(VclError::NotConnected) => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => return Err(e),
        }
    }

    Ok(resp)
}

/// One-shot synchronous UDP query: create a fresh ephemeral session,
/// send `query` to `peer`, busy-poll for the first response up to
/// `timeout`, and close.
///
/// Designed to be called from `tokio::task::spawn_blocking`. Keeps
/// the session's whole lifecycle on one thread so VCL's
/// worker-per-thread invariant holds (cross-thread session access
/// returns VPPCOM_EBADFD). Also sidesteps the main tokio thread's
/// `vppcom_session_listen`→`usleep` chain — the blocking pool has
/// its own threads, so many of these can run in parallel without
/// starving the reactor.
///
/// Does NOT take a VclReactor. The reactor is for async session I/O
/// and this function does busy-wait I/O on the caller's thread.
pub fn query_udp_sync(
    peer: SocketAddr,
    source: Option<std::net::IpAddr>,
    query: &[u8],
    timeout: Duration,
) -> Result<(Vec<u8>, SocketAddr)> {
    use rand::Rng;
    const LOW: u16 = 32768;
    const HIGH: u16 = 60999;

    // Ensure the thread is a VCL worker. Idempotent on the safe
    // wrapper (thread-local short-circuit) and serialized via a
    // process-wide mutex so we can never race the libvppcom worker-
    // pool growth path against a concurrent session_create.
    crate::app::register_worker_thread();

    let handle = SessionHandle::create_udp(true)?;

    // VPP 25.10 doesn't auto-assign a source port on `:0` bind —
    // outgoing packets carry source port 0 which real upstreams drop.
    // Pick a random ephemeral port and retry on collision.
    //
    // Source IP: when the caller passes Some(ip), bind to it. Some
    // VPP/VCL configurations (notably routers without a default
    // route towards the upstream's prefix, or multi-interface setups
    // where the FIB-derived source isn't unambiguous) emit packets
    // with src=0.0.0.0 when bound to the wildcard. Real DNS
    // upstreams drop those silently. Caller-supplied source IP must
    // match the peer's address family.
    let bind_ip: std::net::IpAddr = match source {
        Some(ip) => {
            if ip.is_ipv4() != peer.is_ipv4() {
                return Err(VclError::Api(
                    format!(
                        "source IP family ({ip}) doesn't match peer family ({peer})"
                    ),
                    -1,
                ));
            }
            ip
        }
        None if peer.is_ipv4() => "0.0.0.0".parse().unwrap(),
        None => "::".parse().unwrap(),
    };
    let mut rng = rand::thread_rng();
    let mut bound = false;
    for _ in 0..8 {
        let port: u16 = rng.gen_range(LOW..=HIGH);
        let addr = SocketAddr::new(bind_ip, port);
        if handle.bind(addr).is_ok() {
            bound = true;
            break;
        }
    }
    if !bound {
        return Err(VclError::Api("ephemeral bind exhausted".into(), -1));
    }

    handle.listen(0)?;
    handle.sendto(query, peer)?;

    // Busy-poll for the response. `recvfrom` on a non-blocking session
    // returns WouldBlock when the FIFO is empty. We sleep in 5 ms
    // increments — the OS thread yields, giving any main-thread tokio
    // work time to proceed, while we wait on our response.
    let deadline = Instant::now() + timeout;
    let mut buf = vec![0u8; 4096];
    loop {
        match handle.recvfrom(&mut buf) {
            Ok((n, from)) => {
                buf.truncate(n);
                return Ok((buf, from));
            }
            Err(VclError::WouldBlock) => {
                if Instant::now() >= deadline {
                    return Err(VclError::Timeout);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => return Err(e),
        }
    }
}

impl Drop for VclDgramSocket {
    fn drop(&mut self) {
        // Must deregister from the reactor BEFORE the SessionHandle
        // drops and closes the session. Without this the reactor's
        // `waiters` map retains an entry for every ephemeral socket
        // the recursor spins up, piling up under load until mutex
        // contention wedges the runtime.
        self.reactor.deregister(self.handle.0);
    }
}

/// Ask VPP what local source IP it would pick when sending UDP to
/// `peer`. Useful for auto-detecting an outgoing source — particularly
/// for IPv6, where there's no NAT to translate the source for us so
/// the bound IP must be something globally routable that VPP's FIB
/// agrees with. v4 callers usually want the LAN-side IP (so NAT can
/// translate cleanly) rather than what VPP would pick natively, so
/// this is mostly an IPv6 helper in practice.
///
/// Implementation: create a temp UDP session, bind to the wildcard
/// for `peer`'s family, `sendto` a one-byte probe (forces VPP to do
/// route lookup + source selection), then read the session's local
/// addr. Closes the session before returning.
///
/// Cost: leaks one libvppcom shared-memory FIFO segment (~128 MB
/// virtual) per call, same as `query_udp_sync`. Intended for
/// one-shot startup probes only — do NOT call in a hot path.
///
/// The probe byte itself is one UDP datagram to `peer`. Pick a
/// destination you don't mind sending stray packets to (e.g. a root
/// NS or a public DNS resolver).
pub fn probe_local_source(peer: SocketAddr) -> Result<SocketAddr> {
    crate::app::register_worker_thread();
    // Blocking session so `connect` settles synchronously. VPP's
    // non-blocking UDP `connect` returns `WouldBlock` on the first
    // call and would need its own busy-poll loop; for a one-shot
    // startup probe a blocking session is simpler.
    let handle = SessionHandle::create_udp(false)?;

    let bind_ip: IpAddr = if peer.is_ipv4() {
        "0.0.0.0".parse().unwrap()
    } else {
        "::".parse().unwrap()
    };

    use rand::Rng;
    const LOW: u16 = 32768;
    const HIGH: u16 = 60999;
    let mut rng = rand::thread_rng();
    let mut bound = false;
    for _ in 0..8 {
        let port: u16 = rng.gen_range(LOW..=HIGH);
        if handle.bind(SocketAddr::new(bind_ip, port)).is_ok() {
            bound = true;
            break;
        }
    }
    if !bound {
        return Err(VclError::Api("probe: ephemeral bind exhausted".into(), -1));
    }

    // `connect` on a UDP session triggers VPP's route lookup +
    // source-address selection without actually putting a packet on
    // the wire. After this returns, GET_LCL_ADDR should reflect
    // VPP's chosen source.
    handle.connect(peer)?;

    handle.local_addr()
}

/// Long-lived UDP socket for clients that need many sendto/recvfrom
/// operations to different peers — e.g. a DNS recursor's upstream
/// query path. Avoids the per-query session-creation churn of
/// `query_udp_sync`, which leaks libvppcom shared-memory FIFO
/// segments (each new VCL session allocates a 128 MB shared segment
/// from VPP's session layer that is NOT reclaimed on
/// `vppcom_session_close`; ~130 ephemeral sessions OOMs the host).
///
/// This is a SYNC primitive intended for `std::thread`-based callers
/// (not tokio); it busy-polls `recvfrom` like `query_udp_sync` does
/// and shares the same worker-per-thread invariant. Bind and use the
/// socket on the same OS thread, drop on the same thread.
///
/// Trade-off: source-port randomisation is per-socket (per worker
/// thread), not per-query. A recursor with N worker threads gets
/// N distinct source ports across all upstream queries, which is
/// less Kaminsky entropy than random-per-query but still meaningful
/// — the 16-bit TXID + 0x20 case randomisation remain on every
/// query. Increase the worker pool if you want more port diversity.
pub struct VclUdpSyncSocket {
    handle: SessionHandle,
}

impl VclUdpSyncSocket {
    /// Bind a UDP socket to a random ephemeral port on a local
    /// source IP. Source family must match `is_v6`. When `source`
    /// is None, binds the wildcard for the family (`0.0.0.0` or
    /// `::`) — same caveat as `query_udp_sync`: VPP's FIB-based
    /// source selection works on simple setups and emits packets
    /// with src=0 on others.
    pub fn bind(source: Option<IpAddr>, is_v6: bool) -> Result<Self> {
        crate::app::register_worker_thread();
        let handle = SessionHandle::create_udp(true)?;

        let bind_ip: IpAddr = match (source, is_v6) {
            (Some(ip @ IpAddr::V4(_)), false) => ip,
            (Some(ip @ IpAddr::V6(_)), true) => ip,
            (Some(ip), _) => {
                return Err(VclError::Api(
                    format!("source {ip} family doesn't match is_v6={is_v6}"),
                    -1,
                ))
            }
            (None, false) => "0.0.0.0".parse().unwrap(),
            (None, true) => "::".parse().unwrap(),
        };

        use rand::Rng;
        const LOW: u16 = 32768;
        const HIGH: u16 = 60999;
        let mut rng = rand::thread_rng();
        let mut bound = false;
        for _ in 0..8 {
            let port: u16 = rng.gen_range(LOW..=HIGH);
            if handle.bind(SocketAddr::new(bind_ip, port)).is_ok() {
                bound = true;
                break;
            }
        }
        if !bound {
            return Err(VclError::Api("ephemeral bind exhausted".into(), -1));
        }
        handle.listen(0)?;
        Ok(Self { handle })
    }

    /// The local address VPP picked for this socket. Useful for
    /// logging the actual ephemeral port so operator captures can
    /// match.
    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.handle.local_addr()
    }

    /// Send `query` to `peer` and busy-poll for a response from
    /// the same peer IP, up to `timeout`. Stray responses (e.g.
    /// late arrivals from a previous query that timed out on this
    /// socket) are discarded.
    pub fn query(
        &self,
        peer: SocketAddr,
        query: &[u8],
        timeout: Duration,
    ) -> Result<(Vec<u8>, SocketAddr)> {
        self.handle.sendto(query, peer)?;

        let deadline = Instant::now() + timeout;
        let mut buf = vec![0u8; 4096];
        loop {
            match self.handle.recvfrom(&mut buf) {
                Ok((n, from)) => {
                    if from.ip() != peer.ip() {
                        // Stray response from a different peer —
                        // probably a late arrival from a previous
                        // query on this same long-lived socket.
                        // Discard and keep waiting for ours.
                        continue;
                    }
                    let mut out = Vec::with_capacity(n);
                    out.extend_from_slice(&buf[..n]);
                    return Ok((out, from));
                }
                Err(VclError::WouldBlock) => {
                    if Instant::now() >= deadline {
                        return Err(VclError::Timeout);
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(e) => return Err(e),
            }
        }
    }
}
