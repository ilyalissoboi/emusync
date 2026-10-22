#!/usr/bin/env bash
# Runs the AAR's instrumented tests on a running Android emulator or device:
#   android_instrument.sh <dir with the APKs the android-aar job built>
# - emusync-debug-androidTest.apk: the Kotlin API's tests (SyncSettings with the real Keystore,
#   SyncController against a fake backend); the library's test APK carries the library itself.
# - prefab-test-debug.apk and its test APK: a host app that links libemusync.so through the AAR's
#   Prefab package, loads it and creates a client in its sandbox.
set -euo pipefail

apks="$1"
run() {
    local package="$1"
    local out
    out="$(adb shell am instrument -w -r "$package/androidx.test.runner.AndroidJUnitRunner")"
    echo "$out"
    # A passing run ends with "OK (n tests)"; a failing or crashed one doesn't.
    grep -q "^OK (" <<<"$out"
}

adb install -r -t "$apks/emusync-debug-androidTest.apk"
run io.github.ilyalissoboi.emusync.test

adb install -r -t "$apks/prefab-test-debug.apk"
adb install -r -t "$apks/prefab-test-debug-androidTest.apk"
run io.github.ilyalissoboi.emusync.prefabtest.test
