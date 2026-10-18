//! `emusync` command-line tool (DESIGN §12). M0: whole-file card sync through S3.
//!
//! S3 credentials come from `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and optionally
//! `AWS_SESSION_TOKEN`; they are never written to the data directory.

use clap::{Args, Parser, Subcommand, ValueEnum};
use emusync_core::{
    short_id, ConflictPolicy, ContainerDesc, Engine, EngineOptions, HistoryEntry, Resolution, SyncReport,
};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

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
    /// List every version of a card, newest first.
    History { card: String },
    /// Make an old version current again (creates a new version), then sync.
    Restore {
        card: String,
        version: String,
        /// Only record the restore locally; don't run a sync afterwards.
        #[arg(long)]
        no_sync: bool,
    },
    /// List open conflicts.
    Conflicts,
    /// Resolve a card's open conflict, then sync.
    Resolve {
        card: String,
        /// Keep this device's version.
        #[arg(long, conflicts_with = "take")]
        keep_local: bool,
        /// Take the given version (an id prefix from `emusync conflicts`).
        #[arg(long)]
        take: Option<String>,
    },
    /// Show this device's identity and remote.
    Status,
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
    /// List cards known locally or in the repository.
    List,
    /// Stop syncing a card file on this device (history is kept).
    Remove { card: String },
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

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging(cli.verbose);
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
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

fn open(cli: &Cli) -> AnyResult<Engine> {
    let dir = cli.data_dir.clone().unwrap_or_else(default_data_dir);
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

fn run(cli: Cli) -> AnyResult<ExitCode> {
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
                .find(|c| c.location.as_deref().map(PathBuf::from) == std::path::absolute(path).ok())
                .map(|c| c.card_id)
                .unwrap_or_default();
            println!("{} → card {card} (whole-file mode)", path.display());
            println!("run `emusync sync` to upload it or pull the cloud copy");
        }
        Cmd::Card(CardCmd::List) => {
            let cards = engine.cards()?;
            if cards.is_empty() {
                println!("no cards (run `emusync card add <file>`)");
            }
            for c in cards {
                let state = if c.conflict.is_some() {
                    "CONFLICT".to_string()
                } else if c.location.is_none() {
                    "not on this device".to_string()
                } else if c.local_version.is_some() && c.tips.len() == 1 && c.local_version.as_ref() == c.tips.first() {
                    "in sync".to_string()
                } else if c.local_version.is_none() {
                    "not synced yet".to_string()
                } else {
                    "behind".to_string()
                };
                println!("{:<16} {:<11} {:<18} {}", c.card_id, c.mode.as_str(), state, c.location.unwrap_or_default());
                if let Some(reason) = c.inbox {
                    println!("{:<16} waiting to apply: {reason}", "");
                }
            }
        }
        Cmd::Card(CardCmd::Remove { card }) => {
            let unit = engine.resolve_unit(card)?;
            let c = engine
                .containers()?
                .into_iter()
                .find(|c| unit.starts_with(&format!("{}:", c.card_id)))
                .ok_or_else(|| format!("{card} has no container on this device"))?;
            engine.remove_container(c.id)?;
            println!("stopped syncing {} ({})", c.card_id, c.location);
        }
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
        Cmd::History { card } => {
            let unit = engine.resolve_unit(card)?;
            let entries = engine.history(card)?;
            println!("{unit}");
            if entries.is_empty() {
                println!("  no versions yet");
            }
            for e in entries {
                print_entry(&e);
            }
        }
        Cmd::Restore { card, version, no_sync } => {
            let rec = engine.restore(card, version)?;
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
                println!("  resolve with `emusync resolve <card> --keep-local` or `--take <version>`");
            }
        }
        Cmd::Resolve { card, keep_local, take } => {
            let unit = engine.resolve_unit(card)?;
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

struct StderrLogger(log::LevelFilter);

impl log::Log for StderrLogger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= self.0 && (m.target().starts_with("emusync") || m.level() <= log::Level::Warn)
    }
    fn log(&self, r: &log::Record) {
        if self.enabled(r.metadata()) {
            eprintln!("[{}] {}", r.level().as_str().to_ascii_lowercase(), r.args());
        }
    }
    fn flush(&self) {}
}

fn init_logging(verbose: u8) {
    let level = match verbose {
        0 => log::LevelFilter::Warn,
        1 => log::LevelFilter::Info,
        _ => log::LevelFilter::Debug,
    };
    if log::set_logger(Box::leak(Box::new(StderrLogger(level)))).is_ok() {
        log::set_max_level(level);
    }
}
