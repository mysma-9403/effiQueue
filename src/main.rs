mod config;
mod metrics;
mod platform;
mod policy;
mod rabbitmq_connector;
mod system_info;
mod worker;

use clap::{Parser, Subcommand};
use std::sync::Arc;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "effiqueue", version, about)]
struct Cli {
    /// Path to the configuration (TOML or Supervisor-style .conf).
    #[arg(long, default_value = "./data/config.conf")]
    config: String,
    /// Override the controller mode (slo|threshold).
    #[arg(long)]
    mode: Option<String>,
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the control loop (default).
    Run,
    /// Validate the configuration and exit.
    ValidateConfig,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let mut cfg = config::load(&cli.config)?;
    if let Some(m) = cli.mode.as_deref() {
        cfg.mode = match m {
            "slo" => config::Mode::Slo,
            "threshold" => config::Mode::Threshold,
            other => anyhow::bail!("unknown --mode '{other}' (allowed: slo|threshold)"),
        };
    }

    match cli.command.unwrap_or(Cmd::Run) {
        Cmd::ValidateConfig => {
            log_config(&cfg);
            tracing::info!("configuration is valid");
            Ok(())
        }
        Cmd::Run => run(cfg).await,
    }
}

fn bytes_to_gib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0 * 1024.0)
}

/// Log the effective configuration at startup. Also the canonical read site for
/// the advisory `queue`/`autostart`/`autorestart` fields.
fn log_config(cfg: &config::Config) {
    tracing::info!(
        mode = ?cfg.mode,
        queue = %cfg.queue,
        max_workers = cfg.max_workers,
        min_workers = cfg.min_workers,
        shell = cfg.shell,
        autostart = cfg.autostart,
        autorestart = cfg.autorestart,
        slo_drain_time = ?cfg.slo_drain_time,
        ram_budget = ?cfg.ram_budget,
        ram_headroom = ?cfg.ram_headroom,
        "effective configuration"
    );
}

/// Safe RAM budget (bytes) the worker pool may occupy (DESIGN §2.5).
fn slo_ram_budget(cfg: &config::Config, host: &system_info::SystemData, pool_rss: u64) -> u64 {
    if let Some(budget) = cfg.ram_budget {
        return budget;
    }
    let headroom = cfg.ram_headroom.unwrap_or(2 * 1024 * 1024 * 1024);
    let non_worker = host.memory_used.saturating_sub(pool_rss);
    host.memory_total
        .saturating_sub(non_worker)
        .saturating_sub(headroom)
}

/// Emit the signature Feasibility Gap readout as a structured event (DESIGN §3.2).
fn emit_feasibility_gap(gap: &policy::FeasibilityGap, mu: Option<f64>, lambda: f64) {
    let best_drain = match gap.best_drain {
        Some(d) => format!("{:.0}s", d.as_secs_f64()),
        None => "infinity (cannot keep up: mu*capacity <= lambda)".to_string(),
    };
    tracing::warn!(
        workers_needed = gap.workers_needed,
        workers_capacity = gap.workers_capacity,
        feasibility_gap = gap.gap_workers,
        gap_gib = bytes_to_gib(gap.gap_bytes),
        best_drain = %best_drain,
        mu = ?mu,
        lambda,
        slo_s = gap.slo_drain_time.as_secs(),
        "feasibility_gap: SLO is physically unreachable on this host"
    );
}

