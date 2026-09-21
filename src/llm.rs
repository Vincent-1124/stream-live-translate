//! LLM provider abstraction. Each provider turns PCM s16le mono audio into
//! a stream of `SubtitleEvent`s. The pipeline doesn't care which provider is
//! plugged in.
//!
//! Currently implemented:
//!   * `qwen-realtime` — Aliyun DashScope realtime API
//!     (wss://dashscope.aliyuncs.com/api-ws/v1/realtime). Auto-adapts:
//!     translation models (…livetranslate…) get a `translation.language`
//!     session; ASR/audio models (qwen3-asr-*, qwen-audio-*, …) get an
//!     `input_audio_transcription` session instead.
//!   * `openai-realtime` — any OpenAI-compatible realtime WebSocket
//!     endpoint: OpenAI itself (gpt-4o-realtime) or DashScope
//!     compatible-mode (wss://dashscope.aliyuncs.com/compatible-mode/v1/realtime)
//!     for qwen-audio / ASR models. `instructions` are only sent when the
//!     user configured a system prompt, so pure ASR models don't choke.
//!   * `mock` — emits canned Chinese sentences; useful for end-to-end
//!     UI/UX testing without burning API quota.

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use base64::Engine;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, http, Message};
use tracing::{debug, info, warn};

use crate::config::LlmConfig;
use crate::subtitle::{SubtitleEvent, SubtitleSink};

#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Provider identifier (e.g. `qwen-realtime`).
    fn name(&self) -> &'static str;

    /// Open the streaming session. `on_event` is invoked for every partial
    /// or final transcript the model produces.
    ///
    /// `context_rounds` 是热词（R10）经官方"上下文增强"下发的轮次文本，已按
    /// 400 字符/轮切好；为空表示本次会话不携带上下文。只有百炼 Fun-ASR 通道
    /// 会把它放进 `run-task` 的 `payload.input.context`。
    async fn run(
        self: Arc<Self>,
        audio_rx: tokio::sync::mpsc::Receiver<Vec<i16>>,
        sink: SubtitleSink,
        context_rounds: Vec<String>,
    ) -> Result<()>;

    /// 运行期热词源。pipeline 在会话启动前后都会写入；百炼通道订阅它并在
    /// 词表变化时用 `continue-task` 下发。默认忽略（其它通道暂不支持热词）。
    fn set_hotwords(&self, _feed: crate::hotwords::HotwordFeed) {}

    /// 热词下发状态槽；provider 每次真正下发后写一次。
    fn set_hotword_status(&self, _status: Arc<parking_lot::RwLock<crate::hotwords::HotwordStatus>>) {
    }
}

pub fn build(cfg: &LlmConfig) -> Result<Arc<dyn LlmProvider>> {
    match cfg.provider.as_str() {
        "qwen-realtime" => Ok(Arc::new(qwen::QwenRealtime::new(cfg.clone())?)),
        "openai-realtime" => Ok(Arc::new(openai::OpenAiRealtime::new(cfg.clone())?)),
        "fun-asr-realtime" => Ok(Arc::new(funasr::FunAsr::new(cfg.clone())?)),
        "bailian-fun-asr" => Ok(Arc::new(bailian::BailianFunAsr::new(cfg.clone())?)),
        "mock" => Ok(Arc::new(mock::MockProvider::new(cfg.clone())?) as Arc<dyn LlmProvider>),
        other => Err(anyhow!("unknown LLM provider `{other}`")),
    }
}

/// Validate a configured service without starting the live audio pipeline.
/// The Bailian probe starts and finishes an empty task so it checks both the
/// saved credentials and the selected model.
pub async fn test_connection(cfg: &LlmConfig) -> Result<()> {
    match cfg.provider.as_str() {
        "bailian-fun-asr" => bailian::test_connection(cfg.clone()).await,
        "mock" => Ok(()),
        other => Err(anyhow!("provider `{other}` does not support a connection test yet")),
    }
}

/// Unwrap a realtime WebSocket handshake failure into a human-readable
/// error. DashScope/OpenAI answer failed upgrades with an HTTP status +
/// JSON body (invalid key, unknown model, workspace endpoint required…);
/// the default Display swallows that detail.
fn ws_connect_error(e: tokio_tungstenite::tungstenite::Error, ctx: &str) -> anyhow::Error {
    use tokio_tungstenite::tungstenite::Error as WsErr;
    match e {
        WsErr::Http(resp) => {
            let body = resp
                .body()
                .as_ref()
                .map(|b| String::from_utf8_lossy(b).trim().to_string())
                .unwrap_or_default();
            anyhow!(
                "{ctx}被服务器拒绝：HTTP {} {}（请检查 API Key、模型名；若 Key 属于百炼业务空间，请在 Base URL 填专属域名）",
                resp.status().as_u16(),
                body
            )
        }
        other => anyhow!(other).context(ctx.to_string()),
    }
}

// ---------- Qwen DashScope realtime ----------

pub mod qwen {
    use super::*;

    const DEFAULT_ENDPOINT: &str = "wss://dashscope.aliyuncs.com/api-ws/v1/realtime";

    pub struct QwenRealtime {
        cfg: LlmConfig,
        endpoint: String,
    }

