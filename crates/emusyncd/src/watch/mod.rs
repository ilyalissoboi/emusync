//! `emusync watch`: sidecar mode for emulators that can't be modified (DESIGN §6.4).

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
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

#[derive(Args)]
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
}

pub fn run(mut engine: Engine, args: &WatchArgs) -> AnyResult<()> {
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
        if let Some(c) = daemon.tick(Instant::now()) {
            log::debug!("{} cycle done{}", c.trigger, if c.failed { " (failed)" } else { "" });
        }
        for card in daemon.take_rewatch() {
            if watched.remove(&(card.clone(), true)) {
                // Fails if the old directory is already gone, which is fine.
                let _ = watcher.unwatch(&card);
            }
        }
        for dir in daemon.watch_dirs() {
            if watched.insert((dir.path.clone(), dir.recursive)) {
                let mode = if dir.recursive { RecursiveMode::Recursive } else { RecursiveMode::NonRecursive };
                match watcher.watch(&dir.path, mode) {
                    Ok(()) => log::debug!(
                        "watching {}{}",
                        dir.path.display(),
                        if dir.recursive { " recursively" } else { "" }
                    ),
                    Err(e) => log::warn!("can't watch {} ({e}); its cards sync on each poll", dir.path.display()),
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
                    let moved = matches!(
                        event.kind,
                        EventKind::Create(_)
                            | EventKind::Remove(_)
                            | EventKind::Modify(notify::event::ModifyKind::Name(_))
                    );
                    if moved {
                        for path in &event.paths {
                            daemon.on_moved(path);
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
