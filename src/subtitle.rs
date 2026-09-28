//! Subtitle event hub. The LLM provider pushes `SubtitleEvent`s into the
//! hub; the WebSocket fanout task reads from it and pushes to the browser
//! overlay and OBS text source.

use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// Broadcast buffer size for the wire feed and for the durable-recorder feed.
///
/// The recorder feed carries one event per *finished sentence*, so overflowing
/// it means the recorder really is behind and events were lost
/// (`crate::recording` reports that as a `Record::Gap`).
pub const EVENT_CHANNEL_CAPACITY: usize = 256;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SubtitleEvent {
    Partial(String),
    /// Full replacement for a provider's still-open sentence.  Bailian sends
    /// cumulative revisions, so treating those as Partial would duplicate text.
    Replace(String),
    /// A provider finalised a sentence. Constructed by the providers; the sink
    /// resolves it into [`SubtitleEvent::FinalLine`] for the durable recorder.
    Final(String),
    /// A sentence that really was finalised, carrying the immutable, complete
    /// line the hub appended to its history (P1-06).
    ///
    /// Produced by [`SubtitleSink::push`] and consumed by
    /// [`SubtitleHub::subscribe_finalized`] (the recorder's feed), so the
    /// recorder never has to guess "which line was that?" by re-reading
    /// `history().last()` — which records the wrong sentence under concurrent
    /// finals, a `Cleared`, or a broadcast backlog.
    ///
    /// `text` is kept next to the line so the variant is self-describing and
    /// `ws_payload` can render it exactly like `Final`.
    ///
    /// It is deliberately NOT sent on the `subscribe()` (wire) feed: the
    /// overlay/admin fan-out already receives the corresponding `Final` and
    /// would otherwise push the same `{"type":"final"}` frame twice.
    FinalLine {
        text: String,
        line: SubtitleLine,
    },
    Cleared,
}

/// Maximum number of finished sentences kept in the hub's in-memory history.
///
/// The admin panel's history list and the OBS text source only ever render the
/// tail of this list, so it is bounded; without the cap a provider that only
/// ever emits direct finals (never an open partial) grows it for the whole
/// session (P1-07).
pub const HISTORY_LIMIT: usize = 200;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubtitleLine {
    pub id: String,
    pub text: String,
    pub language: String,
    pub started_at_ms: i64,
    pub updated_at_ms: i64,
    pub finalised: bool,
}

#[derive(Default)]
pub struct SubtitleState {
    pub current: Option<SubtitleLine>,
    pub history: Vec<SubtitleLine>,
    /// Some streaming services retransmit the terminal event.  We do not
    /// receive a stable sentence ID from every provider, so retain a very
    /// short local guard only for an identical final arriving without an
    /// open sentence.
    last_final: Option<(String, i64)>,
}

#[derive(Clone)]
pub struct SubtitleSink {
    tx: broadcast::Sender<SubtitleEvent>,
    /// Feed consumed by the durable recorder (`crate::recording::spawn`); see
    /// [`SubtitleHub::subscribe_finalized`].
    finalized_tx: broadcast::Sender<SubtitleEvent>,
    state: Arc<Mutex<SubtitleState>>,
}

/// Append a finished sentence to the bounded history.
///
/// Both finalising paths (an open sentence closed by a final, and a provider
/// that only ever sends direct finals) must go through here: the direct-final
/// path used to push without the cap, so such a provider grew the history for
/// the whole session (P1-07).
fn push_history_capped(s: &mut SubtitleState, line: SubtitleLine) {
    s.history.push(line);
    if s.history.len() > HISTORY_LIMIT {
        let drop = s.history.len() - HISTORY_LIMIT;
        s.history.drain(0..drop);
    }
}

