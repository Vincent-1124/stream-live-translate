cmake_minimum_required(VERSION 3.16...3.28)

# ---------------------------------------------------------------------------
# Version.
#
# The SINGLE authoritative version of this repository is the `version` field
# of the root Cargo.toml. The Rust engine gets it for free (CARGO_PKG_VERSION)
# and every other place derives from it, including this CMake project version.
#
# Why read Cargo.toml (file(READ) + string(REGEX MATCH)) instead of reading
# plugin/version.h (file(STRINGS))?
#   * `project(... VERSION ...)` must be called before any target exists, so
#     the value has to be known right here at the top of the file.
#   * Deriving from Cargo.toml means there is exactly ONE extraction to keep
#     correct, and a stale plugin/version.h can never silently win: the
#     version.h literal is compared against it below and aborts the configure
#     step on mismatch.
# Both were explicitly allowed; this is the one that keeps Cargo.toml supreme.
# ---------------------------------------------------------------------------
set(SLT_CARGO_TOML "${CMAKE_CURRENT_LIST_DIR}/../Cargo.toml")
if(NOT EXISTS "${SLT_CARGO_TOML}")
  message(FATAL_ERROR
    "Cannot find the root Cargo.toml at '${SLT_CARGO_TOML}'. "
    "The plugin build must run from inside the stream-live-translate "
    "repository; Cargo.toml is the authoritative version source.")
endif()
file(READ "${SLT_CARGO_TOML}" SLT_CARGO_TOML_CONTENTS)
# Anchored to the start of a line so dependency tables - `tokio = { version =
# "1.38", ... }` - cannot match. The alternation is spelled `(^|\n)` rather
# than `^` with a MULTILINE flag because CMake regexes have no MULTILINE, and
# `\n` inside a CMake regex is the newline character itself (verified against
# CMake 3.29). Note this must stay a *group*, not a bracket expression: inside
# `[...]` the `\n` would be literal `n` and the folder `target` would match.
string(REGEX MATCH "(^|\n)version[ \t]*=[ \t]*\"([0-9]+\\.[0-9]+\\.[0-9]+)\""
       SLT_CARGO_VERSION_MATCH "${SLT_CARGO_TOML_CONTENTS}")
set(SLT_CARGO_VERSION "${CMAKE_MATCH_2}")
if(NOT SLT_CARGO_VERSION)
  message(FATAL_ERROR
    "Could not extract `version = \"X.Y.Z\"` from '${SLT_CARGO_TOML}'. "
    "Add/correct the [package] version field — it is the authoritative "
    "release version and the plugin build refuses to guess one.")
endif()

project(stream-live-translate-obs VERSION ${SLT_CARGO_VERSION} LANGUAGES C)

message(STATUS
  "stream-live-translate OBS plugin ${SLT_CARGO_VERSION} "
  "(version derived from Cargo.toml)")

# plugin/version.h feeds plugin/stream-live-translate.c. It is hand-maintained
# (nothing generates it), so verify it here instead of trusting it.
set(SLT_VERSION_HEADER "${CMAKE_CURRENT_LIST_DIR}/version.h")
if(NOT EXISTS "${SLT_VERSION_HEADER}")
  message(FATAL_ERROR
    "plugin/version.h is missing. It must define SLT_VERSION and match "
    "Cargo.toml (${SLT_CARGO_VERSION}).")
endif()
file(STRINGS "${SLT_VERSION_HEADER}" SLT_VERSION_LINE
     REGEX "#define[ \t]+SLT_VERSION[ \t]+\"([0-9]+\\.[0-9]+\\.[0-9]+)\"")
if(NOT SLT_VERSION_LINE)
  message(FATAL_ERROR
    "plugin/version.h does not define "
    "`#define SLT_VERSION \"X.Y.Z\"`. Fix it to match Cargo.toml "
    "(${SLT_CARGO_VERSION}).")
endif()
string(REGEX MATCH "\"([0-9]+\\.[0-9]+\\.[0-9]+)\"" SLT_VERSION_DUMMY
       "${SLT_VERSION_LINE}")
