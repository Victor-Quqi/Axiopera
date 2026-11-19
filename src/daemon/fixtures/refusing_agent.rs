// A local AGY-shaped peer. It turns down the terminal's model, and a prompt sent while another
// runs; the running one ends when its gate opens.
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::time::Duration;

fn say(line: String) {
    println!("{line}");
    std::io::stdout().flush().unwrap();
}

fn main() {
    let root = PathBuf::from(std::env::var_os("AXIOPERA_FAKE_HOME").unwrap());
    std::fs::write(root.join("pid"), std::process::id().to_string()).unwrap();
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("requests"))
        .unwrap();
    let mut prompts = 0;
    for line in std::io::stdin().lock().lines() {
        let line = line.unwrap();
        let method = line
            .split("\"method\":\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next());
        let id: String = line
            .split("\"id\":")
            .nth(1)
            .unwrap_or("")
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        // Requests only: the rest are notifications and answers.
        let (Some(method), false) = (method, id.is_empty()) else {
            continue;
        };
        writeln!(log, "{line}").unwrap();
        log.flush().unwrap();
        let reply = |body: &str| format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},{body}}}");
        let refusal = |why: &str| {
            reply(&format!(
                "\"error\":{{\"code\":-32603,\"message\":\"{why}\"}}"
            ))
        };
        let done = reply("\"result\":{\"stopReason\":\"end_turn\"}");
        match method {
            "session/new" => say(reply(
                "\"result\":{\"sessionId\":\"00000000-0000-4000-8000-000000000001\"}",
            )),
            "session/resume" => say(reply(
                "\"result\":{\"models\":{\"availableModels\":[{\"name\":\"Saved\",\"modelId\":\"saved\"}]}}",
            )),
            "session/set_model" => say(refusal("model unavailable")),
            "session/prompt" => {
                prompts += 1;
                match prompts {
                    1 => {
                        std::fs::write(root.join("started"), "").unwrap();
                        let gate = root.join("continue");
                        std::thread::spawn(move || {
                            while !gate.exists() {
                                std::thread::sleep(Duration::from_millis(5));
                            }
                            say(done);
                        });
                    }
                    _ if !root.join("continue").exists() => say(refusal("a permission request is pending")),
                    _ => say(done),
                }
            }
            _ => say(reply("\"result\":{}")),
        }
    }
}
