//! `ocut-tray`: StatusNotifierItem tray icon for OpenCUT.
//!
//! Reads only the snapshot written by `ocutd` (no network, no credentials),
//! watches it with inotify (via `notify`) and re-reads it every 30 s anyway.

mod icon;
mod model;

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::Duration;

use chrono::Utc;
use ksni::blocking::TrayMethods;
use ksni::menu::StandardItem;
use ksni::{MenuItem, ToolTip};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};

use crate::model::{Load, View};

/// Fallback poll interval (also refreshes countdowns and staleness).
const POLL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Msg {
    Reload,
    Quit,
}

struct OcutTray {
    view: View,
    icons: Vec<ksni::Icon>,
    tx: Sender<Msg>,
}

impl OcutTray {
    fn set_view(&mut self, view: View) {
        if view.pct != self.view.pct || view.level != self.view.level || self.icons.is_empty() {
            self.icons = icon::render_all(view.pct, view.level)
                .into_iter()
                .map(|p| ksni::Icon {
                    width: p.width as i32,
                    height: p.height as i32,
                    data: p.data,
                })
                .collect();
        }
        self.view = view;
    }
}

fn info(label: &str) -> MenuItem<OcutTray> {
    StandardItem {
        label: label.replace('_', "__"),
        enabled: false,
        ..Default::default()
    }
    .into()
}

impl ksni::Tray for OcutTray {
    const MENU_ON_ACTIVATE: bool = true;

    fn id(&self) -> String {
        "opencut".into()
    }

    fn title(&self) -> String {
        self.view.summary.clone()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        self.icons.clone()
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: self.view.summary.clone(),
            description: self.view.details.clone(),
            ..Default::default()
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut items: Vec<MenuItem<Self>> = self.view.lines.iter().map(|l| info(l)).collect();
        items.push(MenuItem::Separator);
        items.push(
            StandardItem {
                label: "Refresh".into(),
                icon_name: "view-refresh".into(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.tx.send(Msg::Reload);
                }),
                ..Default::default()
            }
            .into(),
        );
        if self.view.offer_start {
            items.push(
                StandardItem {
                    label: "Start ocutd".into(),
                    icon_name: "media-playback-start".into(),
                    activate: Box::new(|t: &mut Self| start_daemon(t.tx.clone())),
                    ..Default::default()
                }
                .into(),
            );
        }
        items.push(
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.tx.send(Msg::Quit);
                }),
                ..Default::default()
            }
            .into(),
        );
        items
    }

    fn watcher_offline(&self, reason: ksni::OfflineReason) -> bool {
        eprintln!("ocut-tray: tray host offline ({reason:?}); waiting for it to return");
        true
    }
}

/// `systemctl --user start opencut`, off the D-Bus thread; re-reads after.
fn start_daemon(tx: Sender<Msg>) {
    std::thread::spawn(move || {
        let status = Command::new("systemctl")
            .args(["--user", "start", "opencut"])
            .stdin(Stdio::null())
            .status();
        match status {
            Ok(s) if s.success() => {}
            Ok(s) => eprintln!("ocut-tray: systemctl --user start opencut: {s}"),
            Err(e) => eprintln!("ocut-tray: cannot run systemctl: {e}"),
        }
        // The first snapshot takes a moment; the watcher usually catches it.
        std::thread::sleep(Duration::from_secs(2));
        let _ = tx.send(Msg::Reload);
    });
}

/// Watches the snapshot's directory (the file is replaced by rename). If the
/// directory does not exist yet, watches its parent until it appears.
struct SnapshotWatch {
    watcher: Option<RecommendedWatcher>,
    watching: Option<PathBuf>,
}

impl SnapshotWatch {
    fn new(tx: Sender<Msg>) -> Self {
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if res.is_ok() {
                let _ = tx.send(Msg::Reload);
            }
        });
        if let Err(e) = &watcher {
            eprintln!("ocut-tray: file watch unavailable ({e}); polling every 30 s");
        }
        Self {
            watcher: watcher.ok(),
            watching: None,
        }
    }

    fn ensure(&mut self, snapshot: &Path) {
        let Some(w) = self.watcher.as_mut() else {
            return;
        };
        let Some(dir) = snapshot.parent() else {
            return;
        };
        let target = if dir.is_dir() {
            dir
        } else {
            match dir.parent() {
                Some(p) if p.is_dir() => p,
                _ => return,
            }
        };
        if self.watching.as_deref() == Some(target) {
            return;
        }
        if let Some(old) = self.watching.take() {
            let _ = w.unwatch(&old);
        }
        match w.watch(target, RecursiveMode::NonRecursive) {
            Ok(()) => self.watching = Some(target.to_path_buf()),
            Err(e) => eprintln!("ocut-tray: cannot watch {}: {e}", target.display()),
        }
    }
}

const USAGE: &str = "\
ocut-tray: OpenCUT tray icon (StatusNotifierItem)

Usage: ocut-tray [--snapshot PATH]

Reads $XDG_RUNTIME_DIR/opencut/usage.json, written by ocutd.
";

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let mut path_arg: Option<PathBuf> = None;
    while let Some(a) = args.next() {
        match a.to_str() {
            Some("-h" | "--help") => {
                print!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            Some("-V" | "--version") => {
                println!("ocut-tray {}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            Some("--snapshot") => match args.next() {
                Some(p) if !p.is_empty() => path_arg = Some(PathBuf::from(p)),
                _ => {
                    eprint!("ocut-tray: --snapshot needs a PATH\n\n{USAGE}");
                    return ExitCode::from(2);
                }
            },
            _ => {
                eprint!("ocut-tray: unexpected argument {a:?}\n\n{USAGE}");
                return ExitCode::from(2);
            }
        }
    }
    let env = |k| std::env::var_os(k).map(PathBuf::from);
    let Some(path) = path_arg.or_else(|| {
        model::snapshot_path(env("XDG_RUNTIME_DIR"), env("XDG_CACHE_HOME"), env("HOME"))
    }) else {
        eprintln!("ocut-tray: cannot locate the snapshot (no XDG_RUNTIME_DIR or HOME)");
        return ExitCode::from(1);
    };

    let (tx, rx) = mpsc::channel();
    let mut tray = OcutTray {
        view: View::loading(),
        icons: Vec::new(),
        tx: tx.clone(),
    };
    tray.set_view(model::view(&model::load(&path), &path, Utc::now()));
    let handle = match tray.assume_sni_available(true).spawn() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("ocut-tray: cannot register the tray icon: {e}");
            return ExitCode::from(1);
        }
    };

    let mut watch = SnapshotWatch::new(tx);
    watch.ensure(&path);
    loop {
        let mut msg = match rx.recv_timeout(POLL) {
            Ok(m) => m,
            Err(RecvTimeoutError::Timeout) => Msg::Reload,
            Err(RecvTimeoutError::Disconnected) => Msg::Quit,
        };
        // Coalesce bursts of file events into one read.
        std::thread::sleep(Duration::from_millis(100));
        while let Ok(m) = rx.try_recv() {
            if m == Msg::Quit {
                msg = Msg::Quit;
            }
        }
        if msg == Msg::Quit {
            break;
        }
        watch.ensure(&path);
        let load: Load = model::load(&path);
        let view = model::view(&load, &path, Utc::now());
        if handle.update(|t| t.set_view(view)).is_none() {
            break;
        }
    }
    handle.shutdown().wait();
    ExitCode::SUCCESS
}
