//! Identifiable estimation of per-worker throughput (`mu`) and arrival rate
//! (`lambda`).
//!
//! # Why the naive residual method cannot work
//!
//! Each control window yields exactly one equation of queue balance:
//!
//! ```text
//! dB/dt = lambda - mu * n
//! ```
//!
//! with two unknowns. *Every* pair `(mu, lambda)` on that line explains the
//! observation equally well, so no amount of smoothing recovers the truth from a
//! single operating point — the estimate simply freezes at whichever
//! self-consistent point it started from.
//!
//! # The two ways out
//!
//! * [`Source::Direct`] — the broker tells us both rates. RabbitMQ's management
//!   API reports `ack_details.rate` (= `mu * n`) and `publish_details.rate`
//!   (= `lambda`). No inference needed; this is the preferred path.
//!
//! * [`Source::Regression`] — backend-agnostic fallback. Because the controller
//!   moves by exactly one worker at a time, `n` *varies* across windows. Fitting
//!   `dB/dt = lambda - mu*n` by least squares over a sliding window recovers both
//!   parameters: slope is `-mu`, intercept is `lambda`. This is what makes `mu`
//!   observable, and it is the reason the controller scales by one.
//!
//! The regression is only trustworthy when `n` actually moved, so it is gated on
//! the spread `Sxx = sum (n_i - n_mean)^2`. While the gate is closed the estimator
//! reports [`Estimator::needs_probe`] and the controller deliberately perturbs the
//! worker count to open it.

use std::collections::VecDeque;
use std::time::Duration;

/// Where the current estimate came from. The discriminants are exported as the
/// `effiqueue_mu_source` metric, so they are part of the public surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
pub enum Source {
    /// No trustworthy estimate yet.
    None = 0,
    /// Broker-reported ack/publish rates.
    Direct = 1,
    /// Least-squares fit over windows with differing worker counts.
    Regression = 2,
}

/// Broker-reported rates for one queue (messages/second).
#[derive(Debug, Clone, Copy)]
pub struct BrokerRates {
    /// Acknowledgement rate across all consumers — equals `mu * n`.
    pub ack_rate: f64,
    /// Publish rate into the queue — equals `lambda`.
    pub publish_rate: f64,
}

/// One control window handed to the estimator.
#[derive(Debug, Clone)]
pub struct Observation {
    pub backlog: u32,
    /// Worker count active during the window (not the count after scaling).
    pub running: u32,
    /// REAL elapsed time of the window, not the configured poll interval.
    pub dt: Duration,
    /// The running count changed outside our own +/-1 step (crash, operator).
    pub interference: bool,
    /// Broker rates, when a management API is reachable.
    pub rates: Option<BrokerRates>,
}

/// Exponentially-weighted moving average.
#[derive(Debug, Clone)]
pub struct Ewma {
    value: f64,
    alpha: f64,
    initialized: bool,
}

impl Ewma {
    pub fn new(alpha: f64) -> Self {
        Self {
            value: 0.0,
            alpha,
            initialized: false,
        }
    }

