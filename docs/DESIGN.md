# effiQueue — Design

> A worker-process autoscaler for message queues, running on a single fixed
> machine (VM / bare-metal), without a container orchestrator.
>
> The operator declares a **drain-time SLO**. effiQueue measures — live, on this
> machine — how fast one worker actually drains the queue and how much RAM it
> costs, then packs the box exactly to the safe limit. When the SLO is physically
> unreachable on this host, it says so plainly (a **Feasibility Gap**) instead of
> OOM-killing the box or silently missing the target.

Crate name (lowercase): `effiqueue`. Presented as: `effiQueue`.

The code cites this document by section number. Those references are load-bearing
— if you change a rule here, change it in the code, and vice versa.

---

## 1. Layered architecture

The controller is an **observe → decide → act** loop. The central design
decision: the **decide** layer (`policy`, `estimator`) is pure, fully testable
mathematics with no I/O. Everything that touches the outside world — the queue,
host memory, processes — lives in the observe/act layers.

| Layer | Module | Responsibility |
|---|---|---|
| observe | `rabbitmq_connector`, `management` | queue backlog and, where available, broker rates |
| observe | `system_info` | host memory/swap and per-PID RSS |
| decide | `estimator` | measure `mu` and `lambda` |
| decide | `policy` | Little's Law, RAM capacity, the scaling action |
| act | `worker`, `platform` | spawn, drain, kill; the only place with OS `#[cfg]` |
| report | `metrics` | Prometheus exposition |

`main` owns the loop and the state carried across ticks.

---

## 2. The control loop

### 2.1 One tick

Every `poll_interval`:

1. reap workers that exited on their own, classifying short-lived exits as crashes;
2. sample host memory and per-worker RSS in one batched scan;
3. per program: read the queue, feed the estimator, decide, act by **at most one
   worker**;
4. publish a metrics snapshot;
5. sleep — or, on a shutdown signal, drain every worker and exit.

The loop measures **real elapsed time** with `Instant` and hands that to the
estimator. It must not assume the tick took `poll_interval`: any tick that does
real work takes longer, and every rate derived from a wrong `dt` is wrong by the
same factor.

### 2.2 Why `mu` and `lambda` are hard

Each window yields exactly one equation of queue balance:

```
dB/dt = lambda - mu * n
```

with **two** unknowns. Every `(mu, lambda)` pair on that line explains the
observation equally well. This matters more than it looks: an estimator that
computes `lambda` from the previous `mu` and then `mu` from the new `lambda` does
not break the circularity — it converges to a self-consistent point that need not
be the true one, and stays there. Smoothing cannot rescue an unidentifiable
system; it only makes the wrong answer look stable.

Version 0.1.0-alpha shipped exactly that estimator. See `CHANGELOG.md`.

### 2.3 Two identifiable estimators

**Direct (preferred).** The broker already counts both quantities. RabbitMQ's
management API reports, per queue:

| field | meaning |
|---|---|
| `message_stats.ack_details.rate` | acknowledgements per second — this is `mu * n` |
| `message_stats.publish_details.rate` | publishes per second — this is `lambda` |
| `messages` | ready **plus** unacknowledged — the honest backlog |

`mu = ack_rate / n`. Nothing is inferred, and convergence is immediate.

The third row matters independently: a passive AMQP `queue_declare` returns
`message_count`, which is messages **ready** only. It reads zero while workers are
still processing the last messages, which makes naive scale-to-zero flap.

**Regression (fallback).** When no management API is reachable, only backlog is
observable. The way out is that `n` is not constant: the controller moves by one
worker at a time, so different windows are recorded at different worker counts.
Fitting the balance equation across a sliding window by ordinary least squares,

```
y_i = dB_i/dt_i  against  n_i     ->     slope = -mu,  intercept = lambda
```

identifies both parameters. **This is the real content of "scale-by-one makes
`mu` observable"**, and it is why the controller never bulk-scales.

Samples are excluded when they carry no usable signal:

* **interference** — the worker count changed outside our own ±1 step, so `n` for
  that window is unknown;
* **empty queue** — the drain was limited by the work available rather than by
  worker capacity, so `y` understates throughput;
* a window we could not read — the backlog baseline is dropped rather than
  differenced across the gap.

### 2.4 Identifiability gates and probing

Least squares on a rank-deficient design gives a confident wrong answer, which is
the failure mode being eliminated. The fit is therefore gated on the spread

```
Sxx = sum (n_i - n_mean)^2
```

