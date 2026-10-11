//! `ocutd`: OpenCUT daemon. Scans Claude Code logs every 30 s, polls the
//! OAuth usage endpoint only when enabled (120 s default, 60 s while
//! active, 600 s when idle, never under 60 s, backoff on errors), writes
//! the snapshot atomically and sends threshold notifications.

mod notify;

use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use chrono::Utc;
use ocut_core::Config;
use ocut_sources::collect::{Collector, FetchResult, fetch_oauth};
use ocut_sources::fsutil::{create_private_dir, write_private_atomic};
use ocut_sources::paths::{Paths, load_config};
use tokio::signal::unix::{SignalKind, signal};
use tokio::task::JoinHandle;

use notify::NotifyState;

const HELP: &str = "\
ocutd: OpenCUT daemon (writes $XDG_RUNTIME_DIR/opencut/usage.json)

Usage: ocutd [--once] [--config PATH]

  --once         collect once, write the snapshot and exit (no notifications)
  --config PATH  config file (default ~/.config/opencut/config.toml)
  -V, --version  print the version
  -h, --help     print this help";

struct Args {
    once: bool,
    config: Option<std::path::PathBuf>,
}

fn parse_args() -> Result<Args, ExitCode> {
    let mut a = Args {
        once: false,
        config: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--once" => a.once = true,
            "--config" => a.config = it.next().map(Into::into),
            "-V" | "--version" => {
                println!("ocutd {}", env!("CARGO_PKG_VERSION"));
                return Err(ExitCode::SUCCESS);
            }
            "-h" | "--help" => {
                println!("{HELP}");
                return Err(ExitCode::SUCCESS);
            }
            other => {
                eprintln!("ocutd: unknown argument `{other}`\n\n{HELP}");
                return Err(ExitCode::from(2));
            }
        }
    }
    Ok(a)
}

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

fn load(path: &Path) -> Config {
    load_config(path).unwrap_or_else(|e| {
        eprintln!("ocutd: {e}; using defaults");
        Config::default()
    })
}

fn write_snapshot(c: &mut Collector) -> ocut_core::Snapshot {
    let snap = c.snapshot(Utc::now());
    let body = serde_json::to_vec_pretty(&snap).expect("snapshot serialises");
    if let Err(e) = write_private_atomic(&c.paths.snapshot, &body) {
        eprintln!("ocutd: cannot write snapshot: {e}");
    }
    snap
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(code) => return code,
    };
    let Some(mut paths) = Paths::from_process() else {
        eprintln!("ocutd: $HOME is not set");
        return ExitCode::from(2);
    };
    if let Some(c) = args.config {
        paths.config_file = c;
    }
    let cfg = load(&paths.config_file);
    let interval = Duration::from_secs(cfg.polling.snapshot_interval_s);
    let notify_cfg_path = paths.notify_state.clone();
    let mut config_mtime = mtime(&paths.config_file);
    let mut c = Collector::new(paths.with_config(&cfg), cfg, true);
    // OpenCUT's own directories are private even if something created them
    // earlier with a wider mode.
    for dir in [c.paths.snapshot.parent(), Some(c.paths.cache_dir.as_path())]
        .into_iter()
        .flatten()
    {
        if let Err(e) = create_private_dir(dir) {
            eprintln!("ocutd: cannot create {}: {e}", dir.display());
        }
    }

    if args.once {
        let now = Utc::now();
        c.scan_logs(now);
        if c.oauth_due(now) {
            let r = tokio::task::spawn_blocking({
                let p = c.paths.clone();
                move || fetch_oauth(&p, now)
            })
            .await
            .expect("oauth task");
            c.apply_oauth(r, now);
        }
        let s = write_snapshot(&mut c);
        eprintln!(
            "ocutd: wrote {} (status {:?}, {} events)",
            c.paths.snapshot.display(),
            s.status,
            c.event_count()
        );
        return ExitCode::SUCCESS;
    }

    let (Ok(mut sigterm), Ok(mut sighup)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::hangup()),
    ) else {
        eprintln!("ocutd: cannot install signal handlers");
        return ExitCode::from(1);
    };
    let mut notify_state = NotifyState::load(&notify_cfg_path);
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut oauth_task: Option<JoinHandle<FetchResult>> = None;
    eprintln!(
        "ocutd {} started: snapshot {} every {}s, oauth_api {}",
        env!("CARGO_PKG_VERSION"),
        c.paths.snapshot.display(),
        interval.as_secs(),
        if c.config.sources.oauth_api.is_enabled() {
            "on"
        } else {
            "off"
        }
    );

    loop {
        let oauth_wait = c
            .next_oauth_poll()
            .map(|t| (t - Utc::now()).to_std().unwrap_or(Duration::ZERO))
            .unwrap_or(Duration::ZERO);
        let oauth_idle = oauth_task.is_none() && c.config.sources.oauth_api.is_enabled();
        tokio::select! {
            _ = tick.tick() => {}
            _ = tokio::time::sleep(oauth_wait), if oauth_idle => {}
            r = async { oauth_task.as_mut().expect("guarded").await }, if oauth_task.is_some() => {
                oauth_task = None;
                match r {
                    Ok(r) => {
                        if let Err(e) = &r {
                            eprintln!("ocutd: oauth_api: {e}");
                        }
                        c.apply_oauth(r, Utc::now());
                    }
                    Err(e) => eprintln!("ocutd: oauth task failed: {e}"),
                }
            }
            _ = sigterm.recv() => break,
            _ = tokio::signal::ctrl_c() => break,
            _ = sighup.recv() => config_mtime = None,
        }

        // Pick up consent or setting changes without a restart.
        let m = mtime(&c.paths.config_file);
        if m != config_mtime {
            config_mtime = m;
            c.config = load(&c.paths.config_file);
        }

        let now = Utc::now();
        c.scan_logs(now);
        if oauth_task.is_none() && c.oauth_due(now) {
            let p = c.paths.clone();
            oauth_task = Some(tokio::task::spawn_blocking(move || fetch_oauth(&p, now)));
        }
        let snap = write_snapshot(&mut c);

        let notices = notify::evaluate(&mut notify_state, &snap, &c.config.notify, now);
        if let Err(e) = notify_state.save(&notify_cfg_path) {
            eprintln!("ocutd: cannot save notify state: {e}");
        }
        for n in notices {
            tokio::task::spawn_blocking(move || {
                if let Err(e) = notify::send(&n) {
                    eprintln!("ocutd: notification failed: {e}");
                }
            });
        }
    }
    eprintln!("ocutd: stopping");
    ExitCode::SUCCESS
}
