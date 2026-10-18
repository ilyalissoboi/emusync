//! Emulator profiles (DESIGN §6.4): how the sidecar recognises a running emulator and where
//! the emulator keeps its memory cards. Profiles are JSON, so an emulator is added by writing
//! a file, not code. Built-in profiles live in `crates/emusyncd/profiles/`.

use serde::Deserialize;
use std::path::{Path, PathBuf};

const BUILT_IN: &[(&str, &str)] = &[("pcsx2", include_str!("../../profiles/pcsx2.json"))];

/// What a profile file contains: per-OS settings plus the card file extensions it owns.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileFile {
    id: String,
    name: String,
    /// Card files with these extensions (no dot, any case) belong to this emulator.
    card_extensions: Vec<String>,
    #[serde(default)]
    windows: OsSection,
    #[serde(default)]
    macos: OsSection,
    #[serde(default)]
    linux: OsSection,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct OsSection {
    /// Process names (file name of the executable, matched without regard to case).
    #[serde(default)]
    processes: Vec<String>,
    /// Default memory card folders. `{home}`, `{documents}`, `{config}` and `{data}` expand to
    /// the platform's folders.
    #[serde(default)]
    card_dirs: Vec<String>,
    /// Files that exist only while the emulator runs (same placeholders as `card_dirs`).
    #[serde(default)]
    lock_files: Vec<String>,
}

/// A profile resolved for the current OS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub card_extensions: Vec<String>,
    pub processes: Vec<String>,
    pub card_dirs: Vec<PathBuf>,
    pub lock_files: Vec<PathBuf>,
}

/// Platform folders that profile paths may refer to.
#[derive(Debug, Clone, Default)]
pub struct Folders {
    pub home: Option<PathBuf>,
    pub documents: Option<PathBuf>,
    pub config: Option<PathBuf>,
    pub data: Option<PathBuf>,
}

impl Folders {
    pub fn system() -> Self {
        Self {
            home: dirs::home_dir(),
            documents: dirs::document_dir(),
            config: dirs::config_dir(),
            data: dirs::data_dir(),
        }
    }

    /// Expands the placeholders in `pattern`; `None` if a folder it needs is unknown.
    fn expand(&self, pattern: &str) -> Option<PathBuf> {
        let mut out = pattern.to_string();
        for (key, dir) in [
            ("{home}", &self.home),
            ("{documents}", &self.documents),
            ("{config}", &self.config),
            ("{data}", &self.data),
        ] {
            if out.contains(key) {
                out = out.replace(key, &dir.as_ref()?.to_string_lossy());
            }
        }
        Some(PathBuf::from(out))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Windows,
    Mac,
    Linux,
}

impl Os {
    pub fn current() -> Self {
        if cfg!(windows) {
            Os::Windows
        } else if cfg!(target_os = "macos") {
            Os::Mac
        } else {
            Os::Linux
        }
    }
}

impl Profile {
    /// Parses a profile file and resolves it for `os`.
    pub fn parse(json: &str, os: Os, folders: &Folders) -> Result<Self, String> {
        let f: ProfileFile = serde_json::from_str(json).map_err(|e| format!("invalid profile: {e}"))?;
        if f.card_extensions.is_empty() {
            return Err(format!("profile {:?} lists no card_extensions", f.id));
        }
        let section = match os {
            Os::Windows => f.windows,
            Os::Mac => f.macos,
            Os::Linux => f.linux,
        };
        let expand = |v: &[String]| v.iter().filter_map(|p| folders.expand(p)).collect::<Vec<_>>();
        Ok(Self {
            card_dirs: expand(&section.card_dirs),
            lock_files: expand(&section.lock_files),
            processes: section.processes,
            card_extensions: f.card_extensions.iter().map(|e| e.trim_start_matches('.').to_ascii_lowercase()).collect(),
            id: f.id,
            name: f.name,
        })
    }