impl SubtitleSink {
    pub fn push(&self, ev: SubtitleEvent) {
        // The line appended to the history by *this* call, if any. Captured
        // under the same lock as the append, so it can never be confused with a
        // later final (P1-06).
        let mut finalized: Option<SubtitleLine> = None;
        match &ev {
            SubtitleEvent::Partial(text) => {
                let mut s = self.state.lock();
                let now = chrono::Utc::now().timestamp_millis();
                if let Some(cur) = s.current.as_mut() {
                    cur.text.push_str(text);
                    cur.updated_at_ms = now;
                    cur.finalised = false;
                } else {
                    s.current = Some(SubtitleLine {
                        id: uuid::Uuid::new_v4().to_string(),
                        text: text.clone(),
                        language: "auto".into(),
                        started_at_ms: now,
                        updated_at_ms: now,
                        finalised: false,
                    });
                }
            }
            SubtitleEvent::Replace(text) => {
                let mut s = self.state.lock();
                let now = chrono::Utc::now().timestamp_millis();
                if let Some(cur) = s.current.as_mut() {
                    cur.text = text.clone();
                    cur.updated_at_ms = now;
                    cur.finalised = false;
                } else {
                    s.current = Some(SubtitleLine {
                        id: uuid::Uuid::new_v4().to_string(),
                        text: text.clone(),
                        language: "auto".into(),
                        started_at_ms: now,
                        updated_at_ms: now,
                        finalised: false,
                    });
                }
            }
            SubtitleEvent::Final(text) => {
                let mut s = self.state.lock();
                let now = chrono::Utc::now().timestamp_millis();
                if s.current.is_none()
                    && s.last_final
                        .as_ref()
                        .is_some_and(|(last, at)| last == text && now.saturating_sub(*at) < 500)
                {
                    return;
                }
                if s.current.is_some() {
                    // A final result is authoritative.  It may be shorter
                    // than a partial after punctuation or a decoding revision,
                    // so length is not a safe proxy for correctness.
                    let mut line = {
                        let cur = s.current.as_mut().expect("is_some() checked above");
                        if !text.is_empty() {
                            cur.text = text.clone();
                        }
                        cur.finalised = true;
                        cur.updated_at_ms = now;
                        cur.clone()
                    };
                    if let Some(lang) = detect_lang(&line.text) {
                        line.language = lang;
                    }
                    finalized = Some(line.clone());
                    push_history_capped(&mut s, line);
                    s.current = None;
                } else if !text.is_empty() {
                    let mut line = SubtitleLine {
                        id: uuid::Uuid::new_v4().to_string(),
                        text: text.clone(),
                        language: "auto".into(),
                        started_at_ms: now,
                        updated_at_ms: now,
                        finalised: true,
                    };
                    if let Some(lang) = detect_lang(&line.text) {
                        line.language = lang;
                    }
                    finalized = Some(line.clone());
                    push_history_capped(&mut s, line);
                    s.current = None;
                }
                if !text.is_empty() {
                    s.last_final = Some((text.clone(), now));
                }
            }
            SubtitleEvent::FinalLine { .. } => {
                // Only this sink produces these, for the recorder feed; they are
                // never pushed back in. If one arrives it has already been
                // applied to the state, so appending it again would duplicate
                // the sentence.
            }
            SubtitleEvent::Cleared => {
                let mut s = self.state.lock();
                s.current = None;
            }
        }
        if let (Some(line), SubtitleEvent::Final(text)) = (finalized, &ev) {
            // Hand the recorder the exact line, out of band.
            //
            // Why a second channel instead of sending `FinalLine` on the wire
            // feed: every existing subscriber (the overlay/admin WebSocket
            // fan-out in `server.rs`, the provider drain tests in `llm.rs`)
            // matches `SubtitleEvent::Final(String)`. Replacing it on that feed
            // would either duplicate the `final` frame on the wire or silently
            // stop matching those subscribers — a cross-module break for a
            // purely internal need. The wire feed stays byte-identical; the
            // recorder gets a feed that carries finished sentences only, which
            // also means「清空历史」(`Cleared`) structurally cannot delete or
            // corrupt the recording.
            let _ = self.finalized_tx.send(SubtitleEvent::FinalLine {
                text: text.clone(),
                line,
            });
        }
        let _ = self.tx.send(ev);
    }

