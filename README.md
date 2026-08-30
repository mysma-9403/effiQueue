# effiQueue

**The queue autoscaler that knows your machine's capacity.**

You declare a *drain-time SLO*. effiQueue measures — live, on this machine — how
fast one worker actually drains the queue and how much RAM it costs, then packs
the box exactly to the safe limit. When the SLO is physically unreachable on
this host, it tells you plainly (a **Feasibility Gap**) instead of OOM-killing
the box or silently missing the target.

> ⚠️ **Beta — not yet battle-tested.** The control model and the throughput
> estimator are unit-tested against known ground truth and exercised end-to-end
> in CI against a real RabbitMQ, but the tuning defaults have not yet been
> validated across a range of production workloads. See
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

1. reads the queue — the **management API** when reachable (true backlog
   including in-flight messages, plus ack/publish rates), otherwise a passive
   AMQP `queue_declare`;
2. measures throughput **µ** (msgs/s/worker) and arrival rate **λ**
   ([see below](#measuring-µ-and-λ));
3. computes `workers_needed` via **Little's Law** to hit the drain-time SLO;
4. computes `workers_capacity` = `RAM budget / largest worker RSS` (cross-checked
   against swap pressure);
5. steps one worker toward `min(needed, capacity)`, with hysteresis + cooldown
   and a spike fast-path;
6. if `needed > capacity`, pins to capacity and emits the **Feasibility Gap**.

### Measuring µ and λ

This is the part that makes effiQueue different, so it is worth being precise
about. Each control window gives one equation of queue balance —
`dB/dt = λ − µ·n` — with **two** unknowns. A single operating point cannot
identify both; any `(µ, λ)` on that line fits equally well. There are two honest
ways out, and effiQueue uses whichever is available:

- **Broker rates (preferred).** RabbitMQ's management API reports
  `ack_details.rate` (which *is* `µ·n`) and `publish_details.rate` (which *is*
  `λ`). Nothing is inferred. Enabled automatically when the endpoint is
  reachable.

- **Perturbation + regression (fallback).** Because the controller scales by
  exactly **one worker at a time**, `n` varies across windows. A least-squares
  fit of `dB/dt` against `n` recovers `slope = −µ` and `intercept = λ`. This is
  what "scale-by-one makes µ observable" actually means — and it is why the
  controller never bulk-scales.

The fallback refuses to answer until the worker count has genuinely varied
(rank-deficiency gate), and if it is pinned at the ceiling with µ still unknown
it steps one worker down on purpose to create the spread. Watch
`effiqueue_mu_source` to see which path is live: `0` none, `1` broker, `2`
regression.

### The Feasibility Gap

```
WARN feasibility_gap: SLO 120s unreachable on this host:
     need 78 workers (~39 GB), machine safely fits 24 (~12 GB),
     best achievable drain ~= ∞ (can't keep up: µ*capacity < λ);
     short by 54 workers / ~27 GB.
```

The controller pins to 24 workers instead of trying to spawn 78 and OOM-killing
the box. The operator gets an unambiguous signal: loosen the SLO, or add RAM.

## Install

Download a binary for your platform from the
[releases page](https://github.com/mysma-9403/effiQueue/releases) — Linux
(gnu/musl, x86_64/aarch64), macOS (Intel/Apple Silicon) and Windows. Each
release ships `SHA256SUMS.txt`.

Or build from source (Rust 1.88 or newer):

```sh
cargo build --release
```

## Quickstart

```sh
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

Two formats are accepted: **TOML** and the legacy Supervisor-style `.conf`
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
| `management` | `true` | use the RabbitMQ management API for backlog + rates |
| `management_url` | (derived) | explicit `http://[user:pass@]host:port` endpoint |
| `metrics_addr` | — | optional Prometheus `/metrics` listen address |
| `swap_ratio_cap` | `0.2` | swap-used fraction above which growth is blocked; `1.0` disables |
| `depth_threshold` | `40` | (threshold mode) scale-up above this backlog |
| `alpha_mu`, `alpha_lambda` | `0.3` | EWMA smoothing for µ / λ, in (0, 1] |
| `hysteresis`, `cooldown_ticks`, `spike_backlog` | `1`, `2`, `1000` | controller damping / fast-path |

By default the management endpoint is derived from `queue_connection` (same host
and credentials, port 15672). Set `management = false` to stay on AMQP only, or
`management_url` to point somewhere else.

In multi-program configs, `poll_interval`, `drain_timeout`, `ram_budget` /
`ram_headroom` and `metrics_addr` are **process-wide** — one loop cadence, one
shared RAM budget, one metrics endpoint. Everything else can be set per
`[[program]]`.

Legacy Supervisor keys (`process_name`, `autostart`, `autorestart`) are accepted;
the old `max` is mapped to `max_workers` with a deprecation warning. `queue` is
accepted but must name a backend that exists — `rabbitmq` is the only one today,
and anything else is now a startup error rather than being silently ignored.

## Metrics

Set `metrics_addr` (e.g. `127.0.0.1:9101`) to expose Prometheus metrics at
`GET /metrics`, including the signature series `effiqueue_workers_needed`,
`effiqueue_workers_capacity`, `effiqueue_feasibility_gap`, plus
`effiqueue_mu`, `effiqueue_lambda`, `effiqueue_mu_source`, `effiqueue_workers`,
`effiqueue_backlog`, per-worker and pool RSS, the RAM budget, `best_drain`,
estimator spread, crash-loop back-off and scale/probe counters.

## Live view

```sh
effiqueue top --url http://127.0.0.1:9101/metrics
```

```
┌ feasibility gap ──────────────────────────────────────────────────────────┐
│SLO 120s unreachable on this host                                          │
│need 78 workers, this machine safely fits 24 — short by 54 (~27.0GB)       │
│best achievable drain: never                                               │
└───────────────────────────────────────────────────────────────────────────┘
┌ capacity ─────────────────────────────────────────────────────────────────┐
│██████████████                running   10                                 │
│██████████████████████████████needed    78 ████████████████████████████████│
│█████████████████████████████ capacity  24                                 │
└───────────────────────────────────────────────────────────────────────────┘
```

`top` is a **client**, not part of the daemon: it polls the `/metrics` endpoint
the daemon already exposes and touches nothing in the control loop, so it costs
a running instance nothing and works against a remote host. `←/→` switches
program, `p` pauses, `q` quits.

It ships in the released binaries. `cargo build --no-default-features` drops it
(and the `ratatui` dependency) if you want the daemon and nothing else.

## Deployment

- **systemd** (recommended for VMs / bare-metal): see
  [`deploy/effiqueue.service`](deploy/effiqueue.service). Run under a dedicated
  unprivileged user; use absolute paths for `--config`.
- **Docker**: see the [`Dockerfile`](Dockerfile) (static musl build). Note that
  effiQueue spawns worker processes on the *same* host, so the worker runtime
  (e.g. PHP) must be present alongside it.

`SIGTERM` and `SIGINT` both trigger a graceful drain of every worker before exit.

## Cross-platform

Builds and runs on **Linux, macOS and Windows** (CI matrix covers all three).
Honest caveat: Windows has no `SIGTERM`, so graceful drain there is *best-effort*
— it relies on the worker self-exiting (e.g. Symfony Messenger `--time-limit`)
and falls back to a hard kill after `drain_timeout`. Unix/macOS get a real
`SIGTERM → SIGKILL` drain, addressed to the worker's whole process group.

## Known limitations & risks

- **Beta.** Defaults for `alpha`/cooldown/hysteresis are reasoned but not yet
  tuned against a spread of real workloads.
- The **regression fallback** (used only when the management API is unreachable)
  needs on the order of 60–180 windows to converge, and degrades under very
  bursty arrivals. Prefer the management API; watch `effiqueue_mu_source`.
- **Slew rate is one worker per tick**, so reaching a large capacity takes
  `max_workers × poll_interval`. The spike fast-path skips the cooldown but does
  not enlarge the step.
- Worker RSS is **summed**, which double-counts memory shared between workers
  (significant for e.g. a fleet of PHP workers) and therefore overstates the
  derived budget.
- The **swap brake** blocks growth once `used_swap / total_swap` exceeds
  `swap_ratio_cap`. macOS grows its swapfile on demand, so that ratio runs high
  by construction and the default 0.2 will pin the pool at one worker there;
  raise `swap_ratio_cap` (or set it to `1.0`) on such hosts.
- Without the management API, AMQP reports messages **ready** only — in-flight
  messages are invisible, so the backlog reads low while workers are still busy.
- The worker registry is **in memory only**: after a hard kill of effiQueue
  itself, surviving workers cannot be re-adopted on restart.
- Single queue per program; RabbitMQ is the only implemented backend.
- No `/metrics` authentication, and the management client speaks plaintext HTTP
  — bind both to localhost or a private interface.

## Roadmap

- **Phase 0** ✅ hardening: PID tracking, graceful drain, direct argv spawn, tests, CI.
- **Phase 1** ✅ SLO core: Little's Law, RAM budget, Feasibility Gap.
- **Phase 2** ✅ identifiable µ/λ (broker rates + perturbation regression), metrics,
  multi-program, packaging, release binaries.
- **Phase 3** multi-backend (Redis/SQS/Kafka lag), PSS-based RAM accounting,
  worker re-adoption after restart, richer resilience.

## Documentation

[`docs/DESIGN.md`](docs/DESIGN.md) is the technical design; the code cites it by
section number.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Run the end-to-end smoke test locally
with `MODE=slo ./examples/e2e-smoke.sh` (needs Docker).

## License

Licensed under either of MIT or Apache-2.0 at your option.
