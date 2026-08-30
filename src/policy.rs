//! Pure, testable scaling logic — no filesystem, processes, or network.
//!
//! Implements the Budget-Bound SLO Controller (DESIGN §2–§3): Little's Law for
//! `workers_needed`, a RAM budget for `workers_capacity`, and the signature
//! Feasibility Gap when the SLO cannot fit on this host. Also hosts the simpler
//! `threshold` fallback mode.
//!
//! `mu` and `lambda` themselves are measured in [`crate::estimator`].

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
    /// The estimator cannot identify `mu` until the worker count varies. When
    /// set, the controller may deliberately step a worker to create that spread.
    pub needs_probe: bool,
}

/// Full result of an SLO decision (diagnostics + action).
#[derive(Debug, Clone)]
pub struct Decision {
    pub action: ScalingAction,
    pub workers_needed: Option<u32>,
    pub workers_capacity: u32,
    pub feasibility_gap: Option<FeasibilityGap>,
    /// The action was taken to make `mu` observable, not to meet the SLO.
    pub probing: bool,
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

    let (action, probing) = decide_action(p, i, workers_needed, capacity);

    Decision {
        action,
        workers_needed,
        workers_capacity: capacity,
        feasibility_gap,
        probing,
    }
}

/// Returns the action and whether it was taken purely to make `mu` observable.
fn decide_action(
    p: &SloParams,
    i: &SloInputs,
    needed: Option<u32>,
    capacity: u32,
) -> (ScalingAction, bool) {
    // Meet the floor / ceiling promptly (ignore cooldown).
    if i.running < p.min_workers {
        return (ScalingAction::ScaleUp, false);
    }
    if i.running > p.max_workers {
        return (ScalingAction::ScaleDown, false);
    }

    let Some(needed) = needed else {
        // Bootstrap: no reliable mu yet.
        if i.backlog == 0 {
            return (ScalingAction::Hold, false);
        }
        // Growing the pool both drains the backlog and varies `n`, which is what
        // makes mu identifiable. Prefer it whenever there is room.
        if i.running < capacity && i.running < p.max_workers {
            return (ScalingAction::ScaleUp, i.needs_probe);
        }
        // Pinned at the ceiling with mu still unknown: the worker count would
        // never move again and mu would stay unobservable forever. Step one
        // worker down to buy the spread the regression needs. This is the
        // scale-by-one perturbation the design is built on.
        if i.needs_probe && i.running > p.min_workers {
            return (ScalingAction::ScaleDown, true);
        }
        return (ScalingAction::Hold, false);
    };

    let target = needed.min(capacity).clamp(p.min_workers, p.max_workers);
    let cooldown_ok = i.ticks_since_change >= p.cooldown_ticks;
    let spike = i.backlog > p.spike_backlog;

    if target > i.running.saturating_add(p.hysteresis) {
        // Scale-up may bypass cooldown on a spike (never scale-down).
        if cooldown_ok || spike {
            (ScalingAction::ScaleUp, false)
        } else {
            (ScalingAction::Hold, false)
        }
    } else if target < i.running.saturating_sub(p.hysteresis) {
        if cooldown_ok {
            (ScalingAction::ScaleDown, false)
        } else {
            (ScalingAction::Hold, false)
        }
    } else {
        (ScalingAction::Hold, false)
    }
}

/// Limits + thresholds for the simpler `threshold` fallback mode.
#[derive(Debug, Clone)]
pub struct ThresholdParams {
    pub depth_threshold: u32,
    pub ram_ratio_cap: f64,
    pub min_workers: u32,
    pub max_workers: u32,
    /// Ticks to wait after a change before the next one.
    pub cooldown_ticks: u32,
}

/// Inputs for the `threshold` fallback decision.
#[derive(Debug, Clone)]
pub struct ThresholdInputs {
    pub backlog: u32,
    pub running: u32,
    pub ram_ratio: f64,
    pub ticks_since_change: u32,
}

