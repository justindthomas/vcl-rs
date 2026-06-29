//! VCL session: the low-level handle wrapping a single
//! `vppcom_session_*` lifecycle. Both `VclListener` and `VclStream`
//! are thin wrappers around this.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use crate::error::{Result, VclError};
use crate::ffi;

/// Raw session handle returned by `vppcom_session_create`.
#[derive(Debug)]
pub(crate) struct SessionHandle(pub u32);

impl SessionHandle {
    pub fn create_tcp(nonblocking: bool) -> Result<Self> {
        let rc = unsafe { ffi::vppcom_session_create(ffi::VPPCOM_PROTO_TCP, nonblocking as u8) };
        if rc < 0 {
            return Err(VclError::from_rc(rc));
        }
        Ok(SessionHandle(rc as u32))
    }

    pub fn create_udp(nonblocking: bool) -> Result<Self> {
        let rc = unsafe { ffi::vppcom_session_create(ffi::VPPCOM_PROTO_UDP, nonblocking as u8) };
        if rc < 0 {
            return Err(VclError::from_rc(rc));
        }
        Ok(SessionHandle(rc as u32))
    }

    pub fn close(&self) -> Result<()> {
        let rc = unsafe { ffi::vppcom_session_close(self.0) };
        if rc < 0 {
            return Err(VclError::from_rc(rc));
        }
        Ok(())
    }

    pub fn bind(&self, addr: SocketAddr) -> Result<()> {
        let mut ip_buf = [0u8; 16];
        let mut ep = endpoint_into_buf(addr, &mut ip_buf);
        let rc = unsafe { ffi::vppcom_session_bind(self.0, &mut ep) };
        if rc < 0 {
            return Err(VclError::from_rc(rc));
        }
        Ok(())
    }

    pub fn listen(&self, backlog: u32) -> Result<()> {
        let rc = unsafe { ffi::vppcom_session_listen(self.0, backlog) };
        if rc < 0 {
            return Err(VclError::from_rc(rc));
        }
        Ok(())
    }

    pub fn accept(&self) -> Result<(SessionHandle, SocketAddr)> {
        let mut ip_buf = [0u8; 16];
        let mut ep = ffi::vppcom_endpt_t {
            ip: ip_buf.as_mut_ptr(),
            ..Default::default()
        };
        let rc = unsafe { ffi::vppcom_session_accept(self.0, &mut ep, 0) };
        if rc < 0 {
            return Err(VclError::from_rc(rc));
        }
        // For a cut-through (same-VPP app-to-app) session the peer
        // endpoint comes back unspecified — there is no IP 5-tuple.
        // That is faithful, not an error; callers distinguish it with
        // `SocketAddr::ip().is_unspecified()`.
        let addr = addr_from_endpoint(&ep, &ip_buf);
        Ok((SessionHandle(rc as u32), addr))
    }

    pub fn connect(&self, addr: SocketAddr) -> Result<()> {
        let mut ip_buf = [0u8; 16];
        let mut ep = endpoint_into_buf(addr, &mut ip_buf);
        let rc = unsafe { ffi::vppcom_session_connect(self.0, &mut ep) };
        if rc < 0 {
            return Err(VclError::from_rc(rc));
        }
        Ok(())
    }

    pub fn read(&self, buf: &mut [u8]) -> Result<usize> {
        let rc = unsafe {
            ffi::vppcom_session_read(self.0, buf.as_mut_ptr() as *mut _, buf.len())
        };
        if rc < 0 {
            return Err(VclError::from_rc(rc));
        }
        Ok(rc as usize)
    }

    pub fn write(&self, buf: &[u8]) -> Result<usize> {
        let rc =
            unsafe { ffi::vppcom_session_write(self.0, buf.as_ptr() as *mut _, buf.len()) };
        if rc < 0 {
            return Err(VclError::from_rc(rc));
        }
        Ok(rc as usize)
    }

