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
    pub scale_up_total: u64,
    pub scale_down_total: u64,
}

#[derive(Default)]
pub struct Metrics {
    programs: Mutex<Vec<ProgramSnapshot>>,
}

impl Metrics {
    /// Replace the current snapshot (called once per control-loop tick).
    pub fn set(&self, snapshots: Vec<ProgramSnapshot>) {
        *self.programs.lock().unwrap() = snapshots;
    }

    fn render(&self) -> String {
        let progs = self.programs.lock().unwrap();
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
