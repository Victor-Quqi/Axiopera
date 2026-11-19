// A Codex app-server that says nothing: whoever calls it waits until the gate opens.
use std::path::PathBuf;
use std::time::Duration;

fn main() {
    let root = PathBuf::from(std::env::var_os("CODEX_HOME").unwrap());
    std::fs::write(root.join("pid"), std::process::id().to_string()).unwrap();
    while !root.join("continue").exists() {
        std::thread::sleep(Duration::from_millis(5));
    }
}
