//! VCL application lifecycle.
//!
//! A `VclApp` must be created once per process before any sessions
//! can be opened. It calls `vppcom_app_create` which connects to
//! VPP's session layer via the app-socket-api, sets up shared
//! memory segments for FIFOs, and creates worker-0.
//!
//! Configuration is read from `VCL_CONFIG` env var (defaults to
//! `/etc/vpp/vcl.conf`). The config tells VCL where VPP's
//! app-socket-api socket lives.
//!
//! Drop calls `vppcom_app_destroy`.

use std::ffi::CString;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::{Result, VclError};
use crate::ffi;

static INITIALIZED: AtomicBool = AtomicBool::new(false);

pub struct VclApp {
    _private: (),
}

impl VclApp {
    /// Initialize VCL. Must be called exactly once per process.
    /// Reads config from `VCL_CONFIG` env var or `/etc/vpp/vcl.conf`.
    pub fn init(app_name: &str) -> Result<Self> {
        if INITIALIZED.swap(true, Ordering::SeqCst) {
            return Err(VclError::Api("VCL already initialized".into(), -1));
        }
        let c_name =
            CString::new(app_name).map_err(|_| VclError::Api("invalid app name".into(), -1))?;
        let rc = unsafe { ffi::vppcom_app_create(c_name.as_ptr()) };
        if rc < 0 {
            INITIALIZED.store(false, Ordering::SeqCst);
            return Err(VclError::from_rc(rc));
        }
        tracing::info!(app_name, "VCL application created");
        Ok(VclApp { _private: () })
    }

    /// Get the message-queue eventfd for Tokio integration.
    /// Register this FD with `tokio::io::unix::AsyncFd` to wake
    /// when VCL sessions have events ready.
    pub fn mq_epoll_fd(&self) -> i32 {
        unsafe { ffi::vppcom_mq_epoll_fd() }
    }
}

impl Drop for VclApp {
    fn drop(&mut self) {
        unsafe {
            ffi::vppcom_app_destroy();
        }
        INITIALIZED.store(false, Ordering::SeqCst);
        tracing::info!("VCL application destroyed");
    }
}
