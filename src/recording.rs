//! Durable final-subtitle recording and lightweight TXT/SRT export.
//!
//! The JSONL file is the record of truth for a session; the in-memory index is
//! only a bounded recent tail (see [`RECENT_INDEX_LIMIT`]). Exports stream the
//! file instead of cloning memory (P1-07), the recorder receives the finalised
//! line in the event instead of looking it up in the hub (P1-06), and the
//! recording directory is resolved through the config module's sanitiser
//! (P2-07).

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tracing::{info, warn};

use crate::subtitle::{SubtitleEvent, SubtitleLine};

/// How many of the newest records stay in memory.
///
/// Trade-off: the JSONL file holds the whole session, so the in-memory index
/// only has to answer "what did this session just record" (admin panel, tests)
/// and "what can still be exported if the file was rotated away". Keeping the
/// whole session here duplicated the file in RAM and made
/// `GET /api/recordings/export` clone it (and then build another full copy
/// while formatting) — unbounded on a long stream (P1-07). With this limit the
/// index is a few tens of KiB; the export reads the file for everything older.
pub const RECENT_INDEX_LIMIT: usize = 200;

/// Default for [`RecordingStore::set_enabled`].
///
/// `true` preserves today's shipped behaviour (every finalised sentence is
/// persisted) so this change cannot silently lose a recording. The audit asked
/// for an *explicit* switch for automatic persistence: it now exists
/// (`set_enabled`, plus the `recording.auto_persist` config key the lead must
/// add — see the report). Flipping the **default** to `false` changes shipped
/// behaviour (and silently stops producing evidence) and therefore needs
/// product confirmation before it is changed.
pub const RECORDING_ENABLED_BY_DEFAULT: bool = true;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Record {
    Final {
        session_id: String,
        id: String,
        text: String,
        language: String,
        started_at_ms: i64,
        ended_at_ms: i64,
    },
    Gap {
        session_id: String,
        started_at_ms: i64,
        ended_at_ms: Option<i64>,
        reason: String,
    },
}

#[derive(Clone)]
pub struct RecordingStore {
    session_id: String,
    started_at_ms: i64,
    path: PathBuf,
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    file: Option<File>,
    /// Bounded recent index, oldest first. NOT the whole session — the JSONL
    /// file is (P1-07).
    records: VecDeque<Record>,
    last_final: Option<(String, i64)>,
    /// Automatic persistence switch. Default: [`RECORDING_ENABLED_BY_DEFAULT`].
    enabled: bool,
    /// Records appended since the last successful write to `file`, i.e. the
    /// newest few that exist only in `records` (the disk filled up, or the file
    /// could not be opened at all). An export appends them after the file
    /// content. Reset when a write succeeds or the store is re-enabled.
    unpersisted: usize,
}

