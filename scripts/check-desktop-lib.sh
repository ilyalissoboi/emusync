#!/usr/bin/env bash
# Checks a desktop build of the C ABI (scripts/build-desktop.sh, bindings/cmake/EmuSync.cmake):
#
#   scripts/check-desktop-lib.sh <libemusync.so | libemusync.dylib> [<expected macOS archs>]
#
# - It exports exactly the es_* functions declared in include/emusync.h, and none of the bundled
#   C libraries' symbols (zstd, SQLite, ring, BLAKE3), which would clash with a host's own copies.
# - Linux: its soname is libemusync.so, and it needs only libc-family libraries.
# - macOS: its install name is @rpath/libemusync.dylib; with a second argument such as
#   "arm64 x86_64", it holds exactly those architectures.
# Windows DLLs export only what the Rust code marks for export; the smoke test covers loading.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
lib="$1"
fail() { echo "error: $lib: $*" >&2; exit 1; }

header_functions="$(grep -oE '\bes_[a-z0-9_]+\(' "$root/include/emusync.h" | tr -d '(' | sort -u)"

case "$(uname -s)" in
    Linux)
        dynamic="$(readelf -d "$lib")"
        soname="$(sed -n 's/.*(SONAME).*\[\(.*\)\]/\1/p' <<<"$dynamic")"
        [ "$soname" = libemusync.so ] || fail "SONAME is '$soname', not libemusync.so"
        echo "SONAME: $soname"
        needed="$(sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p' <<<"$dynamic")"
        echo "DT_NEEDED: $(echo $needed)"
        for n in $needed; do
            case "$n" in
                libc.so.* | libm.so.* | libgcc_s.so.* | libdl.so.* | libpthread.so.* | librt.so.* | ld-linux*) ;;
                *) fail "unexpected DT_NEEDED $n" ;;
            esac
        done
        defined="$(nm -D --defined-only "$lib" | awk '$2 ~ /^[TtWw]$/ {print $3}' | sed 's/@.*//' | sort -u)"
        all="$(nm -D "$lib" | awk '{print $NF}')"
        ;;
    Darwin)
        install_name="$(otool -D "$lib" | tail -n +2)"
        [ "$install_name" = "@rpath/libemusync.dylib" ] || fail "install name is '$install_name'"
        echo "install name: $install_name"
        if [ $# -ge 2 ]; then
            archs="$(lipo -archs "$lib" | tr ' ' '\n' | sort | paste -sd' ' -)"
            want="$(echo "$2" | tr ' ' '\n' | sort | paste -sd' ' -)"
            [ "$archs" = "$want" ] || fail "architectures are '$archs', not '$want'"
            echo "architectures: $archs"
        fi
        # One listing per architecture in a universal library; the exports must agree.
        defined="$(nm -gU "$lib" | awk '$2 ~ /^[TtSs]$/ {print $3}' | sed 's/^_//' | sort -u)"
        all="$(nm -g "$lib" | awk '{print $NF}' | sed 's/^_//')"
        ;;
    *)
        fail "run this on Linux or macOS"
        ;;
esac

extra="$(comm -23 <(echo "$defined") <(echo "$header_functions"))"
missing="$(comm -13 <(echo "$defined") <(echo "$header_functions"))"
echo "exports: $(echo "$defined" | wc -l | tr -d ' ') functions"
[ -z "$extra" ] || fail "exports symbols not declared in emusync.h: $(echo $extra)"
[ -z "$missing" ] || fail "does not export: $(echo $missing)"
leaked="$(echo "$all" | grep -E '^(ZSTD_|ZDICT_|HUF_|FSE_|sqlite3|ring_core_|blake3_)' || true)"
[ -z "$leaked" ] || fail "dynamic symbols of the bundled C libraries: $(echo $leaked)"
echo "ok"
