//! First-run consent for the OAuth usage endpoint (source A).
//!
//! Asked once, only on an interactive terminal. Without a terminal the
//! `ask` state is treated as off and nothing is written.

use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;

use ocut_core::{Config, OAuthConsent};
use ocut_sources::fsutil::write_private_atomic;

const PROMPT: &str = "\
OpenCUT can read your real 5h and weekly limits from Anthropic's OAuth usage
endpoint, using the token Claude Code keeps in ~/.claude/.credentials.json.
The endpoint is undocumented. The token is sent only to api.anthropic.com,
never stored, logged or refreshed, and only ocutd polls (at most once a minute).
Without it, OpenCUT uses the statusline hook and local logs (estimates).

Enable the OAuth usage source? [y/N] ";

/// Sets `[sources] oauth_api` in the config file, keeping its other keys.
pub fn save(path: &Path, consent: OAuthConsent) -> Result<(), String> {
    let mut doc: toml::Table = match std::fs::read_to_string(path) {
        Ok(s) => s.parse().map_err(|e| format!("invalid config: {e}"))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => toml::Table::new(),
        Err(e) => return Err(format!("cannot read config: {e}")),
    };
    let sources = doc
        .entry("sources")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    let toml::Value::Table(t) = sources else {
        return Err("invalid config: `sources` is not a table".into());
    };
    let v = match consent {
        OAuthConsent::Ask => "ask",
        OAuthConsent::On => "on",
        OAuthConsent::Off => "off",
    };
    t.insert("oauth_api".into(), toml::Value::String(v.into()));
    let body = toml::to_string_pretty(&doc).map_err(|e| e.to_string())?;
    write_private_atomic(path, body.as_bytes()).map_err(|e| format!("cannot write config: {e}"))
}

fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// Asks once when the config says `ask` and a terminal is attached, saves
/// the answer and updates `cfg`.
pub fn maybe_ask(cfg: &mut Config, config_path: &Path) {
    if cfg.sources.oauth_api != OAuthConsent::Ask || !interactive() {
        return;
    }
    let mut err = std::io::stderr();
    let _ = write!(err, "{PROMPT}");
    let _ = err.flush();
    let mut answer = String::new();
    if std::io::stdin().lock().read_line(&mut answer).is_err() {
        return;
    }
    let consent = if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        OAuthConsent::On
    } else {
        OAuthConsent::Off
    };
    match save(config_path, consent) {
        Ok(()) => {
            cfg.sources.oauth_api = consent;
            let _ = writeln!(
                err,
                "Saved oauth_api = {} in {}. Change it there any time.\n",
                if consent.is_enabled() { "on" } else { "off" },
                config_path.display()
            );
        }
        Err(e) => {
            let _ = writeln!(err, "ocut: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_creates_and_preserves() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("opencut").join("config.toml");
        save(&p, OAuthConsent::Off).unwrap();
        let c = Config::from_toml_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(c.sources.oauth_api, OAuthConsent::Off);

        std::fs::write(
            &p,
            "[notify]\nthresholds = [50]\n[sources]\nstatusline = false\n",
        )
        .unwrap();
        save(&p, OAuthConsent::On).unwrap();
        let c = Config::from_toml_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert!(c.sources.oauth_api.is_enabled());
        assert!(!c.sources.statusline);
        assert_eq!(c.notify.thresholds, vec![50]);
    }

    #[test]
    fn bad_sources_type_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("config.toml");
        std::fs::write(&p, "sources = 1\n").unwrap();
        assert!(save(&p, OAuthConsent::On).is_err());
    }
}
