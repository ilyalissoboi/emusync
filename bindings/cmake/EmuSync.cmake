# EmuSync.cmake: build emusync's C ABI as a shared library from a CMake build and link it as an
# IMPORTED target, on every platform emusync supports:
#
#   Windows   emusync.dll + emusync.dll.lib (MSVC or clang-cl; x64 or ARM64)
#   macOS     libemusync.dylib, install name @rpath/libemusync.dylib; universal when
#             CMAKE_OSX_ARCHITECTURES names both arm64 and x86_64
#   Linux     libemusync.so, soname libemusync.so
#   Android   libemusync.so per ABI, through EmuSyncAndroid.cmake (same results as below)
#
# One include serves a project that builds desktop and Android from one CMake tree (Dolphin,
# PPSSPP):
#
#   include(<emusync checkout>/bindings/cmake/EmuSync.cmake)
#   if(EMUSYNC_FOUND)
#     target_link_libraries(myapp PRIVATE emusync::emusync)
#     target_compile_definitions(myapp PRIVATE HAS_EMUSYNC)
#     emusync_deploy_runtime(myapp)   # desktop: put the library next to myapp, and find it there
#   endif()
#
# Why a shared library and not the static one: emusync bundles its own zstd and SQLite. Linked
# statically, they would meet the host's own copies (PCSX2, Dolphin and PPSSPP all link zstd) in
# one symbol namespace. The shared library keeps them private: on Linux with
# --exclude-libs,ALL, on macOS and Windows because only the es_* functions are exported.
#
# Result:
#   EMUSYNC_FOUND          ON when the library will be built; OFF (with a STATUS message) when
#                          cargo, rustc or the Rust target is missing, or the platform or compiler
#                          is not supported, so callers can compile the feature out.
#   emusync::emusync       IMPORTED SHARED target with include/emusync.h on its include path.
#   emusync_cargo          the custom target that runs cargo. It always runs (cargo's no-op build
#                          takes about a second), so edits to the Rust sources are picked up.
#   EMUSYNC_LIBRARY        the runtime file: emusync.dll, libemusync.dylib or libemusync.so.
#   EMUSYNC_IMPORT_LIBRARY Windows only: emusync.dll.lib, what the linker reads.
#   emusync_deploy_runtime(<target> [DESTINATION <dir>])
#                          desktop only (a no-op on Android, where EmuSyncAndroid.cmake stages the
#                          library for packaging): copies the library next to <target>'s output,
#                          or into <dir>, after each build, and on Linux and macOS adds
#                          $ORIGIN / @loader_path to <target>'s build rpath so it is found there.
#                          A macOS app bundle passes its Contents/Frameworks directory and adds
#                          @executable_path/../Frameworks to its rpath itself.
#
# Options (cache variables, set before the include):
#   EMUSYNC_SOURCE_DIR     the emusync checkout (default: two levels above this file)
#   EMUSYNC_CARGO_PROFILE  release or debug (default: debug for CMAKE_BUILD_TYPE=Debug, else
#                          release, which is what multi-config generators get)
#   EMUSYNC_CARGO_TARGET_DIR  cargo's target directory (default: emusync-cargo in the current
#                          binary directory)
#   EMUSYNC_RUST_TARGET    desktop only: build for this Rust target instead of the one derived
#                          from the platform, e.g. for a cross build
#   EMUSYNC_CARGO, EMUSYNC_RUSTC  cargo and rustc (found on PATH and in ~/.cargo/bin)
#
# Needs CMake 3.19 or later (add_dependencies on an IMPORTED target).

include_guard(GLOBAL)

if(ANDROID OR CMAKE_SYSTEM_NAME STREQUAL "Android")
	include("${CMAKE_CURRENT_LIST_DIR}/EmuSyncAndroid.cmake")
	set(EMUSYNC_FOUND ${EMUSYNC_ANDROID_FOUND})
	set(EMUSYNC_LIBRARY "${EMUSYNC_ANDROID_LIBRARY}")
	function(emusync_deploy_runtime)
	endfunction()
	return()
endif()

set(EMUSYNC_SOURCE_DIR "${CMAKE_CURRENT_LIST_DIR}/../.." CACHE PATH "emusync source checkout")
if(CMAKE_BUILD_TYPE STREQUAL "Debug")
	set(_emusync_default_profile debug)
else()
	set(_emusync_default_profile release)
endif()
set(EMUSYNC_CARGO_PROFILE "${_emusync_default_profile}" CACHE STRING "Cargo profile for the emusync library: release or debug")
set_property(CACHE EMUSYNC_CARGO_PROFILE PROPERTY STRINGS release debug)
set(EMUSYNC_CARGO_TARGET_DIR "" CACHE PATH "Cargo target directory for the emusync library (empty: one per binary directory)")
set(EMUSYNC_RUST_TARGET "" CACHE STRING "Rust target to build the emusync library for (empty: derived from the platform)")

