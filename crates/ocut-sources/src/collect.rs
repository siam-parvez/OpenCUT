//! Gathers every source into a [`Snapshot`]. Used by `ocut --no-daemon`
//! (one shot) and by `ocutd` (kept alive, scanning incrementally).

use chrono::{DateTime, Duration, Local, NaiveTime, Utc};
use ocut_core::burn::{DEFAULT_LOOKBACK, PctSample};
use ocut_core::snapshot::{ErrorEntry, SourcesHealth, WindowSource};
use ocut_core::{AssembleInputs, Config, Plan, PlanPreset, PricingTable, Snapshot, assemble};

use crate::credentials::{CredentialsError, read_credentials};
use crate::jsonl::{JsonlScanner, Offsets};
use crate::oauth::{
    OAuthClient, OAuthError, OAuthSource, backoff_delay, next_poll_interval, random_jitter,
};
use crate::paths::Paths;
use crate::sessions::count_active_sessions;
use crate::statusline::read_cache;
use crate::{LimitReadings, Source, UsageEvent};

/// Events older than this are dropped from memory (the `week` period needs 7 days).
const KEEP_EVENTS: Duration = Duration::days(8);

/// Why an OAuth poll produced no readings. Messages never contain the token.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error(transparent)]
    Credentials(#[from] CredentialsError),
    #[error(transparent)]
    OAuth(#[from] OAuthError),
}

impl FetchError {
    pub fn code(&self) -> &'static str {
        match self {
            FetchError::Credentials(CredentialsError::NotFound) => "no_credentials",
            FetchError::Credentials(_) => "credentials",
            FetchError::OAuth(e) => e.code(),
        }
    }
}

/// Result of one OAuth poll: readings plus the plan from the credentials.
pub type FetchResult = Result<(LimitReadings, Plan), FetchError>;

/// Reads the credentials and calls the usage endpoint once. Blocking; the
/// token is dropped when this returns and never refreshed.
pub fn fetch_oauth(paths: &Paths, now: DateTime<Utc>) -> FetchResult {
    let creds = read_credentials(&paths.credentials)?;
    let plan = creds.plan();
    let mut src = OAuthSource::new(OAuthClient::new(), &creds);
    Ok((src.poll(now)?, plan))
}

/// Start of the local calendar day containing `now`, in UTC.
pub fn local_day_start(now: DateTime<Utc>) -> DateTime<Utc> {
    let local = now.with_timezone(&Local);
    local
        .date_naive()
        .and_time(NaiveTime::MIN)
        .and_local_timezone(Local)
        .earliest()
        .map_or(now - Duration::hours(24), |t| t.with_timezone(&Utc))
}

#[derive(Debug, Default, Clone)]
struct OAuthState {
    readings: Option<LimitReadings>,
    plan: Option<Plan>,
    last_success: Option<DateTime<Utc>>,
    next_poll_at: Option<DateTime<Utc>>,
    failures: u32,
    backoff_s: u64,
    error: Option<ErrorEntry>,
}

pub struct Collector {
    pub paths: Paths,
    pub config: Config,
    /// Whether this process may call the OAuth endpoint at all.
    pub allow_oauth: bool,
    pricing: PricingTable,
    scanner: JsonlScanner,
    events: Vec<UsageEvent>,
    parse_errors: u64,
    files: u64,
    logs_error: Option<ErrorEntry>,
    last_activity: Option<DateTime<Utc>>,
    oauth: OAuthState,
    history: Vec<PctSample>,
    calibrated: Option<PlanPreset>,
}

impl Collector {
    /// Starts with empty offsets, so the first scan reads every log once
    /// (totals for today and the week need the full history).
    pub fn new(paths: Paths, config: Config, allow_oauth: bool) -> Self {
        let pricing = config
            .pricing
            .file
            .as_ref()
            .and_then(|f| std::fs::read_to_string(f).ok())
            .and_then(|s| PricingTable::from_json(&s).ok())
            .unwrap_or_else(PricingTable::builtin);
        let scanner = JsonlScanner::new(paths.project_dirs.clone(), Offsets::default());
        Self {
            paths,
            config,
            allow_oauth,
            pricing,
            scanner,
            events: Vec::new(),
            parse_errors: 0,
            files: 0,
            logs_error: None,
            last_activity: None,
            oauth: OAuthState::default(),
            history: Vec::new(),
            calibrated: None,
        }
    }

