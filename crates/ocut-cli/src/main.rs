//! `ocut`: OpenCUT command-line interface.

mod consent;
mod doctor;
mod format;
mod install;

use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;

use chrono::{DateTime, Duration, Utc};
use clap::{Parser, Subcommand};
use ocut_core::{Config, Snapshot};
use ocut_sources::collect::Collector;
use ocut_sources::paths::{Paths, load_config};
use ocut_sources::statusline::{StatuslineCache, parse_hook_input, render_line, write_cache};

#[derive(Parser)]
#[command(
    name = "ocut",
    version,
    about = "OpenCUT: Claude plan usage (unofficial)",
    after_help = format!("Format keys: {}", format::KEYS)
)]
struct Cli {
    /// Print the full snapshot as JSON (schema opencut/usage@1).
    #[arg(long, conflicts_with = "waybar")]
    json: bool,
    /// Print a Waybar custom-module JSON line (text, tooltip, class).
    #[arg(long)]
    waybar: bool,
    /// Output template, e.g. '{5h.pct:.0}% ↻{5h.reset_in}'.
    #[arg(long, value_name = "TEMPLATE")]
    format: Option<String>,
    /// Compute the snapshot in this process from local sources (never calls the API).
    #[arg(long)]
    no_daemon: bool,
    /// Config file (default ~/.config/opencut/config.toml).
    #[arg(long, value_name = "PATH", global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Check file modes, source health and the Claude Code version.
    Doctor,
    /// Hooks called by Claude Code.
    Hook {
        #[command(subcommand)]
        hook: HookCmd,
    },
    /// Print (and optionally apply) integration snippets.
    Install {
        #[command(subcommand)]
        what: InstallCmd,
    },
}

#[derive(Subcommand)]
enum HookCmd {
    /// Read statusline JSON on stdin, cache rate_limits, print one ANSI line.
    Statusline,
}

#[derive(Subcommand)]
enum InstallCmd {
    /// Show the statusLine entry for ~/.claude/settings.json and a diff.
    Statusline {
        /// Write it (after a backup). Refuses if a statusLine exists.
        #[arg(long)]
        apply: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let Some(mut paths) = Paths::from_process() else {
        eprintln!("ocut: $HOME is not set");
        return ExitCode::from(2);
    };
    if let Some(c) = &cli.config {
        paths.config_file = c.clone();
    }
    let (cfg, config_err) = match load_config(&paths.config_file) {
        Ok(c) => (c, None),
        Err(e) => (Config::default(), Some(e)),
    };
    let paths = paths.with_config(&cfg);

    match cli.cmd {
        Some(Cmd::Hook {
            hook: HookCmd::Statusline,
        }) => hook_statusline(&paths, &cfg),
        Some(Cmd::Doctor) => {
            if doctor::run(&paths, &cfg, config_err.as_deref()) == 0 {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        Some(Cmd::Install {
            what: InstallCmd::Statusline { apply },
        }) => install_statusline(&paths, apply),
        None => {
            if let Some(e) = &config_err {
                eprintln!("ocut: {e} (using defaults)");
            }
            show(&cli, &paths, cfg)
        }
    }
}

/// `ocut hook statusline`. Always exits 0 so Claude Code keeps a status line.
fn hook_statusline(paths: &Paths, cfg: &Config) -> ExitCode {
    let now = Utc::now();
    let mut input = String::new();
    let _ = std::io::stdin().take(1 << 20).read_to_string(&mut input);
    let cache = match parse_hook_input(&input, now) {
        Ok(c) => {
            if cfg.sources.statusline
                && let Err(e) = write_cache(&paths.statusline_cache, &c)
            {
                eprintln!("ocut: {e}");
            }
            c
        }
        Err(_) => StatuslineCache {
            received_at: now,
            ..Default::default()
        },
    };
    println!("{}", render_line(&cache, now));
    ExitCode::SUCCESS
}

fn install_statusline(paths: &Paths, apply: bool) -> ExitCode {
    let shown = paths
        .claude_settings
        .display()
        .to_string()
        .replace(&paths.home.display().to_string(), "~");
    let ocut = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .unwrap_or_else(|_| PathBuf::from("ocut"));
    let current = match std::fs::read_to_string(&paths.claude_settings) {
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            eprintln!("ocut: cannot read {shown}: {e}");
            return ExitCode::from(1);
        }
    };
    let plan = match install::plan(current.as_deref(), &ocut) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("ocut: {e}");
            return ExitCode::from(1);
        }
    };
    let new = match plan {
        install::Plan::AlreadyInstalled => {
            println!("The OpenCUT statusLine is already installed in {shown}.");
            return ExitCode::SUCCESS;
        }
        install::Plan::Conflict => {
            println!(
                "{shown} already has a statusLine. OpenCUT never overwrites it.\n\
                 To combine them, call `{} hook statusline` from your own script \
                 with the same stdin.",
                ocut.display()
            );
            return if apply {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            };
        }
        install::Plan::Add(m) => m,
    };
    let snippet = serde_json::to_string_pretty(&install::snippet(&ocut)).expect("serialises");
    println!("Add this to {shown}:\n\n\"statusLine\": {snippet}\n");
    println!("{}", install::diff(&shown, &ocut));
    if !apply {
        println!(
            "Nothing was written. Run `ocut install statusline --apply` to apply it (a backup is made first)."
        );
        return ExitCode::SUCCESS;
    }
    match install::apply(&paths.claude_settings, &new) {
        Ok(backup) => {
            if let Some(b) = backup {
                println!(
                    "Backup: {}",
                    b.display()
                        .to_string()
                        .replace(&paths.home.display().to_string(), "~")
                );
            }
            println!("Wrote {shown}. Restart Claude Code to use it.");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("ocut: cannot write {shown}: {e}");
            ExitCode::from(1)
        }
    }
}