    pub fn current(&self) -> Option<SubtitleLine> {
        self.state.lock().current.clone()
    }

    pub fn history(&self) -> Vec<SubtitleLine> {
        self.state.lock().history.clone()
    }
}

pub struct SubtitleHub {
    state: Arc<Mutex<SubtitleState>>,
    tx: broadcast::Sender<SubtitleEvent>,
    /// Feed consumed by the durable recorder (`crate::recording::spawn`).
    ///
    /// It carries only `FinalLine` events — one per finished sentence, with the
    /// immutable line attached. Kept separate from `tx` so the overlay/admin
    /// wire format and every existing `subscribe()` consumer are unaffected.
    finalized_tx: broadcast::Sender<SubtitleEvent>,
}

impl Default for SubtitleHub {
    fn default() -> Self {
        let (tx, _rx) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let (finalized_tx, _finalized_rx) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Self {
            state: Arc::new(Mutex::new(SubtitleState::default())),
            tx,
            finalized_tx,
        }
    }
}

impl SubtitleHub {
    pub fn sink(&self) -> SubtitleSink {
        SubtitleSink {
            tx: self.tx.clone(),
            finalized_tx: self.finalized_tx.clone(),
            state: self.state.clone(),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<SubtitleEvent> {
        self.tx.subscribe()
    }

    /// Subscribe to the durable recorder's feed: one
    /// [`SubtitleEvent::FinalLine`] per finished sentence, each carrying the
    /// exact line that was appended to the history (P1-06).
    pub fn subscribe_finalized(&self) -> broadcast::Receiver<SubtitleEvent> {
        self.finalized_tx.subscribe()
    }

    pub fn current(&self) -> Option<SubtitleLine> {
        self.state.lock().current.clone()
    }

    pub fn history(&self) -> Vec<SubtitleLine> {
        self.state.lock().history.clone()
    }

    pub fn clear(&self) {
        let _ = self.tx.send(SubtitleEvent::Cleared);
        self.state.lock().current = None;
    }

    /// Drop the finished-sentence history as well as the open line.
    ///
    /// The panel's 「清空历史」 button used to clear only its locally imported
    /// rows, so the server-side list came straight back on the next refresh and
    /// the button looked broken. This is the backing operation for
    /// `POST /api/subtitles/history/clear`; the on-disk recording
    /// (`/api/recordings/export`) is a separate store and is NOT affected.
    ///
    /// That separation is now structural, not just documented: the durable
    /// recorder consumes [`SubtitleHub::subscribe_finalized`], which never
    /// carries `Cleared`, so clearing the display buffer cannot erase evidence
    /// (deleting evidence is `RecordingStore::delete_disk`) — P1-06.
    pub fn clear_history(&self) {
        let _ = self.tx.send(SubtitleEvent::Cleared);
        let mut s = self.state.lock();
        s.current = None;
        s.history.clear();
        s.last_final = None;
    }
}

/// Wire format for `/ws/subtitles` clients (overlay + admin preview).
/// Must be built by hand: serde_json refuses internally-tagged newtype
/// variants that hold a plain String ("cannot serialize tagged newtype
/// variant ... containing a string").
pub fn ws_payload(ev: &SubtitleEvent) -> serde_json::Value {
    match ev {
        SubtitleEvent::Partial(text) => serde_json::json!({"type": "partial", "text": text}),
        SubtitleEvent::Replace(text) => {
            serde_json::json!({"type": "partial", "text": text, "replace": true})
        }
        SubtitleEvent::Final(text) => serde_json::json!({"type": "final", "text": text}),
        // Never sent on the wire feed today, but rendered identically to
        // `Final` so the frame stays correct if the recorder feed ever grows a
        // subscriber that forwards it.
        SubtitleEvent::FinalLine { text, .. } => serde_json::json!({"type": "final", "text": text}),
        SubtitleEvent::Cleared => serde_json::json!({"type": "cleared"}),
    }
}

fn detect_lang(text: &str) -> Option<String> {
    Some(
        match crate::lang::detect(text) {
            crate::lang::Language::Chinese => "zh",
            crate::lang::Language::English => "en",
            crate::lang::Language::Japanese => "ja",
            crate::lang::Language::Korean => "ko",
            crate::lang::Language::Other => "auto",
        }
        .to_string(),
    )
}

// Keep the unused Instant import so the file builds cleanly if the hub
// later wants monotonic timeouts. Suppress the warning without touching
// every call site.
#[allow(dead_code)]
fn _now_mono() -> Instant {
    Instant::now()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_payload_shape() {
        // The browser overlay / admin preview expect
        // {"type":"partial","text":...} etc. Internally-tagged newtype
        // variants holding a plain String are a serde_json trap (they do
        // not serialize at all), so pin the exact wire format here.
        let p = ws_payload(&SubtitleEvent::Partial("hello".to_string()));
        assert_eq!(p, serde_json::json!({"type": "partial", "text": "hello"}));
        let f = ws_payload(&SubtitleEvent::Final("world".to_string()));
        assert_eq!(f, serde_json::json!({"type": "final", "text": "world"}));
        // The recorder feed's variant must render the same frame, so a future
        // forwarder cannot change the wire format by accident.
        let fl = ws_payload(&SubtitleEvent::FinalLine {
            text: "world".to_string(),
            line: SubtitleLine {
                id: "id".into(),
                text: "world".into(),
                language: "en".into(),
                started_at_ms: 1,
                updated_at_ms: 2,
                finalised: true,
            },
        });
        assert_eq!(fl, serde_json::json!({"type": "final", "text": "world"}));
        let r = ws_payload(&SubtitleEvent::Replace("fixed".to_string()));
        assert_eq!(
            r,
            serde_json::json!({"type":"partial", "text":"fixed", "replace":true})
        );
        let c = ws_payload(&SubtitleEvent::Cleared);
        assert_eq!(c, serde_json::json!({"type": "cleared"}));
    }

    #[test]
    fn final_can_shorten_a_partial_revision() {
        let hub = SubtitleHub::default();
        let sink = hub.sink();
        sink.push(SubtitleEvent::Replace("这是一段需要修正的错误字幕".into()));
        sink.push(SubtitleEvent::Final("这是一段字幕".into()));
        let history = hub.history();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].text, "这是一段字幕");
    }

