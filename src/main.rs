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
    /// Path to the config file (TOML or Supervisor-style .conf).
    #[arg(long, default_value = "./data/config.conf")]
    config: String,
    /// Override the controller mode for all programs (slo|threshold).
    #[arg(long)]
    mode: Option<String>,
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the control loop (default).
    Run,
    /// Validate the config and exit.
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
    let mut cfgs = config::load_all(&cli.config)?;
    if let Some(m) = cli.mode.as_deref() {
        let mode = match m {
            "slo" => config::Mode::Slo,
            "threshold" => config::Mode::Threshold,
            other => anyhow::bail!("unknown --mode '{other}' (allowed: slo|threshold)"),
        };
        for c in &mut cfgs {
            c.mode = mode;
        }
    }

    match cli.command.unwrap_or(Cmd::Run) {
        Cmd::ValidateConfig => {
            for c in &cfgs {
                log_config(c);
            }
            tracing::info!(programs = cfgs.len(), "config valid");
            Ok(())
        }
        Cmd::Run => run(cfgs).await,
    }
}

fn bytes_to_gib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0 * 1024.0)
}

/// Log a program's effective configuration. Also the canonical read site for the
/// advisory `queue`/`autostart`/`autorestart` fields.
fn log_config(cfg: &config::Config) {
    tracing::info!(
        program = %cfg.process_name,
        mode = ?cfg.mode,
        queue = %cfg.queue,
        queue_name = %cfg.queue_name,
        max_workers = cfg.max_workers,
        min_workers = cfg.min_workers,
        shell = cfg.shell,
        autostart = cfg.autostart,
        autorestart = cfg.autorestart,
        slo_drain_time = ?cfg.slo_drain_time,
        ram_budget = ?cfg.ram_budget,
        ram_headroom = ?cfg.ram_headroom,
        "program configuration"
    );
}

/// Host-wide safe RAM budget (bytes) the whole worker pool may occupy (DESIGN §2.5).
fn ram_pool_budget(
    ram_budget: Option<u64>,
    ram_headroom: Option<u64>,
    host: &system_info::SystemData,
    total_worker_rss: u64,
) -> u64 {
    if let Some(budget) = ram_budget {
        return budget;
    }
    let headroom = ram_headroom.unwrap_or(2 * 1024 * 1024 * 1024);
    let non_worker = host.memory_used.saturating_sub(total_worker_rss);
    host.memory_total
        .saturating_sub(non_worker)
        .saturating_sub(headroom)
}

/// Emit the signature Feasibility Gap readout as a structured event (DESIGN §3.2).
fn emit_feasibility_gap(program: &str, gap: &policy::FeasibilityGap, mu: Option<f64>, lambda: f64) {
    let best_drain = match gap.best_drain {
        Some(d) => format!("{:.0}s", d.as_secs_f64()),
        None => "infinite (can't keep up: mu*capacity <= lambda)".to_string(),
    };
    tracing::warn!(
        program,
        workers_needed = gap.workers_needed,
        workers_capacity = gap.workers_capacity,
        feasibility_gap = gap.gap_workers,
        gap_gib = bytes_to_gib(gap.gap_bytes),
        best_drain = %best_drain,
        mu = ?mu,
        lambda,
        slo_s = gap.slo_drain_time.as_secs(),
        "feasibility_gap: SLO physically unreachable on this host"
    );
}

/// Per-program runtime state carried across ticks.
struct Prog {
    cfg: config::Config,
    name: Arc<str>,
    source: rabbitmq_connector::RabbitSource,
    pool: worker::WorkerPool,
    est: policy::Estimators,
    running_in_window: u32,
    expected_running: u32,
    ticks_since_change: u32,
    scale_up_total: u64,
    scale_down_total: u64,
}

