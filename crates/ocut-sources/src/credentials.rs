//! Reads `claudeAiOauth` from `~/.claude/.credentials.json`.
//!
//! Rules (PLAN.md section 2): read only the `claudeAiOauth` subtree; refuse a
//! file whose mode is wider than 0600 or that another user owns; keep the
//! token in memory only; never refresh it; never log or print it.

use std::fmt;
use std::fs::{File, Metadata};
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use ocut_core::Plan;
use serde::Deserialize;

/// An OAuth access token. `Debug` and `Display` are redacted; there is no
/// `Serialize`. Use [`AccessToken::expose`] only to build the `Authorization`
/// header.
#[derive(Clone, PartialEq, Eq)]
pub struct AccessToken(String);

impl AccessToken {
    pub fn new(token: String) -> Self {
        Self(token)
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AccessToken(<redacted>)")
    }
}

impl fmt::Display for AccessToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// The parts of `claudeAiOauth` OpenCUT uses. The refresh token is never read.
#[derive(Debug, Clone)]
pub struct OAuthCredentials {
    pub access_token: AccessToken,
    /// From `expiresAt` (epoch milliseconds).
    pub expires_at: Option<DateTime<Utc>>,
    /// `subscriptionType`, e.g. `pro` or `max`.
    pub subscription_type: Option<String>,
    /// `rateLimitTier`, e.g. `default_claude_max_20x`.
    pub rate_limit_tier: Option<String>,
}

impl OAuthCredentials {
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        self.expires_at.is_some_and(|t| t <= now)
    }

    /// Maps `subscriptionType` / `rateLimitTier` to a plan. A `max`
    /// subscription without a recognisable tier maps to [`Plan::Max5x`], the
    /// smaller budget, so local estimates err on the high side.
    pub fn plan(&self) -> Plan {
        let tier = self
            .rate_limit_tier
            .as_deref()
            .unwrap_or("")
            .to_ascii_lowercase();
        if tier.contains("20x") {
            return Plan::Max20x;
        }
        if tier.contains("5x") {
            return Plan::Max5x;
        }
        match self
            .subscription_type
            .as_deref()
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("pro") => Plan::Pro,
            Some("max") => Plan::Max5x,
            Some("team" | "enterprise") => Plan::Team,
            _ => Plan::Unknown,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CredentialsError {
    #[error("credentials file not found")]
    NotFound,
    #[error("credentials file mode {mode:o} is wider than 0600")]
    InsecureMode { mode: u32 },
    #[error("credentials file is not owned by the current user")]
    WrongOwner,
    #[error("credentials file has no claudeAiOauth entry")]
    MissingOAuth,
    /// The message must not quote file content.
    #[error("credentials file is malformed: {0}")]
    Malformed(String),
    #[error("cannot read credentials file: {0}")]
    Io(#[from] std::io::Error),
}

/// `$CLAUDE_CONFIG_DIR/.credentials.json`, else `~/.claude/.credentials.json`.
pub fn default_credentials_path(home: &Path, claude_config_dir: Option<&Path>) -> PathBuf {
    claude_config_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join(".claude"))
        .join(".credentials.json")
}

/// Checks owner and mode, then parses with [`parse_credentials_json`].
///
/// The checks run on the opened handle, so the file cannot be swapped
/// between the check and the read.
pub fn read_credentials(path: &Path) -> Result<OAuthCredentials, CredentialsError> {
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(CredentialsError::NotFound),
        Err(e) => return Err(e.into()),
    };
    check_metadata(&file.metadata()?, current_euid())?;
    let mut json = String::new();
    file.read_to_string(&mut json)?;
    parse_credentials_json(&json)
}

/// Runs the owner and mode checks without reading the file (for
/// `ocut doctor`). Returns the permission bits.
pub fn check_credentials_file(path: &Path) -> Result<u32, CredentialsError> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(CredentialsError::NotFound),
        Err(e) => return Err(e.into()),
    };
    let meta = file.metadata()?;
    check_metadata(&meta, current_euid())?;
    Ok(meta.mode() & 0o7777)
}

fn current_euid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

fn check_metadata(meta: &Metadata, euid: u32) -> Result<(), CredentialsError> {
    if !meta.is_file() {
        return Err(CredentialsError::Malformed("not a regular file".into()));
    }
    if meta.uid() != euid {
        return Err(CredentialsError::WrongOwner);
    }
    let mode = meta.mode() & 0o7777;
    if mode & !0o600 != 0 {
        return Err(CredentialsError::InsecureMode { mode });
    }
    Ok(())
}

#[derive(Deserialize)]
struct CredentialsFile {
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: Option<RawOAuth>,
}

/// Only the fields OpenCUT needs; `refreshToken`, `scopes` and everything
/// else are skipped by serde without being stored.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawOAuth {
    access_token: Option<String>,
    expires_at: Option<serde_json::Value>,
    subscription_type: Option<String>,
    rate_limit_tier: Option<String>,
}

