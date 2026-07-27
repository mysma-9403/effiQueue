//! Pure, testable scaling logic — no filesystem, processes, or network.
//!
//! Implements the Budget-Bound SLO Controller (DESIGN §2–§3): measured `mu` via
//! scale-by-one perturbation, `lambda` as the queue-balance residual, Little's
//! Law for `workers_needed`, a RAM budget for `workers_capacity`, and the
//! signature Feasibility Gap when the SLO cannot fit on this host. Also hosts
//! the simpler `threshold` fallback mode.

use std::time::Duration;

/// Decision for a single tick — the controller moves by at most one worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalingAction {
    ScaleUp,
    ScaleDown,
    Hold,
}

/// The signature readout: the declared SLO cannot be met within the RAM budget.
#[derive(Debug, Clone, PartialEq)]
pub struct FeasibilityGap {
    pub workers_needed: u32,
    pub workers_capacity: u32,
    pub gap_workers: u32,
    pub gap_bytes: u64,
    /// Best drain achievable at full capacity; `None` = never (mu*cap <= lambda).
    pub best_drain: Option<Duration>,
    pub slo_drain_time: Duration,
}

/// Exponentially-weighted moving average (DESIGN §6).
#[derive(Debug, Clone)]
pub struct EwmaEstimator {
    value: f64,
    alpha: f64,
    initialized: bool,
}

impl EwmaEstimator {
    pub fn new(alpha: f64) -> Self {
        Self {
            value: 0.0,
            alpha,
            initialized: false,
        }
    }

    pub fn update(&mut self, sample: f64) {
        if self.initialized {
            self.value = self.alpha * sample + (1.0 - self.alpha) * self.value;
        } else {
            self.value = sample;
            self.initialized = true;
        }
    }

    pub fn value(&self) -> f64 {
        self.value
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized
    }
}

/// Live estimators for throughput (`mu`) and arrival rate (`lambda`).
#[derive(Debug, Clone)]
pub struct Estimators {
    mu: EwmaEstimator,
    lambda: EwmaEstimator,
    last_backlog: Option<u64>,
}

impl Estimators {
    pub fn new(alpha_mu: f64, alpha_lambda: f64) -> Self {
        Self {
            mu: EwmaEstimator::new(alpha_mu),
            lambda: EwmaEstimator::new(alpha_lambda),
            last_backlog: None,
        }
    }

    /// Observe one control window (DESIGN §2.2–§2.3).
    ///
    /// `running_in_window` = worker count active during the elapsed `dt`.
    /// `interference` = the running count changed outside our own ±1 step
    /// (operator/crash/self-exit) — the `mu` sample is quarantined.
    pub fn observe(
        &mut self,
        backlog: u32,
        running_in_window: u32,
        dt: Duration,
        interference: bool,
    ) {
        let dt_s = dt.as_secs_f64();
        if dt_s <= 0.0 {
            return;
        }
        let backlog_f = backlog as f64;
        let Some(prev) = self.last_backlog else {
            self.last_backlog = Some(backlog as u64);
            return;
        };
        let delta_backlog = backlog_f - prev as f64;
        let running = running_in_window as f64;

        // lambda = residual of the balance, using the *previous* mu (loop broken by one tick).
        let mu_prev = if self.mu.is_initialized() {
            self.mu.value()
        } else {
            0.0
        };
        let drained = mu_prev * running * dt_s;
        let lambda_raw = ((delta_backlog + drained) / dt_s).max(0.0);
        self.lambda.update(lambda_raw);

        // mu from the scale-by-one perturbation, using the *current* lambda.
        if !interference && running_in_window > 0 {
            let observed_drain = self.lambda.value() * dt_s - delta_backlog;
            if observed_drain > 0.0 {
                let mu_raw = observed_drain / (running * dt_s);
                self.mu.update(mu_raw);
            }
        }

        self.last_backlog = Some(backlog as u64);
    }

    pub fn lambda(&self) -> f64 {
        self.lambda.value()
    }

