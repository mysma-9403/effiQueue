//! Minimal Prometheus `/metrics` exposition — no HTTP framework, one TCP task.
//!
//! Exposes the signature series (`workers_needed`, `workers_capacity`,
//! `feasibility_gap`) alongside `mu`/`lambda`/`backlog`/`workers` and scale
//! counters. Floats are stored as milli-units in atomics (no atomic f64).

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Snapshot pushed from the control loop each tick.
pub struct Snapshot {
    pub workers: u64,
    pub backlog: u64,
    /// `-1` = not yet measured (SLO bootstrap) or threshold mode.
    pub workers_needed: i64,
    pub workers_capacity: u64,
    pub feasibility_gap: u64,
    pub mu: f64,
    pub lambda: f64,
}

#[derive(Default)]
pub struct Metrics {
    workers: AtomicU64,
    backlog: AtomicU64,
    workers_needed: AtomicI64,
    workers_capacity: AtomicU64,
    feasibility_gap: AtomicU64,
    mu_milli: AtomicU64,
    lambda_milli: AtomicU64,
    scale_up_total: AtomicU64,
    scale_down_total: AtomicU64,
}

impl Metrics {
    pub fn update(&self, s: &Snapshot) {
        self.workers.store(s.workers, Ordering::Relaxed);
        self.backlog.store(s.backlog, Ordering::Relaxed);
        self.workers_needed
            .store(s.workers_needed, Ordering::Relaxed);
        self.workers_capacity
            .store(s.workers_capacity, Ordering::Relaxed);
        self.feasibility_gap
            .store(s.feasibility_gap, Ordering::Relaxed);
        self.mu_milli
            .store((s.mu * 1000.0).max(0.0) as u64, Ordering::Relaxed);
        self.lambda_milli
            .store((s.lambda * 1000.0).max(0.0) as u64, Ordering::Relaxed);
    }

    pub fn inc_scale_up(&self) {
        self.scale_up_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_scale_down(&self) {
        self.scale_down_total.fetch_add(1, Ordering::Relaxed);
    }

    fn render(&self) -> String {
        let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
        let mu = self.mu_milli.load(Ordering::Relaxed) as f64 / 1000.0;
        let lambda = self.lambda_milli.load(Ordering::Relaxed) as f64 / 1000.0;
        let mut s = String::new();
        let mut metric = |name: &str, kind: &str, help: &str, val: String| {
            s.push_str(&format!(
                "# HELP {name} {help}\n# TYPE {name} {kind}\n{name} {val}\n"
            ));
        };
        metric(
            "effiqueue_workers",
            "gauge",
            "Number of live workers.",
            g(&self.workers).to_string(),
        );
        metric(
            "effiqueue_backlog",
            "gauge",
            "Queue depth (messages).",
            g(&self.backlog).to_string(),
        );
        metric(
            "effiqueue_workers_needed",
            "gauge",
            "Workers per Little's Law (-1 = unknown).",
            self.workers_needed.load(Ordering::Relaxed).to_string(),
        );
        metric(
            "effiqueue_workers_capacity",
            "gauge",
            "Capacity per RAM budget.",
            g(&self.workers_capacity).to_string(),
        );
        metric(
            "effiqueue_feasibility_gap",
            "gauge",
            "Missing workers (needed - capacity).",
            g(&self.feasibility_gap).to_string(),
        );
        metric(
            "effiqueue_mu",
            "gauge",
            "Measured throughput per worker (msgs/s).",
            format!("{mu}"),
        );
        metric(
            "effiqueue_lambda",
            "gauge",
            "Measured arrival rate (msgs/s).",
            format!("{lambda}"),
        );
        metric(
            "effiqueue_scale_up_total",
            "counter",
            "Total number of scale-up actions.",
            g(&self.scale_up_total).to_string(),
        );
        metric(
            "effiqueue_scale_down_total",
            "counter",
            "Total number of scale-down actions.",
            g(&self.scale_down_total).to_string(),
        );
        s
    }
}

/// Serve `/metrics` on `addr` until the process exits. Best-effort: bind errors
/// are logged and the task returns without killing the daemon.
pub async fn serve(addr: String, metrics: Arc<Metrics>) {
    let listener = match TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(%addr, error = %e, "failed to open the metrics endpoint");
            return;
        }
    };
    tracing::info!(%addr, "Prometheus metrics endpoint: GET /metrics");
    loop {
        let (mut sock, _) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "accept error on the metrics endpoint");
                continue;
            }
        };
        let body = metrics.render();
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await; // drain request (best-effort)
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        });
    }
}
