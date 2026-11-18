#!/usr/bin/env bash
# Checks a desktop build of the C ABI (scripts/build-desktop.sh, bindings/cmake/EmuSync.cmake):
#
#   scripts/check-desktop-lib.sh <libemusync.so | libemusync.dylib | emusync.dll> [<expected archs>]
#
# - It exports exactly the es_* functions declared in include/emusync.h, and none of the bundled
#   C libraries' symbols (zstd, SQLite, ring, BLAKE3), which would clash with a host's own copies.
# - Linux: its soname is libemusync.so, it needs only libc-family libraries, and no glibc newer
#   than 2.34 (EMUSYNC_GLIBC_MAX overrides that); with a second argument, x86_64 or aarch64, it
#   is built for that architecture.
# - macOS: its install name is @rpath/libemusync.dylib; with a second argument such as
#   "arm64 x86_64", it holds exactly those architectures.
# - Windows (Git Bash): with a second argument, x64 or arm64, the DLL is built for that
#   architecture. DLLs export only what the Rust code marks for export; the smoke test covers
#   loading.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
lib="$1"
want_arch="${2:-}"
fail() { echo "error: $lib: $*" >&2; exit 1; }
[ -f "$lib" ] || fail "no such file"
check_arch() {
    [ -z "$want_arch" ] || [ "$1" = "$want_arch" ] || fail "architecture is '$1', not '$want_arch'"
    echo "architecture: $1"
}

header_functions="$(grep -oE '\bes_[a-z0-9_]+\(' "$root/include/emusync.h" | tr -d '(' | sort -u)"

case "$(uname -s)" in
    Linux)
        machine="$(readelf -h "$lib" | sed -n 's/^ *Machine: *//p')"
        case "$machine" in
            *X86-64) check_arch x86_64 ;;
            AArch64) check_arch aarch64 ;;
            *) check_arch "$machine" ;;
        esac
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
        # The newest glibc symbol version it links; the x86_64 library of 0.3.0 needed 2.34.
        glibc="$(readelf -V --wide "$lib" | grep -oE 'GLIBC_[0-9]+(\.[0-9]+)+' | sed 's/^GLIBC_//' | sort -uV | tail -1)"
        glibc_max="${EMUSYNC_GLIBC_MAX:-2.34}"
        newest="$(printf '%s\n' "$glibc" "$glibc_max" | sort -V | tail -1)"
        [ "$newest" = "$glibc_max" ] || fail "needs glibc $glibc, newer than $glibc_max"
        echo "glibc: $glibc (at most $glibc_max)"
        defined="$(nm -D --defined-only "$lib" | awk '$2 ~ /^[TtWw]$/ {print $3}' | sed 's/@.*//' | sort -u)"
        all="$(nm -D "$lib" | awk '{print $NF}')"
        ;;
    Darwin)
        # One "<file> (architecture …):" header per slice of a universal library, each followed
        # by that slice's install name; every one must be the @rpath name.
        install_name="$(otool -D "$lib" | grep -v ':$' | sort -u)"
        [ "$install_name" = "@rpath/libemusync.dylib" ] || fail "install names are '$(echo $install_name)'"
        echo "install name: $install_name"
        if [ -n "$want_arch" ]; then
            archs="$(lipo -archs "$lib" | tr ' ' '\n' | sort | paste -sd' ' -)"
            want="$(echo "$want_arch" | tr ' ' '\n' | sort | paste -sd' ' -)"
            [ "$archs" = "$want" ] || fail "architectures are '$archs', not '$want'"
            echo "architectures: $archs"
        fi
        # One listing per architecture in a universal library; the exports must agree.
        defined="$(nm -gU "$lib" | awk '$2 ~ /^[TtSs]$/ {print $3}' | sed 's/^_//' | sort -u)"
        all="$(nm -g "$lib" | awk '{print $NF}' | sed 's/^_//')"
        ;;
    MINGW* | MSYS* | CYGWIN*)
        # The PE header's machine field: the header's offset is at 0x3c, the field 4 bytes in.
        pe="$(od -An -tu4 -j60 -N4 "$lib" | tr -d ' ')"
        machine="$(od -An -tx2 -j$((pe + 4)) -N2 "$lib" | tr -d ' ')"
        case "$machine" in
            8664) check_arch x64 ;;
            aa64) check_arch arm64 ;;
            *) check_arch "PE machine 0x$machine" ;;
        esac
        echo "ok"
        exit 0
        ;;
    *)
        fail "run this on Linux, macOS or Windows (Git Bash)"
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