impl RecordingStore {
    pub fn start(config_path: &Path, recording_dir: &str) -> Self {
        let started_at_ms = now_ms();
        let session_id = format!("{}-{}", started_at_ms, uuid::Uuid::new_v4());
        let dir = safe_recordings_dir(config_path, recording_dir);
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("{session_id}.jsonl"));
        let file = match open_recording_file(&path) {
            Ok(file) => Some(file),
            Err(e) => {
                warn!(error = %e, path = %path.display(), "subtitle recording disabled: cannot open JSONL");
                None
            }
        };
        Self {
            session_id,
            started_at_ms,
            path,
            inner: Arc::new(Mutex::new(Inner {
                file,
                records: VecDeque::new(),
                last_final: None,
                enabled: RECORDING_ENABLED_BY_DEFAULT,
                unpersisted: 0,
            })),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The bounded in-memory index (oldest first), NOT the whole session.
    /// Exports must use [`Self::txt`] / [`Self::srt`], which stream the JSONL
    /// file (P1-07).
    pub fn records(&self) -> Vec<Record> {
        self.inner.lock().unwrap().records.iter().cloned().collect()
    }

    /// Whether automatic persistence is on. See [`Self::set_enabled`].
    pub fn enabled(&self) -> bool {
        self.inner.lock().unwrap().enabled
    }

    /// Turn automatic persistence of this session's finals on or off.
    ///
    /// Default: [`RECORDING_ENABLED_BY_DEFAULT`] (`true` = today's behaviour).
    /// The panel should surface this as a "自动保存本场记录" switch backed by the
    /// config key the lead adds; turning it *off* only stops new writes — it
    /// never deletes the JSONL that is already there. Deleting evidence is
    /// [`Self::delete_disk`], which is a different, explicitly destructive
    /// action (P1-06).
    pub fn set_enabled(&self, enabled: bool) {
        let mut inner = self.inner.lock().unwrap();
        inner.enabled = enabled;
        if enabled && inner.file.is_none() {
            // Re-enabling after a write failure (or after a refused directory)
            // tries to get a file back. Anything that was memory-only is
            // abandoned here: it cannot be interleaved into the file without
            // duplicating records, and the file is the record of truth.
            inner.unpersisted = 0;
            match open_recording_file(&self.path) {
                Ok(file) => inner.file = Some(file),
                Err(e) => {
                    warn!(error = %e, path = %self.path.display(), "subtitle recording disabled: cannot open JSONL")
                }
            }
        }
    }

    fn append(&self, record: Record) {
        let mut inner = self.inner.lock().unwrap();
        if !inner.enabled {
            return;
        }
        let line = match serde_json::to_string(&record) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "subtitle recording serialization failed");
                return;
            }
        };
        let write = inner
            .file
            .as_mut()
            .map(|file| writeln!(file, "{line}").and_then(|_| file.flush()));
        match write {
            Some(Ok(())) => inner.unpersisted = 0,
            Some(Err(e)) => {
                warn!(error = %e, path = %self.path.display(), "subtitle recording write failed; live subtitle continues");
                inner.file = None;
                inner.unpersisted += 1;
            }
            // No file at all (never opened, or deleted): the record stays in the
            // bounded index only.
            None => inner.unpersisted += 1,
        }
        while inner.records.len() >= RECENT_INDEX_LIMIT {
            inner.records.pop_front();
        }
        inner.records.push_back(record);
    }

    pub fn final_line(&self, line: &SubtitleLine) {
        let mut inner = self.inner.lock().unwrap();
        if inner.last_final.as_ref().is_some_and(|(text, at)| {
            text == &line.text && line.started_at_ms.saturating_sub(*at) < 2_000
        }) {
            return;
        }
        inner.last_final = Some((line.text.clone(), line.started_at_ms));
        drop(inner);
        self.append(Record::Final {
            session_id: self.session_id.clone(),
            id: line.id.clone(),
            text: line.text.clone(),
            language: line.language.clone(),
            started_at_ms: line.started_at_ms,
            ended_at_ms: line.updated_at_ms,
        });
    }

    pub fn gap(&self, started_at_ms: i64, ended_at_ms: Option<i64>, reason: impl Into<String>) {
        self.append(Record::Gap {
            session_id: self.session_id.clone(),
            started_at_ms,
            ended_at_ms,
            reason: reason.into(),
        });
    }

    /// Delete this session's on-disk recording and clear the in-memory index.
    ///
    /// Backing call for `DELETE /api/recordings`. This is deliberately NOT
    /// 「清空历史」 (`POST /api/subtitles/history/clear`), which only empties the
    /// server-side subtitle list and never touches the disk: deleting evidence
    /// is a different action from clearing a display buffer, and the UI must
    /// keep them separately labelled (P1-06).
    ///
    /// Idempotent by contract: deleting twice, or deleting when the file was
    /// never created or is already gone, returns `Ok` with the session's
    /// description — the caller must not have to special-case it. Recording
    /// stays off afterwards (re-creating the file the user just deleted would
    /// make the deletion meaningless) until [`Self::set_enabled`]`(true)`.
    pub fn delete_disk(&self) -> std::io::Result<RecordingInfo> {
        let mut info = self.info();
        // Close our handle first: Windows refuses to unlink an open file, and
        // the recorder must not keep appending to a file it no longer owns.
        self.inner.lock().unwrap().file = None;
        match std::fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let mut inner = self.inner.lock().unwrap();
        inner.records.clear();
        inner.last_final = None;
        inner.unpersisted = 0;
        inner.enabled = false;
        drop(inner);
        // The returned description reflects the state the caller now sees.
        info.enabled = false;
        Ok(info)
    }

    /// Delete recordings in this session's directory that started more than
    /// `max_age_days` ago, returning how many files were removed (P2-07).
    ///
    /// Retention is driven by the session start timestamp embedded in the file
    /// name (`<epoch_ms>-<uuid>.jsonl`), not by mtime, so it is deterministic
    /// and testable. `0` disables pruning (keep everything) — the safe default
    /// for a hand-written config. The live session's own file is never removed,
    /// and callers must only run this from an explicit config-driven pass: it
    /// deletes evidence, so it must not happen silently.
    pub fn prune_older_than(&self, max_age_days: u64) -> usize {
        if max_age_days == 0 {
            return 0;
        }
        let Some(dir) = self.path.parent() else {
            return 0;
        };
        let cutoff = now_ms().saturating_sub((max_age_days as i64).saturating_mul(86_400_000));
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) => {
                warn!(error = %e, dir = %dir.display(), "recording retention pass could not read the directory");
                return 0;
            }
        };
        let mut removed = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            if path == self.path {
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(started_at_ms) = session_started_at(&path) else {
                continue;
            };
            if started_at_ms >= cutoff {
                continue;
            }
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    removed += 1;
                    info!(path = %path.display(), age_days = max_age_days, "removed an expired subtitle recording");
                }
                Err(e) => {
                    warn!(error = %e, path = %path.display(), "could not remove an expired subtitle recording")
                }
            }
        }
        removed
    }

    /// Feed every record of this session, in order, to `f`.
    ///
    /// The JSONL file is streamed line by line — the export never clones the
    /// in-memory vector (P1-07). When the file is missing or empty (rotated,
    /// deleted, or never opened) the bounded in-memory index is used instead of
    /// returning an empty document, and records that never reached the disk are
    /// appended after the file content.
    fn for_each_record<F: FnMut(Record)>(&self, mut f: F) {
        let (index, unpersisted) = {
            let inner = self.inner.lock().unwrap();
            (
                inner.records.iter().cloned().collect::<Vec<_>>(),
                inner.unpersisted,
            )
        };
        let mut from_disk = 0usize;
        match File::open(&self.path) {
            Ok(file) => {
                for line in std::io::BufReader::new(file).lines() {
                    let line = match line {
                        Ok(line) => line,
                        Err(e) => {
                            warn!(error = %e, path = %self.path.display(), "recording read failed; export is truncated");
                            break;
                        }
                    };
                    if line.trim().is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<Record>(&line) {
                        Ok(record) => {
                            from_disk += 1;
                            f(record);
                        }
                        Err(e) => {
                            warn!(error = %e, path = %self.path.display(), "skipping unreadable recording line")
                        }
                    }
                }
            }
            Err(e) => {
                warn!(error = %e, path = %self.path.display(), "recording file unavailable; exporting the in-memory index")
            }
        }
        if from_disk > 0 {
            // The newest records that never reached the disk are only in the
            // index; they come last.
            let take = unpersisted.min(index.len());
            for record in &index[index.len() - take..] {
                f(record.clone());
            }
            return;
        }
        for record in index {
            f(record);
        }
    }

    pub fn txt(&self) -> String {
        let mut out = Vec::new();
        self.write_txt(&mut out)
            .expect("writing to Vec cannot fail");
        String::from_utf8(out).expect("recording text is UTF-8")
    }

    /// Write a TXT export incrementally. HTTP callers should prefer this over
    /// [`txt`](Self::txt) so a long session is never duplicated in one String.
    pub fn write_txt(&self, out: &mut impl Write) -> std::io::Result<()> {
        let mut result = Ok(());
        let mut first = true;
        self.for_each_record(|r| {
            if let Record::Final { text, .. } = r {
                if result.is_err() {
                    return;
                }
                if !first {
                    result = out.write_all(b"\n");
                }
                first = false;
                if result.is_ok() {
                    result = out.write_all(text.as_bytes());
                }
            }
        });
        result
    }

    pub fn srt(&self) -> String {
        let mut out = Vec::new();
        self.write_srt(&mut out)
            .expect("writing to Vec cannot fail");
        String::from_utf8(out).expect("recording text is UTF-8")
    }

    /// Write an SRT export incrementally. HTTP callers should prefer this over
    /// [`srt`](Self::srt) and stream/spool the writer as appropriate.
    pub fn write_srt(&self, out: &mut impl Write) -> std::io::Result<()> {
        let mut result = Ok(());
        let mut n = 1;
        self.for_each_record(|r| {
            if let Record::Final {
                text,
                started_at_ms,
                ended_at_ms,
                ..
            } = r
            {
                if result.is_err() {
                    return;
                }
                let start = started_at_ms.saturating_sub(self.started_at_ms).max(0);
                let end = ended_at_ms
                    .saturating_sub(self.started_at_ms)
                    .max(start + 1_000);
                result = writeln!(
                    out,
                    "{n}\n{} --> {}\n{text}\n",
                    srt_time(start),
                    srt_time(end)
                );
                n += 1;
            }
        });
        result
    }
}

