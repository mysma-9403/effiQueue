//! RabbitMQ management-API client — the preferred source of `mu` and `lambda`.
//!
//! A passive `queue_declare` over AMQP reports only `message_count`, which is
//! messages **ready** and excludes anything currently in flight, and says nothing
//! at all about rates. The management plugin reports both:
//!
//! * `message_stats.ack_details.rate` — acknowledgements per second (`mu * n`)
//! * `message_stats.publish_details.rate` — publishes per second (`lambda`)
//! * `messages` — ready **plus** unacknowledged, the honest backlog
//!
//! With those, `mu` and `lambda` are measured rather than inferred, and the
//! identifiability problem described in [`crate::estimator`] disappears.
//!
//! The client speaks just enough HTTP/1.1 to issue one authenticated GET, so the
//! binary stays dependency-light and consistent with the hand-rolled metrics
//! server. Plaintext `http://` only — same trust model as `/metrics`: bind it to
//! a private network. Point `management_url` at a local reverse proxy if the
//! management API is only reachable over TLS.

use crate::http;
use serde::Deserialize;
use std::time::Duration;

const DEFAULT_MANAGEMENT_PORT: u16 = 15672;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum ManagementError {
    #[error("management API request failed: {0}")]
    Http(#[from] http::HttpError),
    #[error("could not parse the management API response: {0}")]
    Body(String),
    #[error("cannot derive a management URL from '{0}': {1}")]
    Uri(String, String),
}

/// What the management API tells us about one queue.
#[derive(Debug, Clone, Copy)]
pub struct QueueStats {
    /// Ready **plus** unacknowledged — the backlog that actually still needs work.
    pub messages: u64,
    pub messages_ready: u64,
    pub messages_unacknowledged: u64,
    /// Acknowledgements per second across all consumers (= `mu * n`).
    pub ack_rate: f64,
    /// Publishes per second into the queue (= `lambda`).
    pub publish_rate: f64,
}

#[derive(Debug, Deserialize)]
struct RawQueue {
    #[serde(default)]
    messages: u64,
    #[serde(default)]
    messages_ready: u64,
    #[serde(default)]
    messages_unacknowledged: u64,
    #[serde(default)]
    message_stats: Option<RawStats>,
}

#[derive(Debug, Default, Deserialize)]
struct RawStats {
    #[serde(default)]
    ack_details: Option<RawRate>,
    #[serde(default)]
    publish_details: Option<RawRate>,
}

#[derive(Debug, Default, Deserialize)]
struct RawRate {
    #[serde(default)]
    rate: f64,
}

/// Connection details for one broker's management API.
#[derive(Debug, Clone)]
pub struct ManagementClient {
    host: String,
    port: u16,
    vhost: String,
    authorization: String,
}

impl ManagementClient {
    /// Derive the management endpoint from an AMQP URI: same host and
    /// credentials, port 15672.
    ///
    /// Returns `Ok(None)` for `amqps://`, where the management API is very
    /// unlikely to be plaintext on 15672 — the caller should fall back rather
    /// than fail. An explicit `management_url` overrides all of this.
    pub fn from_amqp_uri(uri: &str) -> Result<Option<Self>, ManagementError> {
        let parts = UriParts::parse(uri)?;
        if parts.tls {
            return Ok(None);
        }
        Ok(Some(Self {
            authorization: basic_auth(&parts.user, &parts.password),
            host: parts.host,
            port: DEFAULT_MANAGEMENT_PORT,
            vhost: parts.vhost,
        }))
    }

    /// Build from an explicit `http://user:pass@host:port` override. The vhost
    /// still comes from the AMQP URI, since the override addresses the endpoint,
    /// not the queue.
    pub fn from_override(url: &str, amqp_uri: &str) -> Result<Self, ManagementError> {
        let rest = url
            .strip_prefix("http://")
            .ok_or_else(|| ManagementError::Uri(url.into(), "expected an http:// URL".into()))?;
        let rest = rest.trim_end_matches('/');
        let (credentials, authority) = match rest.rsplit_once('@') {
            Some((c, a)) => (Some(c), a),
            None => (None, rest),
        };
        let authority = authority.split('/').next().unwrap_or(authority);
        let (host, port) = split_host_port(authority, DEFAULT_MANAGEMENT_PORT);
        if host.is_empty() {
            return Err(ManagementError::Uri(url.into(), "missing host".into()));
        }
        // Credentials in the override win; otherwise reuse the AMQP ones.
        let (user, password) = match credentials {
            Some(c) => match c.split_once(':') {
                Some((u, p)) => (percent_decode(u), percent_decode(p)),
                None => (percent_decode(c), String::new()),
            },
            None => {
                let p = UriParts::parse(amqp_uri)?;
                (p.user, p.password)
            }
        };
        Ok(Self {
            authorization: basic_auth(&user, &password),
            host,
            port,
            vhost: UriParts::parse(amqp_uri)?.vhost,
        })
    }

    pub fn endpoint(&self) -> String {
        format!("http://{}:{}", self.host, self.port)
    }

    /// Fetch one queue's stats.
    pub async fn queue_stats(&self, queue: &str) -> Result<QueueStats, ManagementError> {
        let path = format!(
            "/api/queues/{}/{}",
            percent_encode(&self.vhost),
            percent_encode(queue)
        );
        let body = self.get(&path).await?;
        let raw: RawQueue =
            serde_json::from_str(&body).map_err(|e| ManagementError::Body(e.to_string()))?;
        let stats = raw.message_stats.unwrap_or_default();
        Ok(QueueStats {
            messages: raw.messages,
            messages_ready: raw.messages_ready,
            messages_unacknowledged: raw.messages_unacknowledged,
            ack_rate: stats.ack_details.unwrap_or_default().rate,
            publish_rate: stats.publish_details.unwrap_or_default().rate,
        })
    }

    async fn get(&self, path: &str) -> Result<String, ManagementError> {
        Ok(http::get(
            &self.host,
            self.port,
            path,
            Some(&format!("Basic {}", self.authorization)),
            REQUEST_TIMEOUT,
        )
        .await?)
    }
}

/// The pieces of an AMQP URI the management client needs.
struct UriParts {
    user: String,
    password: String,
    host: String,
    vhost: String,
    tls: bool,
}

impl UriParts {
    fn parse(uri: &str) -> Result<Self, ManagementError> {
        let bad = |why: &str| ManagementError::Uri(uri.to_string(), why.to_string());
        let (scheme, rest) = uri
            .split_once("://")
            .ok_or_else(|| bad("expected amqp:// or amqps://"))?;
        let tls = match scheme {
            "amqp" => false,
            "amqps" => true,
            other => return Err(bad(&format!("unsupported scheme '{other}'"))),
        };

        let (credentials, rest) = match rest.split_once('@') {
            Some((c, r)) => (Some(c), r),
            None => (None, rest),
        };
        let (user, password) = match credentials {
            Some(c) => match c.split_once(':') {
                Some((u, p)) => (percent_decode(u), percent_decode(p)),
                None => (percent_decode(c), "guest".to_string()),
            },
            // RabbitMQ's documented defaults when the URI omits credentials.
            None => ("guest".to_string(), "guest".to_string()),
        };

        // Everything after the first '/' is the vhost. No '/' at all means the
        // default vhost "/"; a trailing '/' with nothing after it means the
        // empty-named vhost, which is a different thing.
        let (authority, vhost) = match rest.split_once('/') {
            Some((a, v)) => (a, percent_decode(v.split('?').next().unwrap_or(v))),
            None => (rest, "/".to_string()),
        };
        let (host, _) = split_host_port(authority, 5672);
        if host.is_empty() {
            return Err(bad("missing host"));
        }
        Ok(Self {
            user,
            password,
            host,
            vhost,
            tls,
        })
    }
}

/// Split `host:port`, tolerating bracketed IPv6 literals.
fn split_host_port(authority: &str, default_port: u16) -> (String, u16) {
    if let Some(end) = authority.strip_prefix('[').and_then(|r| r.find(']')) {
        let host = &authority[1..=end];
        let port = authority[end + 2..]
            .strip_prefix(':')
            .and_then(|p| p.parse().ok())
            .unwrap_or(default_port);
        return (host.to_string(), port);
    }
    match authority.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(default_port)),
        None => (authority.to_string(), default_port),
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encode a path segment. The default vhost "/" must become "%2F".
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn basic_auth(user: &str, password: &str) -> String {
    http::base64(format!("{user}:{password}").as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_management_endpoint_from_amqp_uri() {
        let c = ManagementClient::from_amqp_uri("amqp://guest:guest@localhost:5672")
            .unwrap()
            .unwrap();
        assert_eq!(c.endpoint(), "http://localhost:15672");
        assert_eq!(c.vhost, "/");
        assert_eq!(c.authorization, "Z3Vlc3Q6Z3Vlc3Q=");
    }

    #[test]
    fn uri_without_credentials_uses_rabbitmq_defaults() {
        let p = UriParts::parse("amqp://broker.internal").unwrap();
        assert_eq!(p.user, "guest");
        assert_eq!(p.password, "guest");
        assert_eq!(p.host, "broker.internal");
        assert_eq!(p.vhost, "/");
    }

    #[test]
    fn named_and_empty_vhosts_are_distinguished() {
        // No path at all -> the default vhost "/".
        assert_eq!(UriParts::parse("amqp://h:5672").unwrap().vhost, "/");
        // A trailing slash -> the vhost literally named "".
        assert_eq!(UriParts::parse("amqp://h:5672/").unwrap().vhost, "");
        assert_eq!(UriParts::parse("amqp://h:5672/prod").unwrap().vhost, "prod");
        // Percent-encoded vhost names round-trip.
        assert_eq!(UriParts::parse("amqp://h/a%2Fb").unwrap().vhost, "a/b");
    }

    #[test]
    fn percent_encodes_the_default_vhost() {
        assert_eq!(percent_encode("/"), "%2F");
        assert_eq!(percent_encode("messages_low"), "messages_low");
        assert_eq!(percent_encode("a b/c"), "a%20b%2Fc");
    }

    #[test]
    fn credentials_are_percent_decoded() {
        let p = UriParts::parse("amqp://us%40er:p%40ss@host:5672/v").unwrap();
        assert_eq!(p.user, "us@er");
        assert_eq!(p.password, "p@ss");
    }

    #[test]
    fn ipv6_literals_parse() {
        let (h, p) = split_host_port("[::1]:15672", 5672);
        assert_eq!((h.as_str(), p), ("::1", 15672));
        let (h, p) = split_host_port("[fe80::1]", 5672);
        assert_eq!((h.as_str(), p), ("fe80::1", 5672));
    }

    #[test]
    fn amqps_declines_auto_derivation() {
        // Guessing plaintext 15672 for a TLS broker would be wrong; the caller
        // falls back to the regression estimator instead of erroring.
        assert!(
            ManagementClient::from_amqp_uri("amqps://guest:guest@host:5671")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn override_url_supplies_its_own_credentials() {
        let c = ManagementClient::from_override(
            "http://admin:s3cret@mgmt.internal:8080/",
            "amqp://guest:guest@broker:5672/prod",
        )
        .unwrap();
        assert_eq!(c.endpoint(), "http://mgmt.internal:8080");
        assert_eq!(c.vhost, "prod"); // vhost still comes from the AMQP URI
        assert_eq!(c.authorization, http::base64(b"admin:s3cret"));
    }

    #[test]
    fn override_url_without_credentials_reuses_amqp_ones() {
        let c = ManagementClient::from_override(
            "http://mgmt.internal:15672",
            "amqp://bob:pw@broker:5672/prod",
        )
        .unwrap();
        assert_eq!(c.authorization, http::base64(b"bob:pw"));
    }

    #[test]
    fn override_rejects_non_http_urls() {
        assert!(ManagementClient::from_override("https://x:15672", "amqp://h").is_err());
    }

    #[test]
    fn deserializes_a_real_management_payload() {
        // Trimmed from an actual GET /api/queues/%2F/<q> response.
        let body = r#"{
            "messages": 1500, "messages_ready": 1400, "messages_unacknowledged": 100,
            "message_stats": {
                "ack": 90210, "ack_details": {"rate": 80.4},
                "publish": 120000, "publish_details": {"rate": 200.5},
                "deliver_get_details": {"rate": 81.0}
            },
            "name": "messages_low", "vhost": "/"
        }"#;
        let raw: RawQueue = serde_json::from_str(body).unwrap();
        assert_eq!(raw.messages, 1500);
        assert_eq!(raw.messages_unacknowledged, 100);
        let stats = raw.message_stats.unwrap();
        assert_eq!(stats.ack_details.unwrap().rate, 80.4);
        assert_eq!(stats.publish_details.unwrap().rate, 200.5);
    }

    #[test]
    fn tolerates_a_queue_with_no_traffic_yet() {
        // An idle queue has no `message_stats` key at all.
        let raw: RawQueue =
            serde_json::from_str(r#"{"messages": 0, "messages_ready": 0}"#).unwrap();
        assert_eq!(raw.messages, 0);
        assert!(raw.message_stats.is_none());
        let stats = raw.message_stats.unwrap_or_default();
        assert_eq!(stats.ack_details.unwrap_or_default().rate, 0.0);
    }
}
