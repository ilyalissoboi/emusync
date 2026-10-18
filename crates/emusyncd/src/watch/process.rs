//! Process detection for the sidecar, behind a trait so tests can drive it.

use std::collections::HashSet;

/// Reports which processes are running.
pub trait ProcessProbe {
    /// Names of the running processes (e.g. `pcsx2-qt.exe`), and their executables' file names
    /// where known, since some systems shorten process names.
    fn running(&mut self) -> HashSet<String>;
}

/// The operating system's process list.
pub struct SystemProbe {
    sys: sysinfo::System,
}

impl SystemProbe {
    pub fn new() -> Self {
        Self { sys: sysinfo::System::new() }
    }
}

impl ProcessProbe for SystemProbe {
    fn running(&mut self) -> HashSet<String> {
        self.sys.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::All,
            true,
            sysinfo::ProcessRefreshKind::nothing().with_exe(sysinfo::UpdateKind::OnlyIfNotSet),
        );
        let mut names = HashSet::new();
        for p in self.sys.processes().values() {
            names.insert(p.name().to_string_lossy().into_owned());
            if let Some(exe) = p.exe().and_then(|e| e.file_name()) {
                names.insert(exe.to_string_lossy().into_owned());
            }
        }
        names
    }
}

/// A process list set by the test.
#[cfg(test)]
#[derive(Clone, Default)]
pub struct FakeProbe(std::rc::Rc<std::cell::RefCell<HashSet<String>>>);

#[cfg(test)]
impl FakeProbe {
    pub fn set(&self, names: &[&str]) {
        *self.0.borrow_mut() = names.iter().map(|n| n.to_string()).collect();
    }
}

#[cfg(test)]
impl ProcessProbe for FakeProbe {
    fn running(&mut self) -> HashSet<String> {
        self.0.borrow().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_probe_sees_this_process() {
        let me = std::env::current_exe().unwrap();
        let name = me.file_name().unwrap().to_string_lossy().into_owned();
        let running = SystemProbe::new().running();
        assert!(running.contains(&name), "{name} not in the process list");
    }
}
