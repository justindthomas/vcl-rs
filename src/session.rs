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

    pub fn close(&self) -> Result<()> {
        let rc = unsafe { ffi::vppcom_session_close(self.0) };
        if rc < 0 {
            return Err(VclError::from_rc(rc));
        }
        Ok(())
    }

    pub fn bind(&self, addr: SocketAddr) -> Result<()> {
        let mut ep = endpoint_from_addr(addr);
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
        let addr = addr_from_endpoint(&ep, &ip_buf);
        Ok((SessionHandle(rc as u32), addr))
    }

    pub fn connect(&self, addr: SocketAddr) -> Result<()> {
        let mut ep = endpoint_from_addr(addr);
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

/// Build a `vppcom_endpt_t` from a Rust `SocketAddr`. The caller
/// must keep `addr_buf` alive for the lifetime of the returned
/// endpoint (the `ip` pointer points into it).
pub fn endpoint_from_addr(addr: SocketAddr) -> ffi::vppcom_endpt_t {
    // We leak a small allocation here for the IP bytes. This is
    // acceptable because endpoints are short-lived (used for one
    // bind/connect call then dropped). A production version could
    // use a stack buffer, but the FFI boundary makes lifetimes
    // awkward.
    let (is_ip4, ip_ptr) = match addr.ip() {
        IpAddr::V4(v4) => {
            let bytes = Box::leak(Box::new(v4.octets()));
            (1u8, bytes.as_mut_ptr())
        }
        IpAddr::V6(v6) => {
            let bytes = Box::leak(Box::new(v6.octets()));
            (0u8, bytes.as_mut_ptr())
        }
    };
    ffi::vppcom_endpt_t {
        unused: 0,
        is_ip4,
        ip: ip_ptr,
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
