//! Build script: keep `dist/admin/` and `dist/overlay/` in sync with the
//! `admin/` and `overlay/` source directories.
//!
//! The binary embeds `dist/` (see `src/embedded.rs`) via `include_dir!`,
//! which historically was a *committed copy* of the front-end sources.
//! Editing `admin/*.js` without re-copying silently shipped a stale UI.
//! Syncing here makes the embedded assets always match the sources.
//!
//! Hardening rules (audit item P2-03) — a silent sync failure means the
//! release binary ships a stale admin panel / overlay, so this script is
//! deliberately strict:
//!
//! * every filesystem call either succeeds or fails the build with the
//!   exact source path, destination path and underlying OS error;
//! * files in `dist/<subdir>` whose source no longer exists are removed
//!   (reported with `cargo:warning=`), so a deleted `admin/old.js` cannot
//!   keep being embedded forever;
//! * after syncing, source and destination are re-read and compared
//!   (name sets + full byte comparison) and the build fails on any drift.
//!
//! Blast radius: this script only ever creates/removes *files* directly
//! inside `dist/admin` and `dist/overlay`. It never deletes or recurses
//! into a directory, and it never touches anything outside those two
//! destination directories (`dist/` itself is passed through untouched and
//! is still contained in the final binary).

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::Path;

/// The (source, destination) directory pairs this script manages.
const MANAGED: [(&str, &str); 2] = [("admin", "dist/admin"), ("overlay", "dist/overlay")];

/// Wrap an `io::Error` so the message names the operation and the full path
/// next to the underlying OS error.
fn path_err(op: &str, path: &Path, err: io::Error) -> io::Error {
    io::Error::new(
        err.kind(),
        format!("{op} `{}` failed: {err}", path.display()),
    )
}

/// Names of the *regular files* directly inside `dir` (no recursion).
///
/// Anything that is not a regular file is skipped, which is what keeps the
/// orphan pass from ever considering a directory for deletion. A read or
/// metadata failure is an error, not an empty list: an unreadable source
/// must fail the build instead of silently embedding stale assets.
fn file_names(dir: &Path) -> io::Result<BTreeSet<OsString>> {
    let entries = fs::read_dir(dir).map_err(|e| path_err("read_dir", dir, e))?;
    let mut names = BTreeSet::new();
    for entry in entries {
        let entry = entry.map_err(|e| path_err("read_dir entry", dir, e))?;
        let path = entry.path();
        let meta = fs::metadata(&path).map_err(|e| path_err("metadata", &path, e))?;
        if meta.is_file() {
            names.insert(entry.file_name());
        }
    }
    Ok(names)
}

/// Names present in the destination but not in the source: the orphans that
/// must be deleted so a removed source file stops being embedded.
///
/// Only regular files can appear here, because it is fed from
/// [`file_names`].
fn orphan_names(src: &BTreeSet<OsString>, dst: &BTreeSet<OsString>) -> Vec<OsString> {
    dst.difference(src).cloned().collect()
}

/// Real byte-for-byte comparison of two files.
///
/// A missing or non-regular `dst` is reported as `Ok(false)` — that is the
/// normal "needs copying" case, and the subsequent copy reports the real
/// OS error if it cannot succeed. Every other failure (unreadable source,
/// permission error, ...) propagates.
fn files_identical(src: &Path, dst: &Path) -> io::Result<bool> {
    let src_meta = fs::metadata(src).map_err(|e| path_err("metadata", src, e))?;
    if !src_meta.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("source `{}` is not a regular file", src.display()),
        ));
    }
    let dst_meta = match fs::metadata(dst) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(path_err("metadata", dst, e)),
    };
    if !dst_meta.is_file() || dst_meta.len() != src_meta.len() {
        return Ok(false);
    }
    let a = fs::read(src).map_err(|e| path_err("read", src, e))?;
    let b = fs::read(dst).map_err(|e| path_err("read", dst, e))?;
    Ok(a == b)
}

/// Re-read `src` and `dst` and describe every difference: names missing in
/// the destination, orphans left in the destination, and files whose bytes
/// differ. An empty result means the two directories are in sync.
///
/// This is the actual verification step — it never trusts the earlier
/// `fs::copy` result.
fn verify_sync(src: &Path, dst: &Path) -> io::Result<Vec<String>> {
    let src_names = file_names(src)?;
    let dst_names = file_names(dst)?;
    let mut problems = Vec::new();

    for name in src_names.difference(&dst_names) {
        problems.push(format!(
            "`{}` exists in `{}` but is missing from `{}`",
            name.to_string_lossy(),
            src.display(),
            dst.display()
        ));
    }
    for name in dst_names.difference(&src_names) {
        problems.push(format!(
            "`{}` exists in `{}` but has no counterpart in `{}` (orphan)",
            name.to_string_lossy(),
            dst.display(),
            src.display()
        ));
    }
    for name in src_names.intersection(&dst_names) {
        let from = src.join(name);
        let to = dst.join(name);
        if !files_identical(&from, &to)? {
            problems.push(format!(
                "`{}` and `{}` have different bytes",
                from.display(),
                to.display()
            ));
        }
    }

    Ok(problems)
}

