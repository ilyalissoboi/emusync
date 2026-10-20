#!/usr/bin/env bash
# Builds the C ABI (crates/emusync-ffi) as a desktop shared library and stages it as a small
# package a host's build can consume without CMake, e.g. PCSX2's Visual Studio (MSBuild) build:
#
#   scripts/build-desktop.sh [--target <rust triple>]... [--debug] [--out <dir>]
#
#   <out>/include/emusync.h
#   Windows: <out>/bin/emusync.dll, <out>/bin/emusync.pdb, <out>/lib/emusync.lib (import library)
#   macOS:   <out>/lib/libemusync.dylib (install name @rpath/libemusync.dylib)
#   Linux:   <out>/lib/libemusync.so    (soname libemusync.so)
#
# Default target: this machine's. On macOS, --target aarch64-apple-darwin --target
# x86_64-apple-darwin builds both and joins them into one universal library. Default out:
# target/desktop. Cargo builds in CARGO_TARGET_DIR if set, else target/. Runs on Linux, macOS and
# Windows (Git Bash; cargo finds the MSVC tools itself). bindings/cmake/EmuSync.cmake does the
# same from a CMake build; the link options below match it.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
profile=release
target_dir="${CARGO_TARGET_DIR:-$root/target}"
out="$root/target/desktop"
targets=()

usage() {
    sed -n '2,17p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

while [ $# -gt 0 ]; do
    case "$1" in
        --target) targets+=("$2"); shift 2 ;;
        --debug) profile=debug; shift ;;
        --out) out="$2"; shift 2 ;;
        -h | --help) usage; exit 0 ;;
        *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
done
if [ ${#targets[@]} -eq 0 ]; then
    targets=("$(rustc -vV | sed -n 's/^host: //p')")
fi

profile_flag=()
[ "$profile" = release ] && profile_flag=(--release)

built=()
for t in "${targets[@]}"; do
    case "$t" in
        *-pc-windows-msvc) lib=emusync.dll; link=() ;;
        *-apple-darwin) lib=libemusync.dylib; link=(-C link-arg=-Wl,-install_name,@rpath/libemusync.dylib) ;;
        *-linux-gnu) lib=libemusync.so; link=(-C link-arg=-Wl,-soname,libemusync.so -C link-arg=-Wl,--exclude-libs,ALL) ;;
        *) echo "unsupported target: $t (Windows MSVC, macOS or Linux GNU)" >&2; exit 2 ;;
    esac
    # -DZSTD_TRACE=0: see bindings/cmake/EmuSync.cmake.
    under="${t//-/_}"
    env "CFLAGS_${under}=-DZSTD_TRACE=0" \
        cargo rustc --locked -p emusync-ffi --lib --crate-type cdylib "${profile_flag[@]}" \
            --manifest-path "$root/Cargo.toml" --target "$t" --target-dir "$target_dir" \
            -- "${link[@]}"
    built+=("$target_dir/$t/$profile/$lib")
done

rm -rf "$out"
mkdir -p "$out/include" "$out/lib"
cp "$root/include/emusync.h" "$out/include/"
case "${targets[0]}" in
    *-pc-windows-msvc)
        [ ${#targets[@]} -eq 1 ] || { echo "one Windows target per package" >&2; exit 2; }
        dir="$(dirname "${built[0]}")"
        mkdir -p "$out/bin"
        cp "$dir/emusync.dll" "$dir/emusync.pdb" "$out/bin/"
        # cargo names the import library emusync.dll.lib; emusync.lib is what MSBuild projects
        # conventionally list in AdditionalDependencies.
        cp "$dir/emusync.dll.lib" "$out/lib/emusync.lib"
        ;;
    *-apple-darwin)
        if [ ${#built[@]} -gt 1 ]; then
            lipo -create -output "$out/lib/libemusync.dylib" "${built[@]}"
        else
            cp "${built[0]}" "$out/lib/"
        fi
        ;;
    *)
        [ ${#targets[@]} -eq 1 ] || { echo "one Linux target per package" >&2; exit 2; }
        cp "${built[0]}" "$out/lib/"
        ;;
esac
echo "emusync package in $out:"
find "$out" -type f | sed "s|^$out/|  |"
