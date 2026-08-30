//! RabbitMQ metric source.
//!
//! Two ways to read one queue, in order of preference:
//!
//! 1. **Management API** — reports the true backlog (ready *plus* unacked) and
//!    the ack/publish rates, so `mu` and `lambda` are measured directly.
//! 2. **AMQP passive `queue_declare`** — always available, but reports only
//!    messages *ready*. `mu` then has to be inferred by regression (see
//!    [`crate::estimator`]).
//!
//! The AMQP connection and channel are kept open across ticks and only rebuilt
//! on error.

use crate::estimator::BrokerRates;
use crate::management::ManagementClient;
use lapin::{
    options::QueueDeclareOptions, types::FieldTable, Channel, Connection, ConnectionProperties,
};
use std::error::Error;

/// Consecutive management failures before backing off.
const MANAGEMENT_FAILURES_BEFORE_BACKOFF: u32 = 3;
/// Upper bound on the back-off, in ticks.
const MANAGEMENT_MAX_BACKOFF: u32 = 60;

/// One tick's reading for a queue.
#[derive(Debug, Clone, Copy)]
pub struct QueueReading {
    /// Messages still needing work. Includes unacked when the management API is
    /// in use; ready-only otherwise.
    pub backlog: u32,
    /// Broker-measured rates, when available.
    pub rates: Option<BrokerRates>,
}

/// A long-lived source for one queue's depth and rates.
pub struct RabbitSource {
    uri: String,
    queue: String,
    conn: Option<Connection>,
    channel: Option<Channel>,
    management: Option<ManagementClient>,
    management_failures: u32,
    management_backoff: u32,
}

impl RabbitSource {
    pub fn new(uri: String, queue: String, management: Option<ManagementClient>) -> Self {
        Self {
            uri,
            queue,
            conn: None,
            channel: None,
            management,
            management_failures: 0,
            management_backoff: 0,
        }
    }

    /// Read the queue. Prefers the management API and silently degrades to AMQP.
    pub async fn read(&mut self) -> Result<QueueReading, Box<dyn Error>> {
        if let Some(reading) = self.read_management().await {
            return Ok(reading);
        }
        let depth = self.queue_depth().await?;
        Ok(QueueReading {
            backlog: depth,
            rates: None,
        })
    }

    /// Try the management API, respecting the back-off. `None` means "fall back".
    async fn read_management(&mut self) -> Option<QueueReading> {
        let client = self.management.as_ref()?;
        if self.management_backoff > 0 {
            self.management_backoff -= 1;
            return None;
        }
        match client.queue_stats(&self.queue).await {
            Ok(stats) => {
                if self.management_failures > 0 {
                    tracing::info!(queue = %self.queue, "management API recovered");
                    self.management_failures = 0;
                }
                tracing::debug!(
                    queue = %self.queue,
                    messages = stats.messages,
                    ready = stats.messages_ready,
                    unacked = stats.messages_unacknowledged,
                    ack_rate = stats.ack_rate,
                    publish_rate = stats.publish_rate,
                    "read queue stats (management API)"
                );
                Some(QueueReading {
                    backlog: stats.messages.min(u32::MAX as u64) as u32,
                    rates: Some(BrokerRates {
                        ack_rate: stats.ack_rate,
                        publish_rate: stats.publish_rate,
                    }),
                })
            }
            Err(e) => {
                self.management_failures = self.management_failures.saturating_add(1);
                if self.management_failures == MANAGEMENT_FAILURES_BEFORE_BACKOFF {
                    tracing::warn!(
                        queue = %self.queue,
                        error = %e,
                        "management API unavailable; falling back to AMQP depth + regression estimator"
                    );
                } else {
                    tracing::debug!(queue = %self.queue, error = %e, "management API read failed");
                }
                if self.management_failures >= MANAGEMENT_FAILURES_BEFORE_BACKOFF {
                    // Exponential, so a dead endpoint costs one timeout a minute
                    // rather than one every tick.
                    self.management_backoff = (1u32
                        << (self.management_failures - MANAGEMENT_FAILURES_BEFORE_BACKOFF).min(6))
                    .min(MANAGEMENT_MAX_BACKOFF);
                }
                None
            }
        }
    }

    /// Read the queue depth over AMQP, reusing the open channel. On error the
    /// channel (and, if needed, the connection) is dropped so the next call
    /// reconnects.
    pub async fn queue_depth(&mut self) -> Result<u32, Box<dyn Error>> {
        let channel = self.ensure_channel().await?;
        match channel
            .queue_declare(
                &self.queue,
                QueueDeclareOptions {
                    passive: true,
                    ..QueueDeclareOptions::default()
                },
                FieldTable::default(),
            )
            .await
        {
            Ok(q) => {
                let count = q.message_count();
                tracing::debug!(queue = %self.queue, count, "read queue depth (AMQP)");
                Ok(count)
            }
            Err(e) => {
                // A channel-level error closes the channel; recreate it next time.
                self.channel = None;
                Err(Box::new(e))
            }
        }
    }

    async fn ensure_channel(&mut self) -> Result<Channel, Box<dyn Error>> {
        if let Some(ch) = &self.channel {
            return Ok(ch.clone());
        }
        if self.conn.is_none() {
            let conn = match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                Connection::connect(&self.uri, ConnectionProperties::default()),
            )
            .await
            {
                Ok(Ok(c)) => c,
                Ok(Err(e)) => return Err(Box::new(e)),
                Err(_) => return Err(Box::<dyn Error>::from("AMQP connect timed out")),
            };
            self.conn = Some(conn);
        }
        let conn = self
            .conn
            .as_ref()
            .ok_or_else(|| Box::<dyn Error>::from("AMQP connection missing"))?;
        match conn.create_channel().await {
            Ok(ch) => {
                self.channel = Some(ch.clone());
                Ok(ch)
            }
            Err(e) => {
                self.conn = None; // the connection is likely dead
                Err(Box::new(e))
            }
        }
    }
}