    /// `mu` only if a clean, positive sample has been observed.
    pub fn mu(&self) -> Option<f64> {
        if self.mu.is_initialized() && self.mu.value() > 0.0 {
            Some(self.mu.value())
        } else {
            None
        }
    }
}

/// Tuning + limits for the SLO controller.
#[derive(Debug, Clone)]
pub struct SloParams {
    pub slo_drain_time: Duration,
    pub min_workers: u32,
    pub max_workers: u32,
    /// Dead-zone half-width around `running` (damps ±1 flapping).
    pub hysteresis: u32,
    /// Ticks to wait after a change before the next one (except fast-path).
    pub cooldown_ticks: u32,
    /// Backlog above which scale-up bypasses cooldown (spike fast-path).
    pub spike_backlog: u32,
}

/// Per-tick inputs to the SLO decision.
#[derive(Debug, Clone)]
pub struct SloInputs {
    pub backlog: u32,
    pub running: u32,
    /// Measured throughput per worker; `None` until a reliable sample exists.
    pub mu: Option<f64>,
    pub lambda: f64,
    /// Measured RSS per worker (bytes); `None` until measured.
    pub worker_rss: Option<u64>,
    pub safe_ram_budget: u64,
    pub swap_pressure: bool,
    pub ticks_since_change: u32,
}

/// Full result of an SLO decision (diagnostics + action).
#[derive(Debug, Clone)]
pub struct Decision {
    pub action: ScalingAction,
    pub workers_needed: Option<u32>,
    pub workers_capacity: u32,
    pub feasibility_gap: Option<FeasibilityGap>,
}

/// The SLO controller decision for one tick (DESIGN §2.4–§2.6, §3).
pub fn decide_slo(p: &SloParams, i: &SloInputs) -> Decision {
    let h = p.slo_drain_time.as_secs_f64();

    // workers_capacity = floor(budget / rss), swap-pressure guarded, capped by max.
    let mut capacity = match i.worker_rss {
        Some(rss) if rss > 0 => (i.safe_ram_budget / rss).min(u32::MAX as u64) as u32,
        _ => p.max_workers, // no RSS measurement yet -> not RAM-constrained
    };
    if i.swap_pressure {
        capacity = capacity.min(i.running); // block scale-up while swapping
    }
    capacity = capacity.min(p.max_workers);

    // workers_needed via Little's Law; None while mu is not yet reliable.
    let workers_needed = match i.mu {
        Some(mu) if mu > 0.0 && h > 0.0 => {
            let n = ((i.backlog as f64 + i.lambda * h) / (mu * h)).ceil();
            Some(n.clamp(0.0, u32::MAX as f64) as u32)
        }
        _ => None,
    };

    let feasibility_gap = match workers_needed {
        Some(needed) if needed > capacity => {
            let gap_workers = needed - capacity;
            let rss = i.worker_rss.unwrap_or(0);
            let gap_bytes = gap_workers as u64 * rss;
            let best_drain = i.mu.and_then(|mu| {
                let net = mu * capacity as f64 - i.lambda;
                if net > 0.0 {
                    Some(Duration::from_secs_f64(i.backlog as f64 / net))
                } else {
                    None
                }
            });
            Some(FeasibilityGap {
                workers_needed: needed,
                workers_capacity: capacity,
                gap_workers,
                gap_bytes,
                best_drain,
                slo_drain_time: p.slo_drain_time,
            })
        }
        _ => None,
    };

    let action = decide_action(p, i, workers_needed, capacity);

    Decision {
        action,
        workers_needed,
        workers_capacity: capacity,
        feasibility_gap,
    }
}

