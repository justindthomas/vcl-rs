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

/// VLS handle type. Distinct from the underlying `vppcom` session
/// handle — VLS keeps its own pool of "vls_handle_t" values that
/// each map to a session under the VLS lock. Negative on error.
pub type vls_handle_t = c_int;
pub const VLS_INVALID_HANDLE: vls_handle_t = -1;

#[link(name = "vppcom")]
extern "C" {
    // --- Application lifecycle ---
    //
    // Use `vls_app_create` (not `vppcom_app_create`) so libvppcom
    // installs the VLS internal-lock + per-thread auto-registration
    // hooks. After this returns OK, every VLS op called from any
    // thread auto-registers that thread (via `vls_mt_detect`) and
    // takes the process-wide VLS lock during the actual session op.
    pub fn vls_app_create(app_name: *const c_char) -> c_int;
    pub fn vppcom_app_destroy();

    // --- VLS session lifecycle ---
    //
    // `vls_handle_t` is the application-facing handle. Internally
    // VLS maps it to the underlying vppcom session under the lock,
    // so the same handle is usable from any thread.
    pub fn vls_create(proto: u8, is_nonblocking: u8) -> vls_handle_t;
    pub fn vls_close(vlsh: vls_handle_t) -> c_int;
    pub fn vls_shutdown(vlsh: vls_handle_t, how: c_int) -> c_int;

    // --- Server path ---
    pub fn vls_bind(vlsh: vls_handle_t, ep: *mut vppcom_endpt_t) -> c_int;
    pub fn vls_listen(vlsh: vls_handle_t, q_len: c_int) -> c_int;
    pub fn vls_accept(
        vlsh: vls_handle_t,
        client_ep: *mut vppcom_endpt_t,
        flags: c_int,
    ) -> vls_handle_t;

    // --- Client path ---
    pub fn vls_connect(vlsh: vls_handle_t, server_ep: *mut vppcom_endpt_t) -> c_int;

    // --- Data I/O (TCP-style; VLS uses ssize_t but c_int fits) ---
    pub fn vls_read(vlsh: vls_handle_t, buf: *mut c_void, n: usize) -> isize;
    pub fn vls_write(vlsh: vls_handle_t, buf: *mut c_void, n: usize) -> c_int;

    // --- Datagram I/O (UDP) ---
    pub fn vls_sendto(
        vlsh: vls_handle_t,
        buf: *mut c_void,
        buflen: c_int,
        flags: c_int,
        ep: *mut vppcom_endpt_t,
    ) -> c_int;
    pub fn vls_recvfrom(
        vlsh: vls_handle_t,
        buf: *mut c_void,
        buflen: u32,
        flags: c_int,
        ep: *mut vppcom_endpt_t,
    ) -> isize;

    // --- Session attributes ---
    //
    // VLS doesn't currently expose a distinct `vls_attr` for SET-side
    // attrs (it shares `vls_attr` with both get + set). Same op codes
    // as classic vppcom_session_attr.
    pub fn vls_attr(
        vlsh: vls_handle_t,
        op: u32,
        buffer: *mut c_void,
        buflen: *mut u32,
    ) -> c_int;

    // --- Epoll (VLS-aware) ---
    pub fn vls_epoll_create() -> vls_handle_t;
    pub fn vls_epoll_ctl(
        ep_vlsh: vls_handle_t,
        op: c_int,
        vlsh: vls_handle_t,
        event: *mut epoll_event,
    ) -> c_int;
    pub fn vls_epoll_wait(
        ep_vlsh: vls_handle_t,
        events: *mut epoll_event,
        maxevents: c_int,
        wait_for_time: f64,
    ) -> c_int;

    // --- Tokio integration ---
    //
    // The MQ epoll fd lives on the underlying vppcom worker. With
    // VLS in lock mode there's effectively one worker shared across
    // all threads, so this fd is process-wide. Must be called from
    // a thread that's been "added" to VLS — easiest is to call
    // `vls_register_vcl_worker` once on that thread before fetching
    // the fd (any earlier `vls_*` op would also have auto-registered
    // it via `vls_mt_detect`).
    pub fn vppcom_mq_epoll_fd() -> c_int;

    // --- VLS thread registration ---
    //
    // Idempotent. Adds the calling thread to VLS's thread set so
    // VLS ops from this thread go through the lock + auto-migrate
    // path. VLS calls this automatically on the first VLS op from
    // a new thread (via `vls_mt_detect`); explicit call is only
    // needed before non-VLS calls (e.g. `vppcom_mq_epoll_fd`).
    pub fn vls_register_vcl_worker();

    // --- Capability probes ---
    pub fn vls_use_eventfd() -> u8;
    pub fn vls_mt_wrk_supported() -> u8;

    // --- Utility ---
    pub fn vppcom_retval_str(retval: c_int) -> *const c_char;
}
