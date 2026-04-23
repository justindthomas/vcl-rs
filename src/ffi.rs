//! Raw FFI bindings for VPP's VCL (libvppcom).
//!
//! These are the C function signatures from `vppcom.h`. The safe
//! Rust wrappers live in sibling modules; this module is `pub(crate)`
//! so nothing outside `vcl-rs` touches raw pointers directly.
//!
//! Link against `libvppcom.so` (installed by VPP packages).

#![allow(non_camel_case_types, dead_code)]

use std::os::raw::{c_char, c_int, c_void};

pub const VPPCOM_PROTO_TCP: u8 = 0;
pub const VPPCOM_PROTO_UDP: u8 = 1;

/// Endpoint descriptor passed to bind/connect/accept.
#[repr(C)]
pub struct vppcom_endpt_t {
    pub unused: u8,
    pub is_ip4: u8,
    pub ip: *mut u8,
    pub port: u16, // network byte order
    pub unused2: u64,
    pub app_tlv_len: u32,
    pub app_tlvs: *mut c_void,
}

impl Default for vppcom_endpt_t {
    fn default() -> Self {
        vppcom_endpt_t {
            unused: 0,
            is_ip4: 1,
            ip: std::ptr::null_mut(),
            port: 0,
            unused2: 0,
            app_tlv_len: 0,
            app_tlvs: std::ptr::null_mut(),
        }
    }
}

// Session attribute operations.
pub const VPPCOM_ATTR_GET_FLAGS: u32 = 2;
pub const VPPCOM_ATTR_SET_FLAGS: u32 = 3;
pub const VPPCOM_ATTR_GET_LCL_ADDR: u32 = 4;
pub const VPPCOM_ATTR_GET_PEER_ADDR: u32 = 6;
pub const VPPCOM_ATTR_SET_TCP_NODELAY: u32 = 27;

// Epoll operations (same values as Linux).
pub const EPOLL_CTL_ADD: c_int = 1;
pub const EPOLL_CTL_DEL: c_int = 2;
pub const EPOLL_CTL_MOD: c_int = 3;

// Epoll event flags.
pub const EPOLLIN: u32 = 0x001;
pub const EPOLLOUT: u32 = 0x004;
pub const EPOLLET: u32 = 1 << 31;
pub const EPOLLHUP: u32 = 0x010;
pub const EPOLLRDHUP: u32 = 0x2000;

/// Mirrors `struct epoll_event` from Linux.
#[repr(C, packed)]
#[derive(Copy, Clone)]
pub struct epoll_event {
    pub events: u32,
    pub data: u64,
}

#[link(name = "vppcom")]
extern "C" {
    // --- Application lifecycle ---
    pub fn vppcom_app_create(app_name: *const c_char) -> c_int;
    pub fn vppcom_app_destroy();

    // --- Session lifecycle ---
    pub fn vppcom_session_create(proto: u8, is_nonblocking: u8) -> c_int;
    pub fn vppcom_session_close(session_handle: u32) -> c_int;
    pub fn vppcom_session_shutdown(session_handle: u32, how: c_int) -> c_int;

    // --- Server path ---
    pub fn vppcom_session_bind(session_handle: u32, ep: *mut vppcom_endpt_t) -> c_int;
    pub fn vppcom_session_listen(session_handle: u32, q_len: u32) -> c_int;
    pub fn vppcom_session_accept(
        session_handle: u32,
        client_ep: *mut vppcom_endpt_t,
        flags: u32,
    ) -> c_int;

    // --- Client path ---
    pub fn vppcom_session_connect(session_handle: u32, server_ep: *mut vppcom_endpt_t) -> c_int;

    // --- Data I/O ---
    pub fn vppcom_session_read(session_handle: u32, buf: *mut c_void, n: usize) -> c_int;
    pub fn vppcom_session_write(session_handle: u32, buf: *mut c_void, n: usize) -> c_int;

    // --- Datagram I/O (UDP) ---
    pub fn vppcom_session_sendto(
        session_handle: u32,
        buf: *mut c_void,
        n: u32,
        flags: c_int,
        ep: *mut vppcom_endpt_t,
    ) -> c_int;
    pub fn vppcom_session_recvfrom(
        session_handle: u32,
        buf: *mut c_void,
        n: u32,
        flags: c_int,
        ep: *mut vppcom_endpt_t,
    ) -> c_int;

    // --- Session attributes ---
    pub fn vppcom_session_attr(
        session_handle: u32,
        op: u32,
        buffer: *mut c_void,
        buflen: *mut u32,
    ) -> c_int;

    // --- Epoll ---
    pub fn vppcom_epoll_create() -> c_int;
    pub fn vppcom_epoll_ctl(
        vep_handle: u32,
        op: c_int,
        session_handle: u32,
        event: *mut epoll_event,
    ) -> c_int;
    pub fn vppcom_epoll_wait(
        vep_handle: u32,
        events: *mut epoll_event,
        maxevents: c_int,
        wait_for_time: f64,
    ) -> c_int;

    // --- Tokio integration: real FD for the message queue ---
    pub fn vppcom_mq_epoll_fd() -> c_int;

    // --- Worker thread management ---
    pub fn vppcom_worker_register() -> c_int;
    pub fn vppcom_worker_unregister();
    pub fn vppcom_worker_index() -> c_int;

    // --- Utility ---
    pub fn vppcom_retval_str(retval: c_int) -> *const c_char;
}
