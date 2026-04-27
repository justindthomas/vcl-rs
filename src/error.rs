//! VCL error types.

use std::ffi::CStr;

#[derive(Debug, thiserror::Error)]
pub enum VclError {
    #[error("VCL call failed: {0} (rc={1})")]
    Api(String, i32),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("session closed")]
    Closed,
    #[error("not connected")]
    NotConnected,
    #[error("would block")]
    WouldBlock,
    #[error("timed out")]
    Timeout,
}

impl VclError {
    pub fn from_rc(rc: i32) -> Self {
        if rc == -libc::EWOULDBLOCK || rc == -libc::EAGAIN || rc == -libc::EINPROGRESS {
            return VclError::WouldBlock;
        }
        // ENOTCONN is distinct from Closed: VPP returns it on read/write
        // against a TCP session whose 3-way handshake hasn't completed
        // yet (state CONNECT). Lumping it with Closed (ECONNRESET/EPIPE)
        // makes the non-blocking TCP connect path look like a hard
        // failure when it just needs more polling. Callers in the
        // poll loop should treat NotConnected like WouldBlock.
        if rc == -libc::ENOTCONN {
            return VclError::NotConnected;
        }
        if rc == -libc::ECONNRESET || rc == -libc::EPIPE {
            return VclError::Closed;
        }
        let msg = vcl_retval_str(rc);
        VclError::Api(msg, rc)
    }
}

fn vcl_retval_str(rc: i32) -> String {
    unsafe {
        let ptr = crate::ffi::vppcom_retval_str(rc);
        if ptr.is_null() {
            return format!("unknown error {}", rc);
        }
        CStr::from_ptr(ptr).to_string_lossy().into_owned()
    }
}

pub type Result<T> = std::result::Result<T, VclError>;
