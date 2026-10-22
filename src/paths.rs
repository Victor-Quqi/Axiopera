use std::path::PathBuf;

/// Overrides `~/.axiopera`, e.g. for an isolated daemon in tests.
pub const HOME_VAR: &str = "AXIOPERA_HOME";

/// `~/.axiopera`, or `AXIOPERA_HOME`.
pub fn default_home() -> PathBuf {
    if let Some(h) = std::env::var_os(HOME_VAR) {
        return PathBuf::from(h);
    }
    let home =
        std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).unwrap_or_default();
    PathBuf::from(home).join(".axiopera")
}

/// FNV-1a: a short name that stays the same across versions (pipe names).
pub fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |h, b| {
        (h ^ *b as u64).wrapping_mul(0x100000001b3)
    })
}
