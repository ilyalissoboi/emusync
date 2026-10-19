# A consumer of EmuSyncAndroid.cmake, the way ARMSX2 would use it: CI configures it with the NDK
# toolchain to build libemusync.so, a shared library and the C smoke test linked against it.
#
#   cmake -S bindings/cmake/android-test -B build -G Ninja \
#     -DCMAKE_TOOLCHAIN_FILE=$ANDROID_NDK_HOME/build/cmake/android.toolchain.cmake \
#     -DANDROID_ABI=arm64-v8a -DANDROID_PLATFORM=android-26 -DCMAKE_BUILD_TYPE=Release
#   cmake --build build
cmake_minimum_required(VERSION 3.19)
project(emusync_android_test C)

# CI also configures it where the library must switch itself off (a Rust target that is not
# installed) and checks that the configure step still succeeds.
option(EMUSYNC_EXPECT_FOUND "Fail unless EmuSyncAndroid.cmake enables the library" ON)

set(CMAKE_LIBRARY_OUTPUT_DIRECTORY "${CMAKE_BINARY_DIR}/libs/${ANDROID_ABI}")
include("${CMAKE_CURRENT_LIST_DIR}/../EmuSyncAndroid.cmake")

if(EMUSYNC_EXPECT_FOUND AND NOT EMUSYNC_ANDROID_FOUND)
	message(FATAL_ERROR "EmuSyncAndroid.cmake did not enable emusync")
elseif(NOT EMUSYNC_EXPECT_FOUND AND EMUSYNC_ANDROID_FOUND)
	message(FATAL_ERROR "EmuSyncAndroid.cmake enabled emusync where it should not")
endif()
if(NOT EMUSYNC_ANDROID_FOUND)
	return()
endif()

# Stands in for ARMSX2's emucore: a shared library that calls the C ABI.
add_library(emusync_host SHARED host.c)
target_link_libraries(emusync_host PRIVATE emusync::emusync)

add_executable(emusync_smoke "${EMUSYNC_SOURCE_DIR}/crates/emusync-ffi/tests/c/smoke.c")
target_link_libraries(emusync_smoke PRIVATE emusync::emusync)
set_target_properties(emusync_smoke PROPERTIES C_STANDARD 99 C_STANDARD_REQUIRED ON)
