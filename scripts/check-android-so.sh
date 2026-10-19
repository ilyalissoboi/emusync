#!/usr/bin/env bash
# Checks an Android libemusync.so (from scripts/build-android.sh or EmuSyncAndroid.cmake):
#
#   scripts/check-android-so.sh <libemusync.so>...
#
# - SONAME is libemusync.so, so a host that links it records the bare name in DT_NEEDED;
# - DT_NEEDED lists only libc, libm, libdl and liblog (no libc++_shared.so);
# - the dynamic symbol table defines exactly the functions declared in include/emusync.h, and
#   neither defines nor references any symbol of the bundled C libraries (zstd, SQLite, ring,
#   blake3), so they cannot clash with the host's own copies;
# - every PT_LOAD segment is aligned to at least 16 KB.
#
# Uses llvm-readelf and llvm-nm from the NDK (ANDROID_NDK_HOME, ANDROID_NDK_ROOT or
# ANDROID_NDK), or from PATH.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
[ $# -gt 0 ] || { sed -n '2,13p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//' >&2; exit 2; }

ndk="${ANDROID_NDK_HOME:-${ANDROID_NDK_ROOT:-${ANDROID_NDK:-}}}"
readelf=llvm-readelf
nm=llvm-nm
if [ -n "$ndk" ]; then
    for bin in "$ndk"/toolchains/llvm/prebuilt/*/bin; do
        if [ -e "$bin/llvm-readelf" ] || [ -e "$bin/llvm-readelf.exe" ]; then
            readelf="$bin/llvm-readelf"
            nm="$bin/llvm-nm"
        fi
    done
fi

failed=0
fail() {
    echo "FAIL: $*" >&2
    failed=1
}

header_functions="$(grep -oE '\bes_[a-z0-9_]+\(' "$root/include/emusync.h" | tr -d '(' | sort -u)"

for so in "$@"; do
    echo "== $so"
    dynamic="$("$readelf" -d "$so")"

    soname="$(sed -n 's/.*(SONAME).*\[\(.*\)\]/\1/p' <<<"$dynamic")"
    [ "$soname" = libemusync.so ] || fail "SONAME is '$soname', not libemusync.so"
    echo "SONAME: $soname"

    needed="$(sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p' <<<"$dynamic")"
    echo "DT_NEEDED: $(echo $needed)"
    for lib in $needed; do
        case "$lib" in
            libc.so | libm.so | libdl.so | liblog.so) ;;
            *) fail "unexpected DT_NEEDED $lib" ;;
        esac
    done

    defined="$("$nm" -D --defined-only "$so" | awk '{print $NF}' | sort -u)"
    extra="$(comm -23 <(echo "$defined") <(echo "$header_functions"))"
    missing="$(comm -13 <(echo "$defined") <(echo "$header_functions"))"
    echo "exports: $(echo "$defined" | wc -l) symbols"
    [ -z "$extra" ] || fail "exports symbols not declared in emusync.h: $(echo $extra)"
    [ -z "$missing" ] || fail "does not export: $(echo $missing)"

    leaked="$("$nm" -D "$so" | awk '{print $NF}' | grep -E '^(ZSTD_|ZDICT_|HUF_|FSE_|sqlite3|ring_core_|blake3_)' || true)"
    [ -z "$leaked" ] || fail "dynamic symbols of the bundled C libraries: $(echo $leaked)"

    aligns="$("$readelf" -lW "$so" | awk '$1 == "LOAD" {print $NF}')"
    echo "PT_LOAD alignment: $(echo $aligns)"
    [ -n "$aligns" ] || fail "no PT_LOAD segments"
    for a in $aligns; do
        [ $((a)) -ge 16384 ] || fail "PT_LOAD aligned to $a, less than 16 KB"
    done
done

if [ "$failed" -ne 0 ]; then
    exit 1
fi
echo "all checks passed"