    /// UDP receive. Returns (bytes_received, peer_addr).
    ///
    /// One call yields one datagram — partial reads never happen for UDP.
    /// VCL fills `ep` with the sender's address; we copy it out before
    /// the endpoint goes out of scope.
    pub fn recvfrom(&self, buf: &mut [u8]) -> Result<(usize, SocketAddr)> {
        let mut ip_buf = [0u8; 16];
        let mut ep = ffi::vppcom_endpt_t {
            ip: ip_buf.as_mut_ptr(),
            ..Default::default()
        };
        let rc = unsafe {
            ffi::vppcom_session_recvfrom(
                self.0,
                buf.as_mut_ptr() as *mut _,
                buf.len() as u32,
                0,
                &mut ep,
            )
        };
        if rc < 0 {
            return Err(VclError::from_rc(rc));
        }
        let addr = addr_from_endpoint(&ep, &ip_buf);
        Ok((rc as usize, addr))
    }

    /// UDP send. Returns bytes written.
    pub fn sendto(&self, buf: &[u8], dst: SocketAddr) -> Result<usize> {
        let mut ip_buf = [0u8; 16];
        let mut ep = endpoint_into_buf(dst, &mut ip_buf);
        let rc = unsafe {
            ffi::vppcom_session_sendto(
                self.0,
                buf.as_ptr() as *mut _,
                buf.len() as u32,
                0,
                &mut ep,
            )
        };
        if rc < 0 {
            return Err(VclError::from_rc(rc));
        }
        Ok(rc as usize)
    }

    pub fn set_nodelay(&self) -> Result<()> {
        let val: u32 = 1;
        let mut len = std::mem::size_of::<u32>() as u32;
        let rc = unsafe {
            ffi::vppcom_session_attr(
                self.0,
                ffi::VPPCOM_ATTR_SET_TCP_NODELAY,
                &val as *const _ as *mut _,
                &mut len,
            )
        };
        if rc < 0 {
            return Err(VclError::from_rc(rc));
        }
        Ok(())
    }

    /// Put the session into non-blocking mode.
    ///
    /// Load-bearing for accepted sessions: `vppcom_session_accept`
    /// produces a session that inherits *blocking* semantics, and a
    /// blocking `vppcom_session_read` parks the calling OS thread
    /// inside libvppcom until data arrives. Since every vcl-io
    /// worker is a `current_thread` tokio runtime, one blocking
    /// read on an idle keep-alive connection freezes the entire
    /// worker — every other task on it (sibling connections, the
    /// reactor tick) stalls until that one client happens to send.
    /// Non-blocking mode makes `read` return `WouldBlock` instead,
    /// so `poll_read` can park the *task* on the reactor and free
    /// the thread.
    ///
    /// libvppcom's `VPPCOM_ATTR_SET_FLAGS` reads the flags word the
    /// same way the kernel does; `O_NONBLOCK` is `0o4000` on Linux.
    pub fn set_nonblocking(&self) -> Result<()> {
        const O_NONBLOCK: u32 = 0o4000;
        let val: u32 = O_NONBLOCK;
        let mut len = std::mem::size_of::<u32>() as u32;
        let rc = unsafe {
            ffi::vppcom_session_attr(
                self.0,
                ffi::VPPCOM_ATTR_SET_FLAGS,
                &val as *const _ as *mut _,
                &mut len,
            )
        };
        if rc < 0 {
            return Err(VclError::from_rc(rc));
        }
        Ok(())
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        let mut ip_buf = [0u8; 16];
        let mut ep = ffi::vppcom_endpt_t {
            ip: ip_buf.as_mut_ptr(),
            ..Default::default()
        };
        let mut len = std::mem::size_of::<ffi::vppcom_endpt_t>() as u32;
        let rc = unsafe {
            ffi::vppcom_session_attr(
                self.0,
                ffi::VPPCOM_ATTR_GET_LCL_ADDR,
                &mut ep as *mut _ as *mut _,
                &mut len,
            )
        };
        if rc < 0 {
            return Err(VclError::from_rc(rc));
        }
        Ok(addr_from_endpoint(&ep, &ip_buf))
    }