    #[test]
    fn ignores_immediate_duplicate_final_without_an_open_sentence() {
        let hub = SubtitleHub::default();
        let sink = hub.sink();
        sink.push(SubtitleEvent::Final("重复的最终字幕".into()));
        sink.push(SubtitleEvent::Final("重复的最终字幕".into()));
        assert_eq!(hub.history().len(), 1);
    }

    /// `clear()` (「清空字幕」) only drops the open line — the panel's
    /// 「清空历史」 needs `clear_history()`, which must also empty the history
    /// and forget the duplicate-final guard so the same sentence can be
    /// recorded again after the clear.
    #[test]
    fn clear_history_drops_the_open_line_and_the_history() {
        let hub = SubtitleHub::default();
        let sink = hub.sink();
        sink.push(SubtitleEvent::Final("第一句".into()));
        sink.push(SubtitleEvent::Replace("正在说的这一句".into()));
        assert_eq!(hub.history().len(), 1);
        assert!(hub.current().is_some());

        hub.clear();
        assert_eq!(
            hub.history().len(),
            1,
            "clear() must keep the history for the panel"
        );

        hub.clear_history();
        assert_eq!(
            hub.history().len(),
            0,
            "clear_history() must empty the history"
        );
        assert!(
            hub.current().is_none(),
            "clear_history() must drop the open line"
        );

        // The dedupe guard is reset with it: a re-sent identical final after an
        // explicit clear is a new sentence, not a retransmission.
        sink.push(SubtitleEvent::Final("第一句".into()));
        assert_eq!(hub.history().len(), 1);
    }

