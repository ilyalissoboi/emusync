//! `emusync` command-line tool (DESIGN §12): memory card sync through S3, per save for PS2
//! cards (unit mode) or as whole files.
//!
//! S3 credentials come from `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and optionally
//! `AWS_SESSION_TOKEN`; they are never written to the data directory.
//!
//! This is a library only so that two binaries can share it: `emusync`, and `emusyncw`, which
//! has no console window on Windows and is what `emusync service` starts at login.

use clap::{Args, Parser, Subcommand, ValueEnum};
use emusync_core::{
    short_id, split_unit, CardMode, ConflictPolicy, ContainerDesc, Engine, EngineOptions, HistoryEntry, Resolution,
    SyncReport,
};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

mod logging;
mod service;
mod watch;

#[derive(Parser)]
#[command(name = "emusync", version, about = "Sync emulator memory cards through S3")]
struct Cli {
    /// Local state directory [env: EMUSYNC_DATA_DIR] (default: the platform data directory).
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    /// Name shown to other devices (stored on first use; default: host name).
    #[arg(long, global = true)]
    device_name: Option<String>,
    /// More logging (-v info, -vv debug).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Manage the remote repository.
    #[command(subcommand)]
    Remote(RemoteCmd),
    /// Manage memory cards registered on this device.
    #[command(subcommand)]
    Card(CardCmd),
    /// Pull, snapshot local changes, apply remote changes, push.
    Sync(SyncArgs),
    /// List every version of a save (or of a card in whole-file mode), newest first.
    History {
        /// A save (e.g. BASLUS-20062GTA3), or a card in whole-file mode (e.g. Mcd001).
        target: String,
    },
    /// Make an old version current again (creates a new version), then sync.
    Restore {
        /// A save (e.g. BASLUS-20062GTA3), or a card in whole-file mode (e.g. Mcd001).
        target: String,
        version: String,
        /// Only record the restore locally; don't run a sync afterwards.
        #[arg(long)]
        no_sync: bool,
    },
    /// List open conflicts.
    Conflicts,
    /// Resolve an open conflict, then sync.
    Resolve {
        /// A save (e.g. BASLUS-20062GTA3), or a card in whole-file mode (e.g. Mcd001).
        target: String,
        /// Keep this device's version.
        #[arg(long, conflicts_with = "take")]
        keep_local: bool,
        /// Take the given version (an id prefix from `emusync conflicts`).
        #[arg(long)]
        take: Option<String>,
    },
    /// Show this device's identity and remote.
    Status,
    /// Run in the foreground: sync cards as the emulator writes them, and apply changes from
    /// other devices only while the emulator is closed.
    Watch(watch::WatchArgs),
    /// Run `emusync watch` at login, for this user only (no admin rights needed).
    #[command(subcommand)]
    Service(service::ServiceCmd),
}

#[derive(Subcommand)]
enum RemoteCmd {
    /// Register the remote, e.g. s3://my-saves/emusync/v1?region=eu-west-1 or
    /// s3://saves/emusync/v1?endpoint=http://nas.local:9000&path_style=1
    Add { uri: String },
    /// Show the configured remote.
    Show,
}

#[derive(Subcommand)]
enum CardCmd {
    /// Register a memory card file. The logical card name defaults to the file stem, so
    /// Mcd001.ps2 on every device is the same card.
    Add {
        path: PathBuf,
        /// Logical card name (default: file stem, e.g. Mcd001).
        #[arg(long)]
        name: Option<String>,
        /// Platform (default: from the extension, e.g. .ps2 → ps2).
        #[arg(long)]
        platform: Option<String>,
        /// Upload from this file but never write to it.
        #[arg(long)]
        read_only: bool,
    },
    /// List cards known locally or in the repository, with their saves.
    List,
    /// Stop syncing a card file on this device (history is kept).
    Remove { card: String },
    /// Choose how a card syncs, on every device, then sync.
    Mode {
        card: String,
        #[arg(value_enum)]
        mode: ModeArg,
        /// Only record the change locally; don't run a sync afterwards.
        #[arg(long)]
        no_sync: bool,
    },
    /// List the saves in a card file without registering or changing it.
    Inspect { path: PathBuf },
}

#[derive(Clone, Copy, ValueEnum)]
enum ModeArg {
    /// Each save syncs separately; saves of different games never conflict (PS2 default).
    Units,
    /// The card file syncs as one unit: byte-identical copies everywhere.
    WholeFile,
}

#[derive(Args)]
struct SyncArgs {
    /// What to do when devices diverged.
    #[arg(long, value_enum, default_value_t = Policy::Ask)]
    policy: Policy,
}