/// Simple depth + RAM% fallback (mirrors the pre-SLO behavior, safely bounded).
///
/// Scale-down is cooldown-gated. Without that gate an AMQP-only depth reading —
/// which counts messages *ready*, not messages in flight — reports zero the
/// instant the last message is delivered, while workers are still processing it,
/// and the pool flaps down and straight back up.
pub fn decide_threshold(p: &ThresholdParams, i: &ThresholdInputs) -> ScalingAction {
    if i.running < p.min_workers {
        return ScalingAction::ScaleUp;
    }
    let cooldown_ok = i.ticks_since_change >= p.cooldown_ticks;
    if i.backlog == 0 && i.running > p.min_workers {
        return if cooldown_ok {
            ScalingAction::ScaleDown
        } else {
            ScalingAction::Hold
        };
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
            needs_probe: false,
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
            needs_probe: false,
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
            needs_probe: false,
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
            needs_probe: false,
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
            needs_probe: false,
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
            cooldown_ticks: 0,
        };
        let inputs = |backlog, running| ThresholdInputs {
            backlog,
            running,
            ram_ratio: 0.5,
            ticks_since_change: 5,
        };
        assert_eq!(
            decide_threshold(&p, &inputs(100, 1)),
            ScalingAction::ScaleUp
        );
        assert_eq!(
            decide_threshold(&p, &inputs(0, 2)),
            ScalingAction::ScaleDown
        );
        assert_eq!(decide_threshold(&p, &inputs(10, 2)), ScalingAction::Hold);
    }

    #[test]
    fn threshold_scale_down_respects_cooldown() {
        // An AMQP depth of 0 can mean "all messages are in flight, unacked".
        // Dropping a worker on the first such tick is how the pool flaps.
        let p = ThresholdParams {
            depth_threshold: 40,
            ram_ratio_cap: 0.9,
            min_workers: 0,
            max_workers: 10,
            cooldown_ticks: 3,
        };
        let at = |ticks_since_change| ThresholdInputs {
            backlog: 0,
            running: 2,
            ram_ratio: 0.5,
            ticks_since_change,
        };
        assert_eq!(decide_threshold(&p, &at(0)), ScalingAction::Hold);
        assert_eq!(decide_threshold(&p, &at(2)), ScalingAction::Hold);
        assert_eq!(decide_threshold(&p, &at(3)), ScalingAction::ScaleDown);
    }

    // --- Probing: the controller's half of the identifiability contract ---

    fn probe_inputs(running: u32, needs_probe: bool) -> SloInputs {
        SloInputs {
            backlog: 5_000,
            running,
            mu: None,
            lambda: 0.0,
            worker_rss: Some(100 * 1024 * 1024),
            safe_ram_budget: 8 * 1024 * 1024 * 1024,
            swap_pressure: false,
            ticks_since_change: 10,
            needs_probe,
        }
    }

    #[test]
    fn probes_down_when_pinned_at_the_ceiling_without_mu() {
        // At max_workers with mu still unknown, holding forever would freeze the
        // worker count and mu could never become observable.
        let mut p = slo_params();
        p.max_workers = 4;
        let d = decide_slo(&p, &probe_inputs(4, true));
        assert_eq!(d.action, ScalingAction::ScaleDown);
        assert!(d.probing, "the step down is a probe, not an SLO decision");
    }

    #[test]
    fn does_not_probe_down_when_mu_is_not_wanted() {
        let mut p = slo_params();
        p.max_workers = 4;
        let d = decide_slo(&p, &probe_inputs(4, false));
        assert_eq!(d.action, ScalingAction::Hold);
        assert!(!d.probing);
    }

    #[test]
    fn probe_never_breaches_the_floor() {
        let mut p = slo_params();
        p.max_workers = 2;
        p.min_workers = 2;
        let d = decide_slo(&p, &probe_inputs(2, true));
        assert_eq!(d.action, ScalingAction::Hold);
    }

    #[test]
    fn prefers_scaling_up_over_probing_down_when_there_is_room() {
        let mut p = slo_params();
        p.max_workers = 10;
        let d = decide_slo(&p, &probe_inputs(3, true));
        assert_eq!(d.action, ScalingAction::ScaleUp);
    }

    #[test]
    fn never_probes_an_empty_queue() {
        let mut p = slo_params();
        p.max_workers = 4;
        let mut i = probe_inputs(4, true);
        i.backlog = 0;
        assert_eq!(decide_slo(&p, &i).action, ScalingAction::Hold);
    }
}
