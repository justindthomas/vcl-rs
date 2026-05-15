//! VCL session: the low-level handle wrapping a single VLS-managed
//! session lifecycle. Both `VclListener` and `VclStream` are thin
//! wrappers around this.
//!
//! Under VLS, each `vls_handle_t` is an entry in the VLS pool that
//! internally points at the underlying vppcom session. Every op
//! takes the VLS process-wide lock for the duration of the call,
//! so sessions are safe to use from any thread.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use crate::error::{Result, VclError};
use crate::ffi;

/// VLS handle wrapper. The underlying VLS API returns `vls_handle_t`
/// (signed) and uses negative values for errors; we validate at
/// construction so the stored `u32` is always a real, positive
/// handle. All FFI calls cast back to `vls_handle_t` at the
/// boundary. Keeping `u32` here means the reactor's
/// register-by-handle API (which has always taken `u32`) stays
/// stable across the libvppcom → VLS port.
#[derive(Debug)]
pub(crate) struct SessionHandle(pub u32);

impl SessionHandle {
    /// Convenience: the VLS-typed view of the inner handle for FFI calls.
    fn vlsh(&self) -> ffi::vls_handle_t {
        self.0 as ffi::vls_handle_t
    }

    pub fn create_tcp(nonblocking: bool) -> Result<Self> {
        let rc = unsafe { ffi::vls_create(ffi::VPPCOM_PROTO_TCP, nonblocking as u8) };
        if rc < 0 {
            return Err(VclError::from_rc(rc));
        }
        Ok(SessionHandle(rc as u32))
    }

    pub fn create_udp(nonblocking: bool) -> Result<Self> {
        let rc = unsafe { ffi::vls_create(ffi::VPPCOM_PROTO_UDP, nonblocking as u8) };
        if rc < 0 {
            return Err(VclError::from_rc(rc));
        }
        Ok(SessionHandle(rc as u32))
    }

    pub fn close(&self) -> Result<()> {
        let rc = unsafe { ffi::vls_close(self.vlsh()) };
        if rc < 0 {
            return Err(VclError::from_rc(rc));
        }
        Ok(())
    }

    pub fn bind(&self, addr: SocketAddr) -> Result<()> {
        let mut ip_buf = [0u8; 16];
        let mut ep = endpoint_into_buf(addr, &mut ip_buf);
        let rc = unsafe { ffi::vls_bind(self.vlsh(), &mut ep) };
        if rc < 0 {
            return Err(VclError::from_rc(rc));
        }
        Ok(())
    }

    pub fn listen(&self, backlog: u32) -> Result<()> {
        let rc = unsafe { ffi::vls_listen(self.vlsh(), backlog as std::os::raw::c_int) };
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
        let rc = unsafe { ffi::vls_accept(self.vlsh(), &mut ep, 0) };
        if rc < 0 {
            return Err(VclError::from_rc(rc));
        }
        let addr = addr_from_endpoint(&ep, &ip_buf);
        Ok((SessionHandle(rc as u32), addr))
    }

    pub fn connect(&self, addr: SocketAddr) -> Result<()> {
        let mut ip_buf = [0u8; 16];
        let mut ep = endpoint_into_buf(addr, &mut ip_buf);
        let rc = unsafe { ffi::vls_connect(self.vlsh(), &mut ep) };
        if rc < 0 {
            return Err(VclError::from_rc(rc));
        }
        Ok(())
    }

    pub fn read(&self, buf: &mut [u8]) -> Result<usize> {
        let rc =
            unsafe { ffi::vls_read(self.vlsh(), buf.as_mut_ptr() as *mut _, buf.len()) };
        if rc < 0 {
            return Err(VclError::from_rc(rc as std::os::raw::c_int));
        }
        Ok(rc as usize)
    }

    pub fn write(&self, buf: &[u8]) -> Result<usize> {
        let rc =
            unsafe { ffi::vls_write(self.vlsh(), buf.as_ptr() as *mut _, buf.len()) };
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
            ffi::vls_recvfrom(
                self.vlsh(),
                buf.as_mut_ptr() as *mut _,
                buf.len() as u32,
                0,
                &mut ep,
            )
        };
        if rc < 0 {
            return Err(VclError::from_rc(rc as std::os::raw::c_int));
        }
        let addr = addr_from_endpoint(&ep, &ip_buf);
        Ok((rc as usize, addr))
    }

    /// UDP send. Returns bytes written.
    pub fn sendto(&self, buf: &[u8], dst: SocketAddr) -> Result<usize> {
        let mut ip_buf = [0u8; 16];
        let mut ep = endpoint_into_buf(dst, &mut ip_buf);
        let rc = unsafe {
            ffi::vls_sendto(
                self.vlsh(),
                buf.as_ptr() as *mut _,
                buf.len() as std::os::raw::c_int,
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
            ffi::vls_attr(
                self.vlsh(),
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

    pub fn local_addr(&self) -> Result<SocketAddr> {
        let mut ip_buf = [0u8; 16];
        let mut ep = ffi::vppcom_endpt_t {
            ip: ip_buf.as_mut_ptr(),
            ..Default::default()
        };
        let mut len = std::mem::size_of::<ffi::vppcom_endpt_t>() as u32;
        let rc = unsafe {
            ffi::vls_attr(
                self.vlsh(),
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
            ffi::vls_attr(
                self.vlsh(),
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
        let _ = unsafe { ffi::vls_close(self.vlsh()) };
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