#[derive(Clone, Copy, ValueEnum)]
enum Policy {
    /// Record the conflict and leave the card alone until `emusync resolve`.
    Ask,
    /// The most recent version wins; the other stays in history.
    Newest,
}

/// Entry point of both binaries.
pub fn main() -> ExitCode {
    let cli = Cli::parse();
    // The watcher is a daemon: its log is its output. Without a terminal (started at login),
    // nobody reads stderr, so it goes to a file instead.
    let watching = matches!(cli.cmd, Cmd::Watch(_));
    let verbose = cli.verbose + u8::from(watching);
    let mut to_file = false;
    if watching && !std::io::stderr().is_terminal() {
        let path = watch::control::log_path(&data_dir(&cli));
        match logging::init_file(verbose, &path) {
            Ok(()) => {
                to_file = true;
                eprintln!("logging to {}", path.display());
            }
            Err(e) => eprintln!("can't log to {} ({e}); logging to stderr", path.display()),
        }
    }
    if !to_file {
        logging::init_stderr(verbose, watching);
    }
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            if to_file {
                log::error!("{e}");
            }
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

type AnyResult<T> = Result<T, Box<dyn std::error::Error>>;

fn default_data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("EMUSYNC_DATA_DIR") {
        return PathBuf::from(d);
    }
    let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let base = if cfg!(windows) {
        env("APPDATA")
    } else if cfg!(target_os = "macos") {
        env("HOME").map(|h| h.join("Library/Application Support"))
    } else {
        env("XDG_DATA_HOME").or_else(|| env("HOME").map(|h| h.join(".local/share")))
    };
    base.unwrap_or_else(|| PathBuf::from(".")).join("emusync")
}

fn data_dir(cli: &Cli) -> PathBuf {
    cli.data_dir.clone().unwrap_or_else(default_data_dir)
}

fn open(cli: &Cli) -> AnyResult<Engine> {
    let dir = data_dir(cli);
    let opts =
        EngineOptions { device_name: cli.device_name.clone(), emulator_id: "emusync-cli".into(), ..Default::default() };
    Ok(Engine::open(dir, opts)?)
}

/// Attaches the configured remote, if any.
fn attach(engine: &mut Engine) -> AnyResult<bool> {
    match engine.remote_uri()? {
        Some(uri) => {
            let remote = emusync_remote::open(&uri, Box::new(emusync_remote::EnvCredentials))?;
            engine.attach_remote(Arc::from(remote));
            Ok(true)
        }
        None => Ok(false),
    }
}

/// The argument that names a unit in commands: the save name, or the card for `@card`.
fn target_of(unit: &str) -> String {
    match split_unit(unit) {
        Some((card, "@card")) => card.to_string(),
        Some((_, name)) => name.to_string(),
        None => unit.to_string(),
    }
}

fn inspect(path: &Path) -> AnyResult<()> {
    let image = std::fs::read(path)?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let fmt = emusync_formats::detect(&name, &image);
    println!("{}: {}, format {}", path.display(), human_size(image.len() as u64), fmt.id());
    fmt.validate(&image).map_err(|e| format!("not a valid card: {e}"))?;
    if !fmt.supports_units() {
        println!("no save-level support for this format; it syncs as a whole file");
        return Ok(());
    }
    let saves = fmt.extract_all(&image)?;
    if saves.is_empty() {
        if emusync_formats::ps2::is_unformatted(&image) {
            println!("unformatted (empty); emusync formats it when the first save arrives");
        } else {
            println!("no saves");
        }
    }
    for s in saves {
        println!("  {:<32} {:>10}  {}", s.name, human_size(s.bytes.len() as u64), fmt.titles(&s.name).join(", "));
    }
    Ok(())
}