set(SLT_PLUGIN_VERSION "${CMAKE_MATCH_1}")
if(NOT SLT_PLUGIN_VERSION STREQUAL SLT_CARGO_VERSION)
  message(FATAL_ERROR
    "Version mismatch: Cargo.toml says ${SLT_CARGO_VERSION} but "
    "plugin/version.h says ${SLT_PLUGIN_VERSION}. Cargo.toml is "
    "authoritative — update plugin/version.h (Windows: "
    "`powershell -File scripts/sync-version.ps1 -Fix`; macOS/Linux: run "
    "`bash scripts/sync-version.sh` and follow its report).")
endif()

set(CMAKE_C_STANDARD 11)
set(CMAKE_C_STANDARD_REQUIRED ON)
if(NOT CMAKE_BUILD_TYPE)
  set(CMAKE_BUILD_TYPE Release)
endif()

# ---------------------------------------------------------------------------
# OBS SDK. We only need the libobs *headers* plus something to satisfy the
# linker; real symbols resolve at runtime against the OBS installation.
#
#   -DLIBOBS_INCLUDE_DIR=<obs-studio source>/libobs
#
# Windows additionally wants an import library generated from the installed
# obs.dll (see scripts/package-plugin.ps1):
#   -DOBS_IMPORT_LIB=<path>/obs.lib
#
# Linux links a stub shared library with soname libobs.so.0:
#   -DOBS_STUB_LIB=<path>/libobs.so
#
# macOS uses -undefined dynamic_lookup and needs nothing extra.
# ---------------------------------------------------------------------------

if(NOT LIBOBS_INCLUDE_DIR)
  message(FATAL_ERROR
    "LIBOBS_INCLUDE_DIR is not set. Point it at an obs-studio checkout's "
    "libobs/ directory (headers only, no build required).")
endif()

add_library(stream-live-translate MODULE stream-live-translate.c)
target_include_directories(stream-live-translate PRIVATE "${LIBOBS_INCLUDE_DIR}")
# For `#include "version.h"` in stream-live-translate.c. That file is owned by
# another agent; the include is the only change it needs (see docs/BUILD.md).
target_include_directories(stream-live-translate PRIVATE "${CMAKE_CURRENT_SOURCE_DIR}")
set_target_properties(stream-live-translate PROPERTIES
  PREFIX ""
  OUTPUT_NAME "stream-live-translate")

if(WIN32)
  target_link_libraries(stream-live-translate PRIVATE ws2_32)
  if(OBS_IMPORT_LIB)
    target_link_libraries(stream-live-translate PRIVATE "${OBS_IMPORT_LIB}")
  else()
    message(FATAL_ERROR
      "OBS_IMPORT_LIB is not set. Generate obs.lib from obs.dll with "
      "dumpbin+lib (see scripts/package-plugin.ps1).")
  endif()
elseif(APPLE)
  find_package(Threads REQUIRED)
  target_link_libraries(stream-live-translate PRIVATE Threads::Threads)
  target_link_options(stream-live-translate PRIVATE "-undefined" "dynamic_lookup")
  set_target_properties(stream-live-translate PROPERTIES SUFFIX "")
else()
  find_package(Threads REQUIRED)
  target_link_libraries(stream-live-translate PRIVATE Threads::Threads)
  if(OBS_STUB_LIB)
    target_link_libraries(stream-live-translate PRIVATE "${OBS_STUB_LIB}")
  else()
    message(FATAL_ERROR
      "OBS_STUB_LIB is not set. Create a stub libobs.so with soname "
      "libobs.so.0 (see scripts/package-plugin.sh).")
  endif()
endif()

if(MSVC)
  # `/utf-8` is REQUIRED, not cosmetic: the source carries Chinese comments (and a
  # Chinese status string), and without it MSVC decodes the file using the system
  # code page (936 here). That is not merely a warning — it corrupts the source
  # badly enough to produce a bogus syntax error, so a perfectly valid plugin
  # looked unbuildable:
  #   plugin\stream-live-translate.c(1378): error C2001: 常量中有换行符
  # Verified: same file, same command, with `/utf-8` added -> exit 0, and the
  # linked DLL exports obs_module_load / obs_module_unload.
  target_compile_options(stream-live-translate PRIVATE /W3 /utf-8)
else()
  target_compile_options(stream-live-translate PRIVATE -Wall -Wextra -Wno-unused-parameter)
endif()