    /// A provider that never opens a partial (every result is a direct final)
    /// used to push into `history` without the cap, so the list grew for the
    /// whole session (P1-07).
    #[test]
    fn direct_finals_are_capped_at_the_history_limit() {
        let hub = SubtitleHub::default();
        let sink = hub.sink();
        for i in 0..250 {
            // Distinct texts: identical ones would be swallowed by the
            // duplicate-final guard instead of exercising the cap.
            sink.push(SubtitleEvent::Final(format!("直接收尾的第 {i} 句")));
        }
        assert_eq!(hub.history().len(), HISTORY_LIMIT);
        assert_eq!(
            hub.history().last().map(|l| l.text.as_str()),
            Some("直接收尾的第 249 句"),
            "the cap must drop the oldest sentences, not the newest"
        );
    }

    /// The recorder feed must hand over the line that was finalised, one event
    /// per push, in order — not "whatever history ends with" (P1-06).
    #[test]
    fn final_events_carry_the_finalised_line() {
        let hub = SubtitleHub::default();
        let mut recorder = hub.subscribe_finalized();
        let sink = hub.sink();

        sink.push(SubtitleEvent::Final("第一句".into()));
        // Second final lands before the consumer polls, so by the time the
        // first event is handled the hub's history already ends with 第二句.
        sink.push(SubtitleEvent::Final("第二句".into()));
        assert_eq!(
            hub.history().last().map(|l| l.text.as_str()),
            Some("第二句")
        );

        let mut carried = Vec::new();
        for _ in 0..2 {
            match recorder.try_recv().expect("finalised event") {
                SubtitleEvent::FinalLine { text, line } => {
                    assert_eq!(text, line.text, "the event must be self-consistent");
                    carried.push(line.text);
                }
                other => panic!("the recorder feed must carry finalised lines, got {other:?}"),
            }
        }
        assert_eq!(carried, vec!["第一句".to_string(), "第二句".to_string()]);
    }

    /// The overlay/admin wire feed keeps the exact events it had before: one
    /// `Final(String)` per finalised sentence, never a second frame for the
    /// recorder's `FinalLine`.
    #[test]
    fn the_wire_feed_is_unchanged_by_the_recorder_feed() {
        let hub = SubtitleHub::default();
        let mut wire = hub.subscribe();
        let sink = hub.sink();
        sink.push(SubtitleEvent::Partial("正在".into()));
        sink.push(SubtitleEvent::Final("正在说的一句".into()));

        assert!(matches!(wire.try_recv(), Ok(SubtitleEvent::Partial(t)) if t == "正在"));
        assert!(matches!(wire.try_recv(), Ok(SubtitleEvent::Final(t)) if t == "正在说的一句"));
        assert!(
            wire.try_recv().is_err(),
            "a final must produce exactly one wire frame"
        );
    }

    /// 「清空历史」 must not reach the recorder: the recorder feed is a
    /// separate channel and `clear_history` only publishes `Cleared` on the wire
    /// feed (P1-06: deleting evidence ≠ clearing a display buffer).
    #[test]
    fn clearing_history_does_not_reach_the_recorder_feed() {
        let hub = SubtitleHub::default();
        let mut recorder = hub.subscribe_finalized();
        let sink = hub.sink();
        sink.push(SubtitleEvent::Final("已录制的一句".into()));
        hub.clear_history();
        assert!(matches!(
            recorder.try_recv(),
            Ok(SubtitleEvent::FinalLine { .. })
        ));
        assert!(
            recorder.try_recv().is_err(),
            "Cleared must not be published on the recorder feed"
        );
    }
}
