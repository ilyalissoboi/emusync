//! PSP memory sticks (DESIGN §27): the save folders in two devices' `PSP/SAVEDATA` sync one by
//! one over the in-memory remote, and game install data and other files are left alone.

use emusync_core::{CardMode, ConflictPolicy, ContainerDesc, Engine, EngineOptions};
use emusync_remote::MemoryRemote;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

struct Device {
    engine: Engine,
    savedata: PathBuf,
    _dir: tempfile::TempDir,
}

fn set_folder_time(path: &Path, t: SystemTime) {
    #[cfg(windows)]
    let f = {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_WRITE_ATTRIBUTES, FILE_FLAG_BACKUP_SEMANTICS
        fs::OpenOptions::new().access_mode(0x0100).custom_flags(0x0200_0000).open(path).unwrap()
    };
    #[cfg(not(windows))]
    let f = fs::File::open(path).unwrap();
    f.set_modified(t).unwrap();
}

/// Sets a file's or folder's modification time to `secs_ago` seconds ago.
fn age(path: &Path, secs_ago: u64) {
    let t = SystemTime::now() - Duration::from_secs(secs_ago);
    if path.is_dir() {
        set_folder_time(path, t);
    } else {
        fs::File::options().write(true).open(path).unwrap().set_modified(t).unwrap();
    }
}

impl Device {
    /// A memory stick as PPSSPP sets one up; with `savedata`, its `SAVEDATA` folder exists.
    fn new(remote: &Arc<MemoryRemote>, name: &str, savedata: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let psp = dir.path().join("memstick").join("PSP");
        fs::create_dir_all(&psp).unwrap();
        let path = psp.join("SAVEDATA");
        if savedata {
            fs::create_dir(&path).unwrap();
            fs::write(path.join(".nomedia"), b"").unwrap();
        }
        let mut engine = Engine::open(
            dir.path().join("data"),
            EngineOptions { device_name: Some(name.into()), policy: ConflictPolicy::Ask, ..Default::default() },
        )
        .unwrap();
        engine.add_remote(remote.clone()).unwrap();
        engine.add_container(ContainerDesc { location: path.clone(), ..Default::default() }).unwrap();
        let c = &engine.containers().unwrap()[0];
        assert_eq!((c.card_id.as_str(), c.format.as_str()), ("psp:SAVEDATA", "psp.savedata"));
        Device { engine, savedata: path, _dir: dir }
    }

    /// The game saves: `files` in the save folder `name`, replacing what was there, written
    /// `secs_ago` seconds ago.
    fn game_saves(&self, name: &str, files: &[(&str, &[u8])], secs_ago: u64) {
        let folder = self.savedata.join(name);
        let _ = fs::remove_dir_all(&folder);
        fs::create_dir(&folder).unwrap();
        for (f, data) in files {
            fs::write(folder.join(f), data).unwrap();
            age(&folder.join(f), secs_ago);
        }
        age(&folder, secs_ago);
    }

    /// File name → contents of a save folder, or `None` if it isn't there.
    fn save(&self, name: &str) -> Option<Vec<(String, Vec<u8>)>> {
        let folder = self.savedata.join(name);
        let mut out: Vec<(String, Vec<u8>)> = fs::read_dir(&folder)
            .ok()?
            .map(|e| e.unwrap())
            .map(|e| (e.file_name().into_string().unwrap(), fs::read(e.path()).unwrap()))
            .collect();
        out.sort();
        Some(out)
    }
}

fn param_sfo(text: &str) -> Vec<u8> {
    let mut b = b"\0PSF".to_vec();
    b.extend_from_slice(text.as_bytes());
    b
}

