use std::fs::File;
use std::io::{self, BufRead};
use std::path::Path;

#[derive(Debug)]
pub struct Config {
    pub(crate) command: String,
    pub(crate) process_name: String,
    pub(crate) max: u32,
    pub(crate) autostart: bool,
    pub(crate) autorestart: bool,
    pub(crate) queue: String,
    pub(crate) queue_connection: String,
    pub(crate) queue_name: String
}

pub(crate) fn load_config(file_path: &str) -> io::Result<Config> {
    let path = Path::new(file_path);
    let file = File::open(path)?;
    let mut config = Config {
        command: String::new(),
        process_name: String::new(),
        max: 0,
        autostart: false,
        autorestart: false,
        queue: String::new(),
        queue_connection: String::new(),
        queue_name: String::new(),
    };

    for line in io::BufReader::new(file).lines() {
        let line = line?;
        if let Some((key, value)) = line.split_once('=') {
            match key.trim() {
                "command" => config.command = value.trim().to_string(),
                "process_name" => config.process_name = value.trim().to_string(),
                "max" => config.max = value.trim().parse().expect("max should be a number"),
                "autostart" => config.autostart = value.trim().parse().expect("autostart should be a boolean"),
                "autorestart" => config.autorestart = value.trim().parse().expect("autorestart should be a boolean"),
                "queue" => config.queue = value.trim().to_string(),
                "queue_connection" => config.queue_connection = value.trim().to_string(),
                "queue_name" => config.queue_name = value.trim().to_string(),
                _ => {}, // Ignoruje nieznane klucze
            }
        }
    }

    Ok(config)
}