and on a minimum sample count. A negative fitted `mu` ("more workers drained
less") is noise, not physics, and is rejected. Until the gates open, `mu()`
returns `None` and the controller treats throughput as unknown.

That creates an obligation. If the pool sat pinned at `max_workers`, `n` would
never move and `mu` would never become observable. So the estimator exposes
`needs_probe()`, and the controller answers it: with a backlog present and no
room to grow, it steps **one** worker down purely to buy the spread — never
below one worker, since halting all progress to learn something is a bad trade,
and at a ceiling of one there is nothing to learn anyway. This is a
deliberate, bounded, logged perturbation, surfaced as `effiqueue_probe_total`.

Practical note: with `n` varying by only ±1, `Sxx` is small and the estimator's
variance is correspondingly high. Under bursty arrivals the fallback needs on the
order of 60–180 windows to be accurate — minutes to tens of minutes at a 10 s
poll interval. Prefer the direct path whenever the management API is available.

### 2.5 The RAM budget

The binding constraint no elastic autoscaler models: this machine has a finite,
measured amount of memory.

```
workers_capacity = floor(safe_ram_budget / worker_rss)
```

* `safe_ram_budget` is either the explicit `ram_budget`, or
  `memory_total - (memory_used - worker_rss) - ram_headroom` — that is, total less
  everything that is *not* our workers, less the headroom the OS keeps.
* `worker_rss` is the **largest** live worker, not the mean. A worker spawned
  seconds ago has barely faulted its pages in; averaging it in would overstate how
  many more fit, precisely when scaling up.
* Under swap pressure (`used_swap / total_swap > swap_ratio_cap`, default 0.2)
  capacity is clamped to the current count: no growth while the machine is
  already paging. It is never clamped below **one** worker, though — with an
  empty pool our contribution to the pressure is zero, so refusing to start the
  first worker cannot relieve it and would simply leave the queue undrained
  forever, since the bootstrap path needs `running < capacity` to move.
  The threshold is configurable because some hosts (macOS especially, which
  sizes its swapfile on demand) sit above any fixed ratio by construction.
* Capacity is finally clamped by `max_workers`.

With several programs the budget is **shared**: each may claim only what the
others, and this tick's earlier scale-ups, leave free.

Known limitation: summing RSS double-counts pages shared between workers (a fleet
of PHP workers shares a great deal), which overstates worker memory and therefore
overstates the derived budget. Using PSS on Linux would be more accurate.

### 2.6 Little's Law and the action

```
workers_needed = ceil( (backlog + lambda * H) / (mu * H) )
```

for a drain-time SLO `H` — enough workers to clear what is queued *and* what will
arrive while clearing it. `workers_needed` is `None` while `mu` is unknown.

```
target = clamp( min(workers_needed, workers_capacity), min_workers, max_workers )
```

The step is damped:

* **hysteresis** — a dead zone around `running`, so ±1 noise does not flap;
* **cooldown** — `cooldown_ticks` between changes, letting one perturbation settle
  before the next;
* **spike fast-path** — above `spike_backlog`, scale-*up* may bypass cooldown.
  Scale-down never does.

Floor and ceiling are enforced ahead of all of that.

Known limitation: the slew rate is one worker per `poll_interval`, so reaching a
large capacity takes `max_workers × poll_interval`. The spike fast-path bypasses
the cooldown but does not increase the step size.

---

## 3. The Feasibility Gap

### 3.1 What it is

When `workers_needed > workers_capacity`, the declared SLO cannot be met on this
host. The controller pins to capacity — it does **not** try to spawn its way into
an OOM kill — and reports the shortfall as a first-class result rather than a
silent miss.

### 3.2 The event

Emitted as a structured `tracing` event with `workers_needed`,
`workers_capacity`, `feasibility_gap`, `gap_gib`, `best_drain`, `mu`, `lambda`
and the SLO. `best_drain` is the drain time achievable at full capacity, or
"infinite" when `mu * capacity <= lambda` — the queue can never be cleared at all.

It is logged loudly when the gap opens, then periodically while it persists, so a
genuinely under-provisioned host does not drown its own logs. Clearing the gap is
also logged.

Exposed as `effiqueue_feasibility_gap` alongside `effiqueue_workers_needed` and
`effiqueue_workers_capacity` — the signature series to alert on.

### 3.3 Worked example

Backlog 50 000, `H` = 120 s, `mu` = 8 msg/s/worker, `lambda` = 200 msg/s,
worker RSS 512 MB, budget 12 GB:

```
workers_needed   = ceil((50000 + 200*120) / (8*120)) = 78
workers_capacity = floor(12GB / 512MB)               = 24
gap              = 54 workers ~ 27 GB
best_drain       = infinite, because 8*24 = 192 < 200 = lambda
```

The operator gets an unambiguous instruction: loosen the SLO, or add RAM.

---

## 4. Process management

Identity and count come from an in-memory registry of retained `Child` handles —
never from scanning process names. Workers are spawned directly by argv; `shell =
true` wraps them in `sh -c` / `cmd /C`.

On Unix each worker is spawned as a **process-group leader**, and stop signals
address the group. Without that, `shell = true` would only ever signal the `sh`,
and any worker that forks would leave children behind.

Stopping is `SIGTERM`, then a poll until `drain_timeout`, then `SIGKILL`. Drains
run **off** the control loop: a 30 s drain must not stall a 10 s loop, nor the
other programs sharing it. The pool's count drops as soon as the worker is
detached.

Windows has no `SIGTERM`, so graceful drain there is best-effort: it depends on
the worker self-exiting (e.g. Symfony Messenger `--time-limit`), with
`TerminateProcess` as the fallback. In-flight messages may be lost.

A worker that exits sooner than 10 s is counted as a crash. After three
consecutive crashes, spawning is backed off exponentially, so a broken command is
not respawned once per tick forever.

Shutdown is triggered by `SIGTERM` as well as `SIGINT`, since `docker stop`,
`systemctl stop` and a bare `kill` all send the former. Workers are drained
concurrently, so shutdown costs one `drain_timeout` rather than N.

---

## 5. Configuration

Two on-disk formats: TOML, and a Supervisor-style `.conf` compatibility shim.
Both deserialize into one raw shape and are then validated into strong types.
Bad input never panics; it returns a typed error naming the key.

### 5.1 Validation rules

* `command`, `queue_connection`, `queue_name` are required; `max_workers` must be
  present and greater than zero; `min_workers <= max_workers`.
* `mode` is exactly `slo` or `threshold`.
* In `slo` mode, `slo_drain_time` is required, and **exactly one** of
  `ram_budget` / `ram_headroom` may be set (neither ⇒ a 2 GB headroom default).
* `alpha_mu` and `alpha_lambda` must lie in `(0, 1]`.
* `queue` must name a backend that exists. Accepting `redis` and quietly reading
  RabbitMQ would be worse than refusing to start.
* Size and duration suffixes are range-checked. Release builds disable overflow
  checks, so an unchecked multiply would turn a nonsense value into a plausible
  small one.

In multi-program mode, `[[program]]` entries override shared top-level values.
`poll_interval`, `drain_timeout`, `ram_budget`/`ram_headroom` and `metrics_addr`
are **process-wide**, not per-program: one loop cadence, one shared RAM budget,
one metrics endpoint.

### 5.2 Tuning defaults

| Key | Default | Rationale |
|---|---|---|
| `poll_interval` | `10s` | fast enough to react, slow enough that RSS settles |
| `alpha_mu`, `alpha_lambda` | `0.3` | smoothing on top of the fit/broker rates |
| `hysteresis` | `1` | one-worker dead zone |
| `cooldown_ticks` | `2` | one perturbation settles before the next |
| `spike_backlog` | `1000` | above this, scale-up skips the cooldown |
| `drain_timeout` | `30s` | typical upper bound on one message |
| `ram_headroom` | `2GB` | left for the OS and page cache |
| `swap_ratio_cap` | `0.2` | growth brake once the host is paging; `1.0` disables |
| `management` | `true` | derive the management endpoint and prefer it |

---

## 6. Observability

`GET /metrics` on `metrics_addr`, Prometheus text format, one `program` label per
program. No authentication — bind it to localhost or a private interface.

| Series | Meaning |
|---|---|
| `effiqueue_workers` | live workers |
| `effiqueue_backlog` | queue depth |
| `effiqueue_workers_needed` | Little's Law result; `-1` = not yet measured |
| `effiqueue_workers_capacity` | capacity from the RAM budget |
| `effiqueue_feasibility_gap` | missing workers |
| `effiqueue_mu`, `effiqueue_lambda` | measured rates |
| `effiqueue_mu_source` | 0 none, 1 broker rates, 2 regression |
| `effiqueue_probing` | 1 while perturbing to identify `mu` |
| `effiqueue_scale_up_total`, `effiqueue_scale_down_total`, `effiqueue_probe_total` | counters |

`effiqueue_mu_source` is the one to watch first. `0` means the controller does not
know throughput and is running on the bootstrap path; `1` means the management API
is doing the work; `2` means it is inferring from perturbation.

---

## 7. Known limitations

* Worker RSS is summed, so shared pages are double-counted (§2.5).
* The regression fallback needs minutes of history and degrades under very bursty
  arrivals (§2.4).
* Slew rate is one worker per tick (§2.6).
* The worker registry is in memory only. After a hard kill of effiQueue itself,
  surviving workers cannot be re-adopted on restart.
* RabbitMQ is the only implemented backend.
* The management client speaks plaintext HTTP only; put a local proxy in front of
  a TLS-only management API and point `management_url` at it.
