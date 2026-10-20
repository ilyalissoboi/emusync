# A consumer of EmuSync.cmake on a desktop platform, the way PCSX2, Dolphin or PPSSPP would use
# it: CI configures it on Windows, macOS and Linux, builds the library and the C smoke test linked
# against it, and runs the smoke test from its build directory (ctest).
#
#   cmake -S bindings/cmake/desktop-test -B build -DCMAKE_BUILD_TYPE=Release
#   cmake --build build --config Release
#   ctest --test-dir build -C Release --output-on-failure
cmake_minimum_required(VERSION 3.19)
project(emusync_desktop_test C)

option(EMUSYNC_EXPECT_FOUND "Fail unless EmuSync.cmake enables the library" ON)

include("${CMAKE_CURRENT_LIST_DIR}/../EmuSync.cmake")

if(EMUSYNC_EXPECT_FOUND AND NOT EMUSYNC_FOUND)
	message(FATAL_ERROR "EmuSync.cmake did not enable emusync")
elseif(NOT EMUSYNC_EXPECT_FOUND AND EMUSYNC_FOUND)
	message(FATAL_ERROR "EmuSync.cmake enabled emusync where it should not")
endif()
if(NOT EMUSYNC_FOUND)
	return()
endif()

add_executable(emusync_smoke "${EMUSYNC_SOURCE_DIR}/crates/emusync-ffi/tests/c/smoke.c")
target_link_libraries(emusync_smoke PRIVATE emusync::emusync)
set_target_properties(emusync_smoke PROPERTIES C_STANDARD 99 C_STANDARD_REQUIRED ON)
# The library goes next to the executable, which is where the executable looks for it: Windows
# searches the executable's directory, and the rpath on Linux and macOS points there.
emusync_deploy_runtime(emusync_smoke)

enable_testing()
set(work "${CMAKE_CURRENT_BINARY_DIR}/smoke-work")
add_test(NAME smoke-setup COMMAND "${CMAKE_COMMAND}" -E make_directory "${work}")
add_test(NAME smoke COMMAND emusync_smoke "${work}")
set_tests_properties(smoke-setup PROPERTIES FIXTURES_SETUP smoke-work)
set_tests_properties(smoke PROPERTIES FIXTURES_REQUIRED smoke-work PASS_REGULAR_EXPRESSION "ok:")
