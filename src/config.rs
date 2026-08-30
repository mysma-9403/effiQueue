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
    /// Use the RabbitMQ management API for backlog + rates when reachable.
    pub management: bool,
    /// Explicit `http://[user:pass@]host:port` management endpoint. Overrides
    /// the address derived from `queue_connection`.
    pub management_url: Option<String>,
}

/// Raw, unvalidated shape read from disk.
#[derive(Debug, Default, Clone, Deserialize)]
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
    management: Option<bool>,
    management_url: Option<String>,
    /// TOML `[[program]]` array (multi-program mode). Absent for single-program / `.conf`.
    #[serde(rename = "program")]
    programs: Option<Vec<RawProgram>>,
}

/// Per-program overrides for multi-program mode (`[[program]]`). Any unset field
/// falls back to the shared top-level value.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
struct RawProgram {
    mode: Option<String>,
    command: Option<String>,
    process_name: Option<String>,
    max_workers: Option<u32>,
    min_workers: Option<u32>,
    shell: Option<bool>,
    queue: Option<String>,
    queue_connection: Option<String>,
    queue_name: Option<String>,
    autostart: Option<bool>,
    autorestart: Option<bool>,
    slo_drain_time: Option<String>,
    max: Option<u32>,
}

/// Load one or more program configs. A TOML file with `[[program]]` yields one
/// [`Config`] per program (each carrying the shared global settings); anything
/// else (single TOML or Supervisor `.conf`) yields a single-element vector.
pub fn load_all(path: &str) -> Result<Vec<Config>, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Io {
        path: path.to_string(),
        source: e,
    })?;
    let mut raw = detect_and_parse(path, &text)?;
    match raw.programs.take() {
        Some(list) if !list.is_empty() => list
            .into_iter()
            .map(|p| validate(merge_program(&raw, p)))
            .collect(),
        _ => Ok(vec![validate(raw)?]),
    }
}

