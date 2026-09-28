/*
 * plugin/version.h — version string for the OBS thin-shell plugin.
 *
 * SOURCE OF TRUTH
 * ---------------
 * The single authoritative product version of this repository is the
 * `version` field of the root `Cargo.toml` (`[package] version`, currently
 * "0.0.28"). Both the Rust engine (`env!("CARGO_PKG_VERSION")`) and this
 * plugin release are versioned by that one value.
 *
 * WHAT REGENERATES THIS FILE
 * --------------------------
 * Nothing regenerates this file automatically. It is updated by hand (or by
 * `scripts/sync-version.ps1 -Fix` on Windows / the equivalent sed command
 * from docs/BUILD.md) whenever `Cargo.toml` is bumped. `plugin/CMakeLists.txt`
 * reads the literal below and *fails the configure step* if it does not match
 * `Cargo.toml`, so a stale value can never silently ship.
 *
 * CONSUMERS
 * ---------
 *   - plugin/CMakeLists.txt: the CMake `project(... VERSION ...)` literal is
 *     itself derived from Cargo.toml, and this file is then compared against
 *     that value with `file(STRINGS ... REGEX "#define[ \t]+SLT_VERSION ...")`
 *     so a stale header aborts the configure step.
 *   - plugin/stream-live-translate.c: the load-time log line. That file is
 *     owned by another agent; the required edit is documented in
 *     docs/BUILD.md ("Plugin load-time version log") and is exactly:
 *         #include "version.h"
 *         blog(LOG_INFO, "[SLT] Stream Live Translate plugin loaded (v%s)",
 *              SLT_VERSION);
 *
 * Do NOT extend this macro into two-part/three-part variants: downstream
 * consumers (CMake `VERSION`, archive file names) all expect a bare semver.
 */
#ifndef SLT_VERSION_H
#define SLT_VERSION_H

/* Kept as a bare literal (no macros, no string concatenation) so CMake can
 * extract it textually without running the C preprocessor. */
#define SLT_VERSION "0.0.28"

#endif /* SLT_VERSION_H */
