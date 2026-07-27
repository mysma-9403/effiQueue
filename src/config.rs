//! Typed, validated configuration.
//!
//! Two on-disk formats are accepted: TOML (`*.toml`) and the legacy
//! Supervisor-style `.conf` (kept as a compatibility shim). Both deserialize
//! into [`RawConfig`], which is then validated into a strongly-typed [`Config`].
//! No `expect()`/`panic!` on bad input — everything returns [`ConfigError`].

use serde::Deserialize;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("config syntax error: {0}")]
    Parse(String),
    #[error("invalid value for '{key}': {reason}")]
    Invalid { key: String, reason: String },
}

/// Controller mode. Canonical values: exactly `slo` and `threshold`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Slo,
    Threshold,
}

/// Validated configuration (strong types).
#[derive(Debug, Clone)]
pub struct Config {
    pub mode: Mode,
    pub command: String,
    pub process_name: String,
    pub max_workers: u32,
    pub min_workers: u32,
    pub poll_interval: Duration,
    pub drain_timeout: Duration,
    pub shell: bool,
    pub queue: String,
    pub queue_connection: String,
    pub queue_name: String,
    pub autostart: bool,
    pub autorestart: bool,
    // SLO core (mode = slo).
    pub slo_drain_time: Option<Duration>,
    pub ram_budget: Option<u64>,
    pub ram_headroom: Option<u64>,
    // Controller tuning (DESIGN §5.2) — pinned defaults.
    pub alpha_mu: f64,
    pub alpha_lambda: f64,
    pub hysteresis: u32,
    pub cooldown_ticks: u32,
    pub spike_backlog: u32,
    // threshold-mode knobs.
    pub depth_threshold: u32,
    pub ram_ratio_cap: f64,
    /// Optional Prometheus `/metrics` listen address, e.g. `127.0.0.1:9101`.
    pub metrics_addr: Option<String>,
}

/// Raw, unvalidated shape read from disk.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawConfig {
    mode: Option<String>,
    command: Option<String>,
    process_name: Option<String>,
    max_workers: Option<u32>,
    min_workers: Option<u32>,
    poll_interval: Option<String>,
    drain_timeout: Option<String>,
    shell: Option<bool>,
    queue: Option<String>,
    queue_connection: Option<String>,
    queue_name: Option<String>,
    autostart: Option<bool>,
    autorestart: Option<bool>,
    slo_drain_time: Option<String>,
    ram_budget: Option<String>,
    ram_headroom: Option<String>,
    /// Legacy Supervisor key. Mapped to `max_workers` with a deprecation warning.
    max: Option<u32>,
    alpha_mu: Option<f64>,
    alpha_lambda: Option<f64>,
    hysteresis: Option<u32>,
    cooldown_ticks: Option<u32>,
    spike_backlog: Option<u32>,
    depth_threshold: Option<u32>,
    ram_ratio_cap: Option<f64>,
    metrics_addr: Option<String>,
}

/// Load, detect format, parse and validate a config file.
pub fn load(path: &str) -> Result<Config, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Io {
        path: path.to_string(),
        source: e,
    })?;
    let raw = detect_and_parse(path, &text)?;
    validate(raw)
}

fn detect_and_parse(path: &str, text: &str) -> Result<RawConfig, ConfigError> {
    if path.ends_with(".toml") {
        parse_toml(text)
    } else {
        parse_supervisor_conf(text)
    }
}

fn parse_toml(text: &str) -> Result<RawConfig, ConfigError> {
    toml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))
}

