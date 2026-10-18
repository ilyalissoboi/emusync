//! Files in the data directory through which other `emusync` commands find and stop a running
//! watcher: `watch.pid` while one runs, and `watch.stop` to ask it to stop gracefully. A file
//! works the same on every OS, including for the windowless watcher on Windows, which has no
//! console to send Ctrl+C to.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub fn pid_path(data_dir: &Path) -> PathBuf {
    data_dir.join("watch.pid")
}

fn stop_path(data_dir: &Path) -> PathBuf {
    data_dir.join("watch.stop")
}

pub fn log_path(data_dir: &Path) -> PathBuf {
    data_dir.join("logs").join("watch.log")
}

/// The pid of the watcher running on `data_dir`, if any. A pid file left behind by a watcher
/// that was killed is ignored unless the pid now belongs to another `emusync` process.
pub fn running_pid(data_dir: &Path) -> Option<u32> {
    let pid: u32 = std::fs::read_to_string(pid_path(data_dir)).ok()?.trim().parse().ok()?;
    if pid == std::process::id() {
        return None;
    }
    let pid_s = sysinfo::Pid::from_u32(pid);
    let mut sys = sysinfo::System::new();
    sys.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::Some(&[pid_s]),
        true,
        sysinfo::ProcessRefreshKind::nothing(),
    );
    let name = sys.process(pid_s)?.name().to_string_lossy().to_lowercase();
    name.contains("emusync").then_some(pid)
}

/// Marks this process as the watcher on `data_dir` until dropped.
pub struct PidFile(PathBuf);

impl PidFile {
    pub fn create(data_dir: &Path) -> std::io::Result<Self> {
        // A stop request nobody picked up must not stop this watcher.
        let _ = std::fs::remove_file(stop_path(data_dir));
        let path = pid_path(data_dir);
        std::fs::write(&path, format!("{}\n", std::process::id()))?;
        Ok(Self(path))
    }
}

impl Drop for PidFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Asks the watcher on `data_dir` to stop after its current cycle (with a final sync), and
/// waits up to `timeout` for it to exit. Returns whether it has.
pub fn stop(data_dir: &Path, timeout: Duration) -> std::io::Result<bool> {
    if running_pid(data_dir).is_none() {
        return Ok(true);
    }
    std::fs::write(stop_path(data_dir), b"")?;
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if running_pid(data_dir).is_none() {
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let _ = std::fs::remove_file(stop_path(data_dir));
    Ok(false)
}

/// Whether someone asked this watcher to stop; consumes the request.
pub fn stop_requested(data_dir: &Path) -> bool {
    std::fs::remove_file(stop_path(data_dir)).is_ok()
}

/// Parses `KEY=VALUE` lines: blank lines and `#` comments are skipped, an `export ` prefix and
/// quotes around the value are allowed.
pub fn parse_env(text: &str) -> Result<Vec<(String, String)>, String> {
    let mut vars = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let (key, value) = line.split_once('=').ok_or_else(|| format!("line {}: expected KEY=VALUE", i + 1))?;
        let key = key.trim();
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(format!("line {}: invalid variable name {key:?}", i + 1));
        }
        let value = value.trim();
        let value = [('"', '"'), ('\'', '\'')]
            .iter()
            .find_map(|&(a, b)| value.strip_prefix(a).and_then(|v| v.strip_suffix(b)))
            .unwrap_or(value);
        vars.push((key.to_string(), value.to_string()));
    }
    Ok(vars)
}

/// Sets the variables in `path` for this process. Call before starting any threads.
pub fn load_env_file(path: &Path) -> Result<usize, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("can't read {}: {e}", path.display()))?;
    let vars = parse_env(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(m) = std::fs::metadata(path) {
            if m.permissions().mode() & 0o077 != 0 {
                log::warn!("{} is readable by other users; run `chmod 600` on it", path.display());
            }
        }
    }
    for (k, v) in &vars {
        std::env::set_var(k, v);
    }
    Ok(vars.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_files() {
        let vars = parse_env(
            "# credentials\n\nAWS_ACCESS_KEY_ID=AKIA123\nexport AWS_SECRET_ACCESS_KEY = \"a b=c\"\nX='y'\nEMPTY=\n",
        )
        .unwrap();
        let vars: Vec<(&str, &str)> = vars.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert_eq!(
            vars,
            [("AWS_ACCESS_KEY_ID", "AKIA123"), ("AWS_SECRET_ACCESS_KEY", "a b=c"), ("X", "y"), ("EMPTY", "")]
        );
        assert!(parse_env("JUST A LINE").unwrap_err().contains("line 1"));
        assert!(parse_env("A=1\nBAD KEY=2").unwrap_err().contains("line 2"));
    }

    #[test]
    fn stale_pid_files_and_stop_requests() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(running_pid(dir.path()), None);
        // Our own pid is not "another watcher".
        let pid = PidFile::create(dir.path()).unwrap();
        assert_eq!(running_pid(dir.path()), None);
        drop(pid);
        assert!(!pid_path(dir.path()).exists());
        // A pid that isn't running.
        std::fs::write(pid_path(dir.path()), "4294967290\n").unwrap();
        assert_eq!(running_pid(dir.path()), None);
        assert!(stop(dir.path(), Duration::ZERO).unwrap(), "nothing to stop");
        assert!(!stop_requested(dir.path()), "no request left behind");

        std::fs::write(stop_path(dir.path()), b"").unwrap();
        let _pid = PidFile::create(dir.path()).unwrap();
        assert!(!stop_requested(dir.path()), "a new watcher drops old requests");
        std::fs::write(stop_path(dir.path()), b"").unwrap();
        assert!(stop_requested(dir.path()));
        assert!(!stop_requested(dir.path()));
    }
}