    /// A built-in profile by id, or a profile file by path.
    pub fn load(arg: &str) -> Result<Self, String> {
        let (os, folders) = (Os::current(), Folders::system());
        if let Some((_, json)) = BUILT_IN.iter().find(|(id, _)| id.eq_ignore_ascii_case(arg)) {
            return Self::parse(json, os, &folders);
        }
        let path = Path::new(arg);
        if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("json")) {
            let json = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
            return Self::parse(&json, os, &folders).map_err(|e| format!("{}: {e}", path.display()));
        }
        let ids: Vec<&str> = BUILT_IN.iter().map(|(id, _)| *id).collect();
        Err(format!("unknown profile {arg:?} (built in: {}; or pass a .json profile file)", ids.join(", ")))
    }

    /// Whether a card file belongs to this emulator (by extension).
    pub fn claims(&self, path: &Path) -> bool {
        path.extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .is_some_and(|e| self.card_extensions.contains(&e))
    }

    /// Whether `process` (a running process's name) is this emulator.
    pub fn matches_process(&self, process: &str) -> bool {
        self.processes.iter().any(|p| p.eq_ignore_ascii_case(process))
    }

    /// Card files in the default folders, sorted.
    pub fn discover(&self) -> Vec<PathBuf> {
        let mut found = Vec::new();
        for dir in &self.card_dirs {
            let Ok(entries) = std::fs::read_dir(dir) else { continue };
            for e in entries.flatten() {
                let p = e.path();
                if self.claims(&p) && e.file_type().is_ok_and(|t| t.is_file()) {
                    found.push(p);
                }
            }
        }
        found.sort();
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folders() -> Folders {
        Folders {
            home: Some("/home/u".into()),
            documents: Some("C:/Users/u/Documents".into()),
            config: Some("/home/u/.config".into()),
            data: None,
        }
    }

    #[test]
    fn built_in_profiles_parse_on_every_os() {
        for (id, json) in BUILT_IN {
            for os in [Os::Windows, Os::Mac, Os::Linux] {
                let p = Profile::parse(json, os, &folders()).unwrap();
                assert_eq!(&p.id, id);
                assert!(!p.processes.is_empty(), "{id} has no processes on {os:?}");
                assert!(!p.card_dirs.is_empty(), "{id} has no card folders on {os:?}");
            }
        }
    }

    #[test]
    fn pcsx2_paths_and_processes() {
        let json = BUILT_IN[0].1;
        let win = Profile::parse(json, Os::Windows, &folders()).unwrap();
        assert_eq!(win.card_dirs, vec![PathBuf::from("C:/Users/u/Documents/PCSX2/memcards")]);
        assert!(win.matches_process("PCSX2-QT.EXE"));
        assert!(!win.matches_process("pcsx2-qt"));
        let mac = Profile::parse(json, Os::Mac, &folders()).unwrap();
        assert_eq!(mac.card_dirs, vec![PathBuf::from("/home/u/Library/Application Support/PCSX2/memcards")]);
        assert!(mac.matches_process("PCSX2"));
        let linux = Profile::parse(json, Os::Linux, &folders()).unwrap();
        assert_eq!(linux.card_dirs[0], PathBuf::from("/home/u/.config/PCSX2/memcards"));
        assert!(linux.claims(Path::new("/x/Mcd001.PS2")));
        assert!(!linux.claims(Path::new("/x/Mcd001.ps2.emusync-tmp")));
    }

    #[test]
    fn unknown_folders_drop_the_path_and_bad_profiles_are_rejected() {
        let json = r#"{"id":"x","name":"X","card_extensions":[".SRM"],
            "linux":{"processes":["x"],"card_dirs":["{data}/x","/abs/x"],"lock_files":["{home}/x.lock"]}}"#;
        let p = Profile::parse(json, Os::Linux, &folders()).unwrap();
        assert_eq!(p.card_dirs, vec![PathBuf::from("/abs/x")]);
        assert_eq!(p.lock_files, vec![PathBuf::from("/home/u/x.lock")]);
        assert_eq!(p.card_extensions, vec!["srm"]);
        assert!(Profile::parse(r#"{"id":"x","name":"X","card_extensions":[]}"#, Os::Linux, &folders()).is_err());
        assert!(
            Profile::parse(r#"{"id":"x","name":"X","card_extensions":["a"],"typo":1}"#, Os::Linux, &folders()).is_err()
        );
        assert!(Profile::load("nope").is_err());
        assert!(Profile::load("PCSX2").is_ok());
    }
}