/// Copy `src/*` into `dst/`, delete orphans from `dst/`, then verify.
fn sync_dir(src: &Path, dst: &Path) -> io::Result<()> {
    // Watch the SOURCE directory as well as every source file below it.
    // The per-file lines alone cannot notice a *deleted* source file: there
    // is no path left for cargo to stat, so nothing about it changes. The
    // directory line makes cargo scan the whole directory, which is what
    // turns a deletion into a re-run of this script and lets the orphan
    // pass below actually happen.
    println!("cargo:rerun-if-changed={}", src.display());

    let src_names = file_names(src)?;
    for name in &src_names {
        println!("cargo:rerun-if-changed={}", src.join(name).display());
    }

    // Watch the DESTINATION directory too: a stale `dist/` can appear
    // without any source change (leftover from an older checkout, a file
    // renamed in `dist/` by hand, an orphan reintroduced by a partial
    // revert), and that is exactly when the orphan pass must run. This does
    // not cause a re-run on every build: files are only written when their
    // bytes actually differ, so an in-sync `dist/` keeps its fingerprints.
    println!("cargo:rerun-if-changed={}", dst.display());

    fs::create_dir_all(dst).map_err(|e| path_err("create_dir_all", dst, e))?;

    // 1. Copy new/changed files. Unchanged files are left alone so we don't
    //    rewrite (and re-timestamp) the whole tree on every build.
    for name in &src_names {
        let from = src.join(name);
        let to = dst.join(name);
        if files_identical(&from, &to)? {
            continue;
        }
        fs::copy(&from, &to).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "copy `{}` -> `{}` failed: {e}",
                    from.display(),
                    to.display()
                ),
            )
        })?;
    }

    // 2. Remove orphans: regular files in `dst` with no source counterpart.
    //    Directories are never deleted, and `dst` is the only place we ever
    //    delete from.
    let dst_names = file_names(dst)?;
    for name in orphan_names(&src_names, &dst_names) {
        let path = dst.join(&name);
        let meta = fs::metadata(&path).map_err(|e| path_err("metadata", &path, e))?;
        if !meta.is_file() {
            continue;
        }
        fs::remove_file(&path).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("remove_file `{}` failed: {e}", path.display()),
            )
        })?;
        println!(
            "cargo:warning=build.rs: removed orphan `{}` (no `{}` in `{}`)",
            path.display(),
            name.to_string_lossy(),
            src.display()
        );
    }

    // 3. Verify against the sources, not against the return value of copy.
    let problems = verify_sync(src, dst)?;
    if !problems.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!(
                "sync verification failed for `{}` -> `{}`:\n  - {}",
                src.display(),
                dst.display(),
                problems.join("\n  - ")
            ),
        ));
    }

    Ok(())
}

fn main() {
    // `include_dir!` runs during compilation of this crate, i.e. after
    // this script, so syncing here is safe.
    for (src, dst) in MANAGED {
        let (src, dst) = (Path::new(src), Path::new(dst));
        if let Err(err) = sync_dir(src, dst) {
            panic!(
                "build.rs: cannot sync `{}` -> `{}`: {err}",
                src.display(),
                dst.display()
            );
        }
    }
}

