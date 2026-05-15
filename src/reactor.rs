//! Tokio integration for VLS sessions.
//!
//! VLS exposes a message-queue eventfd via `vppcom_mq_epoll_fd()`
//! that becomes readable whenever any session has pending events.
//! We wrap this in a `tokio::io::unix::AsyncFd` so the Tokio
//! runtime wakes us when VLS has work to do.
//!
//! The reactor owns a VLS epoll set (`vls_epoll_create`). Sessions
//! are registered with `vls_epoll_ctl` and we drain events with
//! `vls_epoll_wait(timeout=0)` (non-blocking) whenever the MQ fd
//! fires.
//!
//! Waiters register via `wait_readable` / `wait_writable` and are
//! woken by a `tokio::sync::Notify` per session.
//!
//! ## Cross-thread correctness via VLS
//!
//! Under VLS the underlying session pool is shared across all
//! "VLS-registered" threads (lazily, via `vls_mt_detect`), and
//! every VLS op (including the epoll-set ones) takes the VLS lock
//! for its duration. So a reactor created on thread A can be
//! driven from any thread; sessions registered from thread A can
//! be polled / drained from thread B. This is what makes a
//! multi-thread tokio runtime on top of vcl-rs viable.
//!
//! ## The `has_event` stall and how we work around it
//!
//! VPP marks each session's RX fifo with a `has_event` flag the
//! first time it notifies the app, and only clears the flag when
//! the app drains that fifo back to empty. If a reader pulls some
//! bytes but not all (e.g. tokio-rustls reads a TLS ClientHello
//! and then parks waiting for the next handshake message), the
//! flag stays set, and subsequent data arrivals on that same
//! session do NOT fire a fresh MQ event. The waiter for that
//! session is parked on a Notify nobody is going to signal again.
//! `vppctl show session verbose 2` exposes this as `has_event 1`
//! with `cursize > 0`.
//!
//! Workaround: a background task ticks periodically, drains any
//! fresh VLS epoll events the normal way (catches missed-edge
//! cases on the eventfd), and then `notify_one()`s every waiter
//! in the map regardless. The woken `recvfrom` retries, sees the
//! buffered bytes, makes forward progress; eventually the fifo
//! drains, `has_event` clears, normal event delivery resumes.
//!
//! The `wait_event` loop also `tokio::select!`s between the MQ
//! eventfd and the per-session Notify so the periodic notify-all
//! can wake a task whose MQ side is dormant.

use std::collections::HashMap;
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::unix::AsyncFd;
use tokio::io::Interest;
use tokio::sync::Notify;

use crate::error::{Result, VclError};
use crate::ffi;

struct ReactorInner {
    /// VLS epoll handle (from vls_epoll_create).
    vep: ffi::vls_handle_t,
    /// Per-session notification. When a VLS epoll event fires for
    /// a session, we notify its waiter. Keyed on the raw
    /// `vls_handle_t` value (signed; callers pass us non-negative
    /// values from `SessionHandle::0`).
    waiters: HashMap<u32, Arc<Notify>>,
}

#[derive(Clone)]
pub struct VclReactor {
    inner: Arc<Mutex<ReactorInner>>,
    /// The MQ eventfd wrapped in AsyncFd for Tokio integration.
    /// Shared via Arc so clones can spawn the poll loop once.
    mq_fd: Arc<AsyncFd<MqFd>>,
}

/// Newtype so we can impl `AsRawFd` for the MQ file descriptor.
struct MqFd(RawFd);

impl AsRawFd for MqFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

