//! `emusync service`: start `emusync watch` at login, per user and without admin rights, with
//! the service manager of each OS (Task Scheduler, launchd, systemd --user).
//!
//! Credentials are never written into the service definition. The watcher gets them from the
//! environment the service manager gives it (on Windows, the user's environment variables), or
//! from a file the user names with `--env-file`.

mod defs;

use crate::watch::{control, WatchArgs};
use crate::AnyResult;
use clap::Subcommand;
use defs::Command;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

#[derive(Subcommand)]
pub enum ServiceCmd {
    /// Start `emusync watch` at every login, and now. Takes the options of `emusync watch`;
    /// run it again to change them.
    Install {
        #[command(flatten)]
        watch: WatchArgs,
        /// Don't start it now, only at the next login.
        #[arg(long)]
        no_start: bool,
    },
    /// Stop the watcher (after a final sync) and stop starting it at login.
    Uninstall,
    /// Show whether the watcher is installed and running, and the end of its log.
    Status,
    /// Start the installed watcher now.
    Start,
    /// Stop the watcher after a final sync, until the next login or `emusync service start`.
    Stop,
}

/// What the service needs from the global options.
pub struct Context {
    /// Absolute.
    pub data_dir: PathBuf,
    pub device_name: Option<String>,
    pub verbose: u8,
    /// The remote is Dropbox: its sign-in is in the credential store, not the environment.
    pub dropbox: bool,
}

const STOP_TIMEOUT: Duration = Duration::from_secs(defs::STOP_TIMEOUT_SECS);

