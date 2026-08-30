mod config;
mod estimator;
mod management;
mod metrics;
mod platform;
mod policy;
mod rabbitmq_connector;
mod system_info;
mod worker;

use clap::{Parser, Subcommand};
use std::sync::Arc;
use std::time::{Duration, Instant};

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

/// How often the feasibility gap is re-logged at WARN while it persists.
/// Between those it stays at DEBUG so a genuinely under-provisioned host does
/// not drown its own logs.
const GAP_RELOG_TICKS: u32 = 30;

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
    if cfgs.is_empty() {
        anyhow::bail!("no programs defined in {}", cli.config);
    }
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
        management = cfg.management,
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
fn emit_feasibility_gap(
    program: &str,
    gap: &policy::FeasibilityGap,
    mu: Option<f64>,
    lambda: f64,
    loud: bool,
) {
    let best_drain = match gap.best_drain {
        Some(d) => format!("{:.0}s", d.as_secs_f64()),
        None => "infinite (can't keep up: mu*capacity <= lambda)".to_string(),
    };
    macro_rules! event {
        ($level:ident) => {
            tracing::$level!(
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
            )
        };
    }
    if loud {
        event!(warn);
    } else {
        event!(debug);
    }
}

/// Resolve the management endpoint for a program, if one is wanted and derivable.
fn build_management_client(cfg: &config::Config) -> Option<management::ManagementClient> {
    if !cfg.management {
        tracing::info!(program = %cfg.process_name, "management API disabled by config");
        return None;
    }
    let result = match cfg.management_url.as_deref() {
        Some(url) => management::ManagementClient::from_override(url, &cfg.queue_connection)
            .map(Some)
            .map_err(|e| e.to_string()),
        None => management::ManagementClient::from_amqp_uri(&cfg.queue_connection)
            .map_err(|e| e.to_string()),
    };
    match result {
        Ok(Some(client)) => {
            tracing::info!(
                program = %cfg.process_name,
                endpoint = %client.endpoint(),
                "management API configured; mu and lambda will be measured directly"
            );
            Some(client)
        }
        Ok(None) => {
            tracing::info!(
                program = %cfg.process_name,
                "no plaintext management endpoint derivable (amqps); using the regression estimator. \
                 Set management_url to point at one."
            );
            None
        }
        Err(e) => {
            tracing::warn!(program = %cfg.process_name, error = %e, "management API not configured; using the regression estimator");
            None
        }
    }
}

/// Per-program runtime state carried across ticks.
struct Prog {
    cfg: config::Config,
    name: Arc<str>,
    source: rabbitmq_connector::RabbitSource,
    pool: worker::WorkerPool,
    est: estimator::Estimator,
    running_in_window: u32,
    expected_running: u32,
    ticks_since_change: u32,
    ticks_since_gap_log: u32,
    gap_active: bool,
    scale_up_total: u64,
    scale_down_total: u64,
    probe_total: u64,
}

/// Shutdown signals, registered **once** before the control loop starts.
///
/// Registration has to happen up front, not inside the loop's `select!`. Until
/// the first `signal()` call the process still carries the default disposition,
/// so a `SIGTERM` arriving during the very first tick terminates it outright —
/// no drain, orphaned workers. And re-subscribing each tick would drop any
/// signal recorded before the new subscription, losing every `SIGTERM` that
/// lands while the tick body is busy (an unreachable broker blocks it for
/// seconds).
#[cfg(unix)]
struct ShutdownSignal {
    term: Option<tokio::signal::unix::Signal>,
    interrupt: Option<tokio::signal::unix::Signal>,
}

