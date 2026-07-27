# Contributing to effiQueue

Thanks for your interest! effiQueue is early (alpha), so contributions,
bug reports and design feedback are all welcome.

## Development setup

- Rust stable (MSRV 1.75). Install via [rustup](https://rustup.rs/).
- A RabbitMQ broker for end-to-end testing (Docker is easiest — see below).

```sh
git clone https://github.com/mysma-9403/effiQueue
cd effiQueue
cargo build
```

## Checks (must pass before a PR)

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all
```

CI runs the same on Linux, Windows and macOS.

## End-to-end smoke test

A scripted end-to-end run (spins up RabbitMQ in Docker, drives a scale
up/down, checks `/metrics`) lives in [`examples/`](examples/):

```sh
./examples/e2e-smoke.sh
```

## Scope

effiQueue is intentionally a **small, single-host, non-orchestrator** worker
autoscaler. Out of scope: container orchestration, multi-host scheduling, a
control-plane. In scope: better estimators, more `MetricSource` backends,
observability, packaging.

## Style & commits

- Formatting is enforced by `rustfmt`; lint by `clippy` (warnings are errors).
- Keep the scaling logic in `policy.rs` pure and unit-tested.
- Clear, imperative commit messages. No CLA or DCO sign-off is required.

## License

By contributing, you agree that your contributions are dual-licensed under
MIT and Apache-2.0, matching the project (see `LICENSE-MIT` / `LICENSE-APACHE`).