    pub fn peer_addr(&self) -> Result<SocketAddr> {
        let mut ip_buf = [0u8; 16];
        let mut ep = ffi::vppcom_endpt_t {
            ip: ip_buf.as_mut_ptr(),
            ..Default::default()
        };
        let mut len = std::mem::size_of::<ffi::vppcom_endpt_t>() as u32;
        let rc = unsafe {
            ffi::vppcom_session_attr(
                self.0,
                ffi::VPPCOM_ATTR_GET_PEER_ADDR,
                &mut ep as *mut _ as *mut _,
                &mut len,
            )
        };
        if rc < 0 {
            return Err(VclError::from_rc(rc));
        }
        Ok(addr_from_endpoint(&ep, &ip_buf))
    }
}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        let _ = unsafe { ffi::vppcom_session_close(self.0) };
    }
}

/// Build a `vppcom_endpt_t` that borrows IP bytes from a caller-owned
/// 16-byte buffer. The buffer must outlive the returned endpoint —
/// VCL reads through the `ip` pointer during the FFI call, so a
/// stack array in the same function frame is the typical pattern.
///
/// IPv4 addresses populate the first 4 bytes; IPv6 fills all 16. The
/// caller need not zero `ip_buf` first (we overwrite the prefix used
/// by `is_ip4`), but a freshly-zeroed buffer is harmless and is what
/// every callsite in this crate hands in.
pub fn endpoint_into_buf(addr: SocketAddr, ip_buf: &mut [u8; 16]) -> ffi::vppcom_endpt_t {
    let is_ip4 = match addr.ip() {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            ip_buf[..4].copy_from_slice(&octets);
            1u8
        }
        IpAddr::V6(v6) => {
            ip_buf.copy_from_slice(&v6.octets());
            0u8
        }
    };
    ffi::vppcom_endpt_t {
        unused: 0,
        is_ip4,
        ip: ip_buf.as_mut_ptr(),
        port: addr.port().to_be(),
        unused2: 0,
        app_tlv_len: 0,
        app_tlvs: std::ptr::null_mut(),
    }
}

fn addr_from_endpoint(ep: &ffi::vppcom_endpt_t, ip_buf: &[u8; 16]) -> SocketAddr {
    let port = u16::from_be(ep.port);
    if ep.is_ip4 != 0 {
        let mut octets = [0u8; 4];
        octets.copy_from_slice(&ip_buf[..4]);
        SocketAddr::new(IpAddr::V4(Ipv4Addr::from(octets)), port)
    } else {
        SocketAddr::new(IpAddr::V6(Ipv6Addr::from(*ip_buf)), port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(addr: SocketAddr, want_is_ip4: u8) {
        let mut ip_buf = [0u8; 16];
        let ep = endpoint_into_buf(addr, &mut ip_buf);
        assert_eq!(ep.is_ip4, want_is_ip4, "is_ip4 flag for {addr}");
        // Port rides the wire big-endian.
        assert_eq!(ep.port, addr.port().to_be(), "BE port for {addr}");
        let back = addr_from_endpoint(&ep, &ip_buf);
        assert_eq!(back, addr, "endpoint round-trip for {addr}");
    }

    #[test]
    fn endpoint_roundtrips_v4_and_v6() {
        // A wrong is_ip4 flag, mis-sized address copy, or byte-swapped
        // port silently sends datagrams to the wrong host (or a port-0
        // packet upstreams drop). Pin the marshalling both directions.
        roundtrip("192.0.2.7:179".parse().unwrap(), 1);
        roundtrip("[2001:db8::1]:53".parse().unwrap(), 0);
        // High port exercises both big-endian bytes.
        roundtrip("203.0.113.9:65000".parse().unwrap(), 1);
    }
}
