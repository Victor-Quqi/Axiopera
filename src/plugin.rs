//! `axiopera plugin cleanup`: remove what Codex keeps of the Axiopera plugin after the user
//! uninstalled it: the hooks' trust records in `config.toml`, which would trust the same hooks
//! again on a reinstall without review, the baseline Codex keeps of the plugin's contents, and
//! empty folders. Nothing is touched while the plugin is still installed.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::agent::{self, codex_remote::Remote};
use crate::ipc::Agent;

const PLUGIN: &str = "axiopera";
/// The plugin's ID from this repository's marketplace, tried even without a trust record left.
const PLUGIN_ID: &str = "axiopera@axiopera";
/// Codex keys a plugin hook's trust record `<plugin ID>:<hooks file>:<event>:...`.
const HOOKS_FILE: &str = "codex/hooks.json:";

pub async fn cleanup(home: &Path) -> Result<i32> {
    let codex_home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| agent::home_dir().join(".codex"));
    let mut done = Vec::new();
    let file = codex_home.join("config.toml");
    let keys = match std::fs::read_to_string(&file) {
        Ok(text) => trust_records(&text).with_context(|| format!("reading {}", file.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", file.display())),
    };
    if !keys.is_empty() {
        let configured = crate::config::load(home)?
            .agents
            .get(Agent::Codex)
            .path
            .clone();
        let env: Vec<(String, String)> = std::env::vars().collect();
        let exe = agent::resolve_exe(Agent::Codex, configured.as_deref(), &env)?;
        let paths: Vec<String> = keys
            .iter()
            .map(|k| format!("hooks.state.\"{k}\""))
            .collect();
        Remote::private(&exe, Some(&codex_home))
            .await?
            .clear_config(&paths)
            .await?;
        done.push(format!(
            "removed {} hook trust record(s) from {}",
            keys.len(),
            file.display()
        ));
    }
    let ids: BTreeSet<&str> = keys
        .iter()
        .filter_map(|k| k.split_once(':').map(|(id, _)| id))
        .chain([PLUGIN_ID])
        .collect();
    let trust_dir = codex_home.join(".hook-trust");
    let mut baselines = 0;
    for path in trust_baselines(&codex_home, &ids) {
        match std::fs::remove_file(&path) {
            Ok(()) => baselines += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {}", path.display())),
        }
    }
    let mut folders = vec![codex_home.join("plugins").join("cache").join(PLUGIN)];
    if baselines > 0 {
        done.push(format!(
            "removed {baselines} hook trust baseline(s) from {}",
            trust_dir.display()
        ));
        // Emptied by us, it held only our baselines.
        folders.insert(0, trust_dir);
    }
    for dir in folders {
        let empty = std::fs::read_dir(&dir).is_ok_and(|mut entries| entries.next().is_none());
        if empty {
            std::fs::remove_dir(&dir).with_context(|| format!("removing {}", dir.display()))?;
            done.push(format!("removed the empty folder {}", dir.display()));
        }
    }
    if done.is_empty() {
        done.push("nothing to clean up".to_string());
    }
    println!("{}", done.join("\n"));
    Ok(crate::ipc::exit::OK)
}

/// The keys of our hooks' trust records; an error while the plugin is installed.
fn trust_records(config_toml: &str) -> Result<Vec<String>> {
    let config: toml::Table = toml::from_str(config_toml)?;
    let installed = config
        .get("plugins")
        .and_then(|p| p.as_table())
        .and_then(|p| p.keys().find(|id| is_ours(id)));
    if let Some(id) = installed {
        bail!("the plugin is still installed; remove it first with `codex plugin remove {id}`");
    }
    let states = config
        .get("hooks")
        .and_then(|h| h.get("state"))
        .and_then(|s| s.as_table());
    Ok(states
        .into_iter()
        .flat_map(|s| s.keys())
        .filter(|k| {
            k.split_once(':')
                .is_some_and(|(id, rest)| is_ours(id) && rest.starts_with(HOOKS_FILE))
        })
        .cloned()
        .collect())
}

/// Plugin IDs are `<plugin>@<marketplace>`.
fn is_ours(id: &str) -> bool {
    id.split_once('@').is_some_and(|(name, _)| name == PLUGIN)
}

/// Codex 0.164 pins a trusted plugin's contents in `.hook-trust/<name>`, named after the user
/// `config.toml` and the plugin ID, and keeps the file after an uninstall. Codex canonicalizes
/// `CODEX_HOME` but not the default home, so both spellings are tried.
fn trust_baselines(codex_home: &Path, ids: &BTreeSet<&str>) -> BTreeSet<PathBuf> {
    let mut homes = vec![codex_home.to_path_buf()];
    if let Ok(canonical) = std::fs::canonicalize(codex_home) {
        homes.push(without_verbatim_prefix(canonical));
    }
    let dir = codex_home.join(".hook-trust");
    let mut paths = BTreeSet::new();
    for home in homes {
        let config_toml = home.join("config.toml");
        for id in ids {
            paths.insert(dir.join(baseline_name(&config_toml, id)));
        }
    }
    paths
}

/// SHA-256 of both parts, each preceded by its length, as Codex names the file.
fn baseline_name(config_toml: &Path, id: &str) -> String {
    let mut data = Vec::new();
    for part in [config_toml.as_os_str().as_encoded_bytes(), id.as_bytes()] {
        data.extend((part.len() as u64).to_le_bytes());
        data.extend(part);
    }
    sha256(&data).iter().map(|b| format!("{b:02x}")).collect()
}

/// A canonical Windows path as Codex keeps it: `\\?\C:\...` becomes `C:\...` and
/// `\\?\UNC\server\...` becomes `\\server\...`.
fn without_verbatim_prefix(path: PathBuf) -> PathBuf {
    let Some(text) = path.to_str() else {
        return path;
    };
    if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{unc}"));
    }
    match text.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => path,
    }
}

