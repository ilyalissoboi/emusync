//! The C ABI as a C or C++ host sees it: programs in tests/c/ are compiled with the platform's C
//! compiler (found by the `cc` crate) against include/emusync.h, linked with the static library,
//! and run.

use emusync::abi;
use std::mem::{offset_of, size_of};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The static library, built once per test run. `cargo test` only builds the rlib that the
/// tests link, so this runs `cargo build` for the library, in a target directory of its own: the
/// outer cargo still holds the lock on the main one.
fn static_lib() -> &'static Path {
    static LIB: OnceLock<PathBuf> = OnceLock::new();
    LIB.get_or_init(|| {
        let target_dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("c-abi");
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let output = Command::new(cargo)
            .args(["build", "-p", "emusync-ffi", "--lib", "--manifest-path"])
            .arg(crate_dir().join("Cargo.toml"))
            .arg("--target-dir")
            .arg(&target_dir)
            .output()
            .expect("cannot run cargo");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "building the static library failed:\n{stderr}");
        ["libemusync.a", "emusync.lib"]
            .iter()
            .map(|n| target_dir.join("debug").join(n))
            .find(|p| p.exists())
            .unwrap_or_else(|| panic!("no static library in {}", target_dir.join("debug").display()))
    })
}

/// What Rust's standard library needs from the system (`--print native-static-libs`).
fn system_libs(msvc: bool) -> Vec<&'static str> {
    if msvc {
        vec!["bcrypt.lib", "advapi32.lib", "kernel32.lib", "ntdll.lib", "userenv.lib", "ws2_32.lib", "dbghelp.lib"]
    } else if cfg!(target_os = "macos") {
        vec!["-framework", "CoreFoundation", "-framework", "Security", "-lSystem", "-lc", "-lm"]
    } else {
        vec!["-lgcc_s", "-lutil", "-lrt", "-lpthread", "-lm", "-ldl", "-lc"]
    }
}

/// Compiles and links one program; returns the executable.
fn build(src: &str, out: &Path) -> PathBuf {
    let cpp = src.ends_with(".cpp");
    let mut b = cc::Build::new();
    b.target(env!("EMUSYNC_BUILD_TARGET"))
        .host(env!("EMUSYNC_BUILD_HOST"))
        .opt_level(0)
        .debug(false)
        .cargo_metadata(false)
        .cpp(cpp)
        .warnings(true)
        .extra_warnings(true)
        .warnings_into_errors(true)
        .out_dir(out)
        .include(crate_dir().join("../../include"));
    let tool = b.get_compiler();
    let msvc = tool.is_like_msvc();
    if !msvc {
        b.flag(if cpp { "-std=c++11" } else { "-std=c99" });
        b.flag("-pedantic");
    }
    let tool = b.get_compiler();
    let name = Path::new(src).file_stem().unwrap().to_str().unwrap();
    let exe = out.join(if cfg!(windows) { format!("{name}.exe") } else { name.to_string() });
    let mut cmd = tool.to_command();
    cmd.arg(crate_dir().join("tests/c").join(src));
    if msvc {
        cmd.arg(format!("/Fe{}", exe.display()));
        cmd.arg(format!("/Fo{}\\", out.display()));
        cmd.arg("/link").arg(static_lib()).args(system_libs(true));
    } else {
        cmd.arg("-o").arg(&exe).arg(static_lib()).args(system_libs(false));
        if !cfg!(target_os = "macos") {
            cmd.arg("-pthread");
        }
    }
    let output = cmd.output().unwrap_or_else(|e| panic!("cannot run {cmd:?}: {e}"));
    assert!(
        output.status.success(),
        "building {src} failed: {cmd:?}\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    exe
}

fn run(exe: &Path, args: &[&Path]) -> String {
    let output = Command::new(exe).args(args).output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "{} failed ({}):\n{stdout}\n{}",
        exe.display(),
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    stdout
}

#[test]
fn c_smoke_test() {
    let dir = tempfile::tempdir().unwrap();
    let exe = build("smoke.c", dir.path());
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let out = run(&exe, &[&work]);
    assert!(out.contains("ok:"), "{out}");
}

#[test]
fn header_compiles_as_cpp() {
    let dir = tempfile::tempdir().unwrap();
    let exe = build("header_check.cpp", dir.path());
    assert_eq!(run(&exe, &[]).trim(), "no space in container");
}

/// Every struct size and field offset in the header equals the Rust definition.
#[test]
fn struct_layouts_match_the_header() {
    let dir = tempfile::tempdir().unwrap();
    let exe = build("layout.c", dir.path());
    let c_side = run(&exe, &[]);

    let mut rust = Vec::new();
    macro_rules! s {
        ($t:ident) => {
            rust.push(format!("{} {}", stringify!($t), size_of::<abi::$t>()))
        };
    }
    macro_rules! f {
        ($t:ident, $($f:ident),*) => {
            $(rust.push(format!("{}.{} {}", stringify!($t), stringify!($f), offset_of!(abi::$t, $f)));)*
        };
    }
    for e in ["es_status", "es_log_level", "es_event_type", "es_card_mode"] {
        rust.push(format!("{e} 4"));
    }
    s!(es_config);
    f!(
        es_config,
        data_dir,
        device_name,
        emulator_id,
        emulator_version,
        vfs,
        conflict_policy,
        flags,
        snapshot_budget_bytes,
        log,
        log_user,
        log_level
    );
    s!(es_vfs_stat);
    f!(es_vfs_stat, mtime_unix_ms, is_dir);
    s!(es_vfs);
    f!(es_vfs, user, stat, list, read, write, mkdir, rename, remove, set_mtime, join, parent, name);
    s!(es_credentials);
    f!(es_credentials, kind, access_key_id, secret_access_key, session_token, expires_unix_ms, bearer_token);
    s!(es_credential_provider);
    f!(es_credential_provider, get);
    s!(es_container_desc);
    f!(es_container_desc, format, location, platform, card, title_scope, flags);
    s!(es_title);
    f!(es_title, platform, serial, content_hash, display_name);
    s!(es_session_begin_args);
    f!(es_session_begin_args, title, mounted, mounted_count, timeout_ms, flags, state_compat_key);
    s!(es_session_info);
    f!(
        es_session_info,
        id,
        units_applied,
        units_blocked,
        conflicts,
        remote_reached,
        other_device_active,
        resume_state_id,
        resume_state_unix_ms
    );
    s!(es_state_desc);
    f!(es_state_desc, title, path, compat_key, slot, screenshot_path);
    s!(es_state_info);
    f!(es_state_info, id, device_name, created_unix_ms, size, stale_vs_saves);
    s!(es_version_info);
    f!(es_version_info, version_id, device_name, modified_unix_ms, playtime_ms, size, is_tombstone);
    s!(es_conflict_info);
    f!(es_conflict_info, container, unit_id, unit_name, unit_title, title_key, local, remote, remote_count);
    s!(es_event);
    rust.push(format!("es_event.type {}", offset_of!(abi::es_event, type_)));
    f!(
        es_event,
        status,
        container,
        conflict_id,
        bytes_done,
        bytes_total,
        title_key,
        unit_id,
        unit_name,
        device_name,
        message,
        _internal
    );

    let c_lines: Vec<&str> = c_side.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    assert_eq!(c_lines, rust);
}
