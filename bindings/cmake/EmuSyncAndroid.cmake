# EmuSyncAndroid.cmake: build emusync's C ABI as libemusync.so from an Android NDK CMake build
# (Android Gradle Plugin `externalNativeBuild`, or a plain NDK toolchain build) and link it as an
# IMPORTED shared library. Modelled on how ARMSX2 builds librashader
# (platforms/android/app/src/main/cpp/3rdparty/librashader/CMakeLists.txt).
#
#   include(<emusync checkout>/bindings/cmake/EmuSyncAndroid.cmake)
#   if(EMUSYNC_ANDROID_FOUND)
#     target_link_libraries(mylib PRIVATE emusync::emusync)
#     target_compile_definitions(mylib PRIVATE HAS_EMUSYNC)
#   endif()
#
# Result:
#   EMUSYNC_ANDROID_FOUND  ON when the library will be built; OFF (with a STATUS message) when
#                          cargo, rustc or the Rust target for this ABI is missing, so callers can
#                          compile the feature out instead of failing.
#   emusync::emusync       IMPORTED SHARED target with include/emusync.h on its include path.
#   emusync_cargo          the custom target that runs cargo. It always runs (cargo's no-op build
#                          takes about a second), so edits to the Rust sources are picked up, and
#                          then copies libemusync.so into CMAKE_LIBRARY_OUTPUT_DIRECTORY (the
#                          per-ABI directory AGP packages) when that is set.
#   EMUSYNC_ANDROID_LIBRARY  path of the built libemusync.so.
#
# Options (cache variables, set before the include):
#   EMUSYNC_SOURCE_DIR     the emusync checkout (default: two levels above this file)
#   EMUSYNC_CARGO_PROFILE  release or debug (default: debug for CMAKE_BUILD_TYPE=Debug, else release)
#   EMUSYNC_CARGO_TARGET_DIR  cargo's target directory (default: emusync-cargo in the current
#                          binary directory; point several builds at one to share the cache)
#   EMUSYNC_CARGO, EMUSYNC_RUSTC  cargo and rustc (found on PATH and in ~/.cargo/bin)
#
# Needs CMake 3.19 or later (add_dependencies on an IMPORTED target).

include_guard(GLOBAL)

set(EMUSYNC_SOURCE_DIR "${CMAKE_CURRENT_LIST_DIR}/../.." CACHE PATH "emusync source checkout")
if(CMAKE_BUILD_TYPE STREQUAL "Debug")
	set(_emusync_default_profile debug)
else()
	set(_emusync_default_profile release)
endif()
set(EMUSYNC_CARGO_PROFILE "${_emusync_default_profile}" CACHE STRING "Cargo profile for libemusync.so: release or debug")
set_property(CACHE EMUSYNC_CARGO_PROFILE PROPERTY STRINGS release debug)
set(EMUSYNC_CARGO_TARGET_DIR "" CACHE PATH "Cargo target directory for libemusync.so (empty: one per binary directory)")