    impl QwenRealtime {
        pub fn new(cfg: LlmConfig) -> Result<Self> {
            let endpoint = cfg
                .endpoint
                .clone()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_ENDPOINT.to_string());
            if cfg.api_key.is_empty() {
                return Err(anyhow!("Qwen API key is empty; please fill it in the admin panel"));
            }
            // Fun-ASR 流式识别（qwen-audio-*-asr-flash-streaming 等）走的是另一套
            // WebSocket 协议，与本插件使用的 DashScope Realtime（/api-ws/v1/realtime）
            // 不兼容，直接给出明确提示，而不是连接后各种报错。
            if cfg.model.to_lowercase().contains("streaming") {
                return Err(anyhow!(
                    "模型 {} 属于 Fun-ASR 流式识别(Streaming)接口，与插件使用的 DashScope Realtime 协议不兼容，无法出字幕。请改用 Realtime 语音模型：qwen3.5-livetranslate-flash-realtime（同传翻译）、qwen3-asr-flash-realtime / qwen-audio-3.0-realtime-flash（实时识别）等。",
                    cfg.model
                ));
            }
            Ok(Self { cfg, endpoint })
        }
    }

    #[async_trait]
    impl LlmProvider for QwenRealtime {
        fn name(&self) -> &'static str {
            "qwen-realtime"
        }

        async fn run(
            self: Arc<Self>,
            mut audio_rx: tokio::sync::mpsc::Receiver<Vec<i16>>,
            sink: SubtitleSink,
            _context_rounds: Vec<String>,
        ) -> Result<()> {
            let url = format!("{}?model={}", self.endpoint, self.cfg.model);
            let mut req = url
                .into_client_request()
                .with_context(|| "build qwen ws request")?;
            req.headers_mut()
                .insert("Authorization", http::HeaderValue::from_str(&format!("Bearer {}", self.cfg.api_key))?);

            let (ws, _resp) = match tokio_tungstenite::connect_async(req).await {
                Ok(pair) => pair,
                Err(e) => return Err(ws_connect_error(e, "连接 DashScope 实时服务")),
            };
            info!("connected to qwen realtime");
            let (mut write_half, mut read_half) = ws.split();

            // Configure session. The DashScope realtime API serves two
            // model families with different session schemas:
            //   * Translation models (…livetranslate…): need
            //     `translation.language` (default is "en", so mandatory).
            //     `input_audio_transcription` only accepts a dedicated ASR
            //     model name or null; we disable it because the overlay
            //     shows the translation stream only.
            //   * ASR / audio models (qwen3-asr-*, qwen-audio-*, …):
            //     `translation` is not in their schema (sending it fails
            //     the session); instead enable `input_audio_transcription`
            //     so we receive transcription delta/completed events.
            // Both use server VAD: the server detects speech end itself
            // and auto-commits, so we feed it a *continuous* audio stream.
            let asr_mode = !self.cfg.model.to_lowercase().contains("livetranslate");
            let segment_ms = self.cfg.segment_ms;
            // 低延迟模式（segment_ms > 0）= 手动模式：关掉服务端 VAD，由本机按段提交。
            // 服务端 VAD 开着的时候，它只按自己的判定提交（即“等一句话说完才出结果”），
            // 客户端发的 commit 会被忽略，低延迟就失效了；官方 manual 模式下
            // 客户端 commit() 才会触发识别 / 翻译。
            let manual_mode = segment_ms > 0;
            let turn_detection = if manual_mode {
                serde_json::Value::Null
            } else {
                serde_json::json!({ "type": "server_vad" })
            };
            let session_cfg: serde_json::Value = if asr_mode {
                serde_json::json!({
                    "modalities": ["text"],
                    "sample_rate": 16000,
                    "input_audio_format": "pcm",
                    "input_audio_transcription": { "model": self.cfg.model },
                    "turn_detection": turn_detection
                })
            } else {
                serde_json::json!({
                    "modalities": ["text"],
                    "sample_rate": 16000,
                    "input_audio_format": "pcm",
                    "input_audio_transcription": null,
                    "turn_detection": turn_detection.clone(),
                    "translation": { "language": self.cfg.target_lang }
                })
            };
            let session = serde_json::json!({ "type": "session.update", "session": session_cfg });
            write_half
                .send(Message::Text(session.to_string().into()))
                .await?;

            // Pump audio in one task, read events in another.
            let sink_for_read = sink.clone();
            let read = {
                let sink = sink_for_read.clone();
                async move {
                    // pending：当前这一轮/这一句服务端给到的「累计稳定文本」。
                    // 用来把累积类事件 diff 成「只推新增」，避免重复 append。
                    let mut pending = String::new();
                    // 通道隔离：livetranslate 走“译文”通道；其它（ASR / 语音识别 /
                    // 语音对话模型）只显示源语言转写通道，把模型自己的闲聊回应
                    // （response.text.* / audio_transcript.*）丢掉，避免字幕出现废话。
                    let transcribe_channel = asr_mode;
                    while let Some(msg) = read_half.next().await {
                        let msg = match msg {
                            Ok(m) => m,
                            Err(e) => {
                                warn!(error=%e, "qwen ws read error");
                                break;
                            }
                        };
                        match msg {
                            Message::Text(t) => {
                                if let Ok(ev) = serde_json::from_str::<QwenEvent>(&t) {
                                    apply_qwen_event(&ev, &sink, &mut pending, transcribe_channel);
                                } else {
                                    debug!(payload=%t, "unparsed qwen event");
                                }
                            }
                            Message::Close(c) => {
                                info!(?c, "qwen ws closed by server");
                                break;
                            }
                            Message::Ping(_)
                            | Message::Pong(_)
                            | Message::Binary(_)
                            | Message::Frame(_) => {}
                        }
                    }
                }
            };

            // Audio pump with optional low-latency segmentation.
            //
            //   * segment_ms == 0（默认）：只 append。服务端 server_vad 在
            //     「一句话说完、静音达标」后自行 commit，整句返回 —— 句子最
            //     完整，但字幕要等整句话说完（延迟≈整句话时长）。
            //   * segment_ms > 0（低延迟模式）：切到手动模式（会话里关掉服务端
            //     VAD），本机按段收集「正在说的语音」并 input_audio_buffer.commit，
            //     由客户端 commit 触发识别/翻译，字幕按段推进。
            //     手动模式下**只把说话的音频**放进缓冲区：段与段之间的静音不塞
            //     进去，否则下次提交会变成一大段空白音频（延迟暴涨、还容易幻觉）。
            //
            // 手动模式下两个通道的差别：
            //   * ASR/转写通道：commit 即出识别结果，不需要触发模型生成；
            //   * 同传翻译通道：译文属于模型的 response，提交后补一个
            //     response.create 确保产出（若服务端已自动产出，overlay 的
            //     重复句检测会兜掉多余的一句）。
            let translation_channel = !asr_mode;
            let write = async move {
                let commit_enabled = segment_ms > 0;
                let mut voiced_ms: u64 = 0; // 当前这段里已累计的有声时长(ms)
                let mut tail_ms: u64 = 0; // 有声之后跟随的静音时长(ms)
                let mut in_segment = false; // 是否正在收集一段语音（手动模式用）

                while let Some(chunk) = audio_rx.recv().await {
                    if chunk.is_empty() {
                        continue;
                    }
                    let ms = (chunk.len() as u64) * 1000 / 16_000;
                    let voiced = crate::vad::rms(&chunk) >= 0.008;

                    if commit_enabled {
                        if voiced {
                            if !in_segment {
                                in_segment = true;
                                voiced_ms = 0;
                                tail_ms = 0;
                            }
                            voiced_ms = voiced_ms.saturating_add(ms);
                            tail_ms = 0;
                        } else if in_segment {
                            tail_ms = tail_ms.saturating_add(ms);
                        }
                        // 非说话期间（两段之间）不往缓冲区里塞音频。
                        if !in_segment {
                            continue;
                        }
                    }

                    let mut bytes = Vec::with_capacity(chunk.len() * 2);
                    for s in &chunk {
                        bytes.extend_from_slice(&s.to_le_bytes());
                    }
                    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                    let msg = serde_json::json!({
                        "type": "input_audio_buffer.append",
                        "audio": b64,
                    });
                    if write_half.send(Message::Text(msg.to_string().into())).await.is_err() {
                        break;
                    }
                    if !commit_enabled {
                        continue;
                    }

                    if voiced && voiced_ms >= segment_ms {
                        // 说满一段就提交（人还在说，段内继续累计下一段）。
                        let cm = serde_json::json!({ "type": "input_audio_buffer.commit" });
                        if write_half
                            .send(Message::Text(cm.to_string().into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                        if translation_channel {
                            let rc = serde_json::json!({ "type": "response.create" });
                            if write_half
                                .send(Message::Text(rc.to_string().into()))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        voiced_ms = 0;
                        tail_ms = 0;
                    } else if tail_ms >= 400 {
                        // 说完（尾静音够长）收尾提交这段，然后停止收集等下一段。
                        let cm = serde_json::json!({ "type": "input_audio_buffer.commit" });
                        if write_half
                            .send(Message::Text(cm.to_string().into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                        if translation_channel {
                            let rc = serde_json::json!({ "type": "response.create" });
                            if write_half
                                .send(Message::Text(rc.to_string().into()))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        voiced_ms = 0;
                        tail_ms = 0;
                        in_segment = false;
                    }
                }
                // 会话结束前把没提交完的尾巴交出去。
                if commit_enabled && in_segment {
                    let cm = serde_json::json!({ "type": "input_audio_buffer.commit" });
                    let _ = write_half
                        .send(Message::Text(cm.to_string().into()))
                        .await;
                    if translation_channel {
                        let rc = serde_json::json!({ "type": "response.create" });
                        let _ = write_half
                            .send(Message::Text(rc.to_string().into()))
                            .await;
                    }
                }
                // Flush the tail of the session, then close gracefully.
                let _ = write_half
                    .send(Message::Text(
                        serde_json::json!({"type": "session.finish"}).to_string().into(),
                    ))
                    .await;
                let _ = write_half.close().await;
            };

            tokio::select! {
                _ = read => {}
                _ = write => {}
            }
            Ok(())
        }
    }

    #[derive(Debug, Deserialize)]
    #[serde(tag = "type")]
    enum QwenEvent {
        #[serde(rename = "session.created")]
        SessionCreated {
            session: serde_json::Value,
        },
        #[serde(rename = "session.updated")]
        SessionUpdated {
            session: serde_json::Value,
        },
        /// 逐 token 增量（OpenAI 风格）。
        #[serde(rename = "response.text.delta")]
        ResponseTextDelta {
            #[serde(default)]
            delta: Option<String>,
        },
        /// Streaming translation increment. `text` is the confirmed text
        /// *for this event*; `stash` is a speculative tail. Both are
        /// cumulative ("stable prefix" of the current response), so we only
        /// push the part that is new relative to what we already showed.
        #[serde(rename = "response.text.text")]
        ResponseTextText {
            #[serde(default)]
            text: Option<String>,
            #[serde(default)]
            stash: Option<String>,
        },
        /// Final complete translation of one utterance.
        #[serde(rename = "response.text.done")]
        ResponseTextDone {
            #[serde(default)]
            text: Option<String>,
        },
        /// DashScope realtime (audio_transcript channel): cumulative
        /// translation text. `text` is confirmed, `stash` is the tail.
        #[serde(rename = "response.audio_transcript.text")]
        AudioTranscriptText {
            #[serde(default)]
            text: Option<String>,
            #[serde(default)]
            stash: Option<String>,
        },
        #[serde(rename = "response.audio_transcript.delta")]
        AudioTranscriptDelta {
            #[serde(default)]
            delta: Option<String>,
        },
        #[serde(rename = "response.audio_transcript.done")]
        AudioTranscriptDone {
            #[serde(default)]
            text: Option<String>,
        },
        /// Streaming ASR increment.
        #[serde(rename = "conversation.item.input_audio_transcription.delta")]
        TranscriptionDelta {
            #[serde(default)]
            text: Option<String>,
        },
        /// ASR cumulative "stable so far" text (stash).
        #[serde(rename = "conversation.item.input_audio_transcription.text")]
        TranscriptionText {
            #[serde(default)]
            text: Option<String>,
            #[serde(default)]
            stash: Option<String>,
        },
        /// Source-language ASR stream final (only when transcription enabled).
        #[serde(rename = "conversation.item.input_audio_transcription.completed")]
        Completed {
            #[serde(default)]
            transcript: Option<String>,
        },
        #[serde(rename = "error")]
        Error { error: serde_json::Value },
        #[serde(other)]
        Other,
    }

  /// 把一个服务端事件应用成字幕更新。
  /// `pending` 记录当前这一句服务端给出的「累计稳定文本」，累积类事件
  /// （text / stash / transcript）据此只推送新增部分，避免重复。
  ///
  /// `transcribe == true`（非 livetranslate 的 ASR / 语音识别 / 语音对话类
  /// 模型）：只监听源语言转写事件 input_audio_transcription.*，丢弃模型的
  /// response.text.* / audio_transcript.*（那是模型自己的闲聊回应，不是
  /// 说话人内容，显示出来就是“废话”）。
  /// `transcribe == false`（livetranslate 同传模型）：只监听译文事件
  /// response.text.* / audio_transcript.*，转写通道本就未开启。
  fn apply_qwen_event(
    ev: &QwenEvent,
    sink: &SubtitleSink,
    pending: &mut String,
    transcribe: bool,
  ) {
    if transcribe {
        // ---- 转写通道：ASR / 语音识别类模型 ----
        match ev {
            QwenEvent::TranscriptionDelta { text } => {
                if let Some(t) = text {
                    if !t.is_empty() {
                        sink.push(SubtitleEvent::Partial(t.clone()));
                        pending.push_str(t);
                    }
                }
            }
            QwenEvent::TranscriptionText { text, stash } => {
                if let Some(s) = text.as_deref().or(stash.as_deref()) {
                    accumulate(s, sink, pending);
                }
            }
            QwenEvent::Completed { transcript } => {
                finalize_sentence(transcript.as_deref().unwrap_or("").trim(), sink, pending);
            }
            QwenEvent::Error { error } => {
                warn!(?error, "qwen error event");
            }
            _ => {}
        }
    } else {
        // ---- 译文通道：livetranslate 同传翻译 ----
        match ev {
            QwenEvent::ResponseTextDelta { delta } | QwenEvent::AudioTranscriptDelta { delta } => {
                if let Some(d) = delta {
                    if !d.is_empty() {
                        sink.push(SubtitleEvent::Partial(d.clone()));
                        pending.push_str(d);
                    }
                }
            }
            QwenEvent::ResponseTextText { text, stash }
            | QwenEvent::AudioTranscriptText { text, stash } => {
                if let Some(s) = text.as_deref().or(stash.as_deref()) {
                    accumulate(s, sink, pending);
                }
            }
            QwenEvent::ResponseTextDone { text } | QwenEvent::AudioTranscriptDone { text } => {
                finalize_sentence(text.as_deref().unwrap_or("").trim(), sink, pending);
            }
            QwenEvent::Error { error } => {
                warn!(?error, "qwen error event");
            }
            _ => {}
        }
    }
  }

  /// 服务端发来的文本 s 是「到目前为稳定的累积内容」。若它是在我们已显示
  /// 文本上的增长就只推新增；若服务器重开一轮（新的 commit / 修正）就把
  /// 上一句收尾，再用 s 新起一行。
  fn accumulate(s: &str, sink: &SubtitleSink, pending: &mut String) {
    if s.starts_with(pending.as_str()) {
        let pc = pending.chars().count();
        let suffix: String = s.chars().skip(pc).collect();
        if !suffix.is_empty() {
            sink.push(SubtitleEvent::Partial(suffix));
            *pending = s.to_string();
        }
    } else if pending.is_empty() {
        // 新一句的起点。
        sink.push(SubtitleEvent::Partial(s.to_string()));
        *pending = s.to_string();
    } else {
        // 服务器切换到了新的一轮：先收尾上一句，再开新行显示 s。
        sink.push(SubtitleEvent::Final(pending.clone()));
        *pending = s.to_string();
        sink.push(SubtitleEvent::Partial(s.to_string()));
    }
  }

  /// 一轮结果收尾：优先用服务端给的完整文本（可能修正/补全 partial），
  /// 否则用我们累积的文本。收尾后清空 pending。
  fn finalize_sentence(s: &str, sink: &SubtitleSink, pending: &mut String) {
    if !s.is_empty() {
        sink.push(SubtitleEvent::Final(s.to_string()));
    } else if !pending.is_empty() {
        sink.push(SubtitleEvent::Final(pending.clone()));
    }
    pending.clear();
  }
}

// ---------- OpenAI realtime ----------

pub mod openai {
    use super::*;

    const DEFAULT_ENDPOINT: &str = "wss://api.openai.com/v1/realtime";

    pub struct OpenAiRealtime {
        cfg: LlmConfig,
        endpoint: String,
    }

    impl OpenAiRealtime {
        pub fn new(cfg: LlmConfig) -> Result<Self> {
            let endpoint = cfg
                .endpoint
                .clone()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_ENDPOINT.to_string());
            if cfg.api_key.is_empty() {
                return Err(anyhow!("API key 为空，请先在管理面板填写"));
            }
            if endpoint.starts_with("http:") || endpoint.starts_with("https:") {
                return Err(anyhow!(
                    "Base URL 必须是 WebSocket 地址（wss://...），当前填的是 HTTP 接口：{endpoint}"
                ));
            }
            Ok(Self { cfg, endpoint })
        }
    }

    #[async_trait]
    impl LlmProvider for OpenAiRealtime {
        fn name(&self) -> &'static str {
            "openai-realtime"
        }

        async fn run(
            self: Arc<Self>,
            mut audio_rx: tokio::sync::mpsc::Receiver<Vec<i16>>,
            sink: SubtitleSink,
            _context_rounds: Vec<String>,
        ) -> Result<()> {
            let url = format!("{}?model={}", self.endpoint, self.cfg.model);
            let mut req = url.into_client_request()?;
            req.headers_mut().insert(
                "Authorization",
                http::HeaderValue::from_str(&format!("Bearer {}", self.cfg.api_key))?,
            );
            req.headers_mut()
                .insert("OpenAI-Beta", http::HeaderValue::from_static("realtime=v1"));

            let (ws, _) = tokio_tungstenite::connect_async(req)
                .await
                .map_err(|e| ws_connect_error(e, "连接 OpenAI 兼容实时服务"))?;
            let (mut write_half, mut read_half) = ws.split();
            // Build the session config carefully:
            //   * `turn_detection: server_vad` — with null the server never
            //     auto-commits the audio buffer, so no transcript ever comes
            //     out unless the client sends manual commits (we don't).
            //   * `instructions` only when the user configured a system
            //     prompt: pure ASR models (qwen-audio-*, qwen3-asr-* on the
            //     DashScope compatible-mode endpoint) reject or ignore it,
            //     and forcing an interpreter prompt there breaks the session.
            let mut session_cfg = serde_json::json!({
                "modalities": ["text"],
                "input_audio_format": "pcm16",
                "turn_detection": { "type": "server_vad" }
            });
            if let Some(prompt) = self.cfg.system_prompt.as_deref().filter(|p| !p.trim().is_empty()) {
                session_cfg["instructions"] = serde_json::Value::String(prompt.to_string());
            }
            // 低延迟模式 = 手动模式：关掉服务端 VAD，改由本机按段 commit 触发识别。
            // 否则服务端只按自己的判定提交（等一句话说完），客户端 commit 会被忽略。
            if self.cfg.segment_ms > 0 {
                session_cfg["turn_detection"] = serde_json::Value::Null;
            }
            // 实时字幕模式：开启「用户语音转写」通道（OpenAI / GLM 等 OpenAI
            // 兼容 realtime）。OpenAI 官方端点默认用 gpt-4o-mini-transcribe；
            // 其它厂商（如 GLM）没有明确子模型名时先用会话主模型名试探。
            // 实时字幕模式：仅当非「本机网关模式」时生效——网关（如
            // huggingface/speech-to-speech 这类本地 OpenAI Realtime 兼容服务）
            // 直接把要显示的字幕文字经 response.text.* 返回，不需要再等
            // input_audio_transcription 事件。
            let transcribe_mode = self.cfg.transcribe && !self.cfg.gateway_text;
            let segment_ms = self.cfg.segment_ms;
            let gateway_text = self.cfg.gateway_text;
            if transcribe_mode {
                let default_tm = if self.endpoint.to_lowercase().contains("api.openai.com") {
                    "gpt-4o-mini-transcribe".to_string()
                } else {
                    self.cfg.model.clone()
                };
                let tm = {
                    let m = self.cfg.transcription_model.trim().to_string();
                    if m.is_empty() { default_tm } else { m }
                };
                session_cfg["input_audio_transcription"] = serde_json::json!({ "model": tm });
            }
            write_half.send(Message::Text(
                serde_json::json!({ "type": "session.update", "session": session_cfg })
                    .to_string()
                    .into(),
            ))
            .await?;

            let read = {
                let sink = sink.clone();
                async move {
                    while let Some(msg) = read_half.next().await {
                        let Ok(msg) = msg else { break };
                        if let Message::Text(t) = msg {
                            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) {
                                match v.get("type").and_then(|s| s.as_str()).unwrap_or("") {
                                    "conversation.item.input_audio_transcription.delta" => {
                                        if let Some(d) = v.get("delta").and_then(|s| s.as_str()) {
                                            sink.push(SubtitleEvent::Partial(d.to_string()));
                                        }
                                    }
                                    "conversation.item.input_audio_transcription.completed" => {
                                        if let Some(d) = v.get("transcript").and_then(|s| s.as_str()) {
                                            sink.push(SubtitleEvent::Final(d.to_string()));
                                        }
                                    }
                                    // 只显示「说话人的转写」（input_audio_transcription.*）。
                                    // response.text.* 是模型自己的回复（AI 自言自语），
                                    // 不是讲述人讲的话，一律不作为字幕显示。
                                    "response.text.delta" | "response.text.done" => {
                                        debug!(payload=%t, "ignored assistant text (not speaker speech)");
                                    }
                                    "error" => {
                                        warn!(payload=%t, "openai error event");
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                }
            };

            let write = async move {
                // 分段 commit（低延迟用）。只提交音频缓冲区：字幕走的是「说话人转写」
                // 通道，不发送 response.create —— 那会触发模型生成 AI 回复，
                // 字幕就会变成 AI 的自言自语。
                macro_rules! do_commit {
                    () => {{
                        let c = serde_json::json!({ "type": "input_audio_buffer.commit" });
                        if write_half.send(Message::Text(c.to_string().into())).await.is_err() {
                            break;
                        }
                    }};
                }

                let commit_enabled = segment_ms > 0;
                let mut voiced_ms: u64 = 0; // 当前这段里已累计的有声时长(ms)
                let mut tail_ms: u64 = 0; // 有声之后跟随的静音时长(ms)
                let mut in_segment = false; // 是否正在收集一段语音（手动模式用）

                while let Some(chunk) = audio_rx.recv().await {
                    let ms = (chunk.len() as u64) * 1000 / 16_000;
                    let voiced = crate::vad::rms(&chunk) >= 0.008;

                    if commit_enabled {
                        if voiced {
                            if !in_segment {
                                in_segment = true;
                                voiced_ms = 0;
                                tail_ms = 0;
                            }
                            voiced_ms = voiced_ms.saturating_add(ms);
                            tail_ms = 0;
                        } else if in_segment {
                            tail_ms = tail_ms.saturating_add(ms);
                        }
                        // 非说话期间（两段之间）不往缓冲区里塞音频。
                        if !in_segment {
                            continue;
                        }
                    }

                    let mut bytes = Vec::with_capacity(chunk.len() * 2);
                    for s in &chunk {
                        bytes.extend_from_slice(&s.to_le_bytes());
                    }
                    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                    let msg = serde_json::json!({
                        "type": "input_audio_buffer.append",
                        "audio": b64,
                    });
                    if write_half.send(Message::Text(msg.to_string().into())).await.is_err() {
                        break;
                    }
                    if !commit_enabled {
                        continue;
                    }

                    if voiced && voiced_ms >= segment_ms {
                        do_commit!();
                        voiced_ms = 0;
                        tail_ms = 0;
                    } else if tail_ms >= 400 {
                        do_commit!();
                        voiced_ms = 0;
                        tail_ms = 0;
                        in_segment = false;
                    }
                }
                // 会话结束前把尾巴交出去。
                if commit_enabled && in_segment {
                    let c = serde_json::json!({ "type": "input_audio_buffer.commit" });
                    let _ = write_half.send(Message::Text(c.to_string().into())).await;
                }
            };

            tokio::select! {
                _ = read => {}
                _ = write => {}
            }
            Ok(())
        }
    }
}

// ---------- FunASR 本地流式识别 ----------
// 适配阿里 FunASR 私有化部署的实时识别 WebSocket 服务（funasr-wss 风格的
// Docker 一键部署：启动后默认 ws://127.0.0.1:10095，服务端已加载 SenseVoice /
// Fun-ASR-Nano / Paraformer 等流式模型）。协议与 OpenAI Realtime 不同：
//   1) 先发一段 JSON 起始消息（mode=2pass、chunk_size、is_speaking=true…）
//   2) 之后持续发送 16k 单声道 s16le PCM 二进制帧
//   3) 说话结束：发 {"is_speaking": false} 触发服务端出该句最终文本
// 服务端回 JSON：mode 含 online/2pass 的中间结果（text）、offline 的最终结果
// （text / is_final）。本 provider 把中间结果按增量显示为字幕、最终结果收尾。

pub mod funasr {
    use super::*;

    const DEFAULT_ENDPOINT: &str = "ws://127.0.0.1:10095";

    pub struct FunAsr {
        cfg: LlmConfig,
        endpoint: String,
    }

    impl FunAsr {
        pub fn new(cfg: LlmConfig) -> Result<Self> {
            let endpoint = cfg
                .endpoint
                .clone()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_ENDPOINT.to_string());
            if !endpoint.starts_with("ws://") && !endpoint.starts_with("wss://") {
                return Err(anyhow!(
                    "FunASR 端点必须是 WebSocket 地址，例如 ws://127.0.0.1:10095"
                ));
            }
            Ok(Self { cfg, endpoint })
        }
    }

    #[async_trait]
    impl LlmProvider for FunAsr {
        fn name(&self) -> &'static str {
            "fun-asr-realtime"
        }

        async fn run(
            self: Arc<Self>,
            mut audio_rx: tokio::sync::mpsc::Receiver<Vec<i16>>,
            sink: SubtitleSink,
            _context_rounds: Vec<String>,
        ) -> Result<()> {
            let url = self.endpoint.clone();
            let (ws, _resp) = tokio_tungstenite::connect_async(url)
                .await
                .map_err(|e| anyhow!("连接 FunASR 服务失败：{e}"))?;
            let (mut write_half, mut read_half) = ws.split();

            // 起始配置：2pass 模式 = 说话过程中实时给中间结果，语音段结束给最终结果。
            // chunk_size=[5,10,5] 是常见实时配置（5*10ms 前/后文+10*10ms 主块）。
            let start = serde_json::json!({
                "mode": "2pass",
                "chunk_size": [5, 10, 5],
                "wav_name": "stream-live-translate",
                "is_speaking": true,
                "itn": true
            });
            write_half
                .send(Message::Text(start.to_string().into()))
                .await
                .map_err(|e| anyhow!("发送 FunASR 起始消息失败：{e}"))?;

            let sink_read = sink.clone();
            let read = async move {
                let mut seg_partial = String::new(); // 当前句已显示的累计文本
                while let Some(msg) = read_half.next().await {
                    let msg = match msg {
                        Ok(m) => m,
                        Err(e) => {
                            warn!(error=%e, "funasr ws read error");
                            break;
                        }
                    };
                    let Message::Text(t) = msg else { continue };
                    let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) else {
                        continue;
                    };
                    let mtype = v.get("mode").and_then(|s| s.as_str()).unwrap_or("");
                    let text = v
                        .get("text")
                        .and_then(|s| s.as_str())
                        .or_else(|| {
                            v.get("sentence")
                                .and_then(|s| s.get("text"))
                                .and_then(|s| s.as_str())
                        })
                        .unwrap_or("");
                    if text.trim().is_empty() {
                        continue;
                    }
                    let is_final = mtype.to_lowercase().contains("offline")
                        || mtype.to_lowercase().contains("final")
                        || v.get("is_final").and_then(|b| b.as_bool()).unwrap_or(false);
                    if is_final {
                        // 一句话的最终识别：收尾并进历史。
                        let t = text.trim().to_string();
                        sink_read.push(SubtitleEvent::Final(t));
                        seg_partial.clear();
                    } else {
                        // 中间（在线）结果：只推送相对已显示文本的新增部分。
                        if text.starts_with(seg_partial.as_str()) {
                            let pc = seg_partial.chars().count();
                            let suffix: String = text.chars().skip(pc).collect();
                            if !suffix.is_empty() {
                                sink_read.push(SubtitleEvent::Partial(suffix));
                                seg_partial = text.to_string();
                            }
                        } else {
                            // 服务端另起一段/修正：把上一段收尾，再开新行。
                            if !seg_partial.is_empty() {
                                sink_read.push(SubtitleEvent::Final(seg_partial.clone()));
                            }
                            seg_partial = text.to_string();
                            sink_read.push(SubtitleEvent::Partial(text.to_string()));
                        }
                    }
                }
            };

            let write = async move {
                // 本地能量检测：说话停顿约 450ms 就通知服务端结束一段
                // （{"is_speaking": false}），让 2pass 模式出该句最终文本；再出声就
                // 翻转回 true 开新段。FunASR 服务端先收一段 JSON 起始消息、随后只收
                // 二进制 PCM，期间允许随时插入这种 JSON 控制帧。
                macro_rules! seg_flag {
                    ($sp:expr) => {{
                        let m = serde_json::json!({ "is_speaking": $sp });
                        if write_half
                            .send(Message::Text(m.to_string().into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }};
                }

                let mut speaking = true;
                let mut silent_ms: u64 = 0;

                while let Some(chunk) = audio_rx.recv().await {
                    if chunk.is_empty() {
                        continue;
                    }
                    let mut bytes = Vec::with_capacity(chunk.len() * 2);
                    for s in &chunk {
                        bytes.extend_from_slice(&s.to_le_bytes());
                    }
                    // 根据语音/静音决定是否需要先翻转 is_speaking。
                    let ms = (chunk.len() as u64) * 1000 / 16_000;
                    let voiced = crate::vad::rms(&chunk) >= 0.008;
                    if voiced {
                        if !speaking {
                            seg_flag!(true);
                            speaking = true;
                        }
                        silent_ms = 0;
                    } else {
                        silent_ms = silent_ms.saturating_add(ms);
                        if speaking && silent_ms >= 450 {
                            seg_flag!(false);
                            speaking = false;
                            silent_ms = 0;
                        }
                    }
                    if write_half
                        .send(Message::Binary(bytes.into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                // 收尾：让服务端出最后一句话的最终文本（循环已结束，忽略发送结果）。
                if speaking {
                    let m = serde_json::json!({ "is_speaking": false });
                    let _ = write_half.send(Message::Text(m.to_string().into())).await;
                }
            };

            tokio::select! {
                _ = read => {}
                _ = write => {}
            }
            Ok(())
        }
    }
}

// ---------- Bailian Fun-ASR realtime ----------
//
// This is intentionally separate from `funasr`: that provider speaks the
// self-hosted FunASR 2pass protocol, while Bailian uses run-task followed by
// raw binary PCM frames.  Keep the two identifiers separate for old configs.
pub mod bailian {
    use super::*;

    const PUBLIC_BEIJING_ENDPOINT: &str = "wss://dashscope.aliyuncs.com/api-ws/v1/inference";

    #[derive(Debug, PartialEq, Eq)]
    enum StartEvent {
        Started,
        Failed(String),
        Ignore,
    }

    #[derive(Debug, PartialEq, Eq)]
    enum ResultEvent {
        Partial(String),
        Final(String),
        Failed(String),
        Finished,
        Ignore,
    }

    fn event_error(value: &serde_json::Value) -> String {
        value
            .pointer("/header/error_message")
            .and_then(|v| v.as_str())
            .unwrap_or("未知错误")
            .to_string()
    }

    fn parse_start_event(text: &str, task_id: &str) -> StartEvent {
        let value: serde_json::Value = match serde_json::from_str(text) {
            Ok(value) => value,
            Err(_) => return StartEvent::Ignore,
        };
        if value.pointer("/header/task_id").and_then(|v| v.as_str()) != Some(task_id) {
            return StartEvent::Ignore;
        }
        match value.pointer("/header/event").and_then(|v| v.as_str()) {
            Some("task-started") => StartEvent::Started,
            Some("task-failed") => StartEvent::Failed(event_error(&value)),
            _ => StartEvent::Ignore,
        }
    }

    fn parse_result_event(text: &str, task_id: &str) -> ResultEvent {
        let value: serde_json::Value = match serde_json::from_str(text) {
            Ok(value) => value,
            Err(_) => return ResultEvent::Ignore,
        };
        if value.pointer("/header/task_id").and_then(|v| v.as_str()) != Some(task_id) {
            return ResultEvent::Ignore;
        }
        match value.pointer("/header/event").and_then(|v| v.as_str()) {
            Some("result-generated") => {
                let sentence = &value["payload"]["output"]["sentence"];
                if sentence.get("heartbeat").and_then(|v| v.as_bool()) == Some(true) {
                    return ResultEvent::Ignore;
                }
                let text = sentence.get("text").and_then(|v| v.as_str()).unwrap_or("").trim();
                if text.is_empty() {
                    return ResultEvent::Ignore;
                }
                if sentence.get("sentence_end").and_then(|v| v.as_bool()) == Some(true) {
                    ResultEvent::Final(text.to_string())
                } else {
                    ResultEvent::Partial(text.to_string())
                }
            }
            Some("task-failed") => ResultEvent::Failed(event_error(&value)),
            Some("task-finished") => ResultEvent::Finished,
            _ => ResultEvent::Ignore,
        }
    }

    /// 百炼 Fun-ASR 实时通道。
    ///
    /// 热词（R10）走官方「上下文增强」：`run-task` 携带 `payload.input.context`，
    /// 会话运行中词表变化则用 `continue-task` 更新。**不使用** `vocabulary`
    /// 即时热词字段——`fun-asr-realtime` 不支持它。
    pub struct BailianFunAsr {
        cfg: LlmConfig,
        endpoint: String,
        /// 运行期热词源（管理页保存后由 pipeline 写入）。
        hotwords: crate::hotwords::HotwordFeed,
        /// 热词下发状态槽，供管理页显示。
        hotword_status: Arc<parking_lot::RwLock<crate::hotwords::HotwordStatus>>,
        /// pipeline 注入的共享状态槽；注入后以它为准。
        shared_hotword_status:
            Arc<parking_lot::RwLock<Option<Arc<parking_lot::RwLock<crate::hotwords::HotwordStatus>>>>>,
    }

    impl BailianFunAsr {
        pub fn new(cfg: LlmConfig) -> Result<Self> {
            if cfg.api_key.trim().is_empty() { return Err(anyhow!("百炼 API Key 未设置")); }
            let endpoint = cfg.endpoint.clone().filter(|v| !v.trim().is_empty()).unwrap_or_else(|| {
                if cfg.workspace_id.trim().is_empty() { PUBLIC_BEIJING_ENDPOINT.to_string() }
                else { format!("wss://{}.cn-beijing.maas.aliyuncs.com/api-ws/v1/inference", cfg.workspace_id.trim()) }
            });
            if !endpoint.starts_with("wss://") { return Err(anyhow!("百炼 Fun-ASR 端点必须使用 wss://")); }
            let hotwords = crate::hotwords::HotwordFeed::new();
            hotwords.set(crate::hotwords::plan(&cfg.hotwords));
            Ok(Self {
                cfg,
                endpoint,
                hotwords,
                hotword_status: Arc::new(parking_lot::RwLock::new(
                    crate::hotwords::HotwordStatus::default(),
                )),
                shared_hotword_status: Arc::new(parking_lot::RwLock::new(None)),
            })
        }
    }

    /// `run-task` 的 `payload.parameters`。
    ///
    /// 抽成独立函数是为了让"管理页存下的 `speech_noise_threshold` 真的进了
    /// `run-task`"这件事可被单元测试直接断言：`run()`（真实会话）与
    /// `test_connection()`（连接测试）都走这一份构造，不存在两条各写一遍、
    /// 改一条漏一条的可能。
    ///
    /// 该参数在 `run-task` 时下发一次，会话建立后无法修改，因此**改这个值
    /// 必须重启识别会话**（管理页保存配置后会自动重启）。
    pub(crate) fn run_task_parameters(cfg: &LlmConfig) -> serde_json::Value {
        serde_json::json!({
            "format": "pcm",
            "sample_rate": 16000,
            "semantic_punctuation_enabled": cfg.semantic_punctuation_enabled,
            // 服务端已钳制到 [-1.0, 1.0]（config::clamp_speech_noise_threshold）；
            // 这里再钳一次，保证任何调用路径（含手工构造的 cfg）都发不出去越界值。
            "speech_noise_threshold": crate::config::clamp_speech_noise_threshold(cfg.speech_noise_threshold),
            "heartbeat": true,
        })
    }

    /// 完整的 `run-task` 首帧（`run()` 与连接测试共用同一份形状，只差
    /// `language_hints`）。抽出来是为了让"管理页存的值进了 run-task"可被
    /// 单元测试直接断言，而不是靠读代码推断。
    pub(crate) fn run_task_payload(
        cfg: &LlmConfig,
        task_id: &str,
        input: serde_json::Value,
        language_hints: bool,
    ) -> serde_json::Value {
        let mut parameters = run_task_parameters(cfg);
        if language_hints {
            parameters["language_hints"] = serde_json::json!(["zh"]);
        }
        serde_json::json!({
            "header": { "action": "run-task", "task_id": task_id, "streaming": "duplex" },
            "payload": { "task_group": "audio", "task": "asr", "function": "recognition", "model": cfg.model,
                "parameters": parameters, "input": input }
        })
    }

    pub async fn test_connection(cfg: LlmConfig) -> Result<()> {
        let provider = BailianFunAsr::new(cfg)?;
        // 连接测试也携带已保存的热词，这样"测试已保存的连接"能覆盖
        // 上下文增强的请求形状（形状错误会在这里暴露为任务启动失败）。
        let input = provider.hotwords.plan().input_json();
        let mut request = provider.endpoint.into_client_request().context("构造百炼 WebSocket 请求")?;
        request.headers_mut().insert(http::header::AUTHORIZATION, http::HeaderValue::from_str(&format!("Bearer {}", provider.cfg.api_key)).context("API Key 不能用于 HTTP 请求头")?);
        if !provider.cfg.workspace_id.trim().is_empty() {
            request.headers_mut().insert(http::HeaderName::from_static("x-dashscope-workspace"), http::HeaderValue::from_str(&provider.cfg.workspace_id).context("业务空间 ID 不能用于 HTTP 请求头")?);
        }
        let (ws, _) = tokio_tungstenite::connect_async(request).await.map_err(|e| ws_connect_error(e, "连接百炼 Fun-ASR"))?;
        let (mut write, mut read) = ws.split();
        let task_id = uuid::Uuid::new_v4().to_string();
        let start = run_task_payload(&provider.cfg, &task_id, input, false);
        write.send(Message::Text(start.to_string().into())).await.context("启动百炼识别测试")?;
        let started = tokio::time::timeout(std::time::Duration::from_secs(8), async {
            while let Some(message) = read.next().await {
                let message = message.context("读取百炼启动事件")?;
                if let Message::Text(text) = message {
                    match parse_start_event(&text, &task_id) {
                        StartEvent::Started => return Ok(true),
                        StartEvent::Failed(error) => return Err(anyhow!("百炼启动失败：{error}")),
                        StartEvent::Ignore => {}
                    }
                }
            }
            Ok(false)
        }).await.context("等待百炼任务启动超时")??;
        if !started { return Err(anyhow!("百炼连接在任务启动前关闭")); }
        let finish = serde_json::json!({ "header": { "action": "finish-task", "task_id": task_id, "streaming": "duplex" }, "payload": { "input": {} } });
        write.send(Message::Text(finish.to_string().into())).await.context("结束百炼识别测试")?;
        tokio::time::timeout(std::time::Duration::from_secs(8), async {
            while let Some(message) = read.next().await {
                let message = message.context("读取百炼结束事件")?;
                if let Message::Text(text) = message {
                    match parse_result_event(&text, &task_id) {
                        ResultEvent::Finished => return Ok(()),
                        ResultEvent::Failed(error) => return Err(anyhow!("百炼任务失败：{error}")),
                        _ => {}
                    }
                }
            }
            Err(anyhow!("百炼任务结束前连接关闭"))
        }).await.context("等待百炼任务结束超时")??;
        Ok(())
    }

    #[async_trait]
    impl LlmProvider for BailianFunAsr {
        fn name(&self) -> &'static str { "bailian-fun-asr" }

        fn set_hotwords(&self, feed: crate::hotwords::HotwordFeed) {
            // 必须**共享同一个** feed，而不是把自己的那份覆盖掉：管理页保存时
            // 写的是 AppState 里的 feed，运行中的 continue-task 靠订阅它才能收到。
            // （先前这里写成 self.hotwords.set(feed.plan())，结果 provider 订阅的
            //  是另一个实例，热词更新永远到不了会话里——已由实测日志定位。）
            self.hotwords.adopt(&feed);
        }

        fn set_hotword_status(
            &self,
            status: Arc<parking_lot::RwLock<crate::hotwords::HotwordStatus>>,
        ) {
            *self.shared_hotword_status.write() = Some(status);
        }

        async fn run(
            self: Arc<Self>,
            mut audio_rx: tokio::sync::mpsc::Receiver<Vec<i16>>,
            sink: SubtitleSink,
            context_rounds: Vec<String>,
        ) -> Result<()> {
            let mut request = self.endpoint.clone().into_client_request().context("构造百炼 WebSocket 请求")?;
            request.headers_mut().insert(http::header::AUTHORIZATION, http::HeaderValue::from_str(&format!("Bearer {}", self.cfg.api_key)).context("API Key 不能用于 HTTP 请求头")?);
            request.headers_mut().insert(http::header::USER_AGENT, http::HeaderValue::from_static("stream-live-translate/0.1"));
            if !self.cfg.workspace_id.trim().is_empty() {
                request.headers_mut().insert(http::HeaderName::from_static("x-dashscope-workspace"), http::HeaderValue::from_str(&self.cfg.workspace_id).context("业务空间 ID 不能用于 HTTP 请求头")?);
            }
            let (ws, _) = tokio_tungstenite::connect_async(request).await.map_err(|e| ws_connect_error(e, "连接百炼 Fun-ASR"))?;
            let (mut write, mut read) = ws.split();
            let task_id = uuid::Uuid::new_v4().to_string();

            // 热词（上下文增强）：会话开始时下发的轮次由 pipeline 传入；
            // 运行中的更新走订阅到的 watch 通道，用 continue-task 下发。
            let full_plan = self.hotwords.plan();
            let initial_plan = crate::hotwords::plan_from_rounds(&full_plan, context_rounds);
            let mut hotword_rx = self.hotwords.subscribe();
            let mut applied = initial_plan.fingerprint();
            // 有 pipeline 注入的共享槽就写它（管理页读的是那一个）。
            let status_slot = self
                .shared_hotword_status
                .read()
                .clone()
                .unwrap_or_else(|| self.hotword_status.clone());
            let _ = hotword_rx.borrow_and_update();
            status_slot
                .write()
                .record_applied(&initial_plan, "run-task", chrono::Utc::now());

            let start = run_task_payload(&self.cfg, &task_id, initial_plan.input_json(), true);
            write.send(Message::Text(start.to_string().into())).await.context("启动百炼识别任务")?;

            // Service policy requires task-started before audio.  A bounded
            // wait avoids silently streaming PCM into a rejected task.
            let mut started = false;
            while let Some(message) = read.next().await {
                let message = message.context("读取百炼启动事件")?;
                if let Message::Text(text) = message {
                    match parse_start_event(&text, &task_id) {
                        StartEvent::Started => { started = true; break; }
                        StartEvent::Failed(error) => return Err(anyhow!("百炼启动失败：{error}")),
                        StartEvent::Ignore => {}
                    }
                }
            }
            if !started { return Err(anyhow!("百炼连接在任务启动前关闭")); }

            let sink_read = sink.clone();
            let task_for_read = task_id.clone();
            let reader = async move {
                while let Some(message) = read.next().await {
                    let Message::Text(text) = message? else { continue };
                    match parse_result_event(&text, &task_for_read) {
                        ResultEvent::Partial(text) => sink_read.push(SubtitleEvent::Replace(text)),
                        ResultEvent::Final(text) => sink_read.push(SubtitleEvent::Final(text)),
                        ResultEvent::Failed(error) => return Err(anyhow!("百炼任务失败：{error}")),
                        ResultEvent::Finished => return Ok(()),
                        ResultEvent::Ignore => {}
                    }
                }
                Ok::<(), anyhow::Error>(())
            };
            let task_for_write = task_id.clone();
            let writer = async move {
                loop {
                    // 注意：这里**不能**用 `biased;`。音频每 ~20 ms 就到一帧，
                    // 一旦让 select 永远优先音频，热词分支就永远轮不到，
                    // continue-task 会被静默饿死（实测：60 秒会话一次都没触发）。
                    // tokio 默认随机轮询，任何一个分支就绪都会被选中。
                    tokio::select! {
                        chunk = audio_rx.recv() => {
                            let Some(chunk) = chunk else { break };
                            if chunk.is_empty() { continue; }
                            let bytes: Vec<u8> = chunk.iter().flat_map(|s| s.to_le_bytes()).collect();
                            write.send(Message::Binary(bytes.into())).await.context("发送百炼音频")?;
                        }
                        changed = hotword_rx.changed() => {
                            if changed.is_err() {
                                // 发送端消失（feed 已被丢弃）：不会再有新词表。
                                // 退出等待避免空转，音频分支继续用 biased 优先。
                                continue;
                            }
                            let next = hotword_rx.borrow_and_update().clone();
                            let fingerprint = next.fingerprint();
                            if fingerprint == applied {
                                // 不变更不重发：仅记账，不占用服务端上下文轮次。
                                status_slot.write().record_skipped(&next);
                                continue;
                            }
                            let update = serde_json::json!({
                                "header": { "action": "continue-task", "task_id": task_for_write, "streaming": "duplex" },
                                "payload": { "input": next.input_json() }
                            });
                            write.send(Message::Text(update.to_string().into())).await.context("下发百炼热词上下文")?;
                            applied = fingerprint;
                            status_slot.write().record_applied(&next, "continue-task", chrono::Utc::now());
                        }
                    }
                }
                let finish = serde_json::json!({ "header": { "action": "finish-task", "task_id": task_for_write, "streaming": "duplex" }, "payload": { "input": {} } });
                let _ = write.send(Message::Text(finish.to_string().into())).await;
                Ok::<(), anyhow::Error>(())
            };
            // A finite replay closes audio_rx after the final PCM frame. The
            // writer must send finish-task, but that is not the end of the
            // recognition: Bailian commonly emits the final sentence and
            // task-finished afterwards. Keep the reader alive for a bounded
            // drain instead of dropping it as soon as the writer returns.
            tokio::pin!(reader);
            tokio::pin!(writer);
            tokio::select! {
                result = &mut reader => result,
                result = &mut writer => {
                    result?;
                    tokio::time::timeout(std::time::Duration::from_secs(10), &mut reader)
                        .await
                        .context("等待百炼最终识别结果超时")?
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::{
            parse_result_event, parse_start_event, run_task_payload, run_task_parameters,
            ResultEvent, StartEvent,
        };
        use crate::config::{Config, LlmConfig};

        fn event(task_id: &str, event: &str) -> String {
            serde_json::json!({"header": {"task_id": task_id, "event": event}}).to_string()
        }

        fn cfg_with_threshold(value: f32) -> LlmConfig {
            LlmConfig {
                speech_noise_threshold: value,
                ..Config::default().llm
            }
        }

        /// 管理页保存的 `llm.speech_noise_threshold` 必须原样出现在 `run-task`
        /// 的 `payload.parameters.speech_noise_threshold` 里 —— 这是"云端真的
        /// 收到用户选的值"的唯一凭据，不能靠读代码推断。
        #[test]
        fn speech_noise_threshold_reaches_the_run_task_parameters() {
            for value in [-1.0_f32, 0.0, 0.3, 0.6, 0.9, 1.0] {
                let payload =
                    run_task_payload(&cfg_with_threshold(value), "task-1", serde_json::json!({}), true);
                assert_eq!(payload["header"]["action"], "run-task");
                let sent = payload["payload"]["parameters"]["speech_noise_threshold"]
                    .as_f64()
                    .expect("阈值必须是数字") as f32;
                assert_eq!(sent, value, "配置 {value} 没有原样进入 run-task");
            }
        }

        /// 越界值（手改 config.toml 或直接 POST /api/config 都能造出来）在
        /// 下发前必须被钳进官方区间，而不是把云端会拒绝的值原样发出去。
        #[test]
        fn out_of_range_thresholds_are_clamped_before_run_task() {
            assert_eq!(
                run_task_parameters(&cfg_with_threshold(9.0))["speech_noise_threshold"],
                serde_json::json!(1.0)
            );
            assert_eq!(
                run_task_parameters(&cfg_with_threshold(-9.0))["speech_noise_threshold"],
                serde_json::json!(-1.0)
            );
            assert_eq!(
                run_task_parameters(&cfg_with_threshold(f32::NAN))["speech_noise_threshold"],
                serde_json::json!(0.0)
            );
        }

        /// 会话建立后该参数不可改（只能重启会话重发 run-task），所以同一个
        /// cfg 构造出的参数必须每次都一样；`language_hints` 只影响真实会话。
        #[test]
        fn run_task_parameters_are_stable_and_language_hints_are_optional() {
            let cfg = cfg_with_threshold(0.9);
            let a = run_task_parameters(&cfg);
            let b = run_task_parameters(&cfg);
            assert_eq!(a, b);
            assert_eq!(a["format"], "pcm");
            assert_eq!(a["sample_rate"], 16000);
            assert!(a.get("language_hints").is_none());
            let real = run_task_payload(&cfg, "t", serde_json::json!({}), true);
            assert_eq!(real["payload"]["parameters"]["language_hints"][0], "zh");
            // f32 → JSON 会多出 f64 精度尾巴（0.9f32 == 0.8999999761581421），
            // 所以这里比 f32 值而不是比 JSON 字面量。
            assert_eq!(
                real["payload"]["parameters"]["speech_noise_threshold"]
                    .as_f64()
                    .expect("阈值必须是数字") as f32,
                0.9_f32
            );
            let probe = run_task_payload(&cfg, "t", serde_json::json!({}), false);
            assert!(probe["payload"]["parameters"].get("language_hints").is_none());
        }

        #[test]
        fn audio_is_gated_until_task_started() {
            let sequence = [
                event("task-1", "task-starting"),
                event("task-1", "task-started"),
            ];
            let mut started = false;
            let mut audio_sent = Vec::new();
            for message in sequence {
                if matches!(parse_start_event(&message, "task-1"), StartEvent::Started) {
                    started = true;
                }
                if started {
                    audio_sent.push("pcm");
                }
            }
            assert_eq!(audio_sent, vec!["pcm"]);
        }

        #[test]
        fn parses_partial_and_final_results() {
            let partial = serde_json::json!({
                "header": {"task_id": "task-1", "event": "result-generated"},
                "payload": {"output": {"sentence": {"text": "你好", "sentence_end": false}}}
            }).to_string();
            let final_text = serde_json::json!({
                "header": {"task_id": "task-1", "event": "result-generated"},
                "payload": {"output": {"sentence": {"text": "你好世界", "sentence_end": true}}}
            }).to_string();
            assert_eq!(parse_result_event(&partial, "task-1"), ResultEvent::Partial("你好".into()));
            assert_eq!(parse_result_event(&final_text, "task-1"), ResultEvent::Final("你好世界".into()));
        }

        #[test]
        fn recognizes_task_failures() {
            let failed = serde_json::json!({
                "header": {"task_id": "task-1", "event": "task-failed", "error_message": "quota exceeded"}
            }).to_string();
            assert_eq!(parse_start_event(&failed, "task-1"), StartEvent::Failed("quota exceeded".into()));
            assert_eq!(parse_result_event(&failed, "task-1"), ResultEvent::Failed("quota exceeded".into()));
        }

        #[test]
        fn ignores_duplicate_or_late_task_ids() {
            let result = serde_json::json!({
                "header": {"task_id": "old-task", "event": "result-generated"},
                "payload": {"output": {"sentence": {"text": "旧结果", "sentence_end": true}}}
            }).to_string();
            assert_eq!(parse_result_event(&result, "current-task"), ResultEvent::Ignore);
            assert_eq!(parse_start_event(&result, "current-task"), StartEvent::Ignore);
        }
    }
}

// ---------- Mock ----------

pub mod mock {
    use super::*;
    use tokio::time::{sleep, Duration};

    pub struct MockProvider {
        cfg: LlmConfig,
    }
    impl MockProvider {
        pub fn new(cfg: LlmConfig) -> Result<Self> {
            Ok(Self { cfg })
        }
    }
    #[async_trait]
    impl LlmProvider for MockProvider {
        fn name(&self) -> &'static str {
            "mock"
        }
        async fn run(
            self: Arc<Self>,
            mut audio_rx: tokio::sync::mpsc::Receiver<Vec<i16>>,
            sink: SubtitleSink,
            _context_rounds: Vec<String>,
        ) -> Result<()> {
            let phrases = [
                ("Hello everyone, welcome to the stream.", "大家好，欢迎来到直播间。"),
                ("Today we are testing the real-time subtitle plugin.", "今天我们正在测试实时字幕插件。"),
                ("If you can see this, everything is working.", "如果你能看到这行字，说明一切正常工作。"),
                ("Now switching to English. Please listen carefully.", "现在切换到英文，请仔细听。"),
                ("本句是中文，应当原样输出。", "本句是中文，应当原样输出。"),
            ];
            let mut i = 0;
            while audio_rx.recv().await.is_some() {
                let (src, zh) = &phrases[i % phrases.len()];
                let output = if self.cfg.translate_chinese { zh } else { src };
                sink.push(SubtitleEvent::Partial(output.to_string()));
                sleep(Duration::from_millis(900)).await;
                sink.push(SubtitleEvent::Final(output.to_string()));
                i += 1;
            }
            Ok(())
        }
    }
}

// ---------- Helpers shared by providers ----------

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct ProviderCapabilities {
    pub supports_streaming_transcript: bool,
    pub sample_rate: u32,
}