fn decide_action(
    p: &SloParams,
    i: &SloInputs,
    needed: Option<u32>,
    capacity: u32,
) -> ScalingAction {
    // Meet the floor / ceiling promptly (ignore cooldown).
    if i.running < p.min_workers {
        return ScalingAction::ScaleUp;
    }
    if i.running > p.max_workers {
        return ScalingAction::ScaleDown;
    }

    let Some(needed) = needed else {
        // Bootstrap: no reliable mu yet. Cautiously scale up if there is work
        // and room, so mu can be measured; otherwise hold.
        if i.backlog > 0 && i.running < capacity && i.running < p.max_workers {
            return ScalingAction::ScaleUp;
        }
        return ScalingAction::Hold;
    };

    let target = needed.min(capacity).clamp(p.min_workers, p.max_workers);
    let cooldown_ok = i.ticks_since_change >= p.cooldown_ticks;
    let spike = i.backlog > p.spike_backlog;

    if target > i.running.saturating_add(p.hysteresis) {
        // Scale-up may bypass cooldown on a spike (never scale-down).
        if cooldown_ok || spike {
            ScalingAction::ScaleUp
        } else {
            ScalingAction::Hold
        }
    } else if target < i.running.saturating_sub(p.hysteresis) {
        if cooldown_ok {
            ScalingAction::ScaleDown
        } else {
            ScalingAction::Hold
        }
    } else {
        ScalingAction::Hold
    }
}

/// Limits + thresholds for the simpler `threshold` fallback mode.
#[derive(Debug, Clone)]
pub struct ThresholdParams {
    pub depth_threshold: u32,
    pub ram_ratio_cap: f64,
    pub min_workers: u32,
    pub max_workers: u32,
}

/// Inputs for the `threshold` fallback decision.
#[derive(Debug, Clone)]
pub struct ThresholdInputs {
    pub backlog: u32,
    pub running: u32,
    pub ram_ratio: f64,
}