async fn run(cfgs: Vec<config::Config>) -> anyhow::Result<()> {
    let dt = cfgs[0].poll_interval;
    let drain_timeout = cfgs[0].drain_timeout;
    let ram_budget = cfgs[0].ram_budget;
    let ram_headroom = cfgs[0].ram_headroom;
    let metrics_addr = cfgs[0].metrics_addr.clone();

    tracing::info!(programs = cfgs.len(), "starting effiQueue");
    for c in &cfgs {
        log_config(c);
    }

    let mut progs: Vec<Prog> = cfgs
        .into_iter()
        .map(|cfg| {
            let pool =
                worker::WorkerPool::new(cfg.command.clone(), cfg.process_name.clone(), cfg.shell);
            let est = policy::Estimators::new(cfg.alpha_mu, cfg.alpha_lambda);
            let source = rabbitmq_connector::RabbitSource::new(
                cfg.queue_connection.clone(),
                cfg.queue_name.clone(),
            );
            let name: Arc<str> = Arc::from(cfg.process_name.as_str());
            let ticks = cfg.cooldown_ticks;
            Prog {
                cfg,
                name,
                source,
                pool,
                est,
                running_in_window: 0,
                expected_running: 0,
                ticks_since_change: ticks,
                scale_up_total: 0,
                scale_down_total: 0,
            }
        })
        .collect();

    let metrics = Arc::new(metrics::Metrics::default());
    if let Some(addr) = metrics_addr {
        tokio::spawn(metrics::serve(addr, metrics.clone()));
    }

    let mut probe = system_info::ResourceProbe::new();

    loop {
        for p in progs.iter_mut() {
            for (id, status) in p.pool.reap_exited() {
                tracing::info!(program = %p.cfg.process_name, worker_id = id, ?status, "worker exited on its own");
            }
        }

        // Only slo-mode programs need per-worker RSS.
        let need_rss = progs.iter().any(|p| p.cfg.mode == config::Mode::Slo);
        let all_pids: Vec<u32> = progs.iter().flat_map(|p| p.pool.pids()).collect();

        // Run the blocking sysinfo scans off the async executor, in one section.
        let (host, rss_map) = tokio::task::block_in_place(|| {
            let host = probe.host_memory();
            let rss = if need_rss {
                probe.worker_rss_batch(&all_pids)
            } else {
                std::collections::HashMap::new()
            };
            (host, rss)
        });

        // Per-program pool RSS (basis for the shared, host-wide RAM budget).
        let prog_rss: Vec<u64> = progs
            .iter()
            .map(|p| {
                p.pool
                    .pids()
                    .iter()
                    .filter_map(|pid| rss_map.get(pid).copied())
                    .sum()
            })
            .collect();
        let total_worker_rss: u64 = prog_rss.iter().sum();
        let safe_ram_budget = ram_pool_budget(ram_budget, ram_headroom, &host, total_worker_rss);
        let swap_pressure =
            host.total_swap > 0 && host.used_swap.saturating_mul(5) > host.total_swap;
        let ram_ratio = if host.memory_total > 0 {
            host.memory_used as f64 / host.memory_total as f64
        } else {
            1.0
        };

        tracing::debug!(
            ram_used_gib = bytes_to_gib(host.memory_used),
            ram_total_gib = bytes_to_gib(host.memory_total),
            swap_used_gib = bytes_to_gib(host.used_swap),
            budget_gib = bytes_to_gib(safe_ram_budget),
            worker_rss_gib = bytes_to_gib(total_worker_rss),
            "host and budget"
        );

        // Shared budget: each program may only claim what other programs (and
        // this tick's earlier scale-ups) leave free.
        let mut committed_extra: u64 = 0;
        let mut snapshots: Vec<metrics::ProgramSnapshot> = Vec::with_capacity(progs.len());

        for (idx, p) in progs.iter_mut().enumerate() {
            let running = p.pool.len() as u32;
            let interference = running != p.expected_running;
            let this_rss = prog_rss[idx];
            let worker_rss = (running > 0).then(|| this_rss / running as u64);

            let backlog = match p.source.queue_depth().await {
                Ok(depth) => depth,
                Err(e) => {
                    tracing::warn!(program = %p.cfg.process_name, error = %e, "MetricSource/RabbitMQ error; skipping this program this tick");
                    p.running_in_window = running;
                    p.expected_running = running;
                    snapshots.push(metrics::ProgramSnapshot {
                        name: p.name.clone(),
                        workers: running as u64,
                        backlog: 0,
                        workers_needed: -1,
                        workers_capacity: 0,
                        feasibility_gap: 0,
                        mu: p.est.mu().unwrap_or(0.0),
                        lambda: p.est.lambda(),
                        scale_up_total: p.scale_up_total,
                        scale_down_total: p.scale_down_total,
                    });
                    continue;
                }
            };

            p.est
                .observe(backlog, p.running_in_window, dt, interference);

            let (action, m_needed, m_capacity, m_gap) = match p.cfg.mode {
                config::Mode::Slo => {
                    let others_rss = total_worker_rss.saturating_sub(this_rss);
                    let eff_budget = safe_ram_budget
                        .saturating_sub(others_rss)
                        .saturating_sub(committed_extra);
                    let params = policy::SloParams {
                        slo_drain_time: p.cfg.slo_drain_time.unwrap_or(Duration::from_secs(120)),
                        min_workers: p.cfg.min_workers,
                        max_workers: p.cfg.max_workers,
                        hysteresis: p.cfg.hysteresis,
                        cooldown_ticks: p.cfg.cooldown_ticks,
                        spike_backlog: p.cfg.spike_backlog,
                    };
                    let inputs = policy::SloInputs {
                        backlog,
                        running,
                        mu: p.est.mu(),
                        lambda: p.est.lambda(),
                        worker_rss,
                        safe_ram_budget: eff_budget,
                        swap_pressure,
                        ticks_since_change: p.ticks_since_change,
                    };
                    let d = policy::decide_slo(&params, &inputs);
                    if let Some(gap) = &d.feasibility_gap {
                        emit_feasibility_gap(&p.cfg.process_name, gap, p.est.mu(), p.est.lambda());
                    }
                    tracing::debug!(
                        program = %p.cfg.process_name,
                        backlog,
                        mu = ?p.est.mu(),
                        lambda = p.est.lambda(),
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
                    let params = policy::ThresholdParams {
                        depth_threshold: p.cfg.depth_threshold,
                        ram_ratio_cap: p.cfg.ram_ratio_cap,
                        min_workers: p.cfg.min_workers,
                        max_workers: p.cfg.max_workers,
                    };
                    let a = policy::decide_threshold(
                        &params,
                        &policy::ThresholdInputs {
                            backlog,
                            running,
                            ram_ratio,
                        },
                    );
                    tracing::debug!(program = %p.cfg.process_name, backlog, running, ram_ratio, action = ?a, "threshold decision");
                    (a, -1i64, 0u64, 0u64)
                }
            };

            match action {
                policy::ScalingAction::ScaleUp => {
                    match p.pool.spawn_one() {
                        Ok(_) => {
                            committed_extra += worker_rss.unwrap_or(0);
                            p.scale_up_total += 1;
                        }
                        Err(e) => {
                            tracing::error!(program = %p.cfg.process_name, error = %e, "failed to start worker")
                        }
                    }
                    p.ticks_since_change = 0;
                }
                policy::ScalingAction::ScaleDown => {
                    p.pool.stop_one(drain_timeout).await;
                    p.scale_down_total += 1;
                    p.ticks_since_change = 0;
                }
                policy::ScalingAction::Hold => {
                    p.ticks_since_change = p.ticks_since_change.saturating_add(1);
                }
            }

            let running_after = p.pool.len() as u32;
            p.running_in_window = running_after;
            p.expected_running = running_after;

            snapshots.push(metrics::ProgramSnapshot {
                name: p.name.clone(),
                workers: running_after as u64,
                backlog: backlog as u64,
                workers_needed: m_needed,
                workers_capacity: m_capacity,
                feasibility_gap: m_gap,
                mu: p.est.mu().unwrap_or(0.0),
                lambda: p.est.lambda(),
                scale_up_total: p.scale_up_total,
                scale_down_total: p.scale_down_total,
            });
        }

        metrics.set(snapshots);

        tokio::select! {
            _ = tokio::time::sleep(dt) => {}
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("shutdown signal received; draining workers");
                for p in progs.iter_mut() {
                    if !p.pool.is_empty() {
                        p.pool.shutdown_all(drain_timeout).await;
                    }
                }
                break;
            }
        }
    }

    Ok(())
}
