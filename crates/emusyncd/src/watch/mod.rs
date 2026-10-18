//! `emusync watch`: sidecar mode for emulators that can't be modified (DESIGN §6.4).

pub mod control;
mod daemon;
mod process;
mod profile;

use crate::{AnyResult, Policy};
use clap::Args;
use daemon::{Daemon, Timing};
use emusync_core::{ConflictPolicy, Engine};
use notify::{EventKind, RecursiveMode, Watcher};
use profile::Profile;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Args, Clone)]
pub struct WatchArgs {
    /// Emulator profile: a built-in id (pcsx2) or a .json profile file. Repeat for several
    /// emulators.
    #[arg(long = "profile", default_value = "pcsx2")]
    profiles: Vec<String>,
    /// What to do when devices diverged.
    #[arg(long, value_enum, default_value_t = Policy::Newest)]
    policy: Policy,
    /// Only sync cards registered with `emusync card add`; don't pick up card files from the
    /// emulator's default folders.
    #[arg(long)]
    no_discover: bool,
    /// Quiet time after the last write before a card is synced.
    #[arg(long, default_value_t = 2000, value_name = "MS")]
    debounce_ms: u64,
    /// Shortest interval between polls of the remote; it doubles while nothing changes.
    #[arg(long, default_value_t = 30, value_name = "SECS")]
    poll_min_secs: u64,
    /// Longest interval between polls of the remote.
    #[arg(long, default_value_t = 900, value_name = "SECS")]
    poll_max_secs: u64,
    /// Read environment variables (KEY=VALUE lines), such as AWS_ACCESS_KEY_ID and
    /// AWS_SECRET_ACCESS_KEY, from this file at startup.
    #[arg(long, value_name = "FILE")]
    pub env_file: Option<PathBuf>,
}

impl WatchArgs {
    /// The options as command-line arguments, defaults left out and paths made absolute, to
    /// start the same watcher from a service definition.
    pub fn to_args(&self) -> AnyResult<Vec<String>> {
        let defaults = WatchArgs::default_values();
        let mut args = Vec::new();
        if self.profiles != defaults.profiles {
            for p in &self.profiles {
                // A built-in id stays as is; a profile file is made absolute.
                let path = Path::new(p);
                let p = if path.is_file() { std::path::absolute(path)?.display().to_string() } else { p.clone() };
                args.extend(["--profile".to_string(), p]);
            }
        }
        if !matches!(self.policy, Policy::Newest) {
            args.extend(["--policy".to_string(), "ask".to_string()]);
        }
        if self.no_discover {
            args.push("--no-discover".to_string());
        }
        for (flag, value, default) in [
            ("--debounce-ms", self.debounce_ms, defaults.debounce_ms),
            ("--poll-min-secs", self.poll_min_secs, defaults.poll_min_secs),
            ("--poll-max-secs", self.poll_max_secs, defaults.poll_max_secs),
        ] {
            if value != default {
                args.extend([flag.to_string(), value.to_string()]);
            }
        }
        if let Some(f) = &self.env_file {
            if !f.is_file() {
                return Err(format!("{} is not a file", f.display()).into());
            }
            args.extend(["--env-file".to_string(), std::path::absolute(f)?.display().to_string()]);
        }
        Ok(args)
    }

    fn default_values() -> Self {
        #[derive(clap::Parser)]
        struct P {
            #[command(flatten)]
            w: WatchArgs,
        }
        <P as clap::Parser>::parse_from(["watch"]).w
    }
}

/// Runs the watcher until Ctrl+C, SIGTERM or `emusync service stop`, then does a final sync
/// unless an emulator is running.
pub fn run(mut engine: Engine, args: &WatchArgs, data_dir: &Path) -> AnyResult<()> {
    let _pid_file = control::PidFile::create(data_dir)?;
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    // A second Ctrl+C doesn't wait for the final sync. Card writes are atomic either way.
    if let Err(e) = ctrlc::set_handler(move || {
        if flag.swap(true, Ordering::SeqCst) {
            std::process::exit(130);
        }
    }) {
        log::debug!("no signal handler ({e}); stop with `emusync service stop`");
    }

    let profiles = args.profiles.iter().map(|p| Profile::load(p)).collect::<Result<Vec<_>, _>>()?;
    engine.set_policy(match args.policy {
        Policy::Ask => ConflictPolicy::Ask,
        Policy::Newest => ConflictPolicy::Newest,
    });
    let timing = Timing {
        debounce: Duration::from_millis(args.debounce_ms),
        poll_min: Duration::from_secs(args.poll_min_secs.max(1)),
        poll_max: Duration::from_secs(args.poll_max_secs.max(args.poll_min_secs).max(1)),
        ..Timing::default()
    };
    for p in &profiles {
        let dirs: Vec<String> = p.card_dirs.iter().map(|d| d.display().to_string()).collect();
        log::info!("watching for {} ({}); card folders: {}", p.name, p.processes.join(", "), dirs.join(", "));
    }
    for loc in daemon::unclaimed(&engine.containers()?, &profiles) {
        log::warn!("{loc} belongs to no watched emulator; it is written only while none of them runs");
    }
    if args.no_discover && engine.containers()?.is_empty() {
        return Err("no cards registered (run `emusync card add <file>`, or drop --no-discover)".into());
    }

    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(tx)?;
    let mut watched = HashSet::new();
    let mut daemon =
        Daemon::new(engine, profiles, process::SystemProbe::new(), timing, !args.no_discover, Instant::now());
    loop {
        if stop.load(Ordering::SeqCst) || control::stop_requested(data_dir) {
            log::info!("stopping");
            if let Some(c) = daemon.shutdown(Instant::now()) {
                log::debug!("{} cycle done{}", c.trigger, if c.failed { " (failed)" } else { "" });
            }
            log::info!("stopped");
            return Ok(());
        }
        if let Some(c) = daemon.tick(Instant::now()) {
            log::debug!("{} cycle done{}", c.trigger, if c.failed { " (failed)" } else { "" });
        }
        for dir in daemon.watch_dirs() {
            if watched.insert(dir.clone()) {
                match watcher.watch(&dir, RecursiveMode::NonRecursive) {
                    Ok(()) => log::debug!("watching {}", dir.display()),
                    Err(e) => log::warn!("can't watch {} ({e}); its cards sync on each poll", dir.display()),
                }
            }
        }
        let wait = daemon.next_wakeup().saturating_duration_since(Instant::now());
        let mut next = rx.recv_timeout(wait.clamp(Duration::from_millis(10), Duration::from_secs(1)));
        loop {
            match next {
                Ok(Ok(event)) => {
                    // Reads (ours included) are not changes; a close after writing is.
                    let write_close = matches!(
                        event.kind,
                        EventKind::Access(notify::event::AccessKind::Close(notify::event::AccessMode::Write))
                    );
                    if write_close || !matches!(event.kind, EventKind::Access(_)) {
                        for path in &event.paths {
                            daemon.on_event(path, Instant::now());
                        }
                    }
                }
                Ok(Err(e)) => log::warn!("file watcher: {e}"),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return Err("the file watcher stopped".into()),
            }
            next = rx.try_recv().map_err(|e| match e {
                mpsc::TryRecvError::Empty => RecvTimeoutError::Timeout,
                mpsc::TryRecvError::Disconnected => RecvTimeoutError::Disconnected,
            });
        }
    }
}