/// A daemon snapshot is used while younger than this.
fn max_cached_age(cfg: &Config) -> Duration {
    Duration::seconds((cfg.polling.snapshot_interval_s * 3).max(90) as i64)
}

fn read_daemon_snapshot(paths: &Paths, cfg: &Config, now: DateTime<Utc>) -> Option<Snapshot> {
    let bytes = std::fs::read(&paths.snapshot).ok()?;
    let s: Snapshot = serde_json::from_slice(&bytes).ok()?;
    // A snapshot from the future (clock change, corrupt file) is not fresh.
    let age = now - s.generated_at;
    (s.schema == ocut_core::snapshot::SCHEMA_ID
        && age >= -Duration::seconds(5)
        && age <= max_cached_age(cfg))
    .then_some(s)
}

/// In-process snapshot from local sources only. The OAuth endpoint is polled
/// by `ocutd` alone, so status bars polling `ocut` never add API requests.
fn in_process(paths: &Paths, cfg: Config, now: DateTime<Utc>) -> Snapshot {
    let mut c = Collector::new(paths.clone(), cfg, false);
    c.scan_logs(now);
    c.snapshot(now)
}

fn show(cli: &Cli, paths: &Paths, mut cfg: Config) -> ExitCode {
    consent::maybe_ask(&mut cfg, &paths.config_file);
    let now = Utc::now();
    let template = cli
        .format
        .clone()
        .unwrap_or_else(|| cfg.display.format.clone());
    let snap = match (!cli.no_daemon)
        .then(|| read_daemon_snapshot(paths, &cfg, now))
        .flatten()
    {
        Some(s) => s,
        None => in_process(paths, cfg, now),
    };
    let out = if cli.json {
        Ok(serde_json::to_string_pretty(&snap).expect("snapshot serialises"))
    } else if cli.waybar {
        format::waybar(&snap, &template, now)
    } else {
        format::render(&template, &snap, now)
    };
    match out {
        Ok(s) => {
            println!("{s}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("ocut: {e}");
            ExitCode::from(2)
        }
    }
}
