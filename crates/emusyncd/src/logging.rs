//! Logging: to stderr, or for a watcher without a terminal (started at login by
//! `emusync service`), to a size-capped log file in the data directory.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// A log file is rotated once it would grow past this size.
pub const MAX_LOG_BYTES: u64 = 1 << 20;
/// Rotated files kept next to the log (`watch.log.1` is the newest).
pub const KEEP_LOGS: usize = 3;

/// A log file that rotates by size: `watch.log` → `watch.log.1` → … → `watch.log.<keep>`.
pub struct RotatingFile {
    path: PathBuf,
    file: Option<File>,
    size: u64,
    max: u64,
    keep: usize,
}

impl RotatingFile {
    pub fn open(path: &Path, max: u64, keep: usize) -> std::io::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = File::options().create(true).append(true).open(path)?;
        let size = file.metadata()?.len();
        Ok(Self { path: path.to_path_buf(), file: Some(file), size, max, keep })
    }

    fn rotated(&self, n: usize) -> PathBuf {
        let mut name = self.path.as_os_str().to_owned();
        name.push(format!(".{n}"));
        PathBuf::from(name)
    }

    fn rotate(&mut self) {
        self.file = None;
        let mut renamed = true;
        for n in (1..self.keep).rev() {
            let from = self.rotated(n);
            if from.exists() {
                renamed &= std::fs::rename(&from, self.rotated(n + 1)).is_ok();
            }
        }
        if self.keep > 0 {
            renamed &= std::fs::rename(&self.path, self.rotated(1)).is_ok();
        } else {
            renamed = false;
        }
        // If renaming failed (e.g. on Windows while someone has the log open), start over in
        // place instead, so the size cap still holds.
        self.file = File::options().create(true).append(true).open(&self.path).ok();
        if !renamed {
            if let Some(f) = &self.file {
                let _ = f.set_len(0);
            }
        }
        self.size = 0;
    }

    pub fn write_line(&mut self, line: &str) {
        let len = line.len() as u64 + 1;
        if self.size > 0 && self.size + len > self.max {
            self.rotate();
        }
        if let Some(f) = &mut self.file {
            if writeln!(f, "{line}").is_ok() {
                self.size += len;
            }
        }
    }
}

struct Logger {
    level: log::LevelFilter,
    /// Prefix lines with the UTC time.
    timestamps: bool,
    /// Write here instead of stderr.
    file: Option<Mutex<RotatingFile>>,
}

impl log::Log for Logger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= self.level && (m.target().starts_with("emusync") || m.level() <= log::Level::Warn)
    }
    fn log(&self, r: &log::Record) {
        if !self.enabled(r.metadata()) {
            return;
        }
        let level = r.level().as_str().to_ascii_lowercase();
        let line = if self.timestamps {
            let ms =
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
            let now = emusync_core::hlc::Hlc { ms, counter: 0 }.display_time();
            format!("{now}Z [{level}] {}", r.args())
        } else {
            format!("[{level}] {}", r.args())
        };
        match &self.file {
            Some(f) => f.lock().unwrap_or_else(|e| e.into_inner()).write_line(&line),
            None => eprintln!("{line}"),
        }
    }
    fn flush(&self) {}
}

fn level(verbose: u8) -> log::LevelFilter {
    match verbose {
        0 => log::LevelFilter::Warn,
        1 => log::LevelFilter::Info,
        _ => log::LevelFilter::Debug,
    }
}

fn install(logger: Logger) {
    let level = logger.level;
    if log::set_logger(Box::leak(Box::new(logger))).is_ok() {
        log::set_max_level(level);
    }
}

pub fn init_stderr(verbose: u8, timestamps: bool) {
    install(Logger { level: level(verbose), timestamps, file: None });
}

/// Logs to `path` (rotated by size), and routes panics there too, since nobody sees stderr.
pub fn init_file(verbose: u8, path: &Path) -> std::io::Result<()> {
    let file = RotatingFile::open(path, MAX_LOG_BYTES, KEEP_LOGS)?;
    install(Logger { level: level(verbose), timestamps: true, file: Some(Mutex::new(file)) });
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log::error!("{info}");
        default_hook(info);
    }));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotates_by_size_and_keeps_a_few() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs").join("watch.log");
        let mut f = RotatingFile::open(&path, 100, 2).unwrap();
        for i in 0..20 {
            f.write_line(&format!("line {i:02} {}", "x".repeat(30)));
        }
        let read = |p: &Path| std::fs::read_to_string(p).unwrap();
        // 39 bytes per line: two per file.
        assert_eq!(read(&path).lines().count(), 2);
        assert!(read(&path).ends_with(&format!("line 19 {}\n", "x".repeat(30))));
        assert!(read(&f.rotated(1)).starts_with("line 16"));
        assert!(read(&f.rotated(2)).starts_with("line 14"));
        assert!(!f.rotated(3).exists());

        // Reopening appends, and counts what is already there.
        drop(f);
        let mut f = RotatingFile::open(&path, 100, 2).unwrap();
        f.write_line("short");
        assert!(read(&f.rotated(1)).starts_with("line 16"), "still fits");
        f.write_line(&"z".repeat(30));
        assert!(read(&f.rotated(1)).starts_with("line 18"), "full file rotated before writing");
        assert_eq!(read(&path), format!("{}\n", "z".repeat(30)));
    }

    #[test]
    fn a_line_longer_than_the_cap_is_still_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("watch.log");
        let mut f = RotatingFile::open(&path, 10, 1).unwrap();
        f.write_line(&"y".repeat(50));
        assert_eq!(std::fs::read_to_string(&path).unwrap().len(), 51);
    }
}