/// Build a full `RawConfig` for one program: program fields override the shared
/// top-level defaults, which fill in everything else.
fn merge_program(global: &RawConfig, p: RawProgram) -> RawConfig {
    RawConfig {
        mode: p.mode.or_else(|| global.mode.clone()),
        command: p.command.or_else(|| global.command.clone()),
        process_name: p.process_name.or_else(|| global.process_name.clone()),
        max_workers: p.max_workers.or(global.max_workers),
        min_workers: p.min_workers.or(global.min_workers),
        shell: p.shell.or(global.shell),
        queue: p.queue.or_else(|| global.queue.clone()),
        queue_connection: p
            .queue_connection
            .or_else(|| global.queue_connection.clone()),
        queue_name: p.queue_name.or_else(|| global.queue_name.clone()),
        autostart: p.autostart.or(global.autostart),
        autorestart: p.autorestart.or(global.autorestart),
        slo_drain_time: p.slo_drain_time.or_else(|| global.slo_drain_time.clone()),
        max: p.max.or(global.max),
        programs: None,
        ..global.clone()
    }
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
            "management" => raw.management = Some(parse_bool(&value, "management")?),
            "management_url" => raw.management_url = Some(value),
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

    // Only one backend exists today. Accepting anything else here would silently
    // read RabbitMQ while the operator believes they configured Redis.
    let queue = raw.queue.unwrap_or_else(|| "rabbitmq".to_string());
    if !queue.eq_ignore_ascii_case("rabbitmq") {
        return Err(ConfigError::Invalid {
            key: "queue".into(),
            reason: format!("unsupported backend '{queue}' (only 'rabbitmq' is implemented)"),
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
        queue,
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
        management: raw.management.unwrap_or(true),
        management_url: raw.management_url,
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
    // Release builds have overflow checks off, so an unchecked multiply would
    // silently wrap a nonsense value into a plausible-looking one.
    let secs = n.checked_mul(mult).ok_or_else(|| ConfigError::Invalid {
        key: "duration".into(),
        reason: format!("'{s}' overflows the representable range"),
    })?;
    Ok(Duration::from_secs(secs))
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
    n.checked_mul(mult).ok_or_else(|| ConfigError::Invalid {
        key: "bytes".into(),
        reason: format!("'{s}' overflows the representable range"),
    })
}

/// Expand the `%(process_num)02d` placeholder to a zero-padded index.
pub fn expand_process_num(template: &str, n: u32) -> String {
    template.replace("%(process_num)02d", &format!("{n:02}"))
}

/// A stable name for the *program* (not an individual worker), for use as a
/// metrics label. `process_name` is a per-worker template, so leaving the
/// placeholder in would put a literal `%(process_num)02d` in every label.
pub fn program_label(process_name: &str) -> String {
    let stripped = process_name.replace("%(process_num)02d", "");
    let trimmed = stripped.trim_matches(|c: char| c == '_' || c == '-' || c == '.' || c == ' ');
    if trimmed.is_empty() {
        "program".to_string()
    } else {
        trimmed.to_string()
    }
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

    fn slo_base_with_drain() -> RawConfig {
        RawConfig {
            slo_drain_time: Some("120s".into()),
            ..slo_base()
        }
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

    #[test]
    fn program_label_strips_the_worker_placeholder() {
        // A metrics label must name the program, not a worker template.
        assert_eq!(program_label("consumer_%(process_num)02d"), "consumer");
        assert_eq!(program_label("index-%(process_num)02d"), "index");
        assert_eq!(program_label("mailer"), "mailer");
        // Degenerate templates still yield something usable as a label.
        assert_eq!(program_label("%(process_num)02d"), "program");
        assert_eq!(program_label(""), "program");
    }

    #[test]
    fn rejects_an_unimplemented_queue_backend() {
        // Silently reading RabbitMQ while the operator configured Redis is worse
        // than refusing to start.
        let raw = RawConfig {
            queue: Some("redis".into()),
            ..slo_base_with_drain()
        };
        let err = validate(raw).unwrap_err();
        assert!(matches!(&err, ConfigError::Invalid { key, .. } if key == "queue"));
        assert!(err.to_string().contains("redis"));
    }

    #[test]
    fn accepts_the_supported_backend_case_insensitively() {
        let raw = RawConfig {
            queue: Some("RabbitMQ".into()),
            ..slo_base_with_drain()
        };
        assert_eq!(validate(raw).unwrap().queue, "RabbitMQ");
    }

    #[test]
    fn byte_and_duration_overflow_is_an_error_not_a_wrap() {
        // Release builds disable overflow checks, so an unchecked multiply would
        // turn this into a small, plausible-looking number.
        assert!(parse_bytes("18000000000TB").is_err());
        assert!(parse_duration("99999999999999999999h").is_err());
        // Values that genuinely fit still parse.
        assert_eq!(parse_bytes("1024TB").unwrap(), 1024 * 1024u64.pow(4));
        assert_eq!(parse_duration("48h").unwrap(), Duration::from_secs(172_800));
    }

    #[test]
    fn management_defaults_on_and_is_overridable() {
        assert!(validate(slo_base_with_drain()).unwrap().management);
        let raw = RawConfig {
            management: Some(false),
            management_url: Some("http://mgmt:15672".into()),
            ..slo_base_with_drain()
        };
        let cfg = validate(raw).unwrap();
        assert!(!cfg.management);
        assert_eq!(cfg.management_url.as_deref(), Some("http://mgmt:15672"));
    }

    #[test]
    fn supervisor_conf_reads_the_management_keys() {
        let text = "[program:demo]\n  mode=threshold\n  command=echo hi\n  max_workers=3\n  \
                    queue_connection=amqp://localhost\n  queue_name=q\n  management=false\n  \
                    management_url=http://mgmt:15672\n";
        let cfg = validate(parse_supervisor_conf(text).unwrap()).unwrap();
        assert!(!cfg.management);
        assert_eq!(cfg.management_url.as_deref(), Some("http://mgmt:15672"));
    }

    #[test]
    fn program_inherits_globals_and_overrides() {
        let global = RawConfig {
            mode: Some("threshold".into()),
            queue_connection: Some("amqp://localhost".into()),
            max_workers: Some(4),
            ..Default::default()
        };
        let p1 = RawProgram {
            command: Some("worker-a".into()),
            queue_name: Some("qa".into()),
            ..Default::default()
        };
        let p2 = RawProgram {
            command: Some("worker-b".into()),
            queue_name: Some("qb".into()),
            max_workers: Some(8),
            ..Default::default()
        };
        let c1 = validate(merge_program(&global, p1)).unwrap();
        let c2 = validate(merge_program(&global, p2)).unwrap();
        assert_eq!(c1.command, "worker-a");
        assert_eq!(c1.queue_name, "qa");
        assert_eq!(c1.max_workers, 4); // inherited from global
        assert_eq!(c2.max_workers, 8); // program override
        assert_eq!(c2.queue_connection, "amqp://localhost"); // shared global
    }
}
