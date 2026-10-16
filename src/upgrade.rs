//! A newer release, checked at startup as k9s does, and the command that upgrades this install.
//! One request to GitHub in the background (curl, so no HTTP client is built in); any failure stays silent.
//! Off with E9S_NO_UPGRADE_CHECK=1, in demo mode, and for builds from a checkout.

use std::path::PathBuf;
use std::sync::OnceLock;

/// The newer version, once the check found one.
pub static NEWER: OnceLock<String> = OnceLock::new();

const RELEASES: &str = "https://api.github.com/repos/simion/e9s/releases/latest";

pub fn spawn_check() {
    if std::env::var_os("E9S_NO_UPGRADE_CHECK").is_some() || std::env::var_os("E9S_DEMO").is_some() || command().is_none() {
        return;
    }
    std::thread::spawn(|| {
        let Ok(out) = std::process::Command::new("curl").args(["-fsSL", "--max-time", "5", RELEASES]).output() else { return };
        let latest = serde_json::from_slice::<serde_json::Value>(&out.stdout)
            .ok()
            .and_then(|v| v["tag_name"].as_str().map(|t| t.trim_start_matches('v').to_string()));
        if let Some(v) = latest.filter(|v| newer(v, env!("CARGO_PKG_VERSION"))) {
            let _ = NEWER.set(v);
        }
    });
}

fn newer(latest: &str, current: &str) -> bool {
    let parts = |v: &str| v.split('.').map(|p| p.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
    parts(latest) > parts(current)
}

/// How to upgrade, by where this binary lives. None for a build from a checkout (`make install`, `cargo run`).
/// Worked out once: the header asks on every frame.
pub fn command() -> Option<String> {
    static CMD: OnceLock<Option<String>> = OnceLock::new();
    CMD.get_or_init(find_command).clone()
}

fn find_command() -> Option<String> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let path = exe.to_string_lossy();
    if path.contains("/Cellar/") {
        return Some("brew upgrade e9s".into());
    }
    if path.contains("/target/") {
        return None;
    }
    let cargo_home = std::env::var_os("CARGO_HOME").map(PathBuf::from).unwrap_or_else(|| crate::aws::home().join(".cargo"));
    if exe.starts_with(cargo_home.join("bin")) {
        // `cargo install` records what it installed; the shell installer puts its binary in the same place but does not.
        let installed = std::fs::read_to_string(cargo_home.join(".crates2.json")).unwrap_or_default();
        return Some(if installed.contains("\"e9s ") {
            "cargo install e9s".into()
        } else {
            "curl --proto '=https' --tlsv1.2 -LsSf https://github.com/simion/e9s/releases/latest/download/e9s-installer.sh | sh".into()
        });
    }
    Some("download it from https://github.com/simion/e9s/releases/latest".into())
}

#[cfg(test)]
mod tests {
    #[test]
    fn compares_versions_by_number() {
        use super::newer;
        assert!(newer("0.1.1", "0.1.0"));
        assert!(newer("0.10.0", "0.9.3"), "numbers, not text");
        assert!(newer("1.0.0", "0.99.0"));
        assert!(!newer("0.1.0", "0.1.0"));
        assert!(!newer("0.1.0", "0.2.0"));
    }
}
