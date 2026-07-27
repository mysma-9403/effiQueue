# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0-alpha] - 2026-07-27

First public alpha. The scaling core is unit-tested and cross-platform, but the
throughput (`mu`) estimator has not yet been validated on real production
traffic — see the README's "Known limitations".

### Added
- **Budget-Bound SLO controller**: measured `mu`/`lambda`, Little's Law for
  `workers_needed`, a RAM budget for `workers_capacity`, and the signature
  **Feasibility Gap** readout when an SLO cannot fit on the host.
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

[Unreleased]: https://github.com/mysma-9403/effiQueue/compare/v0.1.0-alpha...HEAD
[0.1.0-alpha]: https://github.com/mysma-9403/effiQueue/releases/tag/v0.1.0-alpha
