use lapin::{
    options::QueueDeclareOptions,
    Connection, ConnectionProperties,
};
use std::error::Error;

pub struct Queue {
    pub length: u32,
}

pub async fn get_queue_message_count(queue_connection: &str, queue_name: &str) -> Result<Queue, Box<dyn Error>> {
    let conn = Connection::connect(queue_connection, ConnectionProperties::default()).await
        .map_err(|err| Box::new(err) as Box<dyn Error>)?;
    let channel = conn.create_channel().await
        .map_err(|err| Box::new(err) as Box<dyn Error>)?;

    let queue = channel.queue_declare(
        queue_name,
        QueueDeclareOptions {
            passive: true,
            ..QueueDeclareOptions::default()
        },
        lapin::types::FieldTable::default(),
    ).await.map_err(|err| Box::new(err) as Box<dyn Error>)?;

    println!("Liczba wiadomości w kolejce '{}': {}", queue_name, queue.message_count());

    Ok(Queue {
        length: queue.message_count(),
    })
}