    pub fn update(&mut self, sample: f64) {
        if !sample.is_finite() {
            return;
        }
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

/// One regression sample: worker count during the window and the observed
/// backlog slope `dB/dt`.
#[derive(Debug, Clone, Copy)]
struct Sample {
    n: f64,
    y: f64,
}

/// Minimum windows before a fit is attempted.
const MIN_SAMPLES: usize = 12;
/// Minimum `sum (n_i - n_mean)^2`. Below this the design matrix is effectively
/// rank-deficient and `mu` is not identifiable. A window in which `n` alternates
/// between two adjacent values reaches ~3.0 after a dozen samples.
const MIN_SPREAD: f64 = 3.0;
/// Sliding-window length. Long enough to average out bursty arrivals (validated
/// down to ~50% arrival jitter), short enough to track drift.
const DEFAULT_WINDOW: usize = 120;
/// Windows a broker-measured estimate stays trusted after the broker stops
/// answering. Beyond this the number is too old to steer by and `mu` reverts to
/// unknown, which puts the controller back on the bootstrap path and re-arms
/// probing.
const DIRECT_GRACE_WINDOWS: u32 = 30;

/// Live estimator for `mu` and `lambda`.
#[derive(Debug, Clone)]
pub struct Estimator {
    mu: Ewma,
    lambda: Ewma,
    source: Source,
    last_backlog: Option<u32>,
    window: VecDeque<Sample>,
    capacity: usize,
    /// Consecutive windows without broker rates. Zero while the broker answers.
    direct_stale: u32,
}

impl Estimator {
    pub fn new(alpha_mu: f64, alpha_lambda: f64) -> Self {
        Self::with_window(alpha_mu, alpha_lambda, DEFAULT_WINDOW)
    }

    pub fn with_window(alpha_mu: f64, alpha_lambda: f64, capacity: usize) -> Self {
        Self {
            mu: Ewma::new(alpha_mu),
            lambda: Ewma::new(alpha_lambda),
            source: Source::None,
            last_backlog: None,
            window: VecDeque::with_capacity(capacity),
            capacity,
            direct_stale: 0,
        }
    }

    /// Drop the backlog baseline after a window we could not read.
    ///
    /// Without this the next successful read would be differenced against a
    /// stale backlog spanning several windows, while being divided by a single
    /// window's `dt` — inflating both rates.
    pub fn forget_backlog(&mut self) {
        self.last_backlog = None;
    }

    /// Feed one control window.
    pub fn observe(&mut self, o: &Observation) {
        let dt_s = o.dt.as_secs_f64();
        if dt_s <= 0.0 {
            return;
        }

        // Preferred path: the broker measured both rates for us.
        if let Some(rates) = o.rates {
            if rates.publish_rate >= 0.0 {
                self.lambda.update(rates.publish_rate);
            }
            if o.running > 0 && rates.ack_rate > 0.0 {
                self.mu.update(rates.ack_rate / o.running as f64);
                self.source = Source::Direct;
                self.direct_stale = 0;
            }
            // Keep collecting regression samples so the fallback stays warm if
            // the management API disappears mid-run. `push_sample` advances the
            // backlog baseline itself — doing it here first would difference the
            // reading against itself and fill the window with zeros.
            self.push_sample(o, dt_s);
            return;
        }

        // No broker rates this window.
        self.push_sample(o, dt_s);
        let fitted = self.refit();

        if fitted {
            self.source = Source::Regression;
            return;
        }
        if self.source == Source::Direct {
            // The estimate is still the broker's, but the broker has gone quiet.
            // Reporting it as `Direct` forever would misstate where the number
            // came from AND permanently suppress probing, since a live direct
            // source never needs one.
            self.direct_stale = self.direct_stale.saturating_add(1);
            if self.direct_stale > DIRECT_GRACE_WINDOWS {
                self.source = Source::None;
            }
        }
    }

    /// Record a `(n, dB/dt)` pair, skipping windows that carry no usable signal.
    fn push_sample(&mut self, o: &Observation, dt_s: f64) {
        let Some(prev) = self.last_backlog else {
            self.last_backlog = Some(o.backlog);
            return;
        };
        self.last_backlog = Some(o.backlog);

        // The worker count during the window is not what we think it was.
        if o.interference {
            return;
        }
        // An empty queue censors the measurement: the drain was limited by the
        // work available, not by worker capacity, so `y` understates throughput.
        if o.backlog == 0 {
            return;
        }

        let y = (o.backlog as f64 - prev as f64) / dt_s;
        if !y.is_finite() {
            return;
        }
        if self.window.len() == self.capacity {
            self.window.pop_front();
        }
        self.window.push_back(Sample {
            n: o.running as f64,
            y,
        });
    }

    /// Ordinary least squares of `y = lambda - mu*n` over the window.
    /// Returns whether a trustworthy fit was produced.
    fn refit(&mut self) -> bool {
        if self.window.len() < MIN_SAMPLES {
            return false;
        }
        let count = self.window.len() as f64;
        let n_mean = self.window.iter().map(|s| s.n).sum::<f64>() / count;
        let y_mean = self.window.iter().map(|s| s.y).sum::<f64>() / count;
        let sxx: f64 = self.window.iter().map(|s| (s.n - n_mean).powi(2)).sum();

        // `n` never moved enough — mu is not identifiable from this window.
        if sxx < MIN_SPREAD {
            return false;
        }
        let sxy: f64 = self
            .window
            .iter()
            .map(|s| (s.n - n_mean) * (s.y - y_mean))
            .sum();
        let slope = sxy / sxx;
        if !slope.is_finite() {
            return false;
        }
        let mu_hat = -slope;
        let lambda_hat = y_mean - slope * n_mean;

        // A non-positive slope means "more workers drained less", which is noise,
        // not physics. Reject rather than publish a nonsense estimate.
        // `is_finite` runs first so the comparison below never sees a NaN.
        if !mu_hat.is_finite() || !lambda_hat.is_finite() || mu_hat <= 0.0 {
            return false;
        }

        self.mu.update(mu_hat);
        self.lambda.update(lambda_hat.max(0.0));
        true
    }

    /// Measured throughput per worker, or `None` while it is not identifiable.
    pub fn mu(&self) -> Option<f64> {
        if self.source != Source::None && self.mu.is_initialized() && self.mu.value() > 0.0 {
            Some(self.mu.value())
        } else {
            None
        }
    }

    pub fn lambda(&self) -> f64 {
        self.lambda.value()
    }

    pub fn source(&self) -> Source {
        self.source
    }

    /// True when the worker count must be perturbed for `mu` to become
    /// observable: no estimate yet, and the window lacks the spread to produce
    /// one. The controller answers by stepping one worker up or down.
    pub fn needs_probe(&self) -> bool {
        // The broker is answering; there is nothing to identify.
        if self.source == Source::Direct && self.direct_stale == 0 {
            return false;
        }
        self.mu().is_none() && self.spread() < MIN_SPREAD
    }

    /// `sum (n_i - n_mean)^2` over the window — the identifiability budget.
    pub fn spread(&self) -> f64 {
        if self.window.len() < 2 {
            return 0.0;
        }
        let count = self.window.len() as f64;
        let n_mean = self.window.iter().map(|s| s.n).sum::<f64>() / count;
        self.window.iter().map(|s| (s.n - n_mean).powi(2)).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DT: Duration = Duration::from_secs(10);

    /// Drive the estimator over a synthetic queue with KNOWN parameters.
    /// Ground truth is `dB/dt = lambda - mu*n`. `perturb` mimics the controller
    /// stepping one worker at a time, which is what makes `mu` observable.
    struct Sim {
        mu: f64,
        lambda: f64,
        backlog: f64,
        n: u32,
        /// Worker count that was running during the window just ended.
        n_window: u32,
        seed: u64,
    }

    impl Sim {
        fn new(mu: f64, lambda: f64, backlog: f64, n: u32) -> Self {
            Self {
                mu,
                lambda,
                backlog,
                n,
                n_window: n,
                seed: 0x2545_F491_4F6C_DD1D,
            }
        }

        /// Deterministic xorshift in [0,1) — no rand dependency in tests.
        fn rand(&mut self) -> f64 {
            self.seed ^= self.seed << 13;
            self.seed ^= self.seed >> 7;
            self.seed ^= self.seed << 17;
            (self.seed >> 11) as f64 / (1u64 << 53) as f64
        }

        fn run(&mut self, est: &mut Estimator, ticks: usize, perturb: bool, jitter: f64) {
            for _ in 0..ticks {
                if perturb {
                    let step = self.rand();
                    if step < 0.34 {
                        self.n = self.n.saturating_sub(1).max(1);
                    } else if step > 0.66 {
                        self.n = (self.n + 1).min(40);
                    }
                }
                self.step(est, self.n, jitter);
            }
        }

        /// One control window, ordered exactly as `main`'s loop orders it: the
        /// backlog we see now moved under the worker count that was running
        /// during the window that just ended, so that is what gets observed.
        /// Pairing the reading with the *new* count instead inverts the fitted
        /// slope whenever the count alternates.
        fn step(&mut self, est: &mut Estimator, n_next: u32, jitter: f64) {
            let dt_s = DT.as_secs_f64();
            est.observe(&Observation {
                backlog: self.backlog as u32,
                running: self.n_window,
                dt: DT,
                interference: false,
                rates: None,
            });
            let lam = if jitter > 0.0 {
                (self.lambda * (1.0 + jitter * (self.rand() * 2.0 - 1.0))).max(0.0)
            } else {
                self.lambda
            };
            self.backlog = (self.backlog + lam * dt_s - self.mu * n_next as f64 * dt_s).max(0.0);
            self.n_window = n_next;
            // Keep the queue in a measurable range; a pinned-empty queue carries
            // no capacity information by construction.
            if self.backlog < 1.0 || self.backlog > 400_000.0 {
                self.backlog = 20_000.0;
            }
        }
    }

    fn assert_close(actual: Option<f64>, truth: f64, tol: f64, what: &str) {
        let got = actual.unwrap_or_else(|| panic!("{what}: no estimate produced"));
        assert!(
            (got - truth).abs() <= tol,
            "{what}: got {got:.3}, truth {truth:.3} (tolerance {tol})"
        );
    }

    #[test]
    fn ewma_initializes_then_smooths() {
        let mut e = Ewma::new(0.5);
        assert!(!e.is_initialized());
        e.update(10.0);
        assert_eq!(e.value(), 10.0);
        e.update(20.0);
        assert_eq!(e.value(), 15.0);
    }

    #[test]
    fn ewma_ignores_non_finite_samples() {
        let mut e = Ewma::new(0.5);
        e.update(10.0);
        e.update(f64::NAN);
        e.update(f64::INFINITY);
        assert_eq!(e.value(), 10.0);
    }

    // --- The regression path: the regimes the old residual estimator failed ---

    #[test]
    fn converges_while_backlog_is_growing() {
        // The README's headline Feasibility Gap scenario: mu=8, lambda=200.
        let mut est = Estimator::new(0.5, 0.5);
        Sim::new(8.0, 200.0, 50_000.0, 10).run(&mut est, 200, true, 0.0);
        assert_close(est.mu(), 8.0, 0.5, "mu (growing backlog)");
        assert_close(Some(est.lambda()), 200.0, 10.0, "lambda (growing backlog)");
    }

    #[test]
    fn converges_in_steady_state() {
        let mut est = Estimator::new(0.5, 0.5);
        Sim::new(8.0, 80.0, 20_000.0, 10).run(&mut est, 200, true, 0.0);
        assert_close(est.mu(), 8.0, 0.5, "mu (steady state)");
        assert_close(Some(est.lambda()), 80.0, 8.0, "lambda (steady state)");
    }

    #[test]
    fn converges_while_draining() {
        let mut est = Estimator::new(0.5, 0.5);
        Sim::new(8.0, 20.0, 80_000.0, 10).run(&mut est, 200, true, 0.0);
        assert_close(est.mu(), 8.0, 0.5, "mu (draining)");
        assert_close(Some(est.lambda()), 20.0, 5.0, "lambda (draining)");
    }

    #[test]
    fn converges_under_bursty_arrivals() {
        let mut est = Estimator::new(0.3, 0.3);
        Sim::new(8.0, 70.0, 30_000.0, 10).run(&mut est, 600, true, 0.5);
        assert_close(est.mu(), 8.0, 2.0, "mu (bursty)");
    }

    #[test]
    fn converges_for_slow_workers() {
        let mut est = Estimator::new(0.5, 0.5);
        Sim::new(2.5, 40.0, 30_000.0, 10).run(&mut est, 200, true, 0.0);
        assert_close(est.mu(), 2.5, 0.3, "mu (slow workers)");
    }

    // --- Identifiability gates ---

    #[test]
    fn withholds_mu_when_worker_count_never_varies() {
        // Without perturbation the system is rank-deficient. Reporting *any* mu
        // here would be the old bug: a confident, self-consistent, wrong number.
        let mut est = Estimator::new(0.5, 0.5);
        Sim::new(8.0, 200.0, 50_000.0, 10).run(&mut est, 200, false, 0.0);
        assert_eq!(est.mu(), None, "mu must stay None when n never varies");
        assert!(est.needs_probe(), "estimator should ask for a probe");
        assert_eq!(est.source(), Source::None);
    }

    #[test]
    fn probe_request_clears_once_identifiable() {
        let mut est = Estimator::new(0.5, 0.5);
        let mut sim = Sim::new(8.0, 200.0, 50_000.0, 10);
        sim.run(&mut est, 30, false, 0.0);
        assert!(est.needs_probe());
        sim.run(&mut est, 200, true, 0.0);
        assert!(!est.needs_probe(), "probe should be satisfied after spread");
        assert_eq!(est.source(), Source::Regression);
    }

    #[test]
    fn empty_queue_windows_are_not_sampled() {
        // A drained queue limits throughput by available work, not capacity.
        let mut est = Estimator::new(0.5, 0.5);
        for _ in 0..100 {
            est.observe(&Observation {
                backlog: 0,
                running: 5,
                dt: DT,
                interference: false,
                rates: None,
            });
        }
        assert_eq!(est.mu(), None);
        assert_eq!(est.spread(), 0.0);
    }

    #[test]
    fn interference_windows_are_not_sampled() {
        let mut est = Estimator::new(0.5, 0.5);
        for i in 0..100 {
            est.observe(&Observation {
                backlog: 1000 + i,
                running: 5 + (i % 3),
                dt: DT,
                interference: true,
                rates: None,
            });
        }
        assert_eq!(est.mu(), None);
        assert_eq!(est.spread(), 0.0);
    }

    // --- The direct path ---

    #[test]
    fn direct_rates_are_used_immediately() {
        let mut est = Estimator::new(1.0, 1.0);
        est.observe(&Observation {
            backlog: 5_000,
            running: 10,
            dt: DT,
            interference: false,
            rates: Some(BrokerRates {
                ack_rate: 80.0,
                publish_rate: 200.0,
            }),
        });
        assert_eq!(est.source(), Source::Direct);
        assert_eq!(est.mu(), Some(8.0)); // 80 acks/s across 10 workers
        assert_eq!(est.lambda(), 200.0);
        assert!(!est.needs_probe(), "direct measurement never needs a probe");
    }

    #[test]
    fn direct_path_keeps_the_regression_window_usable() {
        // While the broker answers, samples are still collected so the fallback
        // is warm if the management API disappears mid-run. The baseline must be
        // advanced exactly once per window — differencing a reading against
        // itself would fill the window with zero slopes and quietly disarm the
        // fallback at the moment it is needed.
        let mut est = Estimator::new(0.5, 0.5);
        let dt_s = DT.as_secs_f64();
        let (mu_true, lambda_true) = (8.0, 200.0);
        let mut backlog = 50_000.0;
        // `n_window` is the count that was running during the window being
        // reported, matching how `main` feeds the estimator.
        let mut n_window = 10u32;

        for i in 0..200u32 {
            est.observe(&Observation {
                backlog: backlog as u32,
                running: n_window,
                dt: DT,
                interference: false,
                rates: Some(BrokerRates {
                    ack_rate: mu_true * n_window as f64,
                    publish_rate: lambda_true,
                }),
            });
            let n_next = if i % 2 == 0 { 11 } else { 10 };
            backlog = (backlog + lambda_true * dt_s - mu_true * n_next as f64 * dt_s).max(1.0);
            if backlog > 400_000.0 {
                backlog = 20_000.0;
            }
            n_window = n_next;
        }
        assert_eq!(est.source(), Source::Direct);
        assert_eq!(est.mu(), Some(8.0));
        assert!(
            est.spread() > 0.0,
            "the regression window collected no spread while the broker was up"
        );

        // The broker goes away. The fallback must take over immediately, using
        // the window filled while the broker was still answering.
        for i in 0..5u32 {
            est.observe(&Observation {
                backlog: backlog as u32,
                running: n_window,
                dt: DT,
                interference: false,
                rates: None,
            });
            let n_next = if i % 2 == 0 { 11 } else { 10 };
            backlog = (backlog + lambda_true * dt_s - mu_true * n_next as f64 * dt_s).max(1.0);
            if backlog > 400_000.0 {
                backlog = 20_000.0;
            }
            n_window = n_next;
        }
        assert_close(est.mu(), 8.0, 1.0, "mu after the broker went away");
        assert_eq!(
            est.source(),
            Source::Regression,
            "mu_source must stop claiming broker rates once the broker is gone"
        );
    }

    #[test]
    fn a_stale_broker_estimate_eventually_reverts_to_unknown() {
        // The broker answers once, then never again, and the queue gives the
        // regression nothing to work with (n pinned). Reporting the original
        // number as a live broker measurement forever would both misstate its
        // provenance and permanently suppress probing, because a live direct
        // source never asks for one.
        let mut est = Estimator::new(1.0, 1.0);
        est.observe(&Observation {
            backlog: 5_000,
            running: 10,
            dt: DT,
            interference: false,
            rates: Some(BrokerRates {
                ack_rate: 80.0,
                publish_rate: 200.0,
            }),
        });
        assert_eq!(est.source(), Source::Direct);
        assert!(!est.needs_probe());

        for i in 0..DIRECT_GRACE_WINDOWS {
            est.observe(&Observation {
                backlog: 5_000 + i,
                running: 10,
                dt: DT,
                interference: false,
                rates: None,
            });
        }
        // Still inside the grace window: the number is old but usable.
        assert_eq!(est.source(), Source::Direct);
        assert_eq!(est.mu(), Some(8.0));

        for i in 0..10 {
            est.observe(&Observation {
                backlog: 6_000 + i,
                running: 10,
                dt: DT,
                interference: false,
                rates: None,
            });
        }
        assert_eq!(est.source(), Source::None);
        assert_eq!(est.mu(), None, "a long-stale estimate must not steer");
        assert!(
            est.needs_probe(),
            "with the broker gone and no fit, the controller must be told to probe"
        );
    }

    #[test]
    fn direct_path_survives_zero_workers() {
        let mut est = Estimator::new(1.0, 1.0);
        est.observe(&Observation {
            backlog: 5_000,
            running: 0,
            dt: DT,
            interference: false,
            rates: Some(BrokerRates {
                ack_rate: 0.0,
                publish_rate: 42.0,
            }),
        });
        assert_eq!(est.mu(), None);
        assert_eq!(est.lambda(), 42.0);
    }

    #[test]
    fn zero_dt_is_ignored() {
        let mut est = Estimator::new(0.5, 0.5);
        est.observe(&Observation {
            backlog: 100,
            running: 2,
            dt: Duration::ZERO,
            interference: false,
            rates: None,
        });
        assert_eq!(est.mu(), None);
    }
}
