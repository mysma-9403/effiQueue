# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] - 2026-08-30

The throughput estimator in 0.1.0-alpha did not work. This release replaces it,
fixes the bugs that were corrupting the same measurement, and adds the CI that
would have caught the problem. **Anyone running 0.1.0-alpha in `slo` mode should
upgrade** — see the note below for what it was actually doing.

### Fixed

- **`mu` and `lambda` are now identifiable.** Each control window gives one
  equation of queue balance (`dB/dt = lambda - mu*n`) with two unknowns, so no
  single operating point can determine both. The old estimator computed
  `lambda` from the previous `mu` and `mu` from the new `lambda`, which does not
  break the circularity — it converges to a self-consistent point that need not
  be the true one. The arithmetic also degenerated on the first sample, so on a
  **growing or steady backlog `mu` never initialised at all**: `workers_needed`
  stayed `None`, and the Feasibility Gap could not fire in the scenario the
  README leads with. In `slo` mode the controller effectively degraded to a slow
  one-worker-per-tick ramp to capacity.

  Measured against a synthetic queue with known parameters (truth `mu` = 8):

  | regime | old `mu` | old `lambda` | truth |
  |---|---|---|---|
  | backlog growing | never initialised | 120 | `mu` 8, `lambda` 200 |
  | steady state | never initialised | 0 | `mu` 8, `lambda` 80 |
  | draining | 6.0 | 0 | `mu` 8, `lambda` 20 |
  | bursty | 4.5 | 48 | `mu` 8, `lambda` 70 |
  | 300 ticks | 3.5, not converging | 35 | `mu` 8, `lambda` 20 |

- Windows were divided by the *configured* `poll_interval`, but a scale-down tick
  actually took up to `drain_timeout` longer, silently corrupting every rate
  derived from it.
- A failed queue read left the backlog baseline stale, so the next window
  differenced across two windows while dividing by one — inflating both rates.
- `workers_capacity` was sized off *mean* worker RSS. A worker spawned seconds
  ago has barely faulted its pages in, so the mean understated per-worker cost
  and overstated how many more fit — exactly when scaling up.
- **No `SIGTERM` handler.** `docker stop`, `systemctl stop` and a bare `kill` all
  send `SIGTERM`; effiQueue exited without draining and left worker processes
  orphaned. (systemd's default `KillMode=control-group` masked this for the
  systemd path only.)
- **No crash-loop back-off**: a command that failed on startup was respawned once
  per tick, forever.
- Stop signals addressed a single PID, so `shell = true` only ever signalled the
  `sh` wrapper. Workers now spawn as process-group leaders and the group is
  signalled.
- `threshold` mode scaled down the instant depth read zero, with no cooldown —
  and an AMQP depth of zero routinely means "all messages are in flight,
  unacked". The pool flapped.
- `parse_bytes` / `parse_duration` overflowed silently in release builds, where
  overflow checks are off.
- An unimplemented `queue` backend was accepted and then ignored, so
  `queue = "redis"` quietly read RabbitMQ.
- `/metrics` answered on any path, so a probe against `/` looked like a working
  scrape target.
- A slow drain blocked the whole control loop, including every other program.
- `min_workers` was silently ignored whenever the queue could not be read: the
  error path skipped the decision entirely, so an unreachable broker left a
  configured pool empty instead of at its floor.
- Metrics labels carried the raw per-worker template, so every series was
  labelled `program="consumer_%(process_num)02d"`.

### Added

- **RabbitMQ management API support** (default on, auto-derived from
  `queue_connection`). Supplies `ack_details.rate` (= `mu*n`) and
  `publish_details.rate` (= `lambda`) directly, plus a backlog that includes
  unacknowledged messages. New keys: `management`, `management_url`.
- **Perturbation regression** as the backend-agnostic fallback: least squares of
  `dB/dt` on `n` recovers `slope = -mu` and `intercept = lambda`. Gated on the
  spread of `n`, since a rank-deficient fit yields a confident wrong answer.
- **Identifiability probing.** When pinned at the ceiling with `mu` still
  unknown, the controller steps one worker down on purpose to create the spread
  the regression needs, rather than freezing where `mu` can never be observed.
- New metrics: `effiqueue_mu_source` (0 none / 1 broker / 2 regression),
  `effiqueue_probing`, `effiqueue_probe_total`.
- Concurrent drain on shutdown — one `drain_timeout`, not N.
- A broker-measured estimate now ages out if the management API goes quiet,
  reverting `mu` to unknown rather than steering on a stale number or reporting
  it as a live broker measurement.
- **Release workflow** publishing binaries, with SHA-256 checksums, for Linux
  (gnu/musl × x86_64/aarch64), macOS (Intel/Apple Silicon) and Windows.
- CI: `--locked` everywhere, MSRV check, `cargo audit`, `cargo publish
  --dry-run`, and an **end-to-end smoke test against a real RabbitMQ in both
  modes**. The `slo` run asserts that `mu` is genuinely measured — the guard
  that the 0.1.0-alpha estimator would fail.
- `docs/DESIGN.md` is now part of the repository (it was gitignored) and is in
  English, with the estimator section corrected.

### Changed

- Version scheme: plain SemVer. `0.x` communicates pre-1.0 maturity, so the
  `-alpha` suffix is gone and release artifacts are marked as pre-releases.
- Docker build caches dependencies in a separate layer.

## [0.1.0-alpha] - 2026-07-27

First public alpha. The scaling core is unit-tested and cross-platform, but the
throughput (`mu`) estimator has not yet been validated on real production
traffic — see the README's "Known limitations".

### Added
- **Budget-Bound SLO controller**: measured `mu`/`lambda`, Little's Law for
  `workers_needed`, a RAM budget for `workers_capacity`, and the signature
  **Feasibility Gap** readout when an SLO cannot fit on a host.
- **Threshold** fallback mode (queue depth + RAM%).
- **PID-tracked worker pool**: direct argv spawn (no generated shell scripts),
  cross-platform graceful stop (`SIGTERM`→`SIGKILL` on Unix, best-effort on
  Windows).
- **Multiple programs per instance** (`[[program]]`) sharing one host RAM
  budget, with per-program overrides.
- **Prometheus `/metrics`** endpoint with per-program labels.
- Typed, validated config (TOML + Supervisor-`.conf` shim), `clap` CLI,
  structured `tracing` logs.
- Persistent RabbitMQ connection with a bounded connect timeout.
- CI matrix (Linux/Windows/macOS), Dockerfile, systemd unit.

[Unreleased]: https://github.com/mysma-9403/effiQueue/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/mysma-9403/effiQueue/compare/v0.1.0-alpha...v0.2.0
[0.1.0-alpha]: https://github.com/mysma-9403/effiQueue/releases/tag/v0.1.0-alpha
