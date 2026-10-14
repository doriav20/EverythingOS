//! Adaptive concurrency control for network filesystems.
//!
//! Scanning a share is bound by round-trip latency, so throughput grows
//! with the number of requests in flight, up to the point where the server
//! (or link) saturates and latency inflates. [`AdaptiveGate`] finds that
//! point with AIMD, like TCP congestion control:
//!
//! * every measurement window, if throughput kept up and latency is not
//!   inflated, the limit grows additively;
//! * if latency inflates without a throughput gain, it shrinks by a quarter;
//! * if operations fail or the server is unreachable, it halves.
//!
//! An optional token bucket caps operations per second so a scan never
//! hammers a busy file server, whatever the latency.

use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};

use crate::walker::{Gate, OpOutcome};

/// Gate configuration.
#[derive(Debug, Clone)]
pub struct GateConfig {
    pub min: usize,
    pub max: usize,
    pub initial: usize,
    /// Cap on operations per second (`None` for unlimited).
    pub max_ops_per_sec: Option<f64>,
    /// Measurement window for adjustments.
    pub window: Duration,
}

impl Default for GateConfig {
    fn default() -> Self {
        GateConfig {
            min: 1,
            max: 64,
            initial: 8,
            max_ops_per_sec: None,
            window: Duration::from_millis(500),
        }
    }
}

/// Live gate statistics.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GateStats {
    pub limit: usize,
    pub in_flight: usize,
    pub ops_per_sec: f64,
    pub avg_latency: Duration,
}

struct State {
    limit: usize,
    in_flight: usize,
    window_start: Instant,
    ops: u64,
    errors: u64,
    unreachable: bool,
    latency_sum: Duration,
    last_throughput: f64,
    last_latency: Duration,
    baseline_latency: Option<Duration>,
    tokens: f64,
    last_refill: Instant,
}

/// AIMD concurrency limiter implementing the walker [`Gate`].
pub struct AdaptiveGate {
    cfg: GateConfig,
    state: Mutex<State>,
    wake: Condvar,
}

impl AdaptiveGate {
    pub fn new(cfg: GateConfig) -> Self {
        let now = Instant::now();
        let initial = cfg.initial.clamp(cfg.min.max(1), cfg.max.max(1));
        AdaptiveGate {
            state: Mutex::new(State {
                limit: initial,
                in_flight: 0,
                window_start: now,
                ops: 0,
                errors: 0,
                unreachable: false,
                latency_sum: Duration::ZERO,
                last_throughput: 0.0,
                last_latency: Duration::ZERO,
                baseline_latency: None,
                tokens: 1.0,
                last_refill: now,
            }),
            cfg,
            wake: Condvar::new(),
        }
    }

    pub fn stats(&self) -> GateStats {
        let s = self.state.lock();
        GateStats {
            limit: s.limit,
            in_flight: s.in_flight,
            ops_per_sec: s.last_throughput,
            avg_latency: s.last_latency,
        }
    }

    fn refill(&self, s: &mut State, now: Instant) {
        if let Some(rate) = self.cfg.max_ops_per_sec {
            let elapsed = now.duration_since(s.last_refill).as_secs_f64();
            // Allow a burst of up to a tenth of a second's worth.
            s.tokens = (s.tokens + elapsed * rate).min((rate / 10.0).max(1.0));
            s.last_refill = now;
        }
    }

    fn adjust(&self, s: &mut State, now: Instant) {
        let secs = now.duration_since(s.window_start).as_secs_f64();
        if s.ops == 0 {
            s.window_start = now;
            return;
        }
        let throughput = s.ops as f64 / secs;
        let latency = s.latency_sum / s.ops as u32;
        let baseline = *s.baseline_latency.get_or_insert(latency);
        let (min, max) = (self.cfg.min.max(1), self.cfg.max.max(1));
        let gained = throughput > s.last_throughput * 1.05;
        if s.unreachable || s.errors * 10 > s.ops {
            s.limit = (s.limit / 2).max(min);
        } else if latency > baseline * 4 && !gained {
            s.limit = (s.limit - s.limit / 4).max(min);
        } else if latency <= baseline * 2 || gained {
            s.limit = (s.limit + (s.limit / 8).max(1)).min(max);
        }
        // Track the uncongested latency: follow improvements at once, but
        // drift up slowly so a permanently slower link is not mistaken for
        // congestion forever.
        s.baseline_latency = Some(if latency < baseline {
            latency
        } else {
            baseline.mul_f64(1.02).min(latency)
        });
        s.last_throughput = throughput;
        s.last_latency = latency;
        s.window_start = now;
        s.ops = 0;
        s.errors = 0;
        s.unreachable = false;
        s.latency_sum = Duration::ZERO;
    }
}

