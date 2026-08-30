//! Minimal Prometheus `/metrics` exposition — no HTTP framework, one TCP task.
//!
//! The control loop pushes a per-program snapshot each tick; the endpoint renders
//! it with a `program="..."` label. Exposes the signature series
//! (`workers_needed`, `workers_capacity`, `feasibility_gap`) plus
//! `mu`/`lambda`/`backlog`/`workers` and scale counters.

use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// One program's metric values for a tick.
#[derive(Clone)]
pub struct ProgramSnapshot {
    pub name: Arc<str>,
    pub workers: u64,
    pub backlog: u64,
    /// `-1` = not yet measured (SLO bootstrap) or threshold mode.
    pub workers_needed: i64,
    pub workers_capacity: u64,
    pub feasibility_gap: u64,
    pub mu: f64,
    pub lambda: f64,
    /// 0 = none, 1 = broker-measured, 2 = regression. See `estimator::Source`.
    pub mu_source: u64,
    /// 1 while this tick's action was an identifiability probe.
    pub probing: u64,
    pub scale_up_total: u64,
    pub scale_down_total: u64,
    pub probe_total: u64,
}

#[derive(Default)]
pub struct Metrics {
    programs: Mutex<Vec<ProgramSnapshot>>,
}

impl Metrics {
    /// Replace the current snapshot (called once per control-loop tick).
    pub fn set(&self, snapshots: Vec<ProgramSnapshot>) {
        *self.lock() = snapshots;
    }