#[cfg(unix)]
impl ShutdownSignal {
    fn install() -> Self {
        use tokio::signal::unix::{signal, SignalKind};
        let register = |kind: SignalKind, name: &str| match signal(kind) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!(error = %e, signal = name, "cannot install a signal handler");
                None
            }
        };
        // SIGTERM is what `docker stop`, `systemctl stop` and a bare `kill` send.
        Self {
            term: register(SignalKind::terminate(), "SIGTERM"),
            interrupt: register(SignalKind::interrupt(), "SIGINT"),
        }
    }

    async fn recv(&mut self) {
        match (self.term.as_mut(), self.interrupt.as_mut()) {
            (Some(t), Some(i)) => tokio::select! {
                _ = t.recv() => tracing::info!("SIGTERM received"),
                _ = i.recv() => tracing::info!("SIGINT received"),
            },
            (Some(t), None) => {
                t.recv().await;
                tracing::info!("SIGTERM received");
            }
            (None, Some(i)) => {
                i.recv().await;
                tracing::info!("SIGINT received");
            }
            // Nothing could be registered; never resolve, so the loop keeps
            // running rather than spinning on an immediately-ready branch.
            (None, None) => std::future::pending().await,
        }
    }
}

#[cfg(not(unix))]
struct ShutdownSignal;

#[cfg(not(unix))]
impl ShutdownSignal {
    fn install() -> Self {
        Self
    }

    async fn recv(&mut self) {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("Ctrl-C received");
    }
}