/// Supervisor-style `.conf` shim: skips `[program:...]` headers and comments,
/// trims indentation, splits on the first `=`, and maps known keys.
fn parse_supervisor_conf(text: &str) -> Result<RawConfig, ConfigError> {
    let mut raw = RawConfig::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty()
            || line.starts_with('#')
            || line.starts_with(';')
            || line.starts_with('[')
        {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim().to_string();
        match key {
            "mode" => raw.mode = Some(value),
            "command" => raw.command = Some(value),
            "process_name" => raw.process_name = Some(value),
            "max_workers" => raw.max_workers = Some(parse_u32(&value, "max_workers")?),
            "min_workers" => raw.min_workers = Some(parse_u32(&value, "min_workers")?),
            "poll_interval" => raw.poll_interval = Some(value),
            "drain_timeout" => raw.drain_timeout = Some(value),
            "shell" => raw.shell = Some(parse_bool(&value, "shell")?),
            "queue" => raw.queue = Some(value),
            "queue_connection" => raw.queue_connection = Some(value),
            "queue_name" => raw.queue_name = Some(value),
            "autostart" => raw.autostart = Some(parse_bool(&value, "autostart")?),
            "autorestart" => raw.autorestart = Some(parse_bool(&value, "autorestart")?),
            "slo_drain_time" => raw.slo_drain_time = Some(value),
            "ram_budget" => raw.ram_budget = Some(value),
            "ram_headroom" => raw.ram_headroom = Some(value),
            "max" => raw.max = Some(parse_u32(&value, "max")?),
            "alpha_mu" => raw.alpha_mu = Some(parse_f64(&value, "alpha_mu")?),
            "alpha_lambda" => raw.alpha_lambda = Some(parse_f64(&value, "alpha_lambda")?),
            "hysteresis" => raw.hysteresis = Some(parse_u32(&value, "hysteresis")?),
            "cooldown_ticks" => raw.cooldown_ticks = Some(parse_u32(&value, "cooldown_ticks")?),
            "spike_backlog" => raw.spike_backlog = Some(parse_u32(&value, "spike_backlog")?),
            "depth_threshold" => raw.depth_threshold = Some(parse_u32(&value, "depth_threshold")?),
            "ram_ratio_cap" => raw.ram_ratio_cap = Some(parse_f64(&value, "ram_ratio_cap")?),
            "metrics_addr" => raw.metrics_addr = Some(value),
            _ => {} // ignore unknown keys
        }
    }
    Ok(raw)
}

fn validate(raw: RawConfig) -> Result<Config, ConfigError> {
    let command = required(raw.command, "command")?;
    let queue_connection = required(raw.queue_connection, "queue_connection")?;
    let queue_name = required(raw.queue_name, "queue_name")?;

    let max_workers = match (raw.max_workers, raw.max) {
        (Some(mw), Some(_)) => {
            tracing::warn!(
                "both 'max' and 'max_workers' were provided; using max_workers, ignoring 'max'"
            );
            mw
        }
        (Some(mw), None) => mw,
        (None, Some(legacy)) => {
            tracing::warn!(
                value = legacy,
                "key 'max' is deprecated; mapping to max_workers (it originally meant a RAM % limit, please verify your config)"
            );
            legacy
        }
        (None, None) => {
            return Err(ConfigError::Invalid {
                key: "max_workers".into(),
                reason: "required field".into(),
            })
        }
    };
    if max_workers == 0 {
        return Err(ConfigError::Invalid {
            key: "max_workers".into(),
            reason: "must be > 0".into(),
        });
    }
    let min_workers = raw.min_workers.unwrap_or(0);
    if min_workers > max_workers {
        return Err(ConfigError::Invalid {
            key: "min_workers".into(),
            reason: format!("min_workers ({min_workers}) > max_workers ({max_workers})"),
        });
    }

    let mode = match raw.mode.as_deref() {
        None | Some("slo") => Mode::Slo,
        Some("threshold") => Mode::Threshold,
        Some(other) => {
            return Err(ConfigError::Invalid {
                key: "mode".into(),
                reason: format!("unknown mode '{other}' (slo|threshold)"),
            })
        }
    };

    let poll_interval = match raw.poll_interval {
        Some(s) => parse_duration(&s)?,
        None => Duration::from_secs(10),
    };
    let drain_timeout = match raw.drain_timeout {
        Some(s) => parse_duration(&s)?,
        None => Duration::from_secs(30),
    };
    let slo_drain_time = raw.slo_drain_time.map(|s| parse_duration(&s)).transpose()?;
    let ram_budget = raw.ram_budget.map(|s| parse_bytes(&s)).transpose()?;
    let mut ram_headroom = raw.ram_headroom.map(|s| parse_bytes(&s)).transpose()?;

    // Mode-dependent validation (DESIGN §5.1).
    let slo_drain_time = if mode == Mode::Slo {
        let slo = slo_drain_time.ok_or_else(|| ConfigError::Invalid {
            key: "slo_drain_time".into(),
            reason: "required in slo mode".into(),
        })?;
        match (ram_budget, ram_headroom) {
            (Some(_), Some(_)) => {
                return Err(ConfigError::Invalid {
                    key: "ram_budget/ram_headroom".into(),
                    reason: "set exactly one of ram_budget / ram_headroom".into(),
                })
            }
            (None, None) => ram_headroom = Some(2 * 1024 * 1024 * 1024), // default 2GB headroom
            _ => {}
        }
        Some(slo)
    } else {
        slo_drain_time
    };

    let alpha_mu = raw.alpha_mu.unwrap_or(0.3);
    let alpha_lambda = raw.alpha_lambda.unwrap_or(0.3);
    for (name, a) in [("alpha_mu", alpha_mu), ("alpha_lambda", alpha_lambda)] {
        if !(a > 0.0 && a <= 1.0) {
            return Err(ConfigError::Invalid {
                key: name.into(),
                reason: "must be in the range (0, 1]".into(),
            });
        }
    }

    Ok(Config {
        mode,
        command,
        process_name: raw
            .process_name
            .unwrap_or_else(|| "consumer_%(process_num)02d".to_string()),
        max_workers,
        min_workers,
        poll_interval,
        drain_timeout,
        shell: raw.shell.unwrap_or(false),
        queue: raw.queue.unwrap_or_else(|| "rabbitmq".to_string()),
        queue_connection,
        queue_name,
        autostart: raw.autostart.unwrap_or(true),
        autorestart: raw.autorestart.unwrap_or(true),
        slo_drain_time,
        ram_budget,
        ram_headroom,
        alpha_mu,
        alpha_lambda,
        hysteresis: raw.hysteresis.unwrap_or(1),
        cooldown_ticks: raw.cooldown_ticks.unwrap_or(2),
        spike_backlog: raw.spike_backlog.unwrap_or(1000),
        depth_threshold: raw.depth_threshold.unwrap_or(40),
        ram_ratio_cap: raw.ram_ratio_cap.unwrap_or(0.9),
        metrics_addr: raw.metrics_addr,
    })
}

