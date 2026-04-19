# vcl-rs

Async Rust wrapper around [VPP's](https://fd.io) Comms Library (VCL) — an lib-injection shim that lets userspace programs open TCP/UDP sockets on VPP's transport stack rather than the kernel's.

Wraps the C VCL calls (`vppcom_session_create`, `vppcom_session_bind`, `vppcom_epoll_wait`, …) in a tokio-friendly API.

## Build

```sh
cargo build --release
```

Requires VPP's VCL library and headers (`libvppcom.so`, `vppcom.h`) on the system. Typical path: `/usr/lib/x86_64-linux-gnu/libvppcom.so` on Debian/Ubuntu with fd.io packages.

## Usage

Launch your program with `LD_PRELOAD=libvcl_ldpreload.so` or initialize VCL explicitly; then use the async listener/stream types exported from this crate the way you'd use tokio's.

## License

MPL-2.0. See [LICENSE](LICENSE).