    fn oauth_enabled(&self) -> bool {
        self.allow_oauth && self.config.sources.oauth_api.is_enabled()
    }

    /// Reads new JSONL bytes and drops events older than 8 days.
    pub fn scan_logs(&mut self, now: DateTime<Utc>) {
        if !self.config.sources.local_logs {
            return;
        }
        match self.scanner.scan() {
            Ok(r) => {
                self.files = r.files;
                // Unreadable files are skipped by the scanner; count them as
                // parse errors so `local_logs` shows that something was missed.
                self.parse_errors += r.parse_errors + r.unreadable;
                self.last_activity = self.last_activity.max(r.last_activity);
                self.events.extend(r.events);
                self.logs_error = None;
            }
            Err(e) => {
                self.logs_error = Some(ErrorEntry {
                    code: "local_logs".into(),
                    message: e.to_string(),
                    at: now,
                })
            }
        }
        self.events.retain(|e| now - e.timestamp <= KEEP_EVENTS);
    }

    /// Number of events kept in memory.
    pub fn event_count(&self) -> usize {
        self.events.len()
    }

    /// Time of the newest JSONL event seen.
    pub fn last_activity(&self) -> Option<DateTime<Utc>> {
        self.last_activity
    }

    /// True when OAuth is enabled and its next poll time has come.
    pub fn oauth_due(&self, now: DateTime<Utc>) -> bool {
        self.oauth_enabled() && self.oauth.next_poll_at.is_none_or(|t| now >= t)
    }

    pub fn next_oauth_poll(&self) -> Option<DateTime<Utc>> {
        self.oauth.next_poll_at
    }

    /// Records a poll result and schedules the next poll (never under 60 s;
    /// exponential backoff with jitter on errors, honouring Retry-After).
    pub fn apply_oauth(&mut self, result: FetchResult, now: DateTime<Utc>) {
        let s = &mut self.oauth;
        let delay = match result {
            Ok((readings, plan)) => {
                s.readings = Some(readings);
                s.plan = Some(plan);
                s.last_success = Some(now);
                s.failures = 0;
                s.error = None;
                next_poll_interval(&self.config.polling, self.last_activity, now)
            }
            Err(e) => {
                let retry_after = match &e {
                    FetchError::OAuth(OAuthError::RateLimited { retry_after })
                    | FetchError::OAuth(OAuthError::Server { retry_after, .. }) => *retry_after,
                    _ => None,
                };
                let delay = match e {
                    // Waiting longer won't help; the user must run `claude`.
                    FetchError::OAuth(OAuthError::TokenExpired) => {
                        s.failures = 0;
                        next_poll_interval(&self.config.polling, self.last_activity, now)
                    }
                    _ => {
                        s.failures += 1;
                        backoff_delay(s.failures, retry_after, random_jitter())
                    }
                };
                s.error = Some(ErrorEntry {
                    code: e.code().into(),
                    message: e.to_string(),
                    at: now,
                });
                delay
            }
        };
        s.backoff_s = if s.failures > 0 { delay.as_secs() } else { 0 };
        s.next_poll_at =
            Some(now + Duration::from_std(delay).unwrap_or_else(|_| Duration::minutes(10)));
    }