fn required(opt: Option<String>, key: &str) -> Result<String, ConfigError> {
    opt.filter(|s| !s.trim().is_empty())
        .ok_or_else(|| ConfigError::Invalid {
            key: key.into(),
            reason: "required field".into(),
        })
}

fn parse_u32(s: &str, key: &str) -> Result<u32, ConfigError> {
    s.trim().parse().map_err(|_| ConfigError::Invalid {
        key: key.into(),
        reason: format!("expected an integer, got '{s}'"),
    })
}

fn parse_f64(s: &str, key: &str) -> Result<f64, ConfigError> {
    s.trim().parse().map_err(|_| ConfigError::Invalid {
        key: key.into(),
        reason: format!("expected a number, got '{s}'"),
    })
}

fn parse_bool(s: &str, key: &str) -> Result<bool, ConfigError> {
    match s.trim() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        other => Err(ConfigError::Invalid {
            key: key.into(),
            reason: format!("expected a boolean, got '{other}'"),
        }),
    }
}

/// Parse a duration like `120s`, `2m`, `1h` (bare number = seconds).
pub fn parse_duration(s: &str) -> Result<Duration, ConfigError> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: u64 = num.parse().map_err(|_| ConfigError::Invalid {
        key: "duration".into(),
        reason: format!("invalid number in '{s}'"),
    })?;
    let mult = match unit.trim() {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        other => {
            return Err(ConfigError::Invalid {
                key: "duration".into(),
                reason: format!("unknown unit '{other}' (use s/m/h)"),
            })
        }
    };
    Ok(Duration::from_secs(n * mult))
}

/// Parse a byte size like `12GB`, `2gb`, `512MB` (base 1024, bare number = bytes).
pub fn parse_bytes(s: &str) -> Result<u64, ConfigError> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: u64 = num.parse().map_err(|_| ConfigError::Invalid {
        key: "bytes".into(),
        reason: format!("invalid number in '{s}'"),
    })?;
    let mult: u64 = match unit.trim().to_ascii_uppercase().as_str() {
        "" | "B" => 1,
        "K" | "KB" => 1024,
        "M" | "MB" => 1024 * 1024,
        "G" | "GB" => 1024 * 1024 * 1024,
        "T" | "TB" => 1024 * 1024 * 1024 * 1024,
        other => {
            return Err(ConfigError::Invalid {
                key: "bytes".into(),
                reason: format!("unknown unit '{other}' (use KB/MB/GB/TB)"),
            })
        }
    };
    Ok(n * mult)
}