/// Extracts the `claudeAiOauth` subtree; ignores every other key.
pub fn parse_credentials_json(json: &str) -> Result<OAuthCredentials, CredentialsError> {
    let file: CredentialsFile = serde_json::from_str(json).map_err(|e| {
        // serde_json messages can quote values; report only the position.
        CredentialsError::Malformed(format!(
            "invalid JSON at line {} column {}",
            e.line(),
            e.column()
        ))
    })?;
    let raw = file.claude_ai_oauth.ok_or(CredentialsError::MissingOAuth)?;
    let token = raw
        .access_token
        .filter(|t| !t.trim().is_empty())
        .ok_or(CredentialsError::MissingOAuth)?;
    let expires_at = match raw.expires_at {
        None | Some(serde_json::Value::Null) => None,
        Some(v) => Some(
            v.as_i64()
                .or_else(|| v.as_f64().map(|f| f as i64))
                .and_then(DateTime::from_timestamp_millis)
                .ok_or_else(|| CredentialsError::Malformed("expiresAt is not epoch ms".into()))?,
        ),
    };
    Ok(OAuthCredentials {
        access_token: AccessToken::new(token),
        expires_at,
        subscription_type: raw.subscription_type,
        rate_limit_tier: raw.rate_limit_tier,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_redacted() {
        let t = AccessToken::new("sk-ant-oat01-CANARY".into());
        assert!(!format!("{t:?}").contains("CANARY"));
        assert!(!format!("{t}").contains("CANARY"));
        assert_eq!(t.expose(), "sk-ant-oat01-CANARY");
    }

    #[test]
    fn path() {
        assert_eq!(
            default_credentials_path(Path::new("/h"), None),
            PathBuf::from("/h/.claude/.credentials.json")
        );
        assert_eq!(
            default_credentials_path(Path::new("/h"), Some(Path::new("/c"))),
            PathBuf::from("/c/.credentials.json")
        );
    }

    const CANARY: &str = "sk-ant-oat01-CANARY-c4n4ry-never-print-me";

    fn creds_json(extra: &str) -> String {
        format!(
            r#"{{"claudeAiOauth":{{"accessToken":"{CANARY}","refreshToken":"sk-ant-ort01-REFRESH-CANARY",
               "expiresAt":1760000000000,"scopes":["user:inference"],"subscriptionType":"max",
               "rateLimitTier":"default_claude_max_20x"{extra}}},
               "mcpOAuth":{{"srv":{{"accessToken":"MCP-CANARY"}}}}}}"#
        )
    }

    #[test]
    fn parses_subtree_only() {
        let c = parse_credentials_json(&creds_json(r#","futureField":{"x":1}"#)).unwrap();
        assert_eq!(c.access_token.expose(), CANARY);
        assert_eq!(c.expires_at.unwrap().timestamp_millis(), 1_760_000_000_000);
        assert_eq!(c.plan(), Plan::Max20x);
        let dbg = format!("{c:?}");
        for secret in ["CANARY", "REFRESH", "MCP"] {
            assert!(!dbg.contains(secret), "{secret} leaked into Debug");
        }
    }

    #[test]
    fn expiry() {
        let c = parse_credentials_json(&creds_json("")).unwrap();
        let at = c.expires_at.unwrap();
        assert!(!c.is_expired(at - chrono::Duration::seconds(1)));
        assert!(c.is_expired(at));
    }

    #[test]
    fn missing_and_malformed() {
        assert!(matches!(
            parse_credentials_json(r#"{"mcpOAuth":{}}"#),
            Err(CredentialsError::MissingOAuth)
        ));
        assert!(matches!(
            parse_credentials_json(r#"{"claudeAiOauth":{"accessToken":""}}"#),
            Err(CredentialsError::MissingOAuth)
        ));
        // A type error must not echo the offending value.
        let e = parse_credentials_json(&format!(
            r#"{{"claudeAiOauth":{{"accessToken":["{CANARY}"]}}}}"#
        ))
        .unwrap_err();
        assert!(matches!(e, CredentialsError::Malformed(_)));
        assert!(!format!("{e} {e:?}").contains("CANARY"));
        let e = parse_credentials_json(&format!(
            r#"{{"claudeAiOauth":{{"accessToken":"{CANARY}","expiresAt":"soon"}}}}"#
        ))
        .unwrap_err();
        assert!(!format!("{e} {e:?}").contains("CANARY"));
    }

    #[test]
    fn plan_mapping() {
        let mk = |s: Option<&str>, t: Option<&str>| OAuthCredentials {
            access_token: AccessToken::new("x".into()),
            expires_at: None,
            subscription_type: s.map(Into::into),
            rate_limit_tier: t.map(Into::into),
        };
        assert_eq!(mk(Some("pro"), None).plan(), Plan::Pro);
        assert_eq!(
            mk(Some("max"), Some("default_claude_max_5x")).plan(),
            Plan::Max5x
        );
        assert_eq!(mk(Some("max"), None).plan(), Plan::Max5x);
        assert_eq!(mk(Some("team"), None).plan(), Plan::Team);
        assert_eq!(mk(None, None).plan(), Plan::Unknown);
    }

    fn write_file(mode: u32) -> (tempfile::TempDir, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".credentials.json");
        std::fs::write(&path, creds_json("")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        (dir, path)
    }

    #[test]
    fn reads_0600_and_0400() {
        for mode in [0o600, 0o400] {
            let (_d, path) = write_file(mode);
            let c = read_credentials(&path).unwrap();
            assert_eq!(c.access_token.expose(), CANARY);
        }
    }

    #[test]
    fn refuses_wide_mode() {
        for mode in [0o644, 0o640, 0o604, 0o700, 0o4600] {
            let (_d, path) = write_file(mode);
            match read_credentials(&path) {
                Err(CredentialsError::InsecureMode { mode: m }) => assert_eq!(m, mode),
                other => panic!("mode {mode:o}: {other:?}"),
            }
        }
    }

    #[test]
    fn refuses_wrong_owner() {
        let (_d, path) = write_file(0o600);
        let meta = std::fs::metadata(&path).unwrap();
        assert!(matches!(
            check_metadata(&meta, meta.uid().wrapping_add(1)),
            Err(CredentialsError::WrongOwner)
        ));
        assert!(check_metadata(&meta, meta.uid()).is_ok());
    }

    #[test]
    fn not_found() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            read_credentials(&dir.path().join("nope.json")),
            Err(CredentialsError::NotFound)
        ));
    }
}