impl Gate for AdaptiveGate {
    fn begin(&self) {
        let mut s = self.state.lock();
        loop {
            let now = Instant::now();
            self.refill(&mut s, now);
            let rate_ok = self.cfg.max_ops_per_sec.is_none() || s.tokens >= 1.0;
            if s.in_flight < s.limit && rate_ok {
                s.in_flight += 1;
                if self.cfg.max_ops_per_sec.is_some() {
                    s.tokens -= 1.0;
                }
                return;
            }
            let wait = match self.cfg.max_ops_per_sec {
                Some(rate) if s.in_flight < s.limit => {
                    Duration::from_secs_f64(((1.0 - s.tokens) / rate).max(0.0005))
                }
                _ => Duration::from_millis(50),
            };
            self.wake.wait_for(&mut s, wait);
        }
    }

    fn end(&self, elapsed: Duration, outcome: OpOutcome) {
        let mut s = self.state.lock();
        s.in_flight -= 1;
        s.ops += 1;
        s.latency_sum += elapsed;
        match outcome {
            OpOutcome::Ok => {}
            OpOutcome::Error => s.errors += 1,
            OpOutcome::Unreachable => s.unreachable = true,
        }
        let now = Instant::now();
        if now.duration_since(s.window_start) >= self.cfg.window {
            self.adjust(&mut s, now);
        }
        drop(s);
        self.wake.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;

    /// Simulated server: latency grows once more than `capacity` requests
    /// are in flight.
    fn run(gate: &Arc<AdaptiveGate>, capacity: u64, outcome: OpOutcome, dur: Duration) -> u64 {
        let in_flight = Arc::new(AtomicU64::new(0));
        let done = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let threads: Vec<_> = (0..48)
            .map(|_| {
                let (gate, in_flight, done, stop) =
                    (gate.clone(), in_flight.clone(), done.clone(), stop.clone());
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        gate.begin();
                        let n = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                        let over = n.saturating_sub(capacity);
                        let latency = Duration::from_micros(2_000 + over * 1_500);
                        std::thread::sleep(latency);
                        in_flight.fetch_sub(1, Ordering::SeqCst);
                        gate.end(latency, outcome);
                        done.fetch_add(1, Ordering::Relaxed);
                    }
                })
            })
            .collect();
        std::thread::sleep(dur);
        stop.store(true, Ordering::Relaxed);
        let finished = done.load(Ordering::Relaxed);
        for t in threads {
            t.join().unwrap();
        }
        finished
    }

    fn cfg() -> GateConfig {
        GateConfig {
            min: 1,
            max: 40,
            initial: 2,
            max_ops_per_sec: None,
            window: Duration::from_millis(40),
        }
    }

    #[test]
    fn grows_while_the_server_keeps_up() {
        let gate = Arc::new(AdaptiveGate::new(cfg()));
        run(&gate, 1000, OpOutcome::Ok, Duration::from_millis(800));
        assert!(gate.stats().limit >= 10, "limit {}", gate.stats().limit);
    }

    #[test]
    fn settles_near_server_capacity() {
        // Latency inflates beyond 6 concurrent requests; the limit must stop
        // well short of the configured maximum of 40.
        let gate = Arc::new(AdaptiveGate::new(cfg()));
        run(&gate, 6, OpOutcome::Ok, Duration::from_millis(1200));
        assert!(gate.stats().limit <= 16, "limit {}", gate.stats().limit);
    }

    #[test]
    fn halves_on_failures() {
        let gate = Arc::new(AdaptiveGate::new(GateConfig {
            initial: 32,
            ..cfg()
        }));
        run(
            &gate,
            1000,
            OpOutcome::Unreachable,
            Duration::from_millis(400),
        );
        assert!(gate.stats().limit <= 4, "limit {}", gate.stats().limit);
    }

    #[test]
    fn respects_the_rate_cap() {
        let gate = Arc::new(AdaptiveGate::new(GateConfig {
            max_ops_per_sec: Some(200.0),
            initial: 40,
            ..cfg()
        }));
        let done = run(&gate, 1000, OpOutcome::Ok, Duration::from_millis(500));
        assert!(done <= 140, "{done} ops in 0.5 s at 200/s");
        assert!(done >= 50, "{done} ops is far below the cap");
    }
}