/// Resolve the recording directory through the config module's sanitiser.
///
/// The old private `recordings_dir()` accepted any absolute path and any
/// relative path containing `..`, so a hand-edited `recording_dir` could write
/// the session's subtitles anywhere on disk (or to a UNC share). We now use
/// [`crate::config::resolve_recording_dir`] and fall back — with a warning — to
/// the safe default under the config directory when it refuses (P2-07).
fn safe_recordings_dir(config_path: &Path, recording_dir: &str) -> PathBuf {
    match crate::config::resolve_recording_dir(config_path, recording_dir) {
        Ok(dir) => dir,
        Err(e) => {
            let fallback = config_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("recordings");
            warn!(
                requested = %recording_dir,
                error = %e,
                fallback = %fallback.display(),
                "recording_dir refused; recording into the default directory under the config directory instead"
            );
            fallback
        }
    }
}

/// Open (create + append) the session JSONL, owner-only on Unix.
///
/// Follows `src/auth.rs`'s token-file style: `0o600` is set at creation time
/// *and* re-applied to an existing file, so a recording created before this
/// change (or with a looser mode) is tightened too. Windows has no portable
/// mode to set — the file inherits the profile ACL of the directory the user's
/// config lives in, which is already user-only (P2-07).
fn open_recording_file(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    tighten_permissions(path);
    Ok(file)
}