impl VclReactor {
    /// Create a new reactor. Call once after `VclApp::init`. Must
    /// be called from a tokio runtime context — both the AsyncFd
    /// registration and the periodic-drain task spawn require it.
    pub fn new() -> Result<Self> {
        // Ensure this thread is added to VLS before fetching the
        // MQ-epoll fd. `vppcom_mq_epoll_fd` reads `vcl_worker_get_current()`,
        // which would be -1 on a thread that hasn't yet called any
        // VLS op. The explicit register is idempotent — VLS would
        // auto-register on the first vls_* call anyway.
        crate::app::register_worker_thread();

        let vep = unsafe { ffi::vls_epoll_create() };
        if vep < 0 {
            return Err(VclError::from_rc(vep));
        }
        let mq_raw = unsafe { ffi::vppcom_mq_epoll_fd() };
        if mq_raw < 0 {
            return Err(VclError::Api(
                "vppcom_mq_epoll_fd returned negative".into(),
                mq_raw,
            ));
        }
        let async_fd = AsyncFd::with_interest(MqFd(mq_raw), Interest::READABLE)
            .map_err(VclError::Io)?;

        let reactor = VclReactor {
            inner: Arc::new(Mutex::new(ReactorInner {
                vep,
                waiters: HashMap::new(),
            })),
            mq_fd: Arc::new(async_fd),
        };

        // Periodic forced-progress task — see module docs for the
        // `has_event` mechanism this works around. Held via Weak so
        // the task self-exits when the reactor is dropped.
        let weak_inner = Arc::downgrade(&reactor.inner);
        let weak_mq = Arc::downgrade(&reactor.mq_fd);
        tokio::spawn(async move {
            // Periodic forced-progress task. The `has_event`-stall
            // workaround is bounded by this interval.
            //
            // Tick is a compromise: tighter intervals respond faster
            // to a wedged session but multiply lock-contention cost
            // because each tick acquires `reactor.inner.lock()` and
            // iterates `notify_one` over every entry in the waiters
            // map. Under sustained DoH load with thousands of
            // accumulated half-handshaked sessions, a 10ms tick
            // produced enough contention with `wait_event` /
            // `register` / `deregister` that the runtime livelocked
            // overnight on jt-router (FIFOs filled, control socket
            // stopped responding). 50ms restores headroom while
            // still bounding the worst-case wedge to a typical
            // resolver retry window.
            // 500ms tick. Each spurious notify wakes every parked
            // task; each waking task that calls `vppcom_session_read`
            // pays a ~1ms blocking call inside libvppcom (the
            // `svm_msg_q_timedwait(timeout=1)` MQ-drain — visible
            // in gdb backtraces of stuck states). With ~10 active
            // TLS sessions and a 50ms tick, that's ~10×1×20 = 200ms
            // of wall-time per second spent inside libvppcom doing
            // nothing useful, which starves the UDP listener and
            // the drainer's own tick out of CPU. 500ms keeps the
            // worst-case `has_event` stall bounded to a typical
            // resolver retry window while cutting the spurious-wake
            // overhead by 10x.
            let mut tick = tokio::time::interval(Duration::from_millis(500));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut last_alive = tokio::time::Instant::now();
            loop {
                tick.tick().await;
                let (Some(inner), Some(_keepalive_mq)) =
                    (weak_inner.upgrade(), weak_mq.upgrade())
                else {
                    return;
                };
                drain_events_with_inner(&inner);
                let (waiters_n, alive_log) = {
                    let inner = inner.lock().unwrap();
                    let n = inner.waiters.len();
                    for notify in inner.waiters.values() {
                        notify.notify_one();
                    }
                    let now = tokio::time::Instant::now();
                    let log =
                        now.duration_since(last_alive) >= Duration::from_secs(60);
                    if log {
                        last_alive = now;
                    }
                    (n, log)
                };
                if alive_log {
                    tracing::info!(
                        waiters = waiters_n,
                        "VclReactor drainer alive"
                    );
                }
            }
        });

        Ok(reactor)
    }

