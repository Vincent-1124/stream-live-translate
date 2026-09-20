//! Durable final-subtitle recording and lightweight TXT/SRT export.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::subtitle::SubtitleEvent;

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
    records: Vec<Record>,
    last_final: Option<(String, i64)>,
}

impl RecordingStore {
    pub fn start(config_path: &Path) -> Self {
        let started_at_ms = now_ms();
        let session_id = format!("{}-{}", started_at_ms, uuid::Uuid::new_v4());
        let dir = config_path.parent().unwrap_or_else(|| Path::new("."))
            .join("recordings");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("{session_id}.jsonl"));
        let file = OpenOptions::new().create(true).append(true).open(&path).ok();
        if file.is_none() { warn!(path = %path.display(), "subtitle recording disabled: cannot open JSONL"); }
        Self { session_id, started_at_ms, path, inner: Arc::new(Mutex::new(Inner { file, records: Vec::new(), last_final: None })) }
    }

    pub fn path(&self) -> &Path { &self.path }
    pub fn session_id(&self) -> &str { &self.session_id }
    pub fn records(&self) -> Vec<Record> { self.inner.lock().unwrap().records.clone() }

    fn append(&self, record: Record) {
        let mut inner = self.inner.lock().unwrap();
        let line = match serde_json::to_string(&record) { Ok(v) => v, Err(e) => { warn!(error = %e, "subtitle recording serialization failed"); return; } };
        let write_error = inner.file.as_mut().and_then(|file| {
            writeln!(file, "{line}").and_then(|_| file.flush()).err()
        });
        if let Some(e) = write_error {
            warn!(error = %e, path = %self.path.display(), "subtitle recording write failed; live subtitle continues");
            inner.file = None;
        }
        inner.records.push(record);
    }

    pub fn final_line(&self, line: &crate::subtitle::SubtitleLine) {
        let mut inner = self.inner.lock().unwrap();
        if inner.last_final.as_ref().is_some_and(|(text, at)| text == &line.text && line.started_at_ms.saturating_sub(*at) < 2_000) { return; }
        inner.last_final = Some((line.text.clone(), line.started_at_ms));
        drop(inner);
        self.append(Record::Final { session_id: self.session_id.clone(), id: line.id.clone(), text: line.text.clone(), language: line.language.clone(), started_at_ms: line.started_at_ms, ended_at_ms: line.updated_at_ms });
    }

    pub fn gap(&self, started_at_ms: i64, ended_at_ms: Option<i64>, reason: impl Into<String>) {
        self.append(Record::Gap { session_id: self.session_id.clone(), started_at_ms, ended_at_ms, reason: reason.into() });
    }

    pub fn txt(&self) -> String {
        self.records().into_iter().filter_map(|r| match r { Record::Final { text, .. } => Some(text), _ => None }).collect::<Vec<_>>().join("\n")
    }

    pub fn srt(&self) -> String {
        let mut out = String::new();
        let mut n = 1;
        for r in self.records() {
            if let Record::Final { text, started_at_ms, ended_at_ms, .. } = r {
                let start = started_at_ms.saturating_sub(self.started_at_ms).max(0);
                let end = ended_at_ms.saturating_sub(self.started_at_ms).max(start + 1_000);
                out.push_str(&format!("{n}\n{} --> {}\n{text}\n\n", srt_time(start), srt_time(end)));
                n += 1;
            }
        }
        out
    }
}

pub fn now_ms() -> i64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64 }
fn srt_time(ms: i64) -> String { format!("{:02}:{:02}:{:02},{:03}", ms / 3_600_000, (ms / 60_000) % 60, (ms / 1_000) % 60, ms % 1_000) }

pub fn spawn(store: RecordingStore, state: Arc<crate::AppState>) {
    let mut rx = state.subtitle.subscribe();
    let event_store = store.clone();
    let event_state = state.clone();
    tokio::spawn(async move {
        while let Ok(event) = rx.recv().await {
            if let SubtitleEvent::Final(_) = event {
                if let Some(line) = event_state.subtitle.history().last() { event_store.final_line(line); }
            }
        }
    });
    tokio::spawn(async move {
        let mut last = (state.status.read().audio_active, state.status.read().llm_connected);
        let mut gap_start = None;
        loop {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let s = state.status.read().clone();
            let current = (s.audio_active, s.llm_connected);
            if last.0 && !current.0 || last.1 && !current.1 { gap_start.get_or_insert_with(now_ms); }
            if gap_start.is_some() && current.0 && current.1 { store.gap(gap_start.take().unwrap(), Some(now_ms()), "connection_or_audio_gap"); }
            last = current;
        }
    });
}

#[derive(Debug, Clone, Serialize)]
pub struct RecordingInfo { pub session_id: String, pub jsonl_path: String }

impl RecordingStore { pub fn info(&self) -> RecordingInfo { RecordingInfo { session_id: self.session_id.clone(), jsonl_path: self.path.display().to_string() } } }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn srt_is_relative_and_monotonic() {
        let store = RecordingStore::start(Path::new("target/test-config.toml"));
        store.append(Record::Final { session_id: store.session_id.clone(), id: "1".into(), text: "你好".into(), language: "zh".into(), started_at_ms: store.started_at_ms, ended_at_ms: store.started_at_ms + 500 });
        assert!(store.srt().contains("00:00:00,000 --> 00:00:01,000"));
    }
}