#[cfg(unix)]
fn tighten_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn tighten_permissions(_path: &Path) {}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// The session start timestamp of a `<epoch_ms>-<uuid>.jsonl` recording file.
fn session_started_at(path: &Path) -> Option<i64> {
    path.file_name()?
        .to_str()?
        .split('-')
        .next()?
        .parse::<i64>()
        .ok()
}

fn srt_time(ms: i64) -> String {
    format!(
        "{:02}:{:02}:{:02},{:03}",
        ms / 3_600_000,
        (ms / 60_000) % 60,
        (ms / 1_000) % 60,
        ms % 1_000
    )
}

/// The durable recorder's event loop.
///
/// Split out of [`spawn`] so a test can drive it with its own receiver and
/// observe the lag report without building an `AppState`.
async fn record_events<F>(
    mut rx: broadcast::Receiver<SubtitleEvent>,
    store: RecordingStore,
    mut on_lag: F,
) where
    F: FnMut(usize) + Send,
{
    loop {
        match rx.recv().await {
            // The feed carries the immutable line: nothing is looked up, so a
            // concurrent final, a `Cleared`, or a backlog cannot make the
            // recorder store the wrong sentence (P1-06).
            Ok(SubtitleEvent::FinalLine { line, .. }) => store.final_line(&line),
            Ok(_) => {}
            Err(broadcast::error::RecvError::Lagged(missed)) => {
                // `while let Ok(event) = rx.recv().await` used to end the loop
                // here: the recorder stopped for the rest of the session with
                // no log and nothing the user could see (P1-06). Report the
                // loss durably and keep recording.
                warn!(missed, "subtitle recorder fell behind; subtitle events were dropped and a gap was recorded");
                store.gap(now_ms(), None, "subtitle_events_lagged");
                on_lag(missed as usize);
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => {
                info!("subtitle hub closed; subtitle recorder exiting deliberately");
                return;
            }
        }
    }
}

pub fn spawn(store: RecordingStore, state: Arc<crate::AppState>) {
    let rx = state.subtitle.subscribe_finalized();
    let event_store = store.clone();
    let lag_state = state.clone();
    tokio::spawn(async move {
        record_events(rx, event_store, move |missed| {
            // The admin panel renders `status.last_error`, so a dropped batch is
            // visible to the user instead of being a silent hole.
            lag_state.status.write().last_error = Some(format!(
                "录制跟不上字幕事件（丢弃 {missed} 条），已写入一个缺口标记；录制仍在继续"
            ));
        })
        .await;
    });
    tokio::spawn(async move {
        let mut last = (
            state.status.read().audio_active,
            state.status.read().llm_connected,
        );
        let mut gap_start = None;
        loop {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let s = state.status.read().clone();
            let current = (s.audio_active, s.llm_connected);
            if last.0 && !current.0 || last.1 && !current.1 {
                gap_start.get_or_insert_with(now_ms);
            }
            if gap_start.is_some() && current.0 && current.1 {
                store.gap(
                    gap_start.take().unwrap(),
                    Some(now_ms()),
                    "connection_or_audio_gap",
                );
            }
            last = current;
        }
    });
}

#[derive(Debug, Clone, Serialize)]
pub struct RecordingInfo {
    pub session_id: String,
    pub jsonl_path: String,
    /// Whether automatic persistence is on (P1-06). Exposed here so
    /// `GET /api/recordings` can show the state of the "自动保存本场记录"
    /// switch without a new endpoint; `false` after `delete_disk`.
    pub enabled: bool,
}

impl RecordingStore {
    pub fn info(&self) -> RecordingInfo {
        RecordingInfo {
            session_id: self.session_id.clone(),
            jsonl_path: self.path.display().to_string(),
            enabled: self.enabled(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A store in the test config's directory (`target/recordings`).
    fn test_store() -> RecordingStore {
        RecordingStore::start(Path::new("target/test-config.toml"), "")
    }

    fn final_record(store: &RecordingStore, i: i64, text: &str) -> Record {
        Record::Final {
            session_id: store.session_id.clone(),
            id: format!("id-{i}"),
            text: text.to_string(),
            language: "zh".into(),
            started_at_ms: store.started_at_ms + i * 1_000,
            ended_at_ms: store.started_at_ms + i * 1_000 + 800,
        }
    }

    fn recorded_texts(store: &RecordingStore) -> Vec<String> {
        store
            .records()
            .into_iter()
            .filter_map(|r| match r {
                Record::Final { text, .. } => Some(text),
                _ => None,
            })
            .collect()
    }

    async fn wait_until(mut cond: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("condition not met within 10s");
    }

    #[test]
    fn srt_is_relative_and_monotonic() {
        let store = test_store();
        store.append(Record::Final {
            session_id: store.session_id.clone(),
            id: "1".into(),
            text: "你好".into(),
            language: "zh".into(),
            started_at_ms: store.started_at_ms,
            ended_at_ms: store.started_at_ms + 500,
        });
        assert!(store.srt().contains("00:00:00,000 --> 00:00:01,000"));
    }

    #[test]
    fn recording_directory_is_relative_to_config_when_not_absolute() {
        let config = Path::new("C:/app/config.toml");
        assert_eq!(
            safe_recordings_dir(config, ""),
            PathBuf::from("C:/app/recordings")
        );
        assert_eq!(
            safe_recordings_dir(config, "captures/live"),
            PathBuf::from("C:/app/captures/live")
        );
    }

    /// P2-07: a hand-edited `recording_dir` must not be able to write the
    /// session's subtitles outside the config directory.
    #[test]
    fn a_traversal_recording_dir_is_refused() {
        let config = Path::new("target/test-config.toml");
        let expected = Path::new("target").join("recordings");
        assert_eq!(safe_recordings_dir(config, "../../evil"), expected);
        let store = RecordingStore::start(config, "../../evil");
        assert!(store.path().starts_with(&expected), "{:?}", store.path());
        assert!(
            !store.path().to_string_lossy().contains("evil"),
            "{:?}",
            store.path()
        );
    }

    #[test]
    fn an_unc_path_recording_dir_is_refused() {
        let config = Path::new("target/test-config.toml");
        let expected = Path::new("target").join("recordings");
        assert_eq!(
            safe_recordings_dir(config, r"\\server\share\subtitles"),
            expected
        );
        let store = RecordingStore::start(config, r"\\server\share\subtitles");
        assert!(store.path().starts_with(&expected), "{:?}", store.path());
    }

    #[test]
    fn an_out_of_tree_absolute_recording_dir_is_refused() {
        #[cfg(windows)]
        let outside = "C:/Windows/Temp/slt-recording-must-be-refused";
        #[cfg(unix)]
        let outside = "/tmp/slt-recording-must-be-refused";
        let config = Path::new("target/test-config.toml");
        let expected = Path::new("target").join("recordings");
        assert_eq!(safe_recordings_dir(config, outside), expected);
        assert!(!RecordingStore::start(config, outside)
            .path()
            .starts_with(outside));
    }

    /// Export must stream the JSONL file: the in-memory index is bounded, so a
    /// `txt()`/`srt()` that cloned `records()` could not possibly return the
    /// whole session (P1-07).
    #[test]
    fn export_streams_from_the_jsonl_file() {
        let store = test_store();
        let n = RECENT_INDEX_LIMIT * 2;
        for i in 0..n as i64 {
            store.append(final_record(&store, i, &format!("第 {i} 句")));
        }
        assert_eq!(
            store.records().len(),
            RECENT_INDEX_LIMIT,
            "the in-memory index must stay bounded"
        );
        assert!(store.records().len() < n);

        let txt = store.txt();
        let lines: Vec<&str> = txt.lines().collect();
        assert_eq!(
            lines.len(),
            n,
            "every recorded sentence must be exported in order"
        );
        assert_eq!(lines[0], "第 0 句");
        assert_eq!(lines[n - 1], format!("第 {} 句", n - 1));

        let srt = store.srt();
        // 800 ms of audio but the SRT writer floors a cue at one second.
        assert!(
            srt.starts_with("1\n00:00:00,000 --> 00:00:01,000\n第 0 句\n\n"),
            "{srt:.120}"
        );
        assert!(srt.contains(&format!("第 {} 句", n - 1)));
    }

    #[test]
    fn writer_exports_incrementally_and_propagates_output_errors() {
        struct BoundedWriter {
            bytes: usize,
            max_write: usize,
            fail_after: Option<usize>,
        }
        impl Write for BoundedWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                assert!(
                    buf.len() <= self.max_write,
                    "export attempted one oversized write"
                );
                if self.fail_after.is_some_and(|limit| self.bytes >= limit) {
                    return Err(std::io::Error::other("sink closed"));
                }
                self.bytes += buf.len();
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let store = test_store();
        for i in 0..(RECENT_INDEX_LIMIT * 2) as i64 {
            store.append(final_record(&store, i, "一小句"));
        }
        let mut txt = BoundedWriter {
            bytes: 0,
            max_write: 64,
            fail_after: None,
        };
        store.write_txt(&mut txt).unwrap();
        assert!(txt.bytes > RECENT_INDEX_LIMIT * "一小句".len());

        let mut closed = BoundedWriter {
            bytes: 0,
            max_write: 64,
            fail_after: Some(0),
        };
        assert!(
            store.write_srt(&mut closed).is_err(),
            "a disconnected response sink must be reported"
        );
    }

    /// A rotated / externally deleted file must not produce an empty export.
    #[test]
    fn export_falls_back_to_the_in_memory_index_when_the_file_is_gone() {
        let store = test_store();
        store.append(final_record(&store, 0, "第一句"));
        store.append(final_record(&store, 1, "第二句"));
        std::fs::remove_file(store.path()).expect("simulate a rotated recording file");
        assert!(!store.path().exists());
        assert_eq!(store.txt(), "第一句\n第二句");
        assert!(store.srt().contains("第二句"));
    }

    /// P1-06: deleting this session's evidence is idempotent and is distinct
    /// from clearing the subtitle history.
    #[test]
    fn delete_disk_removes_the_file_and_is_idempotent() {
        let store = test_store();
        store.final_line(&SubtitleLine {
            id: "1".into(),
            text: "要删除的记录".into(),
            language: "zh".into(),
            started_at_ms: now_ms(),
            updated_at_ms: now_ms(),
            finalised: true,
        });
        assert!(
            store.path().exists(),
            "the JSONL must exist before the delete"
        );
        assert_eq!(recorded_texts(&store).len(), 1);
        assert!(store.enabled(), "recording is on by default");

        let info = store.delete_disk().expect("first delete must succeed");
        assert_eq!(info.jsonl_path, store.path().display().to_string());
        assert!(
            !info.enabled,
            "the panel must see that persistence is off after a delete"
        );
        assert!(
            !store.path().exists(),
            "delete_disk must remove the JSONL file"
        );
        assert!(
            store.records().is_empty(),
            "delete_disk must clear the in-memory index"
        );
        assert!(store.txt().is_empty(), "nothing may be left to export");
        assert!(
            !store.enabled(),
            "delete must not silently start a new recording"
        );

        // Deleting twice is Ok, not a panic or an error.
        let again = store.delete_disk().expect("second delete must not error");
        assert_eq!(again.session_id, info.session_id);
        assert_eq!(again.jsonl_path, info.jsonl_path);

        // So is deleting a file that was never created.
        let ghost = test_store();
        std::fs::remove_file(ghost.path()).expect("simulate a recording that was never created");
        assert!(
            ghost.delete_disk().is_ok(),
            "a missing recording must delete cleanly"
        );
    }

    /// P2-07: retention is explicit (`0` = keep everything) and never touches
    /// the live session's file.
    #[test]
    fn retention_prunes_only_old_recordings() {
        let store = test_store();
        store.append(final_record(&store, 0, "本场记录"));
        let dir = store.path().parent().unwrap().to_path_buf();
        let old = dir.join(format!(
            "{}-{}.jsonl",
            now_ms() - 40 * 24 * 3_600_000,
            uuid::Uuid::new_v4()
        ));
        let recent = dir.join(format!(
            "{}-{}.jsonl",
            now_ms() - 1_000,
            uuid::Uuid::new_v4()
        ));
        std::fs::write(&old, "{\"type\":\"final\"}\n").unwrap();
        std::fs::write(&recent, "{\"type\":\"final\"}\n").unwrap();

        assert_eq!(store.prune_older_than(0), 0, "0 days means keep everything");
        assert!(old.exists() && recent.exists());

        assert_eq!(store.prune_older_than(30), 1);
        assert!(
            !old.exists(),
            "a recording older than the window must be removed"
        );
        assert!(recent.exists(), "a recording inside the window must stay");
        assert!(
            store.path().exists(),
            "the live session's file must never be pruned"
        );
        let _ = std::fs::remove_file(&recent);
    }

    /// P1-06: a recorder that falls behind must report the loss and keep going.
    #[tokio::test]
    async fn a_lagged_recorder_keeps_recording_and_reports_the_gap() {
        let hub = crate::subtitle::SubtitleHub::default();
        let sink = hub.sink();
        let store = test_store();
        let rx = hub.subscribe_finalized();
        // Fill the feed far past its capacity while nobody polls it: exactly
        // what a lagging recorder sees.
        let backlog = crate::subtitle::EVENT_CHANNEL_CAPACITY + 50;
        for i in 0..backlog {
            sink.push(SubtitleEvent::Final(format!("积压 {i}")));
        }

        let reported = Arc::new(AtomicUsize::new(0));
        let seen = reported.clone();
        let task = tokio::spawn(record_events(rx, store.clone(), move |missed| {
            seen.fetch_add(missed, Ordering::SeqCst);
        }));

        wait_until(|| reported.load(Ordering::SeqCst) > 0).await;

        // The loss is durable and visible, read back from the JSONL itself so
        // the bounded index cannot hide it.
        let mut gaps = 0;
        store.for_each_record(|r| {
            if matches!(&r, Record::Gap { reason, .. } if reason == "subtitle_events_lagged") {
                gaps += 1;
            }
        });
        assert_eq!(gaps, 1, "the lag must be written as a Record::Gap");

        // ...and the recorder is still alive.
        sink.push(SubtitleEvent::Final("积压之后的新句子".into()));
        wait_until(|| {
            recorded_texts(&store)
                .iter()
                .any(|t| t == "积压之后的新句子")
        })
        .await;
        task.abort();
    }

    /// P1-06: the recorded text is the line carried by the event, not whatever
    /// happens to be last in the hub's history.
    #[tokio::test]
    async fn the_recorder_records_the_line_it_was_given() {
        let hub = crate::subtitle::SubtitleHub::default();
        let sink = hub.sink();
        let store = test_store();
        let rx = hub.subscribe_finalized();
        let task = tokio::spawn(record_events(rx, store.clone(), |_| {}));

        // Both finals are pushed before the recorder polls, so when it handles
        // the first one the history already ends with 第二句: the old
        // `history().last()` recorder stored 第二句 twice (the second being
        // dropped by the duplicate guard) and lost 第一句 entirely.
        sink.push(SubtitleEvent::Final("第一句".into()));
        sink.push(SubtitleEvent::Final("第二句".into()));
        assert_eq!(
            hub.history().last().map(|l| l.text.as_str()),
            Some("第二句")
        );

        wait_until(|| !recorded_texts(&store).is_empty()).await;
        // Give a wrong implementation the chance to append its second record.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            recorded_texts(&store),
            vec!["第一句".to_string(), "第二句".to_string()]
        );
        task.abort();
    }
}