/// Simple depth + RAM% fallback (mirrors the pre-SLO behavior, safely bounded).
pub fn decide_threshold(p: &ThresholdParams, i: &ThresholdInputs) -> ScalingAction {
    if i.running < p.min_workers {
        return ScalingAction::ScaleUp;
    }
    if i.backlog == 0 && i.running > p.min_workers {
        return ScalingAction::ScaleDown;
    }
    if i.backlog > p.depth_threshold && i.running < p.max_workers && i.ram_ratio < p.ram_ratio_cap {
        return ScalingAction::ScaleUp;
    }
    ScalingAction::Hold
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slo_params() -> SloParams {
        SloParams {
            slo_drain_time: Duration::from_secs(120),
            min_workers: 0,
            max_workers: 100,
            hysteresis: 0,
            cooldown_ticks: 0,
            spike_backlog: u32::MAX,
        }
    }

    #[test]
    fn ewma_initializes_then_smooths() {
        let mut e = EwmaEstimator::new(0.5);
        assert!(!e.is_initialized());
        e.update(10.0);
        assert_eq!(e.value(), 10.0);
        e.update(20.0);
        assert_eq!(e.value(), 15.0);
    }

    #[test]
    fn design_example_feasibility_gap() {
        // DESIGN §3.3: backlog 50000, H=120s, mu=8, lambda=200, rss=512MB, budget=12GB.
        let p = slo_params();
        let i = SloInputs {
            backlog: 50_000,
            running: 10,
            mu: Some(8.0),
            lambda: 200.0,
            worker_rss: Some(512 * 1024 * 1024),
            safe_ram_budget: 12 * 1024 * 1024 * 1024,
            swap_pressure: false,
            ticks_since_change: 0,
        };
        let d = decide_slo(&p, &i);
        assert_eq!(d.workers_needed, Some(78));
        assert_eq!(d.workers_capacity, 24);
        let gap = d.feasibility_gap.expect("gap expected");
        assert_eq!(gap.gap_workers, 54);
        assert_eq!(gap.best_drain, None); // mu*capacity (192) < lambda (200)
                                          // target pinned to capacity (24) > running (10) -> scale up.
        assert_eq!(d.action, ScalingAction::ScaleUp);
    }

    #[test]
    fn scales_to_zero_when_idle() {
        let p = slo_params();
        let i = SloInputs {
            backlog: 0,
            running: 3,
            mu: Some(50.0),
            lambda: 0.0,
            worker_rss: Some(100 * 1024 * 1024),
            safe_ram_budget: 8 * 1024 * 1024 * 1024,
            swap_pressure: false,
            ticks_since_change: 5,
        };
        let d = decide_slo(&p, &i);
        assert_eq!(d.workers_needed, Some(0));
        assert_eq!(d.action, ScalingAction::ScaleDown);
    }

    #[test]
    fn bootstraps_up_without_reliable_mu() {
        let p = slo_params();
        let i = SloInputs {
            backlog: 500,
            running: 0,
            mu: None,
            lambda: 0.0,
            worker_rss: None,
            safe_ram_budget: 8 * 1024 * 1024 * 1024,
            swap_pressure: false,
            ticks_since_change: 0,
        };
        assert_eq!(decide_slo(&p, &i).action, ScalingAction::ScaleUp);
    }

    #[test]
    fn swap_pressure_blocks_scale_up() {
        let mut p = slo_params();
        p.max_workers = 100;
        let i = SloInputs {
            backlog: 50_000,
            running: 4,
            mu: Some(8.0),
            lambda: 10.0,
            worker_rss: Some(100 * 1024 * 1024),
            safe_ram_budget: 64 * 1024 * 1024 * 1024,
            swap_pressure: true,
            ticks_since_change: 10,
        };
        let d = decide_slo(&p, &i);
        assert_eq!(d.workers_capacity, 4); // clamped to running under swap pressure
        assert_eq!(d.action, ScalingAction::Hold);
    }

    #[test]
    fn cooldown_blocks_but_spike_bypasses() {
        let mut p = slo_params();
        p.cooldown_ticks = 3;
        p.spike_backlog = 1000;
        let base = SloInputs {
            backlog: 5000,
            running: 2,
            mu: Some(1.0),
            lambda: 100.0,
            worker_rss: Some(50 * 1024 * 1024),
            safe_ram_budget: 64 * 1024 * 1024 * 1024,
            swap_pressure: false,
            ticks_since_change: 0, // within cooldown
        };
        // backlog 5000 > spike_backlog 1000 -> fast-path scale up despite cooldown.
        assert_eq!(decide_slo(&p, &base).action, ScalingAction::ScaleUp);
    }

    #[test]
    fn threshold_mode_basic() {
        let p = ThresholdParams {
            depth_threshold: 40,
            ram_ratio_cap: 0.9,
            min_workers: 0,
            max_workers: 10,
        };
        assert_eq!(
            decide_threshold(
                &p,
                &ThresholdInputs {
                    backlog: 100,
                    running: 1,
                    ram_ratio: 0.5
                }
            ),
            ScalingAction::ScaleUp
        );
        assert_eq!(
            decide_threshold(
                &p,
                &ThresholdInputs {
                    backlog: 0,
                    running: 2,
                    ram_ratio: 0.5
                }
            ),
            ScalingAction::ScaleDown
        );
        assert_eq!(
            decide_threshold(
                &p,
                &ThresholdInputs {
                    backlog: 10,
                    running: 2,
                    ram_ratio: 0.5
                }
            ),
            ScalingAction::Hold
        );
    }

    #[test]
    fn estimators_measure_mu_and_lambda() {
        let mut est = Estimators::new(1.0, 1.0); // alpha=1 -> take latest sample
        let dt = Duration::from_secs(1);
        // First observe just seeds last_backlog.
        est.observe(1000, 2, dt, false);
        assert!(est.mu().is_none());
        // Backlog dropped by 100 over 1s with 2 workers, no arrivals.
        // lambda_raw = (delta + mu_prev*running*dt)/dt = (-100 + 0)/1 = -100 -> clamped 0.
        // observed_drain = lambda*dt - delta = 0 - (-100) = 100 -> mu_raw = 100/(2*1) = 50.
        est.observe(900, 2, dt, false);
        assert_eq!(est.mu(), Some(50.0));
    }
}