    /// Builds the snapshot from what has been collected so far, plus the
    /// statusline cache and session count read now.
    pub fn snapshot(&mut self, now: DateTime<Utc>) -> Snapshot {
        let mut sources = SourcesHealth::default();
        let mut errors: Vec<ErrorEntry> = Vec::new();

        sources.local_logs.ok = self.config.sources.local_logs && self.logs_error.is_none();
        sources.local_logs.files = self.files;
        sources.local_logs.parse_errors = self.parse_errors;
        errors.extend(self.logs_error.clone());

        let statusline = if self.config.sources.statusline {
            match read_cache(&self.paths.statusline_cache) {
                Ok(c) => c,
                Err(e) => {
                    errors.push(ErrorEntry {
                        code: "statusline".into(),
                        message: e.to_string(),
                        at: now,
                    });
                    None
                }
            }
        } else {
            None
        };
        if let Some(c) = &statusline {
            sources.statusline.ok = !c.is_stale(now);
            sources.statusline.last_push = Some(c.received_at);
        }

        let oauth_on = self.oauth_enabled();
        let o = &self.oauth;
        sources.oauth_api.enabled = oauth_on;
        sources.oauth_api.ok = oauth_on && o.error.is_none() && o.last_success.is_some();
        sources.oauth_api.last_success = o.last_success;
        sources.oauth_api.next_poll_at = o.next_poll_at.filter(|_| oauth_on);
        sources.oauth_api.backoff_s = o.backoff_s;
        if oauth_on {
            errors.extend(o.error.clone());
        }

        let active_sessions = count_active_sessions(&self.paths.sessions_dir, now).unwrap_or(0);
        let plan = o
            .plan
            .filter(|p| *p != Plan::Unknown)
            .unwrap_or(self.config.sources.local_plan);
        let preset = self.calibrated.filter(|c| c.plan == plan).or_else(|| {
            plan.preset()
                .or_else(|| self.config.sources.local_plan.preset())
        });

        let snap = assemble(AssembleInputs {
            now,
            today_start: local_day_start(now),
            events: &self.events,
            pricing: &self.pricing,
            preset,
            plan,
            oauth: o.readings.as_ref().filter(|_| oauth_on),
            statusline: statusline.as_ref().map(|c| &c.readings),
            pct_history: &self.history,
            active_sessions,
            sources: &sources,
            errors: &errors,
        });

        // Keep the merged 5h pct for projections, and calibrate the local
        // estimate from real readings.
        if let Some(w) = &snap.windows.session_5h
            && w.source != WindowSource::LocalEstimate
        {
            self.history.push(PctSample {
                at: now,
                pct: w.pct,
            });
            if let Some(c) = preset
                .map(|p| p.calibrate(w.pct, snap.cost.session_5h_usd))
                .filter(|c| c.calibrated)
            {
                self.calibrated = Some(c);
            }
        }
        self.history.retain(|s| now - s.at <= DEFAULT_LOOKBACK);
        snap
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Env;
    use chrono::TimeZone;
    use std::path::PathBuf;

    fn fixture_paths(home: &std::path::Path) -> Paths {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures");
        let mut p = Paths::resolve(&Env {
            home: Some(home.to_path_buf()),
            ..Default::default()
        })
        .unwrap();
        p.project_dirs = vec![root.join("jsonl/projects")];
        p.sessions_dir = home.join("no-sessions");
        p
    }

    #[test]
    fn fixture_snapshot_without_oauth() {
        let tmp = tempfile::tempdir().unwrap();
        let mut c = Collector::new(fixture_paths(tmp.path()), Config::default(), false);
        let now = Utc.with_ymd_and_hms(2026, 10, 11, 12, 0, 0).unwrap();
        c.scan_logs(now);
        assert!(!c.oauth_due(now));
        let s = c.snapshot(now);
        assert!(s.sources.local_logs.ok);
        assert!(s.sources.local_logs.files >= 3);
        assert!(!s.sources.oauth_api.enabled);
        assert!(s.errors.is_empty());
    }

    #[test]
    fn oauth_backoff_and_expiry() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.sources.oauth_api = ocut_core::OAuthConsent::On;
        let mut c = Collector::new(fixture_paths(tmp.path()), cfg, true);
        let now = Utc.with_ymd_and_hms(2026, 10, 11, 12, 0, 0).unwrap();
        assert!(c.oauth_due(now));

        c.apply_oauth(
            Err(OAuthError::RateLimited {
                retry_after: Some(std::time::Duration::from_secs(300)),
            }
            .into()),
            now,
        );
        assert!(c.next_oauth_poll().unwrap() >= now + Duration::seconds(300));
        let s = c.snapshot(now);
        assert_eq!(s.errors[0].code, "rate_limited");
        assert!(s.sources.oauth_api.backoff_s >= 300);

        c.apply_oauth(Err(OAuthError::TokenExpired.into()), now);
        let s = c.snapshot(now);
        assert_eq!(s.status, ocut_core::Status::TokenExpired);
        assert_eq!(s.sources.oauth_api.backoff_s, 0);
        assert!(c.next_oauth_poll().unwrap() >= now + Duration::seconds(60));

        c.apply_oauth(Ok((LimitReadings::default(), Plan::Pro)), now);
        let s = c.snapshot(now);
        assert!(s.sources.oauth_api.ok);
        assert!(s.errors.is_empty());
    }

    #[test]
    fn day_start_is_before_now() {
        let now = Utc::now();
        let d = local_day_start(now);
        assert!(d <= now && now - d <= Duration::hours(25));
    }
}