fn run(cli: Cli) -> AnyResult<ExitCode> {
    if let Cmd::Card(CardCmd::Inspect { path }) = &cli.cmd {
        inspect(path)?;
        return Ok(ExitCode::SUCCESS);
    }
    let mut engine = open(&cli)?;
    match &cli.cmd {
        Cmd::Remote(RemoteCmd::Add { uri }) => {
            let remote = emusync_remote::open(uri, Box::new(emusync_remote::EnvCredentials))?;
            let conditional = engine.add_remote(Arc::from(remote))?;
            println!("remote added: {}", engine.remote_uri()?.unwrap_or_default());
            if conditional {
                println!("conditional writes (If-None-Match) supported");
            } else {
                println!("conditional writes not supported; using HEAD-before-PUT");
            }
        }
        Cmd::Remote(RemoteCmd::Show) | Cmd::Status => {
            println!("device:  {} ({})", engine.device_name()?, engine.device_id()?);
            match engine.remote_uri()? {
                Some(uri) => println!(
                    "remote:  {uri} ({})",
                    if engine.conditional_writes() { "conditional writes" } else { "HEAD-before-PUT" }
                ),
                None => println!("remote:  none (run `emusync remote add <uri>`)"),
            }
            if matches!(cli.cmd, Cmd::Status) {
                println!("pending: {} record(s) waiting to upload", engine.pending_uploads()?);
            }
        }
        Cmd::Card(CardCmd::Add { path, name, platform, read_only }) => {
            if !path.is_file() {
                return Err(format!(
                    "{} is not a file (emusync never creates card files; create the card in the emulator first)",
                    path.display()
                )
                .into());
            }
            engine.add_container(ContainerDesc {
                location: path.clone(),
                format: None,
                platform: platform.clone(),
                card: name.clone(),
                read_only: *read_only,
            })?;
            let card = engine
                .cards()?
                .into_iter()
                .find(|c| c.location.as_deref().map(PathBuf::from) == std::path::absolute(path).ok());
            let (card_id, mode) = card.map(|c| (c.card_id, c.mode)).unwrap_or((String::new(), CardMode::WholeFile));
            let mode = match mode {
                CardMode::Units => "unit mode: each save syncs separately",
                CardMode::WholeFile => "whole-file mode",
            };
            println!("{} → card {card_id} ({mode})", path.display());
            println!("run `emusync sync` to upload it or pull the cloud copy");
        }
        Cmd::Card(CardCmd::List) => {
            let cards = engine.cards()?;
            if cards.is_empty() {
                println!("no cards (run `emusync card add <file>`)");
            }
            for c in cards {
                let location = c.location.clone().unwrap_or_else(|| "not on this device".into());
                match c.mode {
                    CardMode::WholeFile => {
                        let state = c.units.first().map_or("not synced yet", |u| u.state());
                        println!("{:<16} {:<11} {:<18} {location}", c.card_id, c.mode.as_str(), state);
                        if let Some(reason) = c.units.first().and_then(|u| u.inbox.clone()) {
                            println!("{:<16} waiting to apply: {reason}", "");
                        }
                    }
                    CardMode::Units => {
                        let n = c.units.iter().filter(|u| !u.deleted).count();
                        println!("{:<16} {:<11} {n} save(s)  {location}", c.card_id, c.mode.as_str());
                        for u in &c.units {
                            if u.deleted && !u.present && u.conflict.is_none() {
                                continue;
                            }
                            println!("    {:<32} {:<18} {}", u.name, u.state(), u.titles.join(", "));
                            if let Some(reason) = &u.inbox {
                                println!("    {:<32} waiting to apply: {reason}", "");
                            }
                        }
                    }
                }
            }
        }
        Cmd::Card(CardCmd::Remove { card }) => {
            let card_id = engine.resolve_card(card)?;
            let c = engine
                .containers()?
                .into_iter()
                .find(|c| c.card_id == card_id)
                .ok_or_else(|| format!("{card} has no container on this device"))?;
            engine.remove_container(c.id)?;
            println!("stopped syncing {} ({})", c.card_id, c.location);
        }
        Cmd::Card(CardCmd::Mode { card, mode, no_sync }) => {
            let mode = match mode {
                ModeArg::Units => CardMode::Units,
                ModeArg::WholeFile => CardMode::WholeFile,
            };
            engine.set_card_mode(card, mode)?;
            println!("{card} now syncs in {} mode on every device", mode.as_str());
            if !no_sync && attach(&mut engine)? {
                let report = engine.sync()?;
                print_report(&engine, &report)?;
            }
        }
        Cmd::Card(CardCmd::Inspect { .. }) => unreachable!("handled before opening the engine"),
        Cmd::Sync(args) => {
            if !attach(&mut engine)? {
                return Err("no remote configured (run `emusync remote add <uri>`)".into());
            }
            engine.set_policy(match args.policy {
                Policy::Ask => ConflictPolicy::Ask,
                Policy::Newest => ConflictPolicy::Newest,
            });
            let report = engine.sync()?;
            print_report(&engine, &report)?;
        }
        Cmd::Watch(args) => {
            if let Some(f) = &args.env_file {
                let n = watch::control::load_env_file(f)?;
                log::debug!("{n} variable(s) from {}", f.display());
            }
            if !attach(&mut engine)? {
                return Err("no remote configured (run `emusync remote add <uri>`)".into());
            }
            watch::run(engine, args, &data_dir(&cli))?;
        }
        Cmd::Service(cmd) => {
            let ctx = service::Context {
                data_dir: std::path::absolute(data_dir(&cli))?,
                device_name: cli.device_name.clone(),
                verbose: cli.verbose,
            };
            if matches!(cmd, service::ServiceCmd::Install { .. }) && engine.remote_uri()?.is_none() {
                return Err("no remote configured (run `emusync remote add <uri>` first)".into());
            }
            drop(engine);
            return service::run(cmd, &ctx);
        }
        Cmd::History { target } => {
            let unit = engine.resolve_unit(target)?;
            let entries = engine.history(&unit)?;
            println!("{unit}");
            if entries.is_empty() {
                println!("  no versions yet");
            }
            for e in entries {
                print_entry(&e);
            }
        }
        Cmd::Restore { target, version, no_sync } => {
            let rec = engine.restore(target, version)?;
            println!("restored {} as new version {}", short_id(&rec.content), short_id(&rec.id));
            if !no_sync && attach(&mut engine)? {
                let report = engine.sync()?;
                print_report(&engine, &report)?;
            }
        }
        Cmd::Conflicts => {
            let conflicts = engine.conflicts()?;
            if conflicts.is_empty() {
                println!("no open conflicts");
            }
            for c in conflicts {
                println!("conflict #{} on {} ({})", c.id, c.unit, c.location.unwrap_or_default());
                for t in &c.tips {
                    print_entry(t);
                }
                let target = target_of(&c.unit);
                println!("  resolve with `emusync resolve {target} --keep-local` or `--take <version>`");
            }
        }
        Cmd::Resolve { target, keep_local, take } => {
            let unit = engine.resolve_unit(target)?;
            let conflict = engine
                .conflicts()?
                .into_iter()
                .find(|c| c.unit == unit)
                .ok_or_else(|| format!("no open conflict on {unit}"))?;
            let resolution = match (keep_local, take) {
                (true, None) => Resolution::KeepLocal,
                (false, Some(v)) => Resolution::Take(v.clone()),
                _ => return Err("pass --keep-local or --take <version>".into()),
            };
            let rec = engine.resolve(conflict.id, resolution)?;
            println!("resolved with merge version {}", short_id(&rec.id));
            if attach(&mut engine)? {
                let report = engine.sync()?;
                print_report(&engine, &report)?;
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn print_entry(e: &HistoryEntry) {
    let v = &e.version;
    let mut tags = Vec::new();
    if e.is_local {
        tags.push("local".to_string());
    }
    if e.is_tip {
        tags.push("tip".to_string());
    }
    if v.parents.len() > 1 {
        tags.push("merge".to_string());
    }
    if v.parents.is_empty() {
        tags.push("first".to_string());
    }
    if v.tombstone {
        tags.push("deleted".to_string());
    }
    if let Some(from) = v.meta.get("restored_from") {
        tags.push(format!("restore of {}", short_id(from)));
    }
    println!(
        "  {} {:<12} {}  {:<16} {:>10}  {}",
        if e.is_local { '*' } else { ' ' },
        short_id(&v.id),
        v.hlc.display_time(),
        e.device,
        human_size(v.size),
        tags.join(", ")
    );
}

fn human_size(n: u64) -> String {
    match n {
        n if n >= 1 << 20 => format!("{:.1} MiB", n as f64 / (1u64 << 20) as f64),
        n if n >= 1 << 10 => format!("{:.1} KiB", n as f64 / 1024.0),
        n => format!("{n} B"),
    }
}

fn print_report(engine: &Engine, r: &SyncReport) -> AnyResult<()> {
    if let Some((old, new)) = &r.reminted {
        println!("device id {old} was in use by another installation; now {new}");
    }
    for a in &r.applied {
        println!("applied   {} ← {} ({})", a.unit, short_id(&a.version), a.location);
    }
    for b in &r.blocked {
        println!("blocked   {}: {}", b.unit, b.reason);
    }
    for m in &r.mode_changes {
        println!("mode      {m} changed on another device");
    }
    for w in &r.warnings {
        println!("warning   {w}");
    }
    if !r.conflicts.is_empty() {
        for c in engine.conflicts()? {
            if r.conflicts.contains(&c.id) {
                println!("CONFLICT  {}: devices diverged; see `emusync conflicts`", c.unit);
            }
        }
    }
    println!(
        "{}: pulled {} version(s), {} new local, uploaded {} version(s) in {} blob(s){}",
        if r.offline { "offline" } else { "synced" },
        r.pulled_records,
        r.new_local_versions,
        r.uploaded_versions,
        r.uploaded_blobs,
        if r.pending_uploads > 0 {
            format!(", {} record(s) waiting to upload", r.pending_uploads)
        } else {
            String::new()
        }
    );
    Ok(())
}