/// SHA-256 (FIPS 180-4), only for the file name above.
fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut message = data.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend((data.len() as u64 * 8).to_be_bytes());
    for block in message.chunks(64) {
        let mut w = [0u32; 64];
        for (i, word) in block.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes(word.try_into().unwrap());
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut v = h;
        for (k, w) in K.iter().zip(w) {
            let [a, b, c, d, e, f, g, hh] = v;
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add((e & f) ^ (!e & g))
                .wrapping_add(*k)
                .wrapping_add(w);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let t2 = s0.wrapping_add((a & b) ^ (a & c) ^ (b & c));
            v = [t1.wrapping_add(t2), a, b, c, d.wrapping_add(t1), e, f, g];
        }
        for (x, y) in h.iter_mut().zip(v) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out = [0u8; 32];
    for (chunk, x) in out.chunks_mut(4).zip(h) {
        chunk.copy_from_slice(&x.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_our_hooks_trust_records_go() {
        let config = r#"
[hooks.state."axiopera@axiopera:codex/hooks.json:stop:0:0"]
trusted_hash = "sha256:a"

[hooks.state."axiopera@fork:codex/hooks.json:post_tool_use:0:0"]
trusted_hash = "sha256:b"

[hooks.state."axiopera-extra@axiopera:codex/hooks.json:stop:0:0"]
trusted_hash = "sha256:c"

[hooks.state."/home/me/.codex/config.toml:stop:0:0"]
trusted_hash = "sha256:d"
"#;
        let mut keys = trust_records(config).unwrap();
        keys.sort();
        assert_eq!(
            keys,
            [
                "axiopera@axiopera:codex/hooks.json:stop:0:0",
                "axiopera@fork:codex/hooks.json:post_tool_use:0:0",
            ]
        );
        let installed = format!("{config}\n[plugins.\"axiopera@axiopera\"]\nenabled = true\n");
        assert!(trust_records(&installed).is_err());
    }

    #[test]
    fn baselines_are_named_as_codex_names_them() {
        let hex =
            |data: &[u8]| -> String { sha256(data).iter().map(|b| format!("{b:02x}")).collect() };
        assert_eq!(
            hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        assert_eq!(
            hex(&[b'a'; 1000]),
            "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3"
        );
        assert_eq!(
            baseline_name(Path::new(r"C:\Users\me\.codex\config.toml"), PLUGIN_ID),
            "af40b214d4e05684a7d253a37a68ef13e2f2cc1fefe19ccb822aaa73e20337f9"
        );
        assert_eq!(
            baseline_name(Path::new("/home/me/.codex/config.toml"), PLUGIN_ID),
            "4fe4816e06763323b5f8260d7a0cf7dddc4679d13a4cd658192fa7b6f8676792"
        );
    }

    #[test]
    fn canonical_windows_paths_lose_the_verbatim_prefix() {
        let simple = |p: &str| without_verbatim_prefix(PathBuf::from(p));
        assert_eq!(simple(r"\\?\C:\Users\me"), PathBuf::from(r"C:\Users\me"));
        assert_eq!(simple(r"\\?\UNC\srv\share"), PathBuf::from(r"\\srv\share"));
        assert_eq!(
            simple(r"\\?\Volume{x}\me"),
            PathBuf::from(r"\\?\Volume{x}\me")
        );
        assert_eq!(simple("/home/me"), PathBuf::from("/home/me"));
    }
}