#[test]
fn saves_sync_between_memory_sticks() {
    let remote = Arc::new(MemoryRemote::new(Default::default()));
    let mut a = Device::new(&remote, "VEGA", true);
    let mut b = Device::new(&remote, "AYN Thor", true);
    let sfo = param_sfo("Daxter");
    a.game_saves("ULUS10041S0", &[("PARAM.SFO", &sfo), ("DATA.BIN", &[1; 4000]), ("ICON0.PNG", b"png")], 7200);
    // Game data installed from the disc, over the limit: never synced.
    let install = a.savedata.join("ULJM05500INSTALL");
    fs::create_dir(&install).unwrap();
    fs::File::create(install.join("DATA.BIN")).unwrap().set_len(33 << 20).unwrap();

    a.engine.sync().unwrap();
    let report = b.engine.sync().unwrap();
    assert_eq!(report.applied.len(), 1, "{report:?}");
    assert_eq!(b.save("ULUS10041S0"), a.save("ULUS10041S0"));
    assert!(!b.savedata.join("ULJM05500INSTALL").exists());
    // The save keeps its time, which PPSSPP shows; B's own files are untouched.
    let time = |d: &Device| fs::metadata(d.savedata.join("ULUS10041S0/DATA.BIN")).unwrap().modified().unwrap();
    assert_eq!(
        time(&a).duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs(),
        time(&b).duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs()
    );
    assert!(b.savedata.join(".nomedia").exists());

    let cards = b.engine.cards().unwrap();
    let unit = &cards[0].units[0];
    assert_eq!(
        (unit.unit.as_str(), unit.titles.clone()),
        ("psp:SAVEDATA:ULUS10041S0", vec!["psp:ULUS10041".to_string()])
    );

    // B plays on and saves; B also starts another game. A gets both.
    b.game_saves("ULUS10041S0", &[("PARAM.SFO", &sfo), ("DATA.BIN", &[2; 4100])], 60);
    b.game_saves("NPJH50040", &[("PARAM.SFO", &param_sfo("Other"))], 60);
    b.engine.sync().unwrap();
    a.engine.sync().unwrap();
    assert_eq!(a.save("ULUS10041S0"), b.save("ULUS10041S0"));
    assert_eq!(a.save("NPJH50040"), b.save("NPJH50040"));
    assert_eq!(fs::metadata(install.join("DATA.BIN")).unwrap().len(), 33 << 20);

    // A deletes a save: gone on B, nothing else touched.
    fs::remove_dir_all(a.savedata.join("NPJH50040")).unwrap();
    a.engine.sync().unwrap();
    b.engine.sync().unwrap();
    assert_eq!(b.save("NPJH50040"), None);
    assert_eq!(b.save("ULUS10041S0"), a.save("ULUS10041S0"));
    assert!(fs::read_dir(&b.savedata).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().contains("emusync")));
}

/// A new PPSSPP installation has `PSP` but no `SAVEDATA` yet: it is created for the saves.
#[test]
fn a_missing_savedata_folder_is_created() {
    let remote = Arc::new(MemoryRemote::new(Default::default()));
    let mut a = Device::new(&remote, "VEGA", true);
    a.game_saves("ULUS10041S0", &[("PARAM.SFO", &param_sfo("Daxter"))], 7200);
    a.engine.sync().unwrap();

    let mut b = Device::new(&remote, "AYN Thor", false);
    assert!(!b.savedata.exists());
    b.engine.sync().unwrap();
    assert_eq!(b.save("ULUS10041S0"), a.save("ULUS10041S0"));
}

/// A save folder whose files didn't change isn't read again; one changed in place is.
#[test]
fn unchanged_saves_are_not_read_again() {
    let remote = Arc::new(MemoryRemote::new(Default::default()));
    let mut a = Device::new(&remote, "VEGA", true);
    a.game_saves("ULUS10041S0", &[("DATA.BIN", b"one")], 7200);
    a.engine.sync().unwrap();
    // The same size and time: taken as unchanged, as rsync and git do.
    let data = a.savedata.join("ULUS10041S0/DATA.BIN");
    let modified = |p: &Path| fs::metadata(p).unwrap().modified().unwrap();
    let (file_time, folder_time) = (modified(&data), modified(&a.savedata.join("ULUS10041S0")));
    fs::write(&data, b"two").unwrap();
    fs::File::options().write(true).open(&data).unwrap().set_modified(file_time).unwrap();
    set_folder_time(&a.savedata.join("ULUS10041S0"), folder_time);
    assert_eq!(a.engine.sync().unwrap().new_local_versions, 0);
    // Written again later: read, and a new version.
    fs::write(&data, b"two").unwrap();
    age(&data, 30);
    assert_eq!(a.engine.sync().unwrap().new_local_versions, 1);
}

#[test]
fn savedata_syncs_per_save_only() {
    let remote = Arc::new(MemoryRemote::new(Default::default()));
    let mut a = Device::new(&remote, "VEGA", true);
    assert!(a.engine.set_card_mode("psp:SAVEDATA", CardMode::WholeFile).is_err());
}
