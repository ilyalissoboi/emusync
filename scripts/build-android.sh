#!/usr/bin/env bash
# Builds the C ABI (crates/emusync-ffi) as an Android shared library, libemusync.so, with the
# NDK's clang as linker and C compiler. No cargo-ndk needed.
#
#   scripts/build-android.sh [--target <rust triple>]... [--api <level>] [--debug] [--out <dir>]
#
# Defaults: --target aarch64-linux-android, --api 26, release. Each library lands in
# <out>/<android abi>/libemusync.so (default out: target/android), next to which the caller puts
# include/emusync.h. The NDK comes from ANDROID_NDK_HOME, ANDROID_NDK_ROOT or ANDROID_NDK.
# Works on Linux, macOS and Windows (Git Bash). bindings/cmake/EmuSyncAndroid.cmake does the same
# from a CMake build.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
api=26
profile=release
out="$root/target/android"
targets=()

usage() {
    sed -n '2,12p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

while [ $# -gt 0 ]; do
    case "$1" in
        --target) targets+=("$2"); shift 2 ;;
        --api) api="$2"; shift 2 ;;
        --debug) profile=debug; shift ;;
        --out) out="$2"; shift 2 ;;
        -h | --help) usage; exit 0 ;;
        *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
done
[ ${#targets[@]} -gt 0 ] || targets=(aarch64-linux-android)

ndk="${ANDROID_NDK_HOME:-${ANDROID_NDK_ROOT:-${ANDROID_NDK:-}}}"
if [ -z "$ndk" ] || [ ! -d "$ndk" ]; then
    echo "error: set ANDROID_NDK_HOME to an Android NDK" >&2
    exit 1
fi
case "$(uname -s)" in
    Linux) host=linux-x86_64 ;;
    Darwin) host=darwin-x86_64 ;; # the NDK ships universal binaries under this name
    MINGW* | MSYS* | CYGWIN*) host=windows-x86_64 ;;
    *) echo "error: unsupported host $(uname -s)" >&2; exit 1 ;;
esac
tc="$ndk/toolchains/llvm/prebuilt/$host"
exe=""
[ "$host" = windows-x86_64 ] && exe=".exe"

for target in "${targets[@]}"; do
    case "$target" in
        aarch64-linux-android) abi=arm64-v8a clang_triple=aarch64-linux-android ;;
        x86_64-linux-android) abi=x86_64 clang_triple=x86_64-linux-android ;;
        armv7-linux-androideabi) abi=armeabi-v7a clang_triple=armv7a-linux-androideabi ;;
        i686-linux-android) abi=x86 clang_triple=i686-linux-android ;;
        *) echo "error: unsupported target $target" >&2; exit 1 ;;
    esac
    # The NDK's clang with an explicit --target, rather than its <triple><api>-clang wrapper
    # scripts: on Windows those are .cmd files, which cargo and the cc crate cannot run reliably.
    clang="$tc/bin/clang$exe"
    if [ ! -x "$clang" ]; then
        echo "error: no clang at $clang" >&2
        exit 1
    fi
    under="${target//-/_}"
    upper="$(echo "$under" | tr '[:lower:]' '[:upper:]')"
    profile_flag=()
    [ "$profile" = release ] && profile_flag=(--release)

    echo "building libemusync.so for $target (API $api, $profile)" >&2
    env \
        "CARGO_TARGET_${upper}_LINKER=$clang" \
        "CC_${under}=$clang" \
        "CFLAGS_${under}=--target=${clang_triple}${api} -DZSTD_TRACE=0" \
        "AR_${under}=$tc/bin/llvm-ar$exe" \
        "RANLIB_${under}=$tc/bin/llvm-ranlib$exe" \
        cargo rustc --locked -p emusync-ffi --lib --crate-type cdylib "${profile_flag[@]}" \
        --manifest-path "$root/Cargo.toml" --target "$target" -- \
        -C "link-arg=--target=${clang_triple}${api}" \
        -C link-arg=-Wl,-soname,libemusync.so \
        -C link-arg=-Wl,--exclude-libs,ALL \
        -C link-arg=-Wl,-z,max-page-size=16384

    mkdir -p "$out/$abi"
    cp "$root/target/$target/$profile/libemusync.so" "$out/$abi/libemusync.so"
    echo "$out/$abi/libemusync.so"
done
