// A local Claude-shaped peer: its turn leaves a background task that ends without a message.
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::time::Duration;

fn main() {
    let root = PathBuf::from(std::env::var_os("AXIOPERA_FAKE_HOME").unwrap());
    std::fs::write(root.join("pid"), std::process::id().to_string()).unwrap();
    let say = |line: &str| {
        println!("{line}");
        std::io::stdout().flush().unwrap();
    };
    for line in std::io::stdin().lock().lines() {
        let line = line.unwrap();
        if !line.contains(r#""type":"user""#) {
            continue;
        }
        // Claude replays the input it has taken, as this peer was started to do.
        say(&line);
        say(r#"{"type":"system","subtype":"background_tasks_changed","tasks":[{}]}"#);
        say(r#"{"type":"result","subtype":"success","result":"started"}"#);
        while !root.join("continue").exists() {
            std::thread::sleep(Duration::from_millis(5));
        }
        say(r#"{"type":"system","subtype":"background_tasks_changed","tasks":[]}"#);
        std::fs::write(root.join("background-ended"), "seen").unwrap();
    }
}
