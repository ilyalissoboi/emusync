# A host's native build consuming the AAR's Prefab package: find_package finds emusync.h and the
# right ABI's libemusync.so without a Rust toolchain.
cmake_minimum_required(VERSION 3.22)
project(emusync_prefab_test C)

find_package(emusync REQUIRED CONFIG)

add_library(prefabtest SHARED prefabtest.c)
target_link_libraries(prefabtest PRIVATE emusync::emusync)