// Build scripts are not compiled in test mode by `cargo test` (cargo builds
// them as a plain host binary), so these tests are run directly with:
//   rustc --test build.rs -o <tmp>/build-rs-tests && <tmp>/build-rs-tests
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("build-rs-{tag}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn write(path: &Path, contents: &str) {
        fs::write(path, contents).expect("write test file");
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).expect("read test file")
    }

    fn names(list: &[&str]) -> BTreeSet<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn orphan_names_are_destination_only() {
        let src = names(&["app.js", "index.html"]);
        let dst = names(&["app.js", "index.html", "old.js"]);

        assert_eq!(orphan_names(&src, &dst), vec![OsString::from("old.js")]);
        // Nothing in the source is ever reported as an orphan.
        assert!(orphan_names(&dst, &src).is_empty());
        assert!(orphan_names(&src, &src).is_empty());
    }

    #[test]
    fn files_identical_compares_bytes_not_just_existence() {
        let dir = tmp_dir("identical");
        let a = dir.join("a.txt");
        let b = dir.join("b.txt");
        write(&a, "hello");

        assert!(!files_identical(&a, &b).expect("missing dst is 'not identical'"));
        write(&b, "hello");
        assert!(files_identical(&a, &b).expect("identical"));
        // Same length, different bytes: must not be treated as in sync.
        write(&b, "HELLO");
        assert!(!files_identical(&a, &b).expect("bytes differ"));

        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn files_identical_rejects_non_file_destination() {
        let dir = tmp_dir("dstdir");
        let a = dir.join("a.txt");
        write(&a, "hello");
        // A *directory* sitting where a file is expected must not count as in
        // sync (and must not be deleted either).
        let b = dir.join("b.txt");
        fs::create_dir(&b).expect("create conflicting dir");

        assert!(!files_identical(&a, &b).expect("dir destination is 'not identical'"));

        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn verify_reports_missing_orphans_and_drift() {
        let dir = tmp_dir("verify");
        let src = dir.join("src");
        let dst = dir.join("dst");
        fs::create_dir_all(&src).expect("mk src");
        fs::create_dir_all(&dst).expect("mk dst");
        write(&src.join("same.txt"), "one");
        write(&src.join("missing.txt"), "two");
        write(&dst.join("same.txt"), "one");
        write(&dst.join("orphan.txt"), "three");

        let problems = verify_sync(&src, &dst).expect("verify runs");
        assert_eq!(problems.len(), 2, "unexpected problems: {problems:?}");
        let joined = problems.join("\n");
        assert!(joined.contains("missing.txt"), "{joined}");
        assert!(joined.contains("orphan.txt"), "{joined}");

        // Byte drift on a file that exists in both is reported as well.
        write(&dst.join("same.txt"), "ONE");
        let problems = verify_sync(&src, &dst).expect("verify runs");
        let joined = problems.join("\n");
        assert!(joined.contains("different bytes"), "{joined}");

        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn sync_dir_copies_changed_removes_orphans_and_verifies() {
        let dir = tmp_dir("sync");
        let src = dir.join("src");
        let dst = dir.join("dst");
        fs::create_dir_all(&src).expect("mk src");
        fs::create_dir_all(&dst).expect("mk dst");

        write(&src.join("keep.txt"), "same");
        write(&src.join("new.txt"), "brand new");
        write(&dst.join("keep.txt"), "same");
        write(&dst.join("stale.txt"), "should be removed");

        sync_dir(&src, &dst).expect("sync succeeds");

        assert_eq!(read(&dst.join("keep.txt")), "same");
        assert_eq!(read(&dst.join("new.txt")), "brand new");
        assert!(!dst.join("stale.txt").exists(), "orphan must be removed");
        assert!(verify_sync(&src, &dst).expect("verify").is_empty());

        // A second run over an already-synced tree is a no-op that still
        // verifies (this is the "skip identical copies" path).
        sync_dir(&src, &dst).expect("second sync succeeds");
        assert!(verify_sync(&src, &dst).expect("verify").is_empty());

        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn sync_dir_fails_when_a_copy_cannot_happen() {
        let dir = tmp_dir("failcopy");
        let src = dir.join("src");
        let dst = dir.join("dst");
        fs::create_dir_all(&src).expect("mk src");
        fs::create_dir_all(&dst).expect("mk dst");
        write(&src.join("app.js"), "console.log(1);");
        // Occupy the destination path with a directory: `fs::copy` must fail
        // and that failure must reach the caller instead of being swallowed.
        fs::create_dir(dst.join("app.js")).expect("create blocking dir");

        let err = sync_dir(&src, &dst).expect_err("copy into a directory must fail");
        let msg = err.to_string();
        assert!(msg.contains("app.js"), "{msg}");
        assert!(msg.contains("copy"), "{msg}");

        // The blocking directory is still there: we never delete directories.
        assert!(dst.join("app.js").is_dir());

        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn sync_dir_fails_when_the_source_is_missing() {
        let dir = tmp_dir("nosrc");
        let src = dir.join("src");
        let dst = dir.join("dst");

        let err = sync_dir(&src, &dst).expect_err("missing source must fail the build");
        let msg = err.to_string();
        assert!(msg.contains("read_dir"), "{msg}");
        assert!(msg.contains("src"), "{msg}");

        fs::remove_dir_all(&dir).expect("cleanup");
    }
}
