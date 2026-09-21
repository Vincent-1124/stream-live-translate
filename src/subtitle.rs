//! Subtitle event hub. The LLM provider pushes `SubtitleEvent`s into the
//! hub; the WebSocket fanout task reads from it and pushes to the browser
//! overlay and OBS text source.

use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SubtitleEvent {
    Partial(String),
    /// Full replacement for a provider's still-open sentence.  Bailian sends
    /// cumulative revisions, so treating those as Partial would duplicate text.
    Replace(String),
    Final(String),
    Cleared,
}

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
    state: Arc<Mutex<SubtitleState>>,
}

impl SubtitleSink {
    pub fn push(&self, ev: SubtitleEvent) {
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
                    s.current = Some(SubtitleLine { id: uuid::Uuid::new_v4().to_string(), text: text.clone(), language: "auto".into(), started_at_ms: now, updated_at_ms: now, finalised: false });
                }
            }
            SubtitleEvent::Final(text) => {
                let mut s = self.state.lock();
                let now = chrono::Utc::now().timestamp_millis();
                if s.current.is_none()
                    && s.last_final.as_ref().is_some_and(|(last, at)| {
                        last == text && now.saturating_sub(*at) < 500
                    })
                {
                    return;
                }
                if let Some(cur) = s.current.as_mut() {
                    // A final result is authoritative.  It may be shorter
                    // than a partial after punctuation or a decoding revision,
                    // so length is not a safe proxy for correctness.
                    if !text.is_empty() {
                        cur.text = text.clone();
                    }
                    cur.finalised = true;
                    cur.updated_at_ms = now;
                    let mut line = cur.clone();
                    if let Some(lang) = detect_lang(&line.text) {
                        line.language = lang;
                    }
                    s.history.push(line);
                    if s.history.len() > 200 {
                        let drop = s.history.len() - 200;
                        s.history.drain(0..drop);
                    }
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
                    s.history.push(line.clone());
                    s.current = None;
                }
                if !text.is_empty() {
                    s.last_final = Some((text.clone(), now));
                }
            }
            SubtitleEvent::Cleared => {
                let mut s = self.state.lock();
                s.current = None;
            }
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
}

impl Default for SubtitleHub {
    fn default() -> Self {
        let (tx, _rx) = broadcast::channel(256);
        Self {
            state: Arc::new(Mutex::new(SubtitleState::default())),
            tx,
        }
    }
}

impl SubtitleHub {
    pub fn sink(&self) -> SubtitleSink {
        SubtitleSink {
            tx: self.tx.clone(),
            state: self.state.clone(),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<SubtitleEvent> {
        self.tx.subscribe()
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
}

/// Wire format for `/ws/subtitles` clients (overlay + admin preview).
/// Must be built by hand: serde_json refuses internally-tagged newtype
/// variants that hold a plain String ("cannot serialize tagged newtype
/// variant ... containing a string").
pub fn ws_payload(ev: &SubtitleEvent) -> serde_json::Value {
    match ev {
        SubtitleEvent::Partial(text) => serde_json::json!({"type": "partial", "text": text}),
        SubtitleEvent::Replace(text) => serde_json::json!({"type": "partial", "text": text, "replace": true}),
        SubtitleEvent::Final(text) => serde_json::json!({"type": "final", "text": text}),
        SubtitleEvent::Cleared => serde_json::json!({"type": "cleared"}),
    }
}

fn detect_lang(text: &str) -> Option<String> {
    Some(match crate::lang::detect(text) {
        crate::lang::Language::Chinese => "zh",
        crate::lang::Language::English => "en",
        crate::lang::Language::Japanese => "ja",
        crate::lang::Language::Korean => "ko",
        crate::lang::Language::Other => "auto",
    }
    .to_string())
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
        let r = ws_payload(&SubtitleEvent::Replace("fixed".to_string()));
        assert_eq!(r, serde_json::json!({"type":"partial", "text":"fixed", "replace":true}));
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
}