/// Expand the `%(process_num)02d` placeholder to a zero-padded index.
pub fn expand_process_num(template: &str, n: u32) -> String {
    template.replace("%(process_num)02d", &format!("{n:02}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bytes_gb_base_1024() {
        assert_eq!(parse_bytes("2GB").unwrap(), 2 * 1024 * 1024 * 1024);
        assert_eq!(parse_bytes("512mb").unwrap(), 512 * 1024 * 1024);
        assert_eq!(parse_bytes("1024").unwrap(), 1024);
    }

    #[test]
    fn parse_duration_seconds_and_minutes() {
        assert_eq!(parse_duration("120s").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(parse_duration("10").unwrap(), Duration::from_secs(10));
    }

    #[test]
    fn expand_process_num_pads() {
        assert_eq!(
            expand_process_num("consumer_%(process_num)02d", 7),
            "consumer_07"
        );
        assert_eq!(
            expand_process_num("consumer_%(process_num)02d", 12),
            "consumer_12"
        );
    }

    #[test]
    fn legacy_max_maps_to_max_workers() {
        let raw = RawConfig {
            mode: Some("threshold".into()),
            command: Some("echo hi".into()),
            queue_connection: Some("amqp://localhost".into()),
            queue_name: Some("q".into()),
            max: Some(9),
            ..Default::default()
        };
        let cfg = validate(raw).unwrap();
        assert_eq!(cfg.max_workers, 9);
    }

    #[test]
    fn supervisor_conf_header_is_ignored() {
        let text = "[program:demo]\n    mode=threshold\n    command=echo hi\n    max_workers=3\n    queue_connection=amqp://localhost\n    queue_name=q\n";
        let cfg = validate(parse_supervisor_conf(text).unwrap()).unwrap();
        assert_eq!(cfg.command, "echo hi");
        assert_eq!(cfg.max_workers, 3);
        assert_eq!(cfg.queue_name, "q");
    }

    #[test]
    fn min_gt_max_is_error() {
        let raw = RawConfig {
            command: Some("echo hi".into()),
            queue_connection: Some("amqp://localhost".into()),
            queue_name: Some("q".into()),
            max_workers: Some(2),
            min_workers: Some(5),
            ..Default::default()
        };
        assert!(matches!(validate(raw), Err(ConfigError::Invalid { .. })));
    }

    #[test]
    fn missing_required_is_error() {
        let raw = RawConfig {
            max_workers: Some(2),
            ..Default::default()
        };
        assert!(matches!(validate(raw), Err(ConfigError::Invalid { .. })));
    }

    fn slo_base() -> RawConfig {
        RawConfig {
            mode: Some("slo".into()),
            command: Some("echo hi".into()),
            queue_connection: Some("amqp://localhost".into()),
            queue_name: Some("q".into()),
            max_workers: Some(4),
            ..Default::default()
        }
    }

    #[test]
    fn slo_requires_drain_time() {
        assert!(matches!(
            validate(slo_base()),
            Err(ConfigError::Invalid { .. })
        ));
    }

    #[test]
    fn slo_rejects_both_ram_settings() {
        let raw = RawConfig {
            slo_drain_time: Some("120s".into()),
            ram_budget: Some("8GB".into()),
            ram_headroom: Some("2GB".into()),
            ..slo_base()
        };
        assert!(matches!(validate(raw), Err(ConfigError::Invalid { .. })));
    }

    #[test]
    fn slo_defaults_headroom_when_neither_set() {
        let raw = RawConfig {
            slo_drain_time: Some("120s".into()),
            ..slo_base()
        };
        let cfg = validate(raw).unwrap();
        assert_eq!(cfg.slo_drain_time, Some(Duration::from_secs(120)));
        assert_eq!(cfg.ram_headroom, Some(2 * 1024 * 1024 * 1024));
        assert_eq!(cfg.ram_budget, None);
    }
}