    /// Register a session handle with the VLS epoll set.
    /// Watches for read + write + edge-triggered.
    pub fn register(&self, session_handle: u32) -> Result<()> {
        let inner = self.inner.lock().unwrap();
        // Level-triggered, NOT edge-triggered. EPOLLET requires the
        // application to fully drain the FIFO on each wake; the
        // pre-VLS reactor used it and that worked because every
        // session op happened on the worker thread and the read
        // loop drained eagerly. Under VLS the upstream-UDP demux
        // loop registers AFTER data may already be buffered, and
        // we never see an empty→non-empty edge — `Rx-f=19934`
        // sits unread. LT fires whenever there's pending readable
        // data, which is the semantic we actually want here.
        let mut ev = ffi::epoll_event {
            events: ffi::EPOLLIN | ffi::EPOLLOUT,
            data: session_handle as u64,
        };
        let rc = unsafe {
            ffi::vls_epoll_ctl(
                inner.vep,
                ffi::EPOLL_CTL_ADD,
                session_handle as ffi::vls_handle_t,
                &mut ev,
            )
        };
        if rc < 0 {
            return Err(VclError::from_rc(rc));
        }
        Ok(())
    }

    /// Deregister a session from the epoll set.
    pub fn deregister(&self, session_handle: u32) {
        let mut inner = self.inner.lock().unwrap();
        inner.waiters.remove(&session_handle);
        unsafe {
            ffi::vls_epoll_ctl(
                inner.vep,
                ffi::EPOLL_CTL_DEL,
                session_handle as ffi::vls_handle_t,
                std::ptr::null_mut(),
            );
        }
    }

    /// Wait until the given session is readable (has data or a
    /// new connection to accept). Integrates with Tokio by waiting
    /// on the MQ eventfd, then draining VLS epoll events.
    pub async fn wait_readable(&self, session_handle: u32, timeout: Duration) -> Result<()> {
        self.wait_event(session_handle, ffi::EPOLLIN, timeout).await
    }

    /// Wait until the given session is writable (TX FIFO has space
    /// or a connect() completed).
    pub async fn wait_writable(&self, session_handle: u32, timeout: Duration) -> Result<()> {
        self.wait_event(session_handle, ffi::EPOLLOUT, timeout).await
    }

    async fn wait_event(&self, session_handle: u32, _event_mask: u32, timeout: Duration) -> Result<()> {
        let notify = {
            let mut inner = self.inner.lock().unwrap();
            inner
                .waiters
                .entry(session_handle)
                .or_insert_with(|| Arc::new(Notify::new()))
                .clone()
        };

        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            // Wait on EITHER the MQ eventfd firing (normal happy
            // path) OR the per-session Notify being signalled by
            // the periodic-drain task (the `has_event`-stall
            // workaround — see module docs). Without the notify
            // branch in this select, wait_event parks forever on
            // mq_fd whenever VPP refuses to fire a fresh MQ event
            // for a session whose previous event we didn't
            // "acknowledge" by draining to empty.
            let progress = tokio::select! {
                ready = self.mq_fd.readable() => {
                    let mut ready = ready.map_err(|e| VclError::Io(e))?;
                    self.drain_events();
                    ready.clear_ready();
                    tokio::time::timeout(Duration::from_millis(0), notify.notified())
                        .await
                        .is_ok()
                }
                _ = notify.notified() => true,
                _ = tokio::time::sleep_until(deadline) => return Err(VclError::Timeout),
            };
            if progress {
                return Ok(());
            }
        }
    }

    /// Non-blocking drain of all pending VLS epoll events. Wakes
    /// the per-session Notify for each session that had an event.
    fn drain_events(&self) {
        drain_events_with_inner(&self.inner);
    }
}

fn drain_events_with_inner(inner_arc: &Arc<Mutex<ReactorInner>>) {
    let inner = inner_arc.lock().unwrap();
    let mut events = [ffi::epoll_event { events: 0, data: 0 }; 64];
    loop {
        let n = unsafe {
            ffi::vls_epoll_wait(
                inner.vep,
                events.as_mut_ptr(),
                events.len() as i32,
                0.0, // non-blocking
            )
        };
        if n <= 0 {
            break;
        }
        for i in 0..n as usize {
            let sh = events[i].data as u32;
            if let Some(notify) = inner.waiters.get(&sh) {
                notify.notify_one();
            }
        }
        if (n as usize) < events.len() {
            break;
        }
    }
}
