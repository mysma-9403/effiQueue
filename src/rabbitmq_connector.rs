//! RabbitMQ metric source. Keeps the connection/channel open across ticks and
//! only reconnects on error — the passive `queue_declare` reads the depth.

use lapin::{
    options::QueueDeclareOptions, types::FieldTable, Channel, Connection, ConnectionProperties,
};
use std::error::Error;

/// A long-lived source for one queue's depth.
pub struct RabbitSource {
    uri: String,
    queue: String,
    conn: Option<Connection>,
    channel: Option<Channel>,
}

impl RabbitSource {
    pub fn new(uri: String, queue: String) -> Self {
        Self {
            uri,
            queue,
            conn: None,
            channel: None,
        }
    }

    /// Read the queue depth, reusing the open channel. On error the channel
    /// (and, if needed, the connection) is dropped so the next call reconnects.
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
                tracing::debug!(queue = %self.queue, count, "read queue depth");
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
        match self.conn.as_ref().unwrap().create_channel().await {
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
