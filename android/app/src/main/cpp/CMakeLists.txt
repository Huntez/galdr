cmake_minimum_required(VERSION 3.22)
project(galdr_native C CXX)

# libopus from the submodule, statically linked into libgaldr.so. Only the codec itself: no
# tests, programs, install rules or the neural add-ons (DRED, OSCE), which need extra model data.
set(OPUS_BUILD_TESTING OFF CACHE BOOL "" FORCE)
set(OPUS_BUILD_PROGRAMS OFF CACHE BOOL "" FORCE)
set(OPUS_INSTALL_PKG_CONFIG_MODULE OFF CACHE BOOL "" FORCE)
set(OPUS_INSTALL_CMAKE_CONFIG_MODULE OFF CACHE BOOL "" FORCE)
add_subdirectory(opus EXCLUDE_FROM_ALL)

add_library(galdr SHARED mic.cpp)
target_compile_features(galdr PRIVATE cxx_std_17)
target_link_libraries(galdr PRIVATE opus aaudio log)
