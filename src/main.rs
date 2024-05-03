use tokio::time::{sleep, Duration};
mod config_reader;
mod rabbitmq_connector;
mod process;
mod system_info;

#[tokio::main]
async fn main() {
    let config_path = "./data/config.conf";
    let config = config_reader::load_config(config_path)
        .expect("Nie udało się wczytać konfiguracji");

    let mut queue_length = 0;

    println!("{}", "test");
    println!("{}", config.queue_connection);
    loop {
        let info = system_info::SystemData::new();
        println!("used swap: {:.2}", kb_to_gb(info.used_swap));
        println!("total swap: {:.2}", kb_to_gb(info.total_swap));
        println!("RAM used: {:.2}", kb_to_gb(info.memory_used));
        println!("RAM total: {:.2}", kb_to_gb(info.memory_total));

        match rabbitmq_connector::get_queue_message_count(&config.queue_connection, &config.queue_name).await {
            Ok(queue) => {
                println!("Długość kolejki: {}", queue.length);
                queue_length = queue.length;
                let total_processes = process::process_info(&config.process_name).await;
                process::make_new_process_if_needed(&config, queue_length, total_processes.length, info.memory_used, info.memory_total).await;
            }
            Err(e) => {
                eprintln!("Wystąpił błąd: {}", e);
                continue;
            }
        }
        sleep(Duration::from_secs(10)).await;
    }
}

fn kb_to_gb(kilobytes: u64) -> f64 {
    let bytes_in_kb: u64 = 1_024;
    let kb_in_gb: u64 = 1_024 * 1_024 * 1_024;
    let total_bytes: u64 = kilobytes * bytes_in_kb;
    total_bytes as f64 / kb_in_gb as f64
}