function(_emusync_android_setup)
	set(EMUSYNC_ANDROID_FOUND OFF PARENT_SCOPE)
	get_filename_component(src "${EMUSYNC_SOURCE_DIR}" ABSOLUTE)

	if(NOT (ANDROID OR CMAKE_SYSTEM_NAME STREQUAL "Android"))
		message(STATUS "emusync: not an Android build -> DISABLED")
		return()
	endif()

	# ---- Android ABI and API level -> Rust target and clang target
	set(abi "${ANDROID_ABI}")
	if(NOT abi)
		set(abi "${CMAKE_ANDROID_ARCH_ABI}")
	endif()
	if(abi STREQUAL "arm64-v8a")
		set(rust_target aarch64-linux-android)
		set(clang_triple aarch64-linux-android)
	elseif(abi STREQUAL "x86_64")
		set(rust_target x86_64-linux-android)
		set(clang_triple x86_64-linux-android)
	elseif(abi STREQUAL "armeabi-v7a")
		set(rust_target armv7-linux-androideabi)
		set(clang_triple armv7a-linux-androideabi)
	elseif(abi STREQUAL "x86")
		set(rust_target i686-linux-android)
		set(clang_triple i686-linux-android)
	else()
		message(STATUS "emusync: unknown Android ABI '${abi}' -> DISABLED")
		return()
	endif()
	if(ANDROID_PLATFORM_LEVEL)
		set(api "${ANDROID_PLATFORM_LEVEL}")
	elseif(ANDROID_NATIVE_API_LEVEL)
		set(api "${ANDROID_NATIVE_API_LEVEL}")
	elseif(CMAKE_SYSTEM_VERSION)
		set(api "${CMAKE_SYSTEM_VERSION}")
	else()
		set(api 26)
	endif()

	# ---- Rust toolchain (optional)
	find_program(EMUSYNC_CARGO cargo HINTS "$ENV{CARGO_HOME}/bin" "$ENV{HOME}/.cargo/bin" "$ENV{USERPROFILE}/.cargo/bin")
	find_program(EMUSYNC_RUSTC rustc HINTS "$ENV{CARGO_HOME}/bin" "$ENV{HOME}/.cargo/bin" "$ENV{USERPROFILE}/.cargo/bin")
	if(NOT EMUSYNC_CARGO OR NOT EXISTS "${EMUSYNC_CARGO}" OR NOT EMUSYNC_RUSTC OR NOT EXISTS "${EMUSYNC_RUSTC}")
		message(STATUS "emusync: cargo or rustc not found -> DISABLED. Install Rust (https://rustup.rs) to enable it.")
		return()
	endif()
	# rustc prints the target's library directory whether or not it is installed; it exists only
	# when it is. Run from the checkout so a rust-toolchain file there would apply.
	execute_process(
		COMMAND "${EMUSYNC_RUSTC}" --print target-libdir --target ${rust_target}
		WORKING_DIRECTORY "${src}"
		OUTPUT_VARIABLE libdir OUTPUT_STRIP_TRAILING_WHITESPACE
		RESULT_VARIABLE rc ERROR_QUIET)
	if(NOT rc EQUAL 0 OR NOT IS_DIRECTORY "${libdir}")
		message(STATUS "emusync: Rust target ${rust_target} not installed -> DISABLED. Run: rustup target add ${rust_target}")
		return()
	endif()

	# ---- NDK clang, used by cargo as linker and by the cc crate as C compiler (ring, SQLite,
	# zstd, blake3). The plain clang with --target rather than the <triple><api>-clang wrappers,
	# which are .cmd scripts on Windows.
	if(NOT CMAKE_C_COMPILER_ID MATCHES "Clang")
		message(STATUS "emusync: C compiler is not the NDK's clang -> DISABLED")
		return()
	endif()
	set(clang "${CMAKE_C_COMPILER}")
	set(ar "${CMAKE_AR}")
	if(NOT ar)
		get_filename_component(bin "${clang}" DIRECTORY)
		set(ar "${bin}/llvm-ar${CMAKE_EXECUTABLE_SUFFIX}")
	endif()
	string(REPLACE "-" "_" under "${rust_target}")
	string(TOUPPER "${under}" upper)

	set(profile "${EMUSYNC_CARGO_PROFILE}")
	if(profile STREQUAL "release")
		set(profile_flag --release)
	elseif(profile STREQUAL "debug")
		set(profile_flag "")
	else()
		message(FATAL_ERROR "emusync: EMUSYNC_CARGO_PROFILE must be release or debug, not '${profile}'")
	endif()
	set(target_dir "${EMUSYNC_CARGO_TARGET_DIR}")
	if(NOT target_dir)
		set(target_dir "${CMAKE_CURRENT_BINARY_DIR}/emusync-cargo")
	endif()
	set(lib "${target_dir}/${rust_target}/${profile}/libemusync.so")

	# The link arguments go to the final rustc call only (after `--`), so they do not invalidate
	# the dependencies' build cache the way RUSTFLAGS would:
	# - rustc sets no SONAME on an Android cdylib. Without one the consumer's DT_NEEDED is the
	#   build-tree path and the library cannot be found at runtime.
	# - 16 KB page alignment, which Android 15+ devices with 16 KB pages require.
	# - --exclude-libs,ALL keeps symbols that come from dependency archives out of the dynamic
	#   symbol table. rustc's version script already hides most of them, but it exports every
	#   #[no_mangle] function of every crate, which includes a blake3 callback for its NEON code.
	# -DZSTD_TRACE=0 drops zstd's weak, undefined ZSTD_trace_* hooks, which a ZSTD_trace_* defined
	# anywhere else in the process (the host's own zstd) would otherwise resolve.
	add_custom_target(emusync_cargo
		COMMAND "${CMAKE_COMMAND}" -E env
			"CARGO_TARGET_${upper}_LINKER=${clang}"
			"CC_${under}=${clang}"
			"CFLAGS_${under}=--target=${clang_triple}${api} -DZSTD_TRACE=0"
			"AR_${under}=${ar}"
			"${EMUSYNC_CARGO}" rustc --locked -p emusync-ffi --lib --crate-type cdylib ${profile_flag}
				--manifest-path "${src}/Cargo.toml"
				--target ${rust_target}
				--target-dir "${target_dir}"
				--
				-C "link-arg=--target=${clang_triple}${api}"
				-C link-arg=-Wl,-soname,libemusync.so
				-C link-arg=-Wl,-z,max-page-size=16384
				-C link-arg=-Wl,--exclude-libs,ALL
		BYPRODUCTS "${lib}"
		WORKING_DIRECTORY "${src}"
		COMMENT "emusync: cargo build of libemusync.so (${rust_target}, API ${api}, ${profile})"
		VERBATIM)

	# AGP packages only the .so files in the per-ABI library output directory; a library that
	# cargo builds behind an IMPORTED target is not one of them. Without this copy the host links
	# and then fails at runtime with UnsatisfiedLinkError. emusync needs no libc++_shared.so: it
	# has no C++ dependencies.
	if(CMAKE_LIBRARY_OUTPUT_DIRECTORY)
		add_custom_command(TARGET emusync_cargo POST_BUILD
			COMMAND "${CMAKE_COMMAND}" -E make_directory "${CMAKE_LIBRARY_OUTPUT_DIRECTORY}"
			COMMAND "${CMAKE_COMMAND}" -E copy_if_different "${lib}" "${CMAKE_LIBRARY_OUTPUT_DIRECTORY}/"
			COMMENT "emusync: staging libemusync.so for packaging"
			VERBATIM)
	endif()

	add_library(emusync::emusync SHARED IMPORTED GLOBAL)
	set_target_properties(emusync::emusync PROPERTIES
		IMPORTED_LOCATION "${lib}"
		IMPORTED_SONAME libemusync.so
		INTERFACE_INCLUDE_DIRECTORIES "${src}/include")
	add_dependencies(emusync::emusync emusync_cargo)

	set(EMUSYNC_ANDROID_FOUND ON PARENT_SCOPE)
	set(EMUSYNC_ANDROID_LIBRARY "${lib}" PARENT_SCOPE)
	message(STATUS "emusync: ENABLED (${rust_target}, API ${api}, ${profile})")
endfunction()

_emusync_android_setup()