    /// Recover from poisoning rather than propagating it: a panic elsewhere must
    /// not turn every subsequent scrape into another panic.
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<ProgramSnapshot>> {
        self.programs.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn render(&self) -> String {
        let progs = self.lock();
        let mut s = String::new();
        block(
            &mut s,
            &progs,
            "effiqueue_workers",
            "gauge",
            "Number of live workers.",
            |p| p.workers.to_string(),
        );
        block(
            &mut s,
            &progs,
            "effiqueue_backlog",
            "gauge",
            "Queue depth (messages).",
            |p| p.backlog.to_string(),
        );
        block(
            &mut s,
            &progs,
            "effiqueue_workers_needed",
            "gauge",
            "Workers per Little's Law (-1 = unknown).",
            |p| p.workers_needed.to_string(),
        );
        block(
            &mut s,
            &progs,
            "effiqueue_workers_capacity",
            "gauge",
            "Capacity from the RAM budget.",
            |p| p.workers_capacity.to_string(),
        );
        block(
            &mut s,
            &progs,
            "effiqueue_feasibility_gap",
            "gauge",
            "Missing workers (needed - capacity).",
            |p| p.feasibility_gap.to_string(),
        );
        block(
            &mut s,
            &progs,
            "effiqueue_mu",
            "gauge",
            "Measured throughput per worker (msgs/s).",
            |p| format!("{}", p.mu),
        );
        block(
            &mut s,
            &progs,
            "effiqueue_lambda",
            "gauge",
            "Measured arrival rate (msgs/s).",
            |p| format!("{}", p.lambda),
        );
        block(
            &mut s,
            &progs,
            "effiqueue_mu_source",
            "gauge",
            "How mu is being measured: 0 none, 1 broker rates, 2 regression.",
            |p| p.mu_source.to_string(),
        );
        block(
            &mut s,
            &progs,
            "effiqueue_probing",
            "gauge",
            "1 while the controller is perturbing worker count to identify mu.",
            |p| p.probing.to_string(),
        );
        block(
            &mut s,
            &progs,
            "effiqueue_scale_up_total",
            "counter",
            "Total scale-ups.",
            |p| p.scale_up_total.to_string(),
        );
        block(
            &mut s,
            &progs,
            "effiqueue_scale_down_total",
            "counter",
            "Total scale-downs.",
            |p| p.scale_down_total.to_string(),
        );
        block(
            &mut s,
            &progs,
            "effiqueue_probe_total",
            "counter",
            "Total identifiability probes.",
            |p| p.probe_total.to_string(),
        );
        s
    }
}

fn block(
    s: &mut String,
    progs: &[ProgramSnapshot],
    name: &str,
    kind: &str,
    help: &str,
    val: impl Fn(&ProgramSnapshot) -> String,
) {
    s.push_str(&format!("# HELP {name} {help}\n# TYPE {name} {kind}\n"));
    for p in progs {
        let label = p.name.replace('\\', "\\\\").replace('"', "\\\"");
        s.push_str(&format!("{name}{{program=\"{label}\"}} {}\n", val(p)));
    }
}

/// Serve `/metrics` on `addr` until the process exits. Best-effort: bind errors
/// are logged and the task returns without killing the daemon.
pub async fn serve(addr: String, metrics: Arc<Metrics>) {
    let listener = match TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(%addr, error = %e, "failed to bind the metrics endpoint");
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
        let metrics = metrics.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            let read = sock.read(&mut buf).await.unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..read]);
            let path = request
                .split_whitespace()
                .nth(1)
                .and_then(|p| p.split('?').next())
                .unwrap_or("");
            // Render only for the real path, so a stray probe on / does not get
            // mistaken for a working scrape target.
            let resp = match path {
                "/metrics" => {
                    let body = metrics.render();
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                }
                "/" => {
                    let body = "effiQueue: metrics are at /metrics\n";
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                }
                _ => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_string(),
            };
            let _ = sock.write_all(resp.as_bytes()).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(name: &str) -> ProgramSnapshot {
        ProgramSnapshot {
            name: Arc::from(name),
            workers: 4,
            backlog: 1200,
            workers_needed: 7,
            workers_capacity: 24,
            feasibility_gap: 0,
            mu: 8.25,
            lambda: 200.0,
            mu_source: 1,
            probing: 0,
            scale_up_total: 12,
            scale_down_total: 3,
            probe_total: 2,
        }
    }

    #[test]
    fn renders_every_series_with_a_program_label() {
        let m = Metrics::default();
        m.set(vec![snapshot("consumer_00")]);
        let out = m.render();
        for series in [
            "effiqueue_workers",
            "effiqueue_backlog",
            "effiqueue_workers_needed",
            "effiqueue_workers_capacity",
            "effiqueue_feasibility_gap",
            "effiqueue_mu",
            "effiqueue_lambda",
            "effiqueue_mu_source",
            "effiqueue_probing",
            "effiqueue_scale_up_total",
            "effiqueue_scale_down_total",
            "effiqueue_probe_total",
        ] {
            assert!(
                out.contains(&format!("# TYPE {series} ")),
                "missing {series}"
            );
            assert!(
                out.contains(&format!("{series}{{program=\"consumer_00\"}} ")),
                "missing labelled sample for {series}"
            );
        }
    }

    #[test]
    fn escapes_quotes_and_backslashes_in_labels() {
        let m = Metrics::default();
        m.set(vec![snapshot(r#"we"ird\name"#)]);
        let out = m.render();
        assert!(out.contains(r#"program="we\"ird\\name""#), "got: {out}");
    }

    #[test]
    fn renders_one_sample_per_program() {
        let m = Metrics::default();
        m.set(vec![snapshot("a"), snapshot("b")]);
        let out = m.render();
        assert_eq!(out.matches("effiqueue_workers{").count(), 2);
        assert_eq!(out.matches("# TYPE effiqueue_workers ").count(), 1);
        assert!(out.contains(r#"effiqueue_workers{program="a"}"#));
        assert!(out.contains(r#"effiqueue_workers{program="b"}"#));
    }

    #[test]
    fn negative_workers_needed_survives_the_round_trip() {
        // -1 is the documented "not measured yet" sentinel.
        let m = Metrics::default();
        let mut s = snapshot("p");
        s.workers_needed = -1;
        m.set(vec![s]);
        assert!(m
            .render()
            .contains(r#"effiqueue_workers_needed{program="p"} -1"#));
    }
}
