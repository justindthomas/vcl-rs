//! Fuzz the TCP DNS response framing path.
//!
//! Models a malicious upstream nameserver: arbitrary bytes arrive
//! over the TCP DNS channel, and we must never panic, OOM, or
//! over-read the buffer. The real `query_tcp_dns_*` functions read
//! a 2-byte big-endian length prefix, then `read_exact(len)` bytes;
//! both steps are reachable here by carving the fuzzer input the
//! same way an upstream's send-buffer would be split.
//!
//! Cross-checks the corresponding outbound framing helper too —
//! `frame_tcp_dns_query` must reject any input above 65535 bytes
//! rather than truncating with `as u16`. Run with:
//!
//!     cargo +nightly fuzz run tcp_dns_response

#![no_main]

use libfuzzer_sys::fuzz_target;
use vcl_rs::{decode_tcp_dns_len, frame_tcp_dns_query};

fuzz_target!(|data: &[u8]| {
    // Outbound: framing must be lossless or a clean error. Cap the
    // input we hand the framer at u16::MAX + 1 so the fuzzer also
    // explores the truncation-rejection edge.
    let cap = core::cmp::min(data.len(), (u16::MAX as usize) + 16);
    let q = &data[..cap];
    match frame_tcp_dns_query(q) {
        Ok(framed) => {
            // Length prefix matches actual body length; body bytes
            // round-trip unchanged. (Can only succeed when ≤65535.)
            assert!(q.len() <= u16::MAX as usize);
            assert_eq!(framed.len(), 2 + q.len());
            let prefix = [framed[0], framed[1]];
            let len = decode_tcp_dns_len(prefix)
                .map(|n| n as u16)
                .unwrap_or(0);
            assert_eq!(len as usize, q.len(), "framing prefix mismatch");
            assert_eq!(&framed[2..], q);
        }
        Err(_) => {
            // Only oversize inputs (or zero, exercised below) error.
            assert!(q.len() > u16::MAX as usize);
        }
    }

    // Inbound: simulate the wire reader. The first 2 bytes are the
    // length prefix; the remainder is the would-be body. The real
    // call site does `read_exact(2)` then `read_exact(len)`, so the
    // safety property is "we never `vec![0u8; len]` allocate more
    // than 65535 and we never accept len == 0".
    if data.len() < 2 {
        return;
    }
    let prefix = [data[0], data[1]];
    let body = &data[2..];
    match decode_tcp_dns_len(prefix) {
        Ok(len) => {
            assert!(len > 0);
            assert!(len <= u16::MAX as usize);
            // Mirror what `query_tcp_dns_*` would do: allocate a
            // buffer of `len` bytes and fill from the input. If the
            // fuzzer gives us fewer than `len` bytes, real
            // `read_exact` would fail; we simulate that by clamping.
            let take = core::cmp::min(len, body.len());
            let _ = body[..take].to_vec();
        }
        Err(_) => {
            // Only the all-zero prefix is rejected.
            assert_eq!(prefix, [0, 0]);
        }
    }
});
