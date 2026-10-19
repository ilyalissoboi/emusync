#!/usr/bin/env bash
# Runs the C smoke test (crates/emusync-ffi/tests/c/smoke.c, built by bindings/cmake/android-test)
# on a running Android emulator or device: android_smoke.sh <dir with libemusync.so and emusync_smoke>
set -euo pipefail

src="$1"
dir=/data/local/tmp/emusync
adb shell "rm -rf $dir && mkdir -p $dir/work"
adb push "$src/libemusync.so" "$src/emusync_smoke" "$dir/"
adb shell "chmod 755 $dir/emusync_smoke"
out="$(adb shell "cd $dir && LD_LIBRARY_PATH=$dir ./emusync_smoke $dir/work; echo exit=\$?")"
echo "$out"
grep -q "ok:" <<<"$out" && grep -q "exit=0" <<<"$out"
