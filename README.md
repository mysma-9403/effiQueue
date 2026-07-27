# effiQueue

**The queue autoscaler that knows your machine's capacity.**

You declare a *drain-time SLO*. effiQueue measures — live, on this machine — how
fast one worker actually drains the queue and how much RAM it costs, then packs
the box exactly to the safe limit. When the SLO is physically unreachable on
this host, it tells you plainly (a **Feasibility Gap**) instead of OOM-killing
the box or silently missing the target.

> ⚠️ **Alpha — not production-ready.** The control model works and is unit-tested,
> but the throughput estimator needs validation on real traffic. See
> [Known limitations](#known-limitations--risks).

---

## Why it exists

Autoscaling almost always assumes an orchestrator (Kubernetes/KEDA) or a PaaS
(Heroku/Judoscale). But a huge amount of real queue processing still runs as OS
processes on long-lived VMs / bare-metal, where the only "scaling tools" are
Supervisor and systemd — and both are blind to queue depth (they run a *static*
number of workers).

effiQueue fills that gap **without an orchestrator**, and adds the one thing no
elastic autoscaler models: the **finite RAM of a single fixed machine** as a
*binding, measured* control variable — not a guessed cap.

| | KEDA | Laravel Horizon | Supervisor / systemd | **effiQueue** |
|---|---|---|---|---|
| Substrate | Kubernetes only | Laravel + Redis | any VM (static) | **any VM / bare-metal** |
| Signal | queue depth | queue time | none (fixed count) | **depth + measured µ + RAM** |
| RAM as control | no (add nodes) | manual cap | no | **yes — binding & measured** |
| Language | any (pods) | PHP | any | **any command** |

## How it works

Every `poll_interval` (default 10s) the control loop:

1. reads the RabbitMQ queue depth (`lapin`, passive `queue_declare`);
2. derives arrival rate **λ** from depth deltas and measures throughput **µ**
   (msgs/s/worker) by perturbation — it scales by **exactly one worker at a
   time**, which makes µ *observable* (bulk-scaling autoscalers can't measure it);
3. computes `workers_needed` via **Little's Law** to hit the drain-time SLO;
4. computes `workers_capacity` = `RAM budget / measured worker RSS` (cross-checked
   against swap pressure);
5. steps one worker toward `min(needed, capacity)`, with hysteresis + cooldown
   and a spike fast-path;
6. if `needed > capacity`, pins to capacity and emits the **Feasibility Gap**.

### The Feasibility Gap

```
WARN feasibility_gap: SLO 120s unreachable on this host:
     need 78 workers (~39 GB), machine safely fits 24 (~12 GB),
     best achievable drain ~= ∞ (can't keep up: µ*capacity < λ);
     short by 54 workers / ~27 GB.
```

The controller pins to 24 workers instead of trying to spawn 78 and OOM-killing
the box. The operator gets an unambiguous signal: loosen the SLO, or add RAM.

## Quickstart

```sh
cargo build --release

# validate a config without running
./target/release/effiqueue --config data/config.slo.toml validate-config

# run (threshold mode, Supervisor-style config)
./target/release/effiqueue --config data/config.conf

# run the SLO controller
./target/release/effiqueue --config data/config.slo.toml
# or override the mode:
./target/release/effiqueue --config data/config.conf --mode slo
```

`RUST_LOG=debug` enables per-tick decision logs.

## Configuration

Two formats are accepted: **TOML** and the legacy **Supervisor-style `.conf`**
(compat shim). Byte suffixes are base-1024 (`12GB`); durations are `120s`/`2m`.

| Key | Default | Meaning |
|---|---|---|
| `mode` | `slo` | `slo` (SLO controller) or `threshold` (depth + RAM% fallback) |
| `command` | (required) | worker command line; parsed to argv, spawned directly (no shell) |
| `queue_connection` | (required) | AMQP URL, e.g. `amqp://guest:guest@localhost:5672` |
| `queue_name` | (required) | queue whose depth is read |
| `max_workers` | (required) | hard ceiling on concurrent workers |
| `min_workers` | `0` | floor (0 = full scale-to-zero) |
| `slo_drain_time` | (required in `slo`) | target drain time — the main operator knob |
| `ram_budget` | — | absolute RAM the pool may use; **or** set `ram_headroom` |
| `ram_headroom` | `2GB` | RAM to leave free for the OS (mutually exclusive with `ram_budget`) |
| `poll_interval` | `10s` | control-loop cadence |
| `drain_timeout` | `30s` | grace period before force-kill on scale-down |
| `shell` | `false` | `true` wraps the command in `sh -c` / `cmd /C` |
| `metrics_addr` | — | optional Prometheus `/metrics` listen address |
| `depth_threshold` | `40` | (threshold mode) scale-up above this backlog |
| `alpha_mu`, `alpha_lambda` | `0.3` | EWMA smoothing for µ / λ, in (0, 1] |
| `hysteresis`, `cooldown_ticks`, `spike_backlog` | `1`, `2`, `1000` | controller damping / fast-path |

Legacy Supervisor keys (`process_name`, `autostart`, `autorestart`, `queue`) are
accepted; the old `max` is mapped to `max_workers` with a deprecation warning.

## Metrics

Set `metrics_addr` (e.g. `127.0.0.1:9101`) to expose Prometheus metrics at
`GET /metrics`, including the signature series `effiqueue_workers_needed`,
`effiqueue_workers_capacity`, `effiqueue_feasibility_gap`, plus
`effiqueue_mu`, `effiqueue_lambda`, `effiqueue_workers`, `effiqueue_backlog`
and scale counters.

## Deployment

- **systemd** (recommended for VMs / bare-metal): see
  [`deploy/effiqueue.service`](deploy/effiqueue.service). Run under a dedicated
  unprivileged user; use absolute paths for `--config`.
- **Docker**: see the [`Dockerfile`](Dockerfile) (static musl build). Note that
  effiQueue spawns worker processes on the *same* host, so the worker runtime
  (e.g. PHP) must be present alongside it.

## Cross-platform

Builds and runs on **Linux, macOS and Windows** (CI matrix covers all three).
Honest caveat: Windows has no `SIGTERM`, so graceful drain there is *best-effort*
— it relies on the worker self-exiting (e.g. Symfony Messenger `--time-limit`)
and falls back to a hard kill after `drain_timeout`. Unix/macOS get a real
`SIGTERM → SIGKILL` drain.

## Known limitations & risks

- **Alpha.** The µ estimator (EWMA + scale-by-one attribution + quarantine on
  external interference) is the make-or-break piece and still needs tuning on
  real traffic; `alpha`/cooldown defaults are provisional.
- Single queue / single program per instance (multi-program is on the roadmap).
- In-code log strings are currently mixed Polish/English — full English pass is
  a pre-1.0 item.
- No `/metrics` authentication — bind to localhost or a private interface.

## Roadmap

- **Faza 0** ✅ hardening: PID tracking, graceful drain, direct argv spawn, tests, CI.
- **Faza 1** ✅ SLO core: measured µ/λ, Little's Law, RAM budget, Feasibility Gap.
- **Faza 2** 🚧 metrics (done), packaging (done), multi-program, English log pass.
- **Faza 3** multi-backend (Redis/SQS/Kafka lag), richer resilience, optional energy-aware deferral.

## License

Licensed under either of MIT or Apache-2.0 at your option.
