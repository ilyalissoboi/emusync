//! `emusyncw`: the same tool without a console window on Windows, for running `emusync watch`
//! at logon (`emusync service install`). It has no terminal, so it logs to a file only.

#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() -> std::process::ExitCode {
    emusyncd::main()
}
