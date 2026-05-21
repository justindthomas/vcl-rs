//! Tokio integration for VCL sessions.
//!
//! VCL exposes a message-queue eventfd via `vppcom_mq_epoll_fd()`
//! that becomes readable whenever any VCL session has pending
//! events. We wrap this in a `tokio::io::unix::AsyncFd` so the
//! Tokio runtime wakes us when VCL has work to do.
//!
//! The reactor owns a VCL epoll set (`vppcom_epoll_create`).
//! Sessions are registered with `vppcom_epoll_ctl` and we drain
//! events with `vppcom_epoll_wait(timeout=0)` (non-blocking)
//! whenever the MQ fd fires.
//!
//! Waiters register via `wait_readable` / `wait_writable` and are
//! woken by a `tokio::sync::Notify` per session.
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
//! Workaround: a background task ticks every 50ms, drains any
//! fresh MQ events the normal way (catches missed-edge cases on
//! the eventfd), and then `notify_one()`s every waiter in the map
//! regardless. The woken `recvfrom` retries, sees the buffered
//! bytes, makes forward progress; eventually the fifo drains,
//! `has_event` clears, normal event delivery resumes. Cost is
//! bounded: 20 wake-ups per second on an otherwise idle dnsd,
//! ~50ms latency floor on the masked case.
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

/// The periodic-drain task ticks every 10 ms. If two consecutive
/// ticks land more than this far apart, the worker thread (or its
/// whole current-thread runtime) was blocked in between — almost
/// always parked in a blocking libvppcom call — and every query in
/// flight on that worker stalled with it. The threshold sits well
/// above normal single-thread scheduler jitter so the warning only
/// fires on a genuine stall.
const REACTOR_STALL_THRESHOLD: Duration = Duration::from_millis(500);

struct ReactorInner {
    /// VCL epoll handle (from vppcom_epoll_create).
    vep: u32,
    /// Per-session notification. When a VCL epoll event fires for
    /// a session, we notify its waiter.
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
        let vep = unsafe { ffi::vppcom_epoll_create() };
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
            .map_err(|e| VclError::Io(e))?;

        let reactor = VclReactor {
            inner: Arc::new(Mutex::new(ReactorInner {
                vep: vep as u32,
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
            // workaround — and, in practice, the dominant source of
            // upstream-response latency, because VPP's cross-worker
            // MQ event delivery is unreliable enough that the demux
            // frequently relies on this tick rather than a real
            // event.
            //
            // History: a 10 ms tick once livelocked dnsd overnight,
            // but that was under the VLS port, where every reactor
            // in the process shared one VLS lock — N reactors each
            // ticking 10 ms compounded into lock thrash. Post-VLS,
            // each vcl-io worker has a fully independent reactor
            // (own MQ, own `__vcl_worker_index`, own waiters map);
            // a 10 ms tick here only wakes this one worker's ~3
            // waiters and touches only this worker's `inner` mutex.
            // No cross-worker contention exists to compound.
            //
            // At 500 ms the average drain latency was ~250 ms per
            // upstream hop; a 10-hop recursive walk inherited
            // multiple seconds of pure tick-wait. At 10 ms that
            // drops to ~5 ms/hop — walk latency becomes dominated by
            // real network RTT again, which is the point.
            let mut tick = tokio::time::interval(Duration::from_millis(10));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // This task is spawned on — and pinned to — the vcl-io
            // worker's current-thread runtime, so the running thread's
            // name identifies which worker stalled.
            let worker = std::thread::current()
                .name()
                .map(str::to_owned)
                .unwrap_or_else(|| "?".to_owned());
            let mut last_alive = tokio::time::Instant::now();
            let mut prev_tick = last_alive;
            loop {
                tick.tick().await;
                // Stall watch. `tokio::time::Instant` reads the real
                // monotonic clock, so even though the tokio timer is
                // frozen while the worker is blocked, the gap measured
                // here is true wall-clock time. A blocked worker can't
                // log while wedged — this fires on resume, making the
                // stall (and its duration) visible after the fact.
                let now = tokio::time::Instant::now();
                let gap = now.duration_since(prev_tick);
                prev_tick = now;
                if gap >= REACTOR_STALL_THRESHOLD {
                    tracing::warn!(
                        worker = %worker,
                        stalled_ms = gap.as_millis() as u64,
                        "VclReactor tick stalled — worker thread was blocked \
                         (likely a blocking libvppcom op); queries in flight \
                         on this worker hung for that long",
                    );
                }
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

    /// Register a session handle with the VCL epoll set.
    /// Watches for read + write + edge-triggered.
    pub fn register(&self, session_handle: u32) -> Result<()> {
        let inner = self.inner.lock().unwrap();
        let mut ev = ffi::epoll_event {
            events: ffi::EPOLLIN | ffi::EPOLLOUT | ffi::EPOLLET,
            data: session_handle as u64,
        };
        let rc = unsafe {
            ffi::vppcom_epoll_ctl(
                inner.vep,
                ffi::EPOLL_CTL_ADD,
                session_handle,
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
            ffi::vppcom_epoll_ctl(
                inner.vep,
                ffi::EPOLL_CTL_DEL,
                session_handle,
                std::ptr::null_mut(),
            );
        }
    }

    /// Wait until the given session is readable (has data or a
    /// new connection to accept). Integrates with Tokio by waiting
    /// on the MQ eventfd, then draining VCL epoll events.
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

    /// Non-blocking drain of all pending VCL epoll events. Wakes
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
            ffi::vppcom_epoll_wait(
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
