// A local Codex-shaped peer: admission can wait for a gate while its process stays alive.
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::time::Duration;

fn main() {
    let root = PathBuf::from(std::env::var_os("AXIOPERA_FAKE_HOME").unwrap());
    let hang = std::env::var("AXIOPERA_FAKE_HANG").unwrap();
    let session = std::env::var("AXIOPERA_FAKE_SESSION").unwrap();
    std::fs::write(root.join("pid"), std::process::id().to_string()).unwrap();
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("requests"))
        .unwrap();
    for line in std::io::stdin().lock().lines() {
        let line = line.unwrap();
        writeln!(log, "{line}").unwrap();
        log.flush().unwrap();
        let method = [
            "initialize",
            "thread/start",
            "thread/resume",
            "turn/start",
            "turn/steer",
            "turn/interrupt",
        ]
        .into_iter()
        .find(|m| line.contains(&format!("\"method\":\"{m}\"")));
        let Some(method) = method else {
            continue;
        };
        let id: String = line
            .split("\"id\":")
            .nth(1)
            .unwrap()
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if method != "initialize" {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(root.join("session.jsonl"))
                .unwrap();
            writeln!(file, "{method}").unwrap();
        }
        std::fs::write(root.join(method.replace('/', "-")), "seen").unwrap();
        if method == hang {
            while !root.join("continue").exists() {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        let result = match method {
            "thread/start" | "thread/resume" => {
                format!("{{\"thread\":{{\"id\":\"fake-session\",\"path\":{session}}}}}")
            }
            "turn/start" => "{\"turn\":{\"id\":\"turn\"}}".to_owned(),
            _ => "{}".to_owned(),
        };
        println!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{result}}}");
        std::io::stdout().flush().unwrap();
    }
}
