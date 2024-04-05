use sysinfo::{System, Process};

use std::process::{Command, Stdio};
use crate::config_reader::Config;

pub struct TotalProcess {
    pub length: usize,
}

fn count_matching_processes(pattern: &str) -> usize {
    let mut sys = System::new_all();
    sys.refresh_all();

    sys.processes().values()
        .filter(|proc| proc.name().starts_with(pattern))
        .count()
}

pub async fn process_info(pattern: &str) -> TotalProcess {
    let count = count_matching_processes(pattern);
    println!("Znaleziono {} procesów pasujących do wzorca '{}'", count, pattern);

    TotalProcess { length: count }
}

pub async fn make_new_process_if_needed(config: &Config, queue_length: u32, mut process_counter: usize, memory_used: u64, memory_total: u64) {
    let ram_usage_percent = (memory_used as f64 / memory_total as f64) * 100.0;

    if (queue_length == 0) {
        kill_matching_processes(&config.process_name);
    }
    println!("Sprawdzam czy robic process");
    if ram_usage_percent < config.max as f64 && queue_length > 40 {
        process_counter += 1;
        let process_name = format!("{}_{:02}", &config.process_name, process_counter);
        println!("Uruchamianie nowego procesu: {}", process_name);
        let command = &config.command.replace("%(process_num)02d", &process_counter.to_string());

        match Command::new("sh")
            .arg("-c")
            .arg(command)
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn() {
            Ok(_) => println!("Pomyślnie uruchomiono proces: {}", process_name),
            Err(e) => eprintln!("Błąd podczas uruchamiania procesu: {}", e),
        }
    }
}

fn kill_matching_processes(command: &str) {
    let pattern = format!("^{}", command);
    if pattern.is_empty() {
        return;
    }

    println!("Zabijanie procesów pasujących do wzorca: '{}'", pattern);

    let output = Command::new("pkill")
        .arg("-f")
        .arg(pattern.clone())
        .output()
        .expect("failed to execute process");

    if output.status.success() {
        println!("Pomyślnie zabito procesy pasujące do wzorca: '{}'", pattern);
    } else {
        let error_message = String::from_utf8_lossy(&output.stderr);
        eprintln!("Nie udało się zabić procesów pasujących do wzorca: '{}'. Błąd: {}", pattern, error_message);
        match output.status.code() {
            Some(code) => eprintln!("Kod błędu: {}", code),
            None => eprintln!("Proces został zakończony przez sygnał"),
        }
    }
}