# Maps a CMake processor or macOS architecture name to the Rust architecture name.
function(_emusync_rust_arch name out)
	string(TOLOWER "${name}" n)
	if(n MATCHES "^(arm64|aarch64|arm64e)$")
		set(${out} aarch64 PARENT_SCOPE)
	elseif(n MATCHES "^(x86_64|amd64|x64|em64t)$")
		set(${out} x86_64 PARENT_SCOPE)
	else()
		set(${out} "" PARENT_SCOPE)
	endif()
endfunction()

function(_emusync_desktop_setup)
	set(EMUSYNC_FOUND OFF PARENT_SCOPE)
	get_filename_component(src "${EMUSYNC_SOURCE_DIR}" ABSOLUTE)

	# ---- Platform -> Rust targets, library file names
	set(targets "")
	if(EMUSYNC_RUST_TARGET)
		set(targets "${EMUSYNC_RUST_TARGET}")
	elseif(APPLE)
		set(archs ${CMAKE_OSX_ARCHITECTURES})
		if(NOT archs)
			set(archs "${CMAKE_SYSTEM_PROCESSOR}")
		endif()
		foreach(a IN LISTS archs)
			_emusync_rust_arch("${a}" arch)
			if(NOT arch)
				message(STATUS "emusync: unsupported macOS architecture '${a}' -> DISABLED")
				return()
			endif()
			list(APPEND targets "${arch}-apple-darwin")
		endforeach()
		list(REMOVE_DUPLICATES targets)
	elseif(WIN32)
		if(MINGW OR NOT (MSVC OR CMAKE_C_SIMULATE_ID STREQUAL "MSVC"))
			message(STATUS "emusync: Windows builds need MSVC or clang-cl (the *-pc-windows-msvc Rust targets) -> DISABLED")
			return()
		endif()
		# Visual Studio generators name the target platform; others use the processor.
		set(proc "${CMAKE_GENERATOR_PLATFORM}")
		if(NOT proc)
			set(proc "${CMAKE_SYSTEM_PROCESSOR}")
		endif()
		_emusync_rust_arch("${proc}" arch)
		if(NOT arch)
			message(STATUS "emusync: unsupported Windows architecture '${proc}' -> DISABLED")
			return()
		endif()
		set(targets "${arch}-pc-windows-msvc")
	elseif(CMAKE_SYSTEM_NAME STREQUAL "Linux")
		_emusync_rust_arch("${CMAKE_SYSTEM_PROCESSOR}" arch)
		if(NOT arch)
			message(STATUS "emusync: unsupported Linux architecture '${CMAKE_SYSTEM_PROCESSOR}' -> DISABLED (set EMUSYNC_RUST_TARGET)")
			return()
		endif()
		set(targets "${arch}-unknown-linux-gnu")
	else()
		message(STATUS "emusync: unsupported platform '${CMAKE_SYSTEM_NAME}' -> DISABLED (set EMUSYNC_RUST_TARGET)")
		return()
	endif()

	if(WIN32)
		set(libname emusync.dll)
		set(implibname emusync.dll.lib)
	elseif(APPLE)
		set(libname libemusync.dylib)
	else()
		set(libname libemusync.so)
	endif()

	# ---- Rust toolchain (optional)
	find_program(EMUSYNC_CARGO cargo HINTS "$ENV{CARGO_HOME}/bin" "$ENV{HOME}/.cargo/bin" "$ENV{USERPROFILE}/.cargo/bin")
	find_program(EMUSYNC_RUSTC rustc HINTS "$ENV{CARGO_HOME}/bin" "$ENV{HOME}/.cargo/bin" "$ENV{USERPROFILE}/.cargo/bin")
	if(NOT EMUSYNC_CARGO OR NOT EXISTS "${EMUSYNC_CARGO}" OR NOT EMUSYNC_RUSTC OR NOT EXISTS "${EMUSYNC_RUSTC}")
		message(STATUS "emusync: cargo or rustc not found -> DISABLED. Install Rust (https://rustup.rs) to enable it.")
		return()
	endif()
	foreach(t IN LISTS targets)
		# rustc prints the target's library directory whether or not it is installed; it exists
		# only when it is. Run from the checkout so a rust-toolchain file there would apply.
		execute_process(
			COMMAND "${EMUSYNC_RUSTC}" --print target-libdir --target ${t}
			WORKING_DIRECTORY "${src}"
			OUTPUT_VARIABLE libdir OUTPUT_STRIP_TRAILING_WHITESPACE
			RESULT_VARIABLE rc ERROR_QUIET)
		if(NOT rc EQUAL 0 OR NOT IS_DIRECTORY "${libdir}")
			message(STATUS "emusync: Rust target ${t} not installed -> DISABLED. Run: rustup target add ${t}")
			return()
		endif()
	endforeach()

	# Multi-config generators (Visual Studio, Xcode) leave CMAKE_BUILD_TYPE empty, so they get
	# release: one optimised library serves every configuration.
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

	# ---- One cargo build per Rust target (two for a universal macOS library)
	# The link arguments go to the final rustc call only (after `--`), so they do not invalidate
	# the dependencies' build cache the way RUSTFLAGS would:
	# - Linux: a bare soname, so the host's DT_NEEDED is libemusync.so and not a build path;
	#   --exclude-libs,ALL keeps symbols from dependency archives out of the dynamic symbol table.
	# - macOS: an @rpath install name, so the host finds the library wherever it ships it.
	# -DZSTD_TRACE=0 drops zstd's weak, undefined ZSTD_trace_* hooks, which a ZSTD_trace_* defined
	# anywhere else in the process (the host's own zstd) would otherwise resolve.
	set(commands "")
	set(built "")
	foreach(t IN LISTS targets)
		string(REPLACE "-" "_" under "${t}")
		set(link_args "")
		if(t MATCHES "linux")
			list(APPEND link_args -C link-arg=-Wl,-soname,libemusync.so -C link-arg=-Wl,--exclude-libs,ALL)
		elseif(t MATCHES "apple")
			list(APPEND link_args -C link-arg=-Wl,-install_name,@rpath/libemusync.dylib)
		endif()
		set(env "CFLAGS_${under}=-DZSTD_TRACE=0")
		if(APPLE AND CMAKE_OSX_DEPLOYMENT_TARGET)
			list(APPEND env "MACOSX_DEPLOYMENT_TARGET=${CMAKE_OSX_DEPLOYMENT_TARGET}")
		endif()
		list(APPEND commands
			COMMAND "${CMAKE_COMMAND}" -E env ${env}
				"${EMUSYNC_CARGO}" rustc --locked -p emusync-ffi --lib --crate-type cdylib ${profile_flag}
					--manifest-path "${src}/Cargo.toml"
					--target ${t}
					--target-dir "${target_dir}"
					--
					${link_args})
		list(APPEND built "${target_dir}/${t}/${profile}/${libname}")
	endforeach()

	list(LENGTH targets ntargets)
	if(ntargets GREATER 1)
		# A universal (fat) library from the per-architecture ones.
		set(lib "${target_dir}/universal-apple-darwin/${profile}/${libname}")
		get_filename_component(libdir "${lib}" DIRECTORY)
		list(APPEND commands
			COMMAND "${CMAKE_COMMAND}" -E make_directory "${libdir}"
			COMMAND lipo -create -output "${lib}" ${built})
		set(label "universal: ${targets}")
	else()
		set(lib "${built}")
		set(label "${targets}")
	endif()
	set(byproducts "${lib}")
	if(WIN32)
		set(implib "${target_dir}/${targets}/${profile}/${implibname}")
		list(APPEND byproducts "${implib}")
	endif()

	add_custom_target(emusync_cargo
		${commands}
		BYPRODUCTS ${byproducts}
		WORKING_DIRECTORY "${src}"
		COMMENT "emusync: cargo build of ${libname} (${label}, ${profile})"
		VERBATIM)

	add_library(emusync::emusync SHARED IMPORTED GLOBAL)
	set_target_properties(emusync::emusync PROPERTIES
		IMPORTED_LOCATION "${lib}"
		INTERFACE_INCLUDE_DIRECTORIES "${src}/include")
	if(WIN32)
		set_target_properties(emusync::emusync PROPERTIES IMPORTED_IMPLIB "${implib}")
		set(EMUSYNC_IMPORT_LIBRARY "${implib}" PARENT_SCOPE)
	elseif(APPLE)
		set_target_properties(emusync::emusync PROPERTIES IMPORTED_SONAME "@rpath/libemusync.dylib")
	else()
		set_target_properties(emusync::emusync PROPERTIES IMPORTED_SONAME libemusync.so)
	endif()
	add_dependencies(emusync::emusync emusync_cargo)

	set(EMUSYNC_FOUND ON PARENT_SCOPE)
	set(EMUSYNC_LIBRARY "${lib}" PARENT_SCOPE)
	message(STATUS "emusync: ENABLED (${label}, ${profile})")
endfunction()

_emusync_desktop_setup()

function(emusync_deploy_runtime target)
	cmake_parse_arguments(PARSE_ARGV 1 arg "" "DESTINATION" "")
	if(NOT EMUSYNC_FOUND)
		return()
	endif()
	if(arg_DESTINATION)
		set(dest "${arg_DESTINATION}")
	else()
		set(dest "$<TARGET_FILE_DIR:${target}>")
	endif()
	add_custom_command(TARGET ${target} POST_BUILD
		COMMAND "${CMAKE_COMMAND}" -E make_directory "${dest}"
		COMMAND "${CMAKE_COMMAND}" -E copy_if_different "${EMUSYNC_LIBRARY}" "${dest}/"
		COMMENT "emusync: copying the library next to ${target}"
		VERBATIM)
	add_dependencies(${target} emusync_cargo)
	if(NOT arg_DESTINATION)
		if(APPLE)
			set_property(TARGET ${target} APPEND PROPERTY BUILD_RPATH "@loader_path")
		elseif(UNIX)
			set_property(TARGET ${target} APPEND PROPERTY BUILD_RPATH "$ORIGIN")
		endif()
	endif()
endfunction()