pub fn run(cmd: &ServiceCmd, ctx: &Context) -> AnyResult<ExitCode> {
    let mgr = Manager::current()?;
    let dir = &ctx.data_dir;
    match cmd {
        ServiceCmd::Install { watch, no_start } => {
            let command = watch_command(ctx, watch)?;
            if control::running_pid(dir).is_some() {
                println!("stopping the running watcher first…");
                stop_watcher(dir)?;
            }
            mgr.install(&command, ctx, !no_start)?;
            println!("installed: {}", mgr.describe());
            println!("command:   {}", command.display());
            println!("log:       {}", control::log_path(dir).display());
            if !no_start {
                report_started(dir);
            }
            if ctx.dropbox {
                println!("credentials: the Dropbox sign-in kept in your credential store");
            } else {
                credentials_hint(watch);
            }
        }
        ServiceCmd::Uninstall => {
            let installed = mgr.installed()?;
            if control::running_pid(dir).is_some() {
                stop_watcher(dir)?;
                println!("stopped the watcher");
            }
            if installed {
                mgr.uninstall()?;
                println!("uninstalled: {}", mgr.describe());
            } else {
                println!("not installed");
            }
        }
        ServiceCmd::Status => {
            if mgr.installed()? {
                println!("service: installed ({})", mgr.describe());
            } else {
                println!("service: not installed (run `emusync service install`)");
            }
            match control::running_pid(dir) {
                Some(pid) => println!("watcher: running (pid {pid})"),
                None => println!("watcher: not running"),
            }
            let log = control::log_path(dir);
            println!("log:     {}", log.display());
            for line in tail(&log, 10) {
                println!("  {line}");
            }
        }
        ServiceCmd::Start => {
            if !mgr.installed()? {
                return Err("not installed (run `emusync service install`)".into());
            }
            if let Some(pid) = control::running_pid(dir) {
                println!("already running (pid {pid})");
            } else {
                mgr.start()?;
                report_started(dir);
            }
        }
        ServiceCmd::Stop => {
            if control::running_pid(dir).is_none() {
                println!("not running");
            } else {
                stop_watcher(dir)?;
                println!("stopped; it starts again at the next login (or `emusync service start`)");
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn stop_watcher(dir: &Path) -> AnyResult<()> {
    if control::stop(dir, STOP_TIMEOUT)? {
        Ok(())
    } else {
        Err(format!(
            "the watcher didn't stop within {} s; see {}",
            STOP_TIMEOUT.as_secs(),
            control::log_path(dir).display()
        )
        .into())
    }
}

/// Waits briefly for the watcher to come up, and says whether it did.
fn report_started(dir: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Some(pid) = control::running_pid(dir) {
            println!("running (pid {pid})");
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    println!("not running yet; check `emusync service status` and the log");
}

/// The last `n` lines of a file.
fn tail(path: &Path, n: usize) -> Vec<String> {
    let text = std::fs::read(path).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].iter().map(|l| l.to_string()).collect()
}

/// `emusync[w] --data-dir <dir> watch <options>`, with this executable.
fn watch_command(ctx: &Context, watch: &WatchArgs) -> AnyResult<Command> {
    let exe = std::env::current_exe()?;
    let program = if cfg!(windows) {
        // The windowless build, so no console window opens at login.
        let w = exe.with_file_name("emusyncw.exe");
        if w.is_file() {
            w
        } else {
            eprintln!("warning: {} not found; a console window will open at login", w.display());
            exe
        }
    } else {
        exe
    };
    let mut args = vec!["--data-dir".to_string(), ctx.data_dir.display().to_string()];
    if let Some(name) = &ctx.device_name {
        args.extend(["--device-name".to_string(), name.clone()]);
    }
    if ctx.verbose > 0 {
        args.push(format!("-{}", "v".repeat(ctx.verbose.into())));
    }
    args.push("watch".to_string());
    args.extend(watch.to_args()?);
    Ok(Command { program, args })
}

fn credentials_hint(watch: &WatchArgs) {
    if let Some(f) = &watch.env_file {
        println!("credentials: read from {} at startup", f.display());
        return;
    }
    if cfg!(windows) {
        let persisted = |name: &str| {
            [r"HKCU\Environment", r"HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Environment"]
                .iter()
                .any(|key| succeeds("reg", &["query", key, "/v", name]))
        };
        if persisted("AWS_ACCESS_KEY_ID") && persisted("AWS_SECRET_ACCESS_KEY") {
            println!("credentials: from your Windows environment variables");
            return;
        }
        println!(
            "note: the watcher only sees environment variables saved in Windows, not ones set in this \
             shell. Save AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY (`setx`, or Settings > System > \
             About > Advanced system settings > Environment Variables), or put them in a file only you \
             can read and reinstall with --env-file <file>."
        );
    } else {
        println!(
            "note: the watcher doesn't see this shell's environment. Put AWS_ACCESS_KEY_ID and \
             AWS_SECRET_ACCESS_KEY in a file only you can read (`chmod 600`) and reinstall with \
             --env-file <file>."
        );
    }
}

/// Runs a command; on failure the error includes its output.
fn exec(program: &str, args: &[&str]) -> AnyResult<String> {
    let out =
        std::process::Command::new(program).args(args).output().map_err(|e| format!("can't run {program}: {e}"))?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if out.status.success() {
        Ok(text)
    } else {
        Err(format!("`{program} {}` failed: {}", args.join(" "), text.trim()).into())
    }
}

fn succeeds(program: &str, args: &[&str]) -> bool {
    std::process::Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// The per-user service manager of this OS.
enum Manager {
    /// A logon task, registered with `schtasks`.
    TaskScheduler,
    /// A LaunchAgent in `~/Library/LaunchAgents`.
    Launchd { plist: PathBuf },
    /// A unit in `~/.config/systemd/user`.
    Systemd { unit: PathBuf },
}

impl Manager {
    fn current() -> AnyResult<Self> {
        if cfg!(windows) {
            Ok(Manager::TaskScheduler)
        } else if cfg!(target_os = "macos") {
            let home = dirs::home_dir().ok_or("no home directory")?;
            let plist = home.join("Library/LaunchAgents").join(format!("{}.plist", defs::LAUNCHD_LABEL));
            Ok(Manager::Launchd { plist })
        } else if cfg!(target_os = "linux") {
            let config = dirs::config_dir().ok_or("no config directory")?;
            Ok(Manager::Systemd { unit: config.join("systemd/user").join(defs::SYSTEMD_UNIT) })
        } else {
            Err("not supported on this OS; start `emusync watch` from your login items instead".into())
        }
    }

    fn describe(&self) -> String {
        match self {
            Manager::TaskScheduler => format!("Task Scheduler task \"{}\", at logon", defs::TASK_NAME),
            Manager::Launchd { plist } => format!("LaunchAgent {}", plist.display()),
            Manager::Systemd { unit } => format!("systemd user unit {}", unit.display()),
        }
    }

    fn installed(&self) -> AnyResult<bool> {
        Ok(match self {
            Manager::TaskScheduler => succeeds("schtasks", &["/Query", "/TN", defs::TASK_NAME]),
            Manager::Launchd { plist } => plist.is_file(),
            Manager::Systemd { unit } => unit.is_file(),
        })
    }

    fn install(&self, cmd: &Command, ctx: &Context, start: bool) -> AnyResult<()> {
        match self {
            Manager::TaskScheduler => {
                let user = windows_user()?;
                let xml = ctx.data_dir.join("emusync-task.xml");
                std::fs::write(&xml, defs::utf16_with_bom(&defs::task_xml(&user, cmd)))?;
                let xml_s = xml.display().to_string();
                let result = exec("schtasks", &["/Create", "/TN", defs::TASK_NAME, "/XML", &xml_s, "/F"]);
                let _ = std::fs::remove_file(&xml);
                result?;
                if start {
                    exec("schtasks", &["/Run", "/TN", defs::TASK_NAME])?;
                }
            }
            Manager::Launchd { plist } => {
                write_file(plist, &defs::launchd_plist(cmd))?;
                let domain = launchd_domain()?;
                let _ = exec("launchctl", &["bootout", &format!("{domain}/{}", defs::LAUNCHD_LABEL)]);
                if start {
                    exec("launchctl", &["bootstrap", &domain, &plist.display().to_string()])?;
                }
            }
            Manager::Systemd { unit } => {
                write_file(unit, &defs::systemd_unit(cmd))?;
                exec("systemctl", &["--user", "daemon-reload"])?;
                exec("systemctl", &["--user", "enable", defs::SYSTEMD_UNIT])?;
                if start {
                    exec("systemctl", &["--user", "restart", defs::SYSTEMD_UNIT])?;
                }
            }
        }
        Ok(())
    }

    fn uninstall(&self) -> AnyResult<()> {
        match self {
            Manager::TaskScheduler => {
                let _ = exec("schtasks", &["/End", "/TN", defs::TASK_NAME]);
                exec("schtasks", &["/Delete", "/TN", defs::TASK_NAME, "/F"])?;
            }
            Manager::Launchd { plist } => {
                let _ = exec("launchctl", &["bootout", &format!("{}/{}", launchd_domain()?, defs::LAUNCHD_LABEL)]);
                std::fs::remove_file(plist)?;
            }
            Manager::Systemd { unit } => {
                let _ = exec("systemctl", &["--user", "disable", "--now", defs::SYSTEMD_UNIT]);
                std::fs::remove_file(unit)?;
                exec("systemctl", &["--user", "daemon-reload"])?;
            }
        }
        Ok(())
    }

    fn start(&self) -> AnyResult<()> {
        match self {
            Manager::TaskScheduler => {
                exec("schtasks", &["/Run", "/TN", defs::TASK_NAME])?;
            }
            Manager::Launchd { plist } => {
                let domain = launchd_domain()?;
                let target = format!("{domain}/{}", defs::LAUNCHD_LABEL);
                if succeeds("launchctl", &["print", &target]) {
                    exec("launchctl", &["kickstart", &target])?;
                } else {
                    exec("launchctl", &["bootstrap", &domain, &plist.display().to_string()])?;
                }
            }
            Manager::Systemd { .. } => {
                exec("systemctl", &["--user", "start", defs::SYSTEMD_UNIT])?;
            }
        }
        Ok(())
    }
}

fn write_file(path: &Path, content: &str) -> AnyResult<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, content)?;
    Ok(())
}

/// `DOMAIN\user` for the logon trigger.
fn windows_user() -> AnyResult<String> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    match (env("USERDOMAIN"), env("USERNAME")) {
        (Some(domain), Some(user)) => Ok(format!(r"{domain}\{user}")),
        _ => Ok(exec("whoami", &[])?.trim().to_string()),
    }
}

/// `gui/<uid>`: the logged-in user's launchd domain.
fn launchd_domain() -> AnyResult<String> {
    Ok(format!("gui/{}", exec("id", &["-u"])?.trim()))
}