async fn run(cfgs: Vec<config::Config>) -> anyhow::Result<()> {
    // These are host-wide, not per-program: the RAM budget is shared by
    // construction, and one process serves one metrics endpoint on one cadence.
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
            let est = estimator::Estimator::new(cfg.alpha_mu, cfg.alpha_lambda);
            let source = rabbitmq_connector::RabbitSource::new(
                cfg.queue_connection.clone(),
                cfg.queue_name.clone(),
                build_management_client(&cfg),
            );
            let name: Arc<str> = Arc::from(config::program_label(&cfg.process_name).as_str());
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
                ticks_since_gap_log: GAP_RELOG_TICKS,
                gap_active: false,
                scale_up_total: 0,
                scale_down_total: 0,
                probe_total: 0,
            }
        })
        .collect();

    let metrics = Arc::new(metrics::Metrics::default());
    if let Some(addr) = metrics_addr {
        tokio::spawn(metrics::serve(addr, metrics.clone()));
    }

    // Registered before the first tick: see ShutdownSignal.
    let mut shutdown = ShutdownSignal::install();

    let mut probe = system_info::ResourceProbe::new();
    // The estimator must divide by the time that actually elapsed. A scale-down
    // tick used to take drain_timeout longer than poll_interval, which silently
    // corrupted every rate derived from it.
    let mut last_tick = Instant::now();

    loop {
        let elapsed = last_tick.elapsed();
        last_tick = Instant::now();

        // The tick body itself must be cancellable. It can block for seconds on
        // an unreachable broker (connect + management timeouts, per program), and
        // waiting that out before honouring a shutdown risks overrunning the
        // grace period `docker stop` and systemd allow — at which point the
        // drain we are trying to perform is cut short by SIGKILL anyway.
        let tick = async {
            for p in progs.iter_mut() {
                for w in p.pool.reap_exited() {
                    if w.crashed {
                        tracing::warn!(
                            program = %p.cfg.process_name, worker_id = w.id, pid = w.pid,
                            status = ?w.status, uptime_s = w.uptime.as_secs_f64(),
                            "worker exited almost immediately (treated as a crash)"
                        );
                    } else {
                        tracing::info!(
                            program = %p.cfg.process_name, worker_id = w.id, pid = w.pid,
                            status = ?w.status, uptime_s = w.uptime.as_secs_f64(),
                            "worker exited on its own"
                        );
                    }
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

            // Per-program pool RSS (basis for the shared, host-wide RAM budget) and
            // the per-worker figure that sizes capacity.
            let prog_rss: Vec<(u64, Option<u64>)> = progs
                .iter()
                .map(|p| {
                    let values: Vec<u64> = p
                        .pool
                        .pids()
                        .iter()
                        .filter_map(|pid| rss_map.get(pid).copied())
                        .collect();
                    // Size capacity off the LARGEST worker, not the mean. A worker
                    // spawned seconds ago has barely faulted its pages in, and
                    // averaging it in would overstate how many more fit.
                    (values.iter().sum(), values.iter().copied().max())
                })
                .collect();
            let total_worker_rss: u64 = prog_rss.iter().map(|(sum, _)| sum).sum();
            let safe_ram_budget =
                ram_pool_budget(ram_budget, ram_headroom, &host, total_worker_rss);
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
                tick_s = elapsed.as_secs_f64(),
                "host and budget"
            );

            // Shared budget: each program may only claim what other programs (and
            // this tick's earlier scale-ups) leave free.
            let mut committed_extra: u64 = 0;
            let mut snapshots: Vec<metrics::ProgramSnapshot> = Vec::with_capacity(progs.len());

            for (idx, p) in progs.iter_mut().enumerate() {
                let running = p.pool.len() as u32;
                let interference = running != p.expected_running;
                let (this_rss, worker_rss) = prog_rss[idx];

                let reading = match p.source.read().await {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!(program = %p.cfg.process_name, error = %e, "MetricSource/RabbitMQ error; skipping this program this tick");
                        // The estimator must not see this gap as a single window, so
                        // its backlog baseline is reset rather than left stale.
                        p.est.forget_backlog();
                        // min_workers is a floor, not a queue-driven decision. An
                        // unreachable broker must not leave a configured pool empty —
                        // the workers reconnect on their own.
                        let mut running = running;
                        if running < p.cfg.min_workers {
                            match p.pool.spawn_one() {
                                Ok(Some(_)) => {
                                    p.scale_up_total += 1;
                                    p.ticks_since_change = 0;
                                    running = p.pool.len() as u32;
                                }
                                Ok(None) => {}
                                Err(e) => {
                                    tracing::error!(program = %p.cfg.process_name, error = %e, "failed to start worker for the min_workers floor");
                                }
                            }
                        }
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
                            mu_source: p.est.source() as u64,
                            probing: 0,
                            scale_up_total: p.scale_up_total,
                            scale_down_total: p.scale_down_total,
                            probe_total: p.probe_total,
                        });
                        continue;
                    }
                };
                let backlog = reading.backlog;

                p.est.observe(&estimator::Observation {
                    backlog,
                    running: p.running_in_window,
                    dt: elapsed,
                    interference,
                    rates: reading.rates,
                });

                let (action, probing, m_needed, m_capacity, m_gap) = match p.cfg.mode {
                    config::Mode::Slo => {
                        let others_rss = total_worker_rss.saturating_sub(this_rss);
                        let eff_budget = safe_ram_budget
                            .saturating_sub(others_rss)
                            .saturating_sub(committed_extra);
                        let params = policy::SloParams {
                            slo_drain_time: p
                                .cfg
                                .slo_drain_time
                                .unwrap_or(Duration::from_secs(120)),
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
                            needs_probe: p.est.needs_probe(),
                        };
                        let d = policy::decide_slo(&params, &inputs);
                        match &d.feasibility_gap {
                            Some(gap) => {
                                // Loud on the first tick of a gap, then periodically.
                                let loud =
                                    !p.gap_active || p.ticks_since_gap_log >= GAP_RELOG_TICKS;
                                emit_feasibility_gap(
                                    &p.cfg.process_name,
                                    gap,
                                    p.est.mu(),
                                    p.est.lambda(),
                                    loud,
                                );
                                p.ticks_since_gap_log = if loud {
                                    0
                                } else {
                                    p.ticks_since_gap_log.saturating_add(1)
                                };
                                p.gap_active = true;
                            }
                            None => {
                                if p.gap_active {
                                    tracing::info!(program = %p.cfg.process_name, "feasibility gap cleared: the SLO now fits on this host");
                                }
                                p.gap_active = false;
                                p.ticks_since_gap_log = GAP_RELOG_TICKS;
                            }
                        }
                        tracing::debug!(
                            program = %p.cfg.process_name,
                            backlog,
                            mu = ?p.est.mu(),
                            mu_source = ?p.est.source(),
                            lambda = p.est.lambda(),
                            spread = p.est.spread(),
                            workers_needed = ?d.workers_needed,
                            workers_capacity = d.workers_capacity,
                            action = ?d.action,
                            probing = d.probing,
                            "SLO decision"
                        );
                        let needed = d.workers_needed.map(|n| n as i64).unwrap_or(-1);
                        let gap = d
                            .feasibility_gap
                            .as_ref()
                            .map(|g| g.gap_workers as u64)
                            .unwrap_or(0);
                        (d.action, d.probing, needed, d.workers_capacity as u64, gap)
                    }
                    config::Mode::Threshold => {
                        let params = policy::ThresholdParams {
                            depth_threshold: p.cfg.depth_threshold,
                            ram_ratio_cap: p.cfg.ram_ratio_cap,
                            min_workers: p.cfg.min_workers,
                            max_workers: p.cfg.max_workers,
                            cooldown_ticks: p.cfg.cooldown_ticks,
                        };
                        let a = policy::decide_threshold(
                            &params,
                            &policy::ThresholdInputs {
                                backlog,
                                running,
                                ram_ratio,
                                ticks_since_change: p.ticks_since_change,
                            },
                        );
                        tracing::debug!(program = %p.cfg.process_name, backlog, running, ram_ratio, action = ?a, "threshold decision");
                        (a, false, -1i64, 0u64, 0u64)
                    }
                };

                match action {
                    policy::ScalingAction::ScaleUp => match p.pool.spawn_one() {
                        Ok(Some(_)) => {
                            committed_extra += worker_rss.unwrap_or(0);
                            p.scale_up_total += 1;
                            if probing {
                                p.probe_total += 1;
                            }
                            p.ticks_since_change = 0;
                        }
                        Ok(None) => {
                            // Crash-loop back-off is active; leave the cooldown clock
                            // running so we do not look like we just scaled.
                            tracing::debug!(
                                program = %p.cfg.process_name,
                                backoff_s = p.pool.spawn_backoff_remaining().map(|d| d.as_secs()),
                                "scale-up suppressed by the crash-loop back-off"
                            );
                            p.ticks_since_change = p.ticks_since_change.saturating_add(1);
                        }
                        Err(e) => {
                            tracing::error!(program = %p.cfg.process_name, error = %e, "failed to start worker");
                            p.ticks_since_change = p.ticks_since_change.saturating_add(1);
                        }
                    },
                    policy::ScalingAction::ScaleDown => {
                        if let Some(w) = p.pool.detach_one(drain_timeout) {
                            if probing {
                                tracing::info!(
                                    program = %p.cfg.process_name, worker_id = w.id,
                                    "stepping one worker down to make mu observable (identifiability probe)"
                                );
                                p.probe_total += 1;
                            }
                            // Drain in the background: a 30s drain must not stall a
                            // 10s control loop, nor the other programs sharing it.
                            tokio::spawn(worker::drain_detached(w, drain_timeout));
                            p.scale_down_total += 1;
                        }
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
                    mu_source: p.est.source() as u64,
                    probing: u64::from(probing),
                    scale_up_total: p.scale_up_total,
                    scale_down_total: p.scale_down_total,
                    probe_total: p.probe_total,
                });
            }

            metrics.set(snapshots);
        };

        let mut stopping = tokio::select! {
            biased;
            _ = shutdown.recv() => true,
            _ = tick => false,
        };

        if !stopping {
            stopping = tokio::select! {
                _ = shutdown.recv() => true,
                _ = tokio::time::sleep(dt) => false,
            };
        }

        if stopping {
            tracing::info!("shutdown signal received; draining workers");
            for p in progs.iter_mut() {
                if !p.pool.is_empty() {
                    p.pool.shutdown_all(drain_timeout).await;
                }
            }
            tracing::info!("all workers drained; exiting");
            break;
        }
    }

    Ok(())
}