async fn run(cfg: config::Config) -> anyhow::Result<()> {
    tracing::info!("starting effiQueue");
    log_config(&cfg);
    let mut pool =
        worker::WorkerPool::new(cfg.command.clone(), cfg.process_name.clone(), cfg.shell);
    let mut probe = system_info::ResourceProbe::new();
    let mut est = policy::Estimators::new(cfg.alpha_mu, cfg.alpha_lambda);

    let metrics = Arc::new(metrics::Metrics::default());
    if let Some(addr) = cfg.metrics_addr.clone() {
        tokio::spawn(metrics::serve(addr, metrics.clone()));
    }

    let dt = cfg.poll_interval;
    let max_backoff = Duration::from_secs(60);
    let mut backoff = dt;

    // Workers active during the just-elapsed window, and the count we expect to
    // observe next tick if nothing changed outside our own ±1 step.
    let mut running_in_window: u32 = 0;
    let mut expected_running: u32 = 0;
    let mut ticks_since_change: u32 = cfg.cooldown_ticks; // allow the first action

    loop {
        let host = probe.host_memory();

        for (id, status) in pool.reap_exited() {
            tracing::info!(worker_id = id, ?status, "worker exited on its own");
        }

        let running = pool.len() as u32;
        let interference = running != expected_running;

        // Per-PID RSS -> average worker RSS (basis for workers_capacity).
        let pool_rss: u64 = pool
            .pids()
            .iter()
            .filter_map(|&p| probe.worker_rss(p))
            .sum();
        let worker_rss = (running > 0).then(|| pool_rss / running as u64);

        tracing::debug!(
            ram_used_gib = bytes_to_gib(host.memory_used),
            ram_total_gib = bytes_to_gib(host.memory_total),
            swap_used_gib = bytes_to_gib(host.used_swap),
            swap_total_gib = bytes_to_gib(host.total_swap),
            workers = running,
            pool_rss_gib = bytes_to_gib(pool_rss),
            "host and pool state"
        );

        let backlog = match rabbitmq_connector::get_queue_message_count(
            &cfg.queue_connection,
            &cfg.queue_name,
        )
        .await
        {
            Ok(queue) => {
                backoff = dt;
                queue.length
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    backoff_ms = backoff.as_millis() as u64,
                    "MetricSource/RabbitMQ error; retrying with backoff"
                );
                tokio::time::sleep(backoff).await;
                backoff = std::cmp::min(backoff.saturating_mul(2), max_backoff);
                continue;
            }
        };

        // Update estimators over the window that used `running_in_window` workers.
        est.observe(backlog, running_in_window, dt, interference);

        let (action, m_needed, m_capacity, m_gap) = match cfg.mode {
            config::Mode::Slo => {
                let swap_pressure =
                    host.total_swap > 0 && host.used_swap.saturating_mul(5) > host.total_swap;
                let params = policy::SloParams {
                    slo_drain_time: cfg.slo_drain_time.unwrap_or(Duration::from_secs(120)),
                    min_workers: cfg.min_workers,
                    max_workers: cfg.max_workers,
                    hysteresis: cfg.hysteresis,
                    cooldown_ticks: cfg.cooldown_ticks,
                    spike_backlog: cfg.spike_backlog,
                };
                let inputs = policy::SloInputs {
                    backlog,
                    running,
                    mu: est.mu(),
                    lambda: est.lambda(),
                    worker_rss,
                    safe_ram_budget: slo_ram_budget(&cfg, &host, pool_rss),
                    swap_pressure,
                    ticks_since_change,
                };
                let d = policy::decide_slo(&params, &inputs);
                if let Some(gap) = &d.feasibility_gap {
                    emit_feasibility_gap(gap, est.mu(), est.lambda());
                }
                tracing::debug!(
                    backlog,
                    mu = ?est.mu(),
                    lambda = est.lambda(),
                    workers_needed = ?d.workers_needed,
                    workers_capacity = d.workers_capacity,
                    action = ?d.action,
                    "SLO decision"
                );
                let needed = d.workers_needed.map(|n| n as i64).unwrap_or(-1);
                let gap = d
                    .feasibility_gap
                    .as_ref()
                    .map(|g| g.gap_workers as u64)
                    .unwrap_or(0);
                (d.action, needed, d.workers_capacity as u64, gap)
            }
            config::Mode::Threshold => {
                let ram_ratio = if host.memory_total > 0 {
                    host.memory_used as f64 / host.memory_total as f64
                } else {
                    1.0
                };
                let params = policy::ThresholdParams {
                    depth_threshold: cfg.depth_threshold,
                    ram_ratio_cap: cfg.ram_ratio_cap,
                    min_workers: cfg.min_workers,
                    max_workers: cfg.max_workers,
                };
                let a = policy::decide_threshold(
                    &params,
                    &policy::ThresholdInputs {
                        backlog,
                        running,
                        ram_ratio,
                    },
                );
                tracing::debug!(backlog, running, ram_ratio, action = ?a, "threshold decision");
                (a, -1i64, 0u64, 0u64)
            }
        };

        metrics.update(&metrics::Snapshot {
            workers: running as u64,
            backlog: backlog as u64,
            workers_needed: m_needed,
            workers_capacity: m_capacity,
            feasibility_gap: m_gap,
            mu: est.mu().unwrap_or(0.0),
            lambda: est.lambda(),
        });

        match action {
            policy::ScalingAction::ScaleUp => {
                if let Err(e) = pool.spawn_one() {
                    tracing::error!(error = %e, "failed to start worker");
                }
                metrics.inc_scale_up();
                ticks_since_change = 0;
            }
            policy::ScalingAction::ScaleDown => {
                pool.stop_one(cfg.drain_timeout).await;
                metrics.inc_scale_down();
                ticks_since_change = 0;
            }
            policy::ScalingAction::Hold => {
                ticks_since_change = ticks_since_change.saturating_add(1);
            }
        }

        running_in_window = pool.len() as u32;
        expected_running = running_in_window;

        tokio::select! {
            _ = tokio::time::sleep(dt) => {}
            _ = tokio::signal::ctrl_c() => {
                tracing::info!(workers = pool.len(), "shutdown signal received; draining workers");
                if !pool.is_empty() {
                    pool.shutdown_all(cfg.drain_timeout).await;
                }
                break;
            }
        }
    }

    Ok(())
}
