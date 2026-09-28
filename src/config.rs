use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Top-level user config. Lives next to the binary as `config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Server bind address for the admin panel + overlay.
    pub server: ServerConfig,
    /// LLM provider settings.
    pub llm: LlmConfig,
    /// Audio capture settings.
    pub audio: AudioConfig,
    /// VAD / music filter.
    pub filter: FilterConfig,
    /// OBS WebSocket integration.
    pub obs: ObsConfig,
    /// Subtitle rendering hints (consumed by browser overlay).
    pub overlay: OverlayConfig,
    /// Directory for per-session JSONL recordings. Empty keeps recordings
    /// beside config.toml under `recordings/`; a relative path is resolved
    /// from that same directory.
    #[serde(default)]
    pub recording_dir: String,
    /// Whether finished subtitles are automatically appended to this session's
    /// JSONL file (P1-06).
    ///
    /// The audit requires automatic persistence to have an explicit switch rather
    /// than being an unstated policy. The default keeps the behaviour this
    /// product has always had (`true`); turning it off stops **new** writes and
    /// never deletes anything already on disk — deleting evidence is the separate
    /// `DELETE /api/recordings` action.
    #[serde(default = "default_auto_persist")]
    pub auto_persist: bool,
    /// Delete recordings older than this many days at startup (P2-07).
    ///
    /// `0` disables pruning (keep everything) and is the safe default for a
    /// hand-written config. The live session's own file is never removed. Like
    /// auto-persistence, whether any retention should happen by default is a
    /// product decision that the audit defers — so the shipped default is
    /// "prune nothing" and the panel exposes it explicitly.
    #[serde(default)]
    pub retention_days: u64,
    /// Guided microphone test timings. Optional so older configs keep working.
    #[serde(default)]
    pub audio_test: AudioTestConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    /// Path to the bundled `overlay/` and `admin/` static assets.
    /// Defaults are fine: main() overrides this with the real exe-relative
    /// path at startup; the field only needs to deserialize.
    #[serde(default = "default_static_dir")]
    pub static_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    /// Provider identifier: `qwen-realtime`, `openai-realtime`, `mock`.
    pub provider: String,
    /// API key (Bearer / DashScope / OpenAI).
    pub api_key: String,
    /// Model name, e.g. `qwen3.5-livetranslate-flash-realtime`.
    pub model: String,
    /// WebSocket endpoint override (rarely needed).
    pub endpoint: Option<String>,
    /// Target output language. Always `"zh"` for this product.
    pub target_lang: String,
    /// Whether to translate Chinese input. `false` = pass through.
    pub translate_chinese: bool,
    /// Optional extra system prompt hint.
    pub system_prompt: Option<String>,
    /// 低延迟分段（毫秒）。0 = 关闭：沿用服务端 server_vad，等一句话说完
    /// 才整句返回（句子完整，但延迟≈整句话时长）。
    /// >0 = 开启：本机累计「正在说」的语音，每满该毫秒就发一次
    /// `input_audio_buffer.commit`，把当前已说的这一段提前识别/翻译并输出，
    /// 字幕按段持续推进，延迟可压到 ~1–2 秒（代价：长句被切成短段）。
    #[serde(default)]
    pub segment_ms: u64,
    /// 实时字幕模式（OpenAI / GLM 等 OpenAI 兼容 realtime 通道使用）：
    /// 开启后会话里启用 input_audio_transcription（用户语音转写），并且
    /// 只把「说话人的转写」当字幕显示，忽略模型自己的回复文本。
    #[serde(default)]
    pub transcribe: bool,
    /// 实时字幕模式下的转写子模型名。留空 = 自动：
    /// OpenAI 官方端点默认 gpt-4o-mini-transcribe；其它厂商按会话主模型尝试。
    #[serde(default)]
    pub transcription_model: String,
    /// 本机网关模式（OpenAI 兼容 local 通道）：网关把「要显示的字幕文字」
    /// 直接经 response.text.* 返回（如 huggingface/speech-to-speech 这类本地
    /// OpenAI Realtime 兼容网关）。开启后不再等待 input_audio_transcription
    /// 事件，把网关回的文字当作字幕显示；配合低延迟分段时，提交后会自动补发
    /// response.create 触发网关出结果。
    #[serde(default)]
    pub gateway_text: bool,
    /// Optional Model Studio workspace.  When present, the Bailian provider
    /// uses the workspace-specific Beijing endpoint; an endpoint override
    /// always wins.
    #[serde(default)]
    pub workspace_id: String,
    /// 百炼 `run-task.parameters.speech_noise_threshold`：语音/噪音判定阈值。
    ///
    /// 只对 `bailian-fun-asr` 生效，随 `run-task` 下发，因此改动它**必须重启
    /// 识别会话**（管理页保存后会自动重启）。
    ///
    /// 取舍（2026-09-21 实测，63.7 分钟同一场直播、同一二进制、只改此值）：
    /// * `0.0` —— 忠实模式。观众欢呼段（30–55 s）产生 5 条垃圾字幕
    ///   （`他实习。` / `谢。` / `这不可。` / `人做好。` / `That just.`），
    ///   整场 344 条 final / 15,309 字。
    /// * `0.9` —— 欢呼段垃圾字幕 0 条，但断句明显变碎：
    ///   498 条 final（+45%）/ 14,477 字（−5.4%）。
    ///
    /// 值越高越能压掉环境噪声与噪声幻觉，代价是可能把主讲人的话也判成噪声、
    /// 并把断句切得更碎。0.3 / 0.6 尚无实测数据。
    /// 取值区间见 [`SPEECH_NOISE_THRESHOLD_MIN`] / [`SPEECH_NOISE_THRESHOLD_MAX`]。
    #[serde(default = "default_speech_noise_threshold")]
    pub speech_noise_threshold: f32,
    #[serde(default = "default_semantic_punctuation")]
    pub semantic_punctuation_enabled: bool,
    /// 热词表（R10）：经百炼 **上下文增强** 下发（`input.context`）。
    /// 仅 `bailian-fun-asr` 使用；其它 provider 忽略。旧配置缺该字段时按
    /// 空表处理，因此加了 `#[serde(default)]` 以保持向后兼容。
    #[serde(default)]
    pub hotwords: Vec<String>,
}

fn default_speech_noise_threshold() -> f32 {
    0.0
}
fn default_semantic_punctuation() -> bool {
    true
}
fn default_auto_persist() -> bool {
    true
}

/// 官方允许的 `speech_noise_threshold` 区间（闭区间）。
/// 越接近 [`SPEECH_NOISE_THRESHOLD_MIN`] 越容易把噪声当语音转写；
/// 越接近 [`SPEECH_NOISE_THRESHOLD_MAX`] 越容易把语音判成噪声。
pub const SPEECH_NOISE_THRESHOLD_MIN: f32 = -1.0;
pub const SPEECH_NOISE_THRESHOLD_MAX: f32 = 1.0;

/// 把 `speech_noise_threshold` 钳制到官方区间，并挡掉 NaN / ±inf。
///
/// 管理页的 `<input type=number min=-1 max=1>` 只是界面提示，手改
/// `config.toml` 或直接 POST `/api/config` 都能绕过它；超区间的值到不了
/// 云端（会被服务端拒绝或静默忽略），所以这里在服务端也钳一次。
pub fn clamp_speech_noise_threshold(value: f32) -> f32 {
    if value.is_nan() {
        return default_speech_noise_threshold();
    }
    value.clamp(SPEECH_NOISE_THRESHOLD_MIN, SPEECH_NOISE_THRESHOLD_MAX)
}

/// 同 [`clamp_speech_noise_threshold`]，但按"是否真的被钳过"返回标记，
/// 供 `/api/status` 告诉管理页当前生效值是不是用户填的那个。
pub fn clamp_speech_noise_threshold_flagged(value: f32) -> (f32, bool) {
    let clamped = clamp_speech_noise_threshold(value);
    (clamped, clamped != value)
}

fn default_ingest_port() -> u16 {
    8788
}

fn default_static_dir() -> PathBuf {
    PathBuf::from("dist")
}

fn default_bg_opacity() -> u32 {
    75
}

fn default_border_radius() -> u32 {
    8
}

fn default_max_lines() -> u32 {
    2
}

fn default_display_delay_ms() -> u64 {
    750
}

fn default_clear_after_ms() -> u64 {
    4000
}

/// Bounds for `overlay.clear_after_ms`, shared by the server so a hand-edited
/// config cannot leave a stale caption on screen forever.
pub const CLEAR_AFTER_MIN_MS: u64 = 1_000;
pub const CLEAR_AFTER_MAX_MS: u64 = 15_000;

/// Clamp a configured clear delay into the supported range. `None` (missing
/// field) keeps the default.
pub fn clamp_clear_after_ms(value: Option<u64>) -> u64 {
    value
        .unwrap_or_else(default_clear_after_ms)
        .clamp(CLEAR_AFTER_MIN_MS, CLEAR_AFTER_MAX_MS)
}

/// Bounds for the overlay's new-page display buffer. `0` is the special
/// "no buffer" value (the first page appears the instant it arrives); any other
/// value lives inside [`DISPLAY_DELAY_MIN_MS`, `DISPLAY_DELAY_MAX_MS`].
pub const DISPLAY_DELAY_MIN_MS: u64 = 500;
pub const DISPLAY_DELAY_MAX_MS: u64 = 1_000;

/// Clamp a configured display delay the same way `overlay/app.js` does, so the
/// value the panel shows, the value the WebSocket pushes and the value the
/// browser enforces are all identical. `0` (no buffering) is legal; anything
/// else is clamped into 500–1000 ms; `None` (missing field) keeps the default.
pub fn clamp_display_delay_ms(value: Option<u64>) -> u64 {
    match value {
        None => default_display_delay_ms(),
        Some(0) => 0,
        Some(other) => other.clamp(DISPLAY_DELAY_MIN_MS, DISPLAY_DELAY_MAX_MS),
    }
}

fn default_audio_test_quiet_ms() -> u64 {
    3_000
}

fn default_audio_test_speech_ms() -> u64 {
    10_000
}

/// Bounds for the guided microphone test. Long enough for a meaningful average,
/// short enough that a calibration run is not a chore.
pub const AUDIO_TEST_MIN_SEGMENT_MS: u64 = 1_000;
pub const AUDIO_TEST_MAX_SEGMENT_MS: u64 = 30_000;

pub fn clamp_audio_test_ms(value: Option<u64>, default_ms: u64) -> u64 {
    value
        .unwrap_or(default_ms)
        .clamp(AUDIO_TEST_MIN_SEGMENT_MS, AUDIO_TEST_MAX_SEGMENT_MS)
}

/// Timings for the guided microphone test (`POST /api/audio-test`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioTestConfig {
    /// Seconds of silence sampled first to establish the background floor.
    #[serde(default = "default_audio_test_quiet_ms")]
    pub quiet_ms: u64,
    /// Seconds of speech sampled afterwards.
    #[serde(default = "default_audio_test_speech_ms")]
    pub speech_ms: u64,
}

impl Default for AudioTestConfig {
    fn default() -> Self {
        Self {
            quiet_ms: default_audio_test_quiet_ms(),
            speech_ms: default_audio_test_speech_ms(),
        }
    }
}

impl AudioTestConfig {
    /// Clamped durations actually used by the server.
    pub fn durations(&self) -> (Duration, Duration) {
        (
            Duration::from_millis(clamp_audio_test_ms(
                Some(self.quiet_ms),
                default_audio_test_quiet_ms(),
            )),
            Duration::from_millis(clamp_audio_test_ms(
                Some(self.speech_ms),
                default_audio_test_speech_ms(),
            )),
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioConfig {
    /// `"system"` (loopback), `"device"` (input mic / specific output) or
    /// `"obs_filter"` (audio streamed in from the OBS plugin's capture
    /// filter; no system capture needed).
    pub mode: String,
    /// When mode=system on macOS, set true to capture system audio via
    /// ScreenCaptureKit (requires the user to grant permission once).
    pub use_screen_capture_kit: bool,
    /// Specific cpal device name. Empty = default.
    pub device: String,
    /// Internal pipeline sample rate in Hz. Must be inside
    /// [`AUDIO_SAMPLE_RATE_MIN`]–[`AUDIO_SAMPLE_RATE_MAX`].
    ///
    /// `0` used to be accepted as "whatever the device offers", which is the
    /// value the audit calls out (P0-04) and which was genuinely unsafe: every
    /// producer in the pipeline now emits a fixed 16 kHz, so a `0` here would
    /// leave the config claiming one thing while the VAD was told another, and
    /// `frame_ms()` derives every segment/debounce decision from that number.
    /// An explicit rate is required instead.
    pub sample_rate: u32,
    /// Channels. 0 = device default.
    pub channels: u16,
    /// TCP port on 127.0.0.1 that receives audio from the OBS plugin
    /// filter (mode = "obs_filter"). 0 disables the ingest listener.
    #[serde(default = "default_ingest_port")]
    pub ingest_port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FilterConfig {
    /// RMS threshold (0.0–1.0) below which we treat the segment as silence.
    pub silence_rms: f32,
    /// Spectral flatness threshold above which the segment is treated as music.
    pub music_spectral_flatness: f32,
    /// Minimum speech segment duration in ms before sending to the model.
    pub min_segment_ms: u32,
    /// Maximum segment duration in ms; we flush at this point even if no VAD end.
    pub max_segment_ms: u32,
}

impl Default for FilterConfig {
    fn default() -> Self {
        Self {
            silence_rms: 0.012,
            music_spectral_flatness: 0.55,
            min_segment_ms: 350,
            max_segment_ms: 8_000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObsConfig {
    /// Whether to attempt connecting to OBS on startup.
    pub auto_connect: bool,
    pub host: String,
    pub port: u16,
    /// OBS WebSocket password (if authentication is enabled).
    pub password: String,
    /// Whether to register a Custom Dock entry pointing to the admin panel.
    pub register_dock: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverlayConfig {
    pub font_family: String,
    pub font_size: u32,
    pub font_color: String,
    pub background_color: String,
    pub background_opacity: f32,
    /// Background width in pixels. 0 = auto (fit content, one line).
    #[serde(default)]
    pub bg_width: u32,
    /// Background height in pixels. 0 = auto (exactly one line tall).
    #[serde(default)]
    pub bg_height: u32,
    /// Background border radius in pixels.
    #[serde(default = "default_border_radius")]
    pub border_radius: u32,
    /// Background opacity 0-100 (0 = fully transparent, 100 = opaque).
    /// Older config files predate this key, hence the explicit default.
    #[serde(default = "default_bg_opacity")]
    pub bg_opacity: u32,
    /// Max caption lines (1–4). A full page is replaced by the next page
    /// instead of growing taller; 1 keeps the compact single-line style.
    #[serde(default = "default_max_lines")]
    pub max_lines: u32,
    /// Small display buffer for a new caption page. It gives cumulative ASR
    /// revisions time to settle without adding delay to an already-visible page.
    /// `0` disables the buffer entirely (the first page is shown the moment the
    /// event arrives); any other value is clamped to 500–1000 ms by both the
    /// server (`clamp_display_delay_ms`) and the overlay.
    #[serde(default = "default_display_delay_ms")]
    pub display_delay_ms: u64,
    /// Clear the caption after this many milliseconds without a new subtitle
    /// event, and reset the current page so the next page is buffered again.
    /// Clamped to 1–15 s so a hand-edited config cannot strand old text on
    /// screen (too long) or clear a sentence mid-read (too short).
    #[serde(default = "default_clear_after_ms")]
    pub clear_after_ms: u64,
    /// `bottom` / `top` / `middle`
    pub position: String,
    /// `single` / `double`
    pub layout: String,
    /// `typewriter` / `fade` / `slide`
    pub animation: String,
    /// When true, mirror lines in OBS via the optional text GDI+ source.
    pub mirror_to_text_source: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig {
                host: "127.0.0.1".into(),
                port: 8787,
                static_dir: PathBuf::from("dist"),
            },
            llm: LlmConfig {
                provider: "bailian-fun-asr".into(),
                api_key: String::new(),
                model: "fun-asr-realtime".into(),
                endpoint: None,
                target_lang: "zh".into(),
                translate_chinese: false,
                system_prompt: None,
                segment_ms: 0,
                transcribe: false,
                transcription_model: String::new(),
                gateway_text: false,
                workspace_id: String::new(),
                speech_noise_threshold: default_speech_noise_threshold(),
                semantic_punctuation_enabled: default_semantic_punctuation(),
                hotwords: Vec::new(),
            },
            audio: AudioConfig {
                mode: "system".into(),
                use_screen_capture_kit: true,
                device: String::new(),
                sample_rate: 16000,
                channels: 1,
                ingest_port: 8788,
            },
            filter: FilterConfig {
                silence_rms: 0.012,
                music_spectral_flatness: 0.55,
                min_segment_ms: 350,
                max_segment_ms: 8_000,
            },
            obs: ObsConfig {
                auto_connect: true,
                host: "127.0.0.1".into(),
                port: 4455,
                password: String::new(),
                register_dock: true,
            },
            overlay: OverlayConfig {
                font_family: "Noto Sans CJK SC, Microsoft YaHei, PingFang SC, sans-serif".into(),
                font_size: 48,
                font_color: "#FFFFFF".into(),
                background_color: "#000000".into(),
                background_opacity: 0.55,
                bg_width: 0,
                bg_height: 0,
                border_radius: 8,
                bg_opacity: 75,
                max_lines: 2,
                display_delay_ms: default_display_delay_ms(),
                clear_after_ms: default_clear_after_ms(),
                position: "bottom".into(),
                layout: "single".into(),
                animation: "typewriter".into(),
                mirror_to_text_source: false,
            },
            recording_dir: String::new(),
            auto_persist: default_auto_persist(),
            retention_days: 0,
            audio_test: AudioTestConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        clamp_clear_after_ms, clamp_display_delay_ms, clamp_speech_noise_threshold,
        clamp_speech_noise_threshold_flagged, default_speech_noise_threshold, path_is_within,
        resolve_recording_dir, AudioTestConfig, Config, AUDIO_SAMPLE_RATE_MAX,
        AUDIO_SAMPLE_RATE_MIN, AUDIO_TEST_MAX_SEGMENT_MS, AUDIO_TEST_MIN_SEGMENT_MS,
        CLEAR_AFTER_MAX_MS, CLEAR_AFTER_MIN_MS, DISPLAY_DELAY_MAX_MS, DISPLAY_DELAY_MIN_MS,
        SPEECH_NOISE_THRESHOLD_MAX, SPEECH_NOISE_THRESHOLD_MIN,
    };
    use std::path::PathBuf;
    use std::time::Duration;

    #[test]
    fn new_configs_default_to_bailian_realtime() {
        let cfg = Config::default();
        assert_eq!(cfg.llm.provider, "bailian-fun-asr");
        assert_eq!(cfg.llm.model, "fun-asr-realtime");
        assert_eq!(cfg.llm.target_lang, "zh");
        assert!(!cfg.llm.translate_chinese);
    }

    #[test]
    fn older_configs_can_omit_new_bailian_fields() {
        let mut raw = toml::to_string(&Config::default()).expect("serialize config");
        for key in [
            "workspace_id = \"\"\r\n",
            "speech_noise_threshold = 0.0\r\n",
            "semantic_punctuation_enabled = true\r\n",
            "display_delay_ms = 750\r\n",
            "recording_dir = \"\"\r\n",
        ] {
            raw = raw.replace(key, "");
        }
        // The hotword list postdates existing user configs; a config written
        // before it existed must still load (empty list), not fail to parse.
        raw = raw
            .lines()
            .filter(|line| !line.starts_with("hotwords"))
            .collect::<Vec<_>>()
            .join("\r\n");
        // A config written before the clear-delay field existed must load with
        // the 4 s default rather than failing to parse.
        raw = raw
            .lines()
            .filter(|line| !line.starts_with("clear_after_ms"))
            .collect::<Vec<_>>()
            .join("\r\n");
        let cfg: Config = toml::from_str(&raw).expect("read older config");
        assert!(cfg.llm.workspace_id.is_empty());
        assert_eq!(cfg.llm.speech_noise_threshold, 0.0);
        assert!(cfg.llm.semantic_punctuation_enabled);
        assert!(cfg.llm.hotwords.is_empty());
        assert_eq!(cfg.overlay.display_delay_ms, 750);
        assert_eq!(cfg.overlay.clear_after_ms, 4000);
        assert_eq!(cfg.audio_test.quiet_ms, 3_000);
        assert_eq!(cfg.audio_test.speech_ms, 10_000);
    }

    #[test]
    fn hotwords_round_trip_through_toml() {
        let mut cfg = Config::default();
        cfg.llm.hotwords = vec!["铨洲智造".into(), "区域赛".into()];
        let raw = toml::to_string(&cfg).expect("serialize");
        let back: Config = toml::from_str(&raw).expect("deserialize");
        assert_eq!(back.llm.hotwords, cfg.llm.hotwords);
    }

    #[test]
    fn clear_after_is_clamped_to_supported_range() {
        assert_eq!(clamp_clear_after_ms(None), 4000);
        assert_eq!(clamp_clear_after_ms(Some(0)), CLEAR_AFTER_MIN_MS);
        assert_eq!(clamp_clear_after_ms(Some(500)), CLEAR_AFTER_MIN_MS);
        assert_eq!(clamp_clear_after_ms(Some(6000)), 6000);
        assert_eq!(clamp_clear_after_ms(Some(999_999)), CLEAR_AFTER_MAX_MS);
    }

    /// The panel offers "0 秒（无缓冲）", so 0 must survive the server-side clamp
    /// instead of being pushed up to the 500 ms floor — while every other
    /// out-of-range value still lands inside the 500–1000 ms window.
    #[test]
    fn display_delay_allows_zero_and_clamps_the_rest() {
        assert_eq!(clamp_display_delay_ms(None), 750);
        assert_eq!(clamp_display_delay_ms(Some(0)), 0);
        assert_eq!(clamp_display_delay_ms(Some(500)), 500);
        assert_eq!(clamp_display_delay_ms(Some(750)), 750);
        assert_eq!(clamp_display_delay_ms(Some(1000)), 1000);
        assert_eq!(clamp_display_delay_ms(Some(1)), DISPLAY_DELAY_MIN_MS);
        assert_eq!(clamp_display_delay_ms(Some(250)), DISPLAY_DELAY_MIN_MS);
        assert_eq!(clamp_display_delay_ms(Some(999_999)), DISPLAY_DELAY_MAX_MS);
        // A config saved with 0 must round-trip through TOML as 0, not as the
        // default: the overlay would otherwise buffer a page the user disabled.
        let mut cfg = Config::default();
        cfg.overlay.display_delay_ms = 0;
        let raw = toml::to_string(&cfg).expect("serialize");
        let back: Config = toml::from_str(&raw).expect("deserialize");
        assert_eq!(back.overlay.display_delay_ms, 0);
        assert_eq!(
            clamp_display_delay_ms(Some(back.overlay.display_delay_ms)),
            0
        );
    }

    /// 云端噪声判定阈值：`-1.0`（更多噪声被转写）～ `1.0`（可能把语音误判为
    /// 噪声）。越界值必须被钳掉，NaN 必须落回默认值，否则会原样写进
    /// `run-task` 参数被云端拒绝。
    #[test]
    fn speech_noise_threshold_is_clamped_and_nan_safe() {
        assert_eq!(
            clamp_speech_noise_threshold(SPEECH_NOISE_THRESHOLD_MIN),
            SPEECH_NOISE_THRESHOLD_MIN
        );
        assert_eq!(
            clamp_speech_noise_threshold(SPEECH_NOISE_THRESHOLD_MAX),
            SPEECH_NOISE_THRESHOLD_MAX
        );
        assert_eq!(clamp_speech_noise_threshold(0.0), 0.0);
        assert_eq!(clamp_speech_noise_threshold(0.9), 0.9);
        assert_eq!(
            clamp_speech_noise_threshold(-9.0),
            SPEECH_NOISE_THRESHOLD_MIN
        );
        assert_eq!(
            clamp_speech_noise_threshold(9.0),
            SPEECH_NOISE_THRESHOLD_MAX
        );
        assert_eq!(
            clamp_speech_noise_threshold(f32::NAN),
            default_speech_noise_threshold()
        );
        assert_eq!(
            clamp_speech_noise_threshold(f32::INFINITY),
            SPEECH_NOISE_THRESHOLD_MAX
        );
        assert_eq!(
            clamp_speech_noise_threshold(f32::NEG_INFINITY),
            SPEECH_NOISE_THRESHOLD_MIN
        );
        // 标记位告诉管理页"当前生效值不是你填的那个"。
        assert_eq!(clamp_speech_noise_threshold_flagged(0.3), (0.3, false));
        assert_eq!(clamp_speech_noise_threshold_flagged(5.0), (1.0, true));
        assert_eq!(clamp_speech_noise_threshold_flagged(f32::NAN), (0.0, true));
    }

    /// 保存 → 读回必须原样，不能被钳成默认值：0.0 是合法值（"关/忠实"档），
    /// 若被当成"缺省"处理，用户就没法选回忠实模式。
    #[test]
    fn speech_noise_threshold_round_trips_through_toml() {
        for value in [-1.0_f32, 0.0, 0.3, 0.6, 0.9, 1.0] {
            let mut cfg = Config::default();
            cfg.llm.speech_noise_threshold = value;
            let raw = toml::to_string(&cfg).expect("serialize");
            let back: Config = toml::from_str(&raw).expect("deserialize");
            assert_eq!(back.llm.speech_noise_threshold, value);
            assert_eq!(
                clamp_speech_noise_threshold(back.llm.speech_noise_threshold),
                value
            );
        }
    }

    /// P0-04: the values the audit names (`0`, `1`, `u32::MAX`) must be refused
    /// rather than clamped, because each of them either leaves the pipeline's
    /// declared rate disagreeing with the rate the frames were produced at, or
    /// overflows the frame arithmetic. The window is inclusive at both ends.
    #[test]
    fn the_sample_rate_window_is_enforced_and_zero_is_no_longer_a_sentinel() {
        let path = std::path::Path::new("target/test-config.toml");
        for bad in [0u32, 1, 7_999, 384_001, u32::MAX] {
            let mut cfg = Config::default();
            cfg.audio.sample_rate = bad;
            let err = cfg
                .validate(path)
                .expect_err(&format!("sample_rate={bad} must be refused"));
            assert!(
                err.to_string().contains("audio.sample_rate"),
                "the message must name the field for {bad}: {err}"
            );
        }
        for good in [
            AUDIO_SAMPLE_RATE_MIN,
            16_000,
            44_100,
            48_000,
            AUDIO_SAMPLE_RATE_MAX,
        ] {
            let mut cfg = Config::default();
            cfg.audio.sample_rate = good;
            assert!(
                cfg.validate(path).is_ok(),
                "sample_rate={good} must be accepted"
            );
        }
        // `clamp` must not quietly "fix" it into a legal value: a rejected value
        // has to stay rejected so the caller can refuse the request.
        let mut cfg = Config::default();
        cfg.audio.sample_rate = 1;
        cfg.clamp();
        assert_eq!(cfg.audio.sample_rate, 1, "clamp must not rewrite the rate");
        assert!(cfg.validate(path).is_err());
    }

    /// P2-07: retention is bounded so a typo (`retention_days = 999999`) cannot
    /// silently delete every past session. `0` means "keep forever" and is the
    /// shipped default.
    #[test]
    fn retention_days_is_bounded_and_zero_keeps_everything() {
        let path = std::path::Path::new("target/test-config.toml");
        assert_eq!(
            Config::default().retention_days,
            0,
            "default must keep everything"
        );
        for good in [0u64, 1, 30, 3_650] {
            let mut cfg = Config::default();
            cfg.retention_days = good;
            assert!(
                cfg.validate(path).is_ok(),
                "retention_days={good} must be accepted"
            );
        }
        let mut cfg = Config::default();
        cfg.retention_days = 3_651;
        let err = cfg
            .validate(path)
            .expect_err("an absurd retention must be refused");
        assert!(err.to_string().contains("retention_days"), "{err}");
    }

    /// P1-06: automatic persistence is on by default (the behaviour this product
    /// has always had) and survives a TOML round trip, including an explicit
    /// `false` — a switch that cannot be turned off is not a switch.
    #[test]
    fn auto_persist_defaults_on_and_round_trips_off() {
        assert!(Config::default().auto_persist);
        for value in [true, false] {
            let mut cfg = Config::default();
            cfg.auto_persist = value;
            let raw = toml::to_string(&cfg).expect("serialize");
            let back: Config = toml::from_str(&raw).expect("deserialize");
            assert_eq!(
                back.auto_persist, value,
                "auto_persist={value} must round-trip"
            );
        }
        // An older config that predates the key keeps the shipped behaviour.
        let mut older = toml::to_string(&Config::default()).expect("serialize");
        older = older
            .lines()
            .filter(|l| !l.starts_with("auto_persist") && !l.starts_with("retention_days"))
            .collect::<Vec<_>>()
            .join("\n");
        let cfg: Config = toml::from_str(&older).expect("older config must still load");
        assert!(
            cfg.auto_persist,
            "a pre-existing config keeps auto-persistence on"
        );
        assert_eq!(cfg.retention_days, 0, "and keeps every recording");
    }

    /// P0-04 / P2-07: `recording_dir` must not be able to write outside the
    /// config directory. The two directions are asserted together on purpose —
    /// a check that rejects everything would pass a rejection-only test, and a
    /// check that accepts everything would pass an acceptance-only test. The
    /// bug this pins is the Windows one: `fs::canonicalize` returns a verbatim
    /// `\\?\C:\…` path for something that EXISTS and a plain `C:\…` for
    /// something that does not, and `Path::starts_with` compares components, so
    /// comparing a canonicalised target against a plain base reported every
    /// out-of-tree absolute path as being inside the tree.
    #[test]
    fn recording_dir_cannot_escape_the_config_directory() {
        let dir = std::env::temp_dir().join(format!("slt-recdir-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let config = dir.join("config.toml");

        // Legal: the default, a plain relative subdirectory, a nested one, and
        // an absolute path *inside* the config directory (both an existing and a
        // not-yet-created target).
        let inside_new = dir.join("captures").join("live");
        for good in ["", "captures", "captures/live", "recordings"] {
            assert!(
                resolve_recording_dir(&config, good).is_ok(),
                "{good:?} must be accepted"
            );
        }
        assert!(
            resolve_recording_dir(&config, &inside_new.to_string_lossy()).is_ok(),
            "an absolute path inside the config dir must be accepted: {}",
            inside_new.display()
        );
        let inside_existing = dir.join("recordings");
        std::fs::create_dir_all(&inside_existing).expect("create inner dir");
        assert!(
            resolve_recording_dir(&config, &inside_existing.to_string_lossy()).is_ok(),
            "an existing absolute path inside the config dir must be accepted"
        );

        // Illegal: traversal, UNC, a drive-relative path, and absolute paths
        // outside the tree — including one that merely *shares a prefix*.
        let outside = std::env::temp_dir().join("slt-escape-elsewhere");
        let sibling = PathBuf::from(format!("{}-evil", dir.display()));
        for bad in [
            "..".to_string(),
            "../escape".to_string(),
            "captures/../../escape".to_string(),
            r"\\attacker\share".to_string(),
            "C:relative".to_string(),
            outside.to_string_lossy().to_string(),
            sibling.to_string_lossy().to_string(),
        ] {
            assert!(
                resolve_recording_dir(&config, &bad).is_err(),
                "{bad:?} must be refused"
            );
        }
        // A sibling that only shares a *string* prefix must not be treated as
        // inside: comparison has to be component-wise.
        assert!(!path_is_within(&sibling, &dir));
        assert!(path_is_within(&inside_new, &dir));
        assert!(path_is_within(&dir, &dir), "a directory is within itself");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn zero_server_port_is_rejected() {
        let mut cfg = Config::default();
        cfg.server.port = 0;
        assert!(cfg.validate(std::path::Path::new("config.toml")).is_err());
    }

    #[test]
    fn config_save_replaces_a_complete_file() {
        let dir = std::env::temp_dir().join(format!("slt-save-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("config.toml");
        let mut cfg = Config::default();
        cfg.llm.model = "first".into();
        cfg.save(&path).expect("first save");
        cfg.llm.model = "second".into();
        cfg.save(&path).expect("replace");
        let read = Config::load_or_create(&path).expect("complete TOML");
        assert_eq!(read.llm.model, "second");
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            1,
            "temporary file was left behind"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn relative_recording_dir_cannot_follow_symlink_outside() {
        let root = std::env::temp_dir().join(format!("slt-link-{}", uuid::Uuid::new_v4()));
        let base = root.join("base");
        let outside = root.join("outside");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, base.join("linked")).unwrap();
        assert!(resolve_recording_dir(&base.join("config.toml"), "linked").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn audio_test_durations_are_clamped() {
        let cfg = AudioTestConfig {
            quiet_ms: 0,
            speech_ms: 999_999,
        };
        let (quiet, speech) = cfg.durations();
        assert_eq!(quiet, Duration::from_millis(AUDIO_TEST_MIN_SEGMENT_MS));
        assert_eq!(speech, Duration::from_millis(AUDIO_TEST_MAX_SEGMENT_MS));
        let (dq, ds) = AudioTestConfig::default().durations();
        assert_eq!(dq, Duration::from_millis(3_000));
        assert_eq!(ds, Duration::from_millis(10_000));
    }

    /// `dist/config.toml` is both the shipped template and the embedded default
    /// (`embedded.rs`), so a typo there breaks first-run for every new user. It
    /// must therefore parse into the same values as `Config::default()`.
    #[test]
    fn the_shipped_config_template_parses_and_matches_the_defaults() {
        let raw = include_str!("../dist/config.toml");
        let cfg: Config = toml::from_str(raw).expect("shipped config.toml must parse");
        let default = Config::default();
        assert_eq!(cfg.llm.provider, default.llm.provider);
        assert_eq!(cfg.llm.model, default.llm.model);
        assert_eq!(cfg.overlay.max_lines, default.overlay.max_lines);
        assert_eq!(
            cfg.overlay.display_delay_ms,
            default.overlay.display_delay_ms
        );
        assert_eq!(cfg.overlay.clear_after_ms, default.overlay.clear_after_ms);
        assert_eq!(cfg.audio_test.quiet_ms, default.audio_test.quiet_ms);
        assert_eq!(cfg.audio_test.speech_ms, default.audio_test.speech_ms);
        assert_eq!(cfg.recording_dir, default.recording_dir);
        // The template must stay inside the range accepted by the server and
        // the browser overlay.
        assert!(
            (1..=4).contains(&cfg.overlay.max_lines),
            "template must advertise 1–4 lines"
        );
    }

    #[test]
    fn the_shipped_wdr_hotwords_all_fit_the_provider_limits() {
        let cfg: Config = toml::from_str(include_str!("../dist/config.toml"))
            .expect("shipped config.toml must parse");
        let plan = crate::hotwords::plan(&cfg.llm.hotwords);
        assert_eq!(
            plan.words.len(),
            310,
            "embedded list must stay deduplicated"
        );
        assert_eq!(plan.dropped_rounds, 0, "embedded words must not be dropped");
        assert!(
            plan.rounds.len() <= crate::hotwords::MAX_ROUNDS,
            "embedded words must fit the provider context window"
        );
        assert!(
            plan.warnings.iter().all(|warning| warning.word.is_empty()),
            "every embedded word must satisfy the provider spelling limits"
        );
    }
}

// ---------------------------------------------------------------------------
// Server-side validation of externally writable configuration (P0-04)
// ---------------------------------------------------------------------------

/// Valid sample-rate window for `audio.sample_rate`. `0` keeps its documented
/// "use the device default" meaning; every other value must be inside this
/// window. The pipeline's internal rate is 16 kHz, so this only bounds what a
/// capture device may be asked for.
pub const AUDIO_SAMPLE_RATE_MIN: u32 = 8_000;
pub const AUDIO_SAMPLE_RATE_MAX: u32 = 384_000;

/// Upper bound for any operator-supplied string that ends up in a request, a
/// file or a log. Long enough for a workspace-scoped endpoint URL plus a system
/// prompt, short enough that a POST cannot make us allocate megabytes.
pub const MAX_STRING_LEN: usize = 4_096;
/// Cap on the system prompt specifically (it is sent on every session update).
pub const MAX_SYSTEM_PROMPT_LEN: usize = 8_192;
/// Cap on the hotword list, matching what `hotwords::plan` will actually ship.
pub const MAX_HOTWORDS: usize = 1_024;
/// Cap on a hotword entry length.
pub const MAX_HOTWORD_LEN: usize = 64;

/// Rejections for a config that must never take effect. Clamping is used for
/// values whose only failure mode is a cosmetic one; everything that could
/// panic, allocate unboundedly or escape a directory is rejected instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationErrors(pub Vec<String>);

impl std::fmt::Display for ValidationErrors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.join("; "))
    }
}

impl std::error::Error for ValidationErrors {}

impl Config {
    /// Clamp every value that only affects presentation or sizing.
    ///
    /// The admin panel's HTML `min`/`max` attributes are a hint, not a
    /// boundary: `POST /api/config` is reachable directly, and a hand-edited
    /// `config.toml` bypasses the panel entirely. Everything the pipeline acts
    /// on is therefore normalised here, on the way in.
    pub fn clamp(&mut self) {
        let c = &mut self.overlay;
        c.font_size = c.font_size.clamp(8, 400);
        // 0 = auto (fit content), so a range rather than a floor.
        c.bg_width = c.bg_width.min(16_384);
        c.bg_height = c.bg_height.min(16_384);
        c.border_radius = c.border_radius.min(512);
        // 0–100 with 0 meaning "fully transparent"; the panel's `|| 75` bug is
        // fixed separately, but 0 must also survive the server clamp.
        c.bg_opacity = c.bg_opacity.min(100);
        c.background_opacity = if c.background_opacity.is_nan() {
            0.0
        } else {
            c.background_opacity.clamp(0.0, 1.0)
        };
        c.max_lines = c.max_lines.clamp(1, 4);
        c.display_delay_ms = clamp_display_delay_ms(Some(c.display_delay_ms));
        c.clear_after_ms = clamp_clear_after_ms(Some(c.clear_after_ms));

        let f = &mut self.filter;
        f.silence_rms = if f.silence_rms.is_nan() {
            0.0
        } else {
            f.silence_rms.clamp(0.0, 1.0)
        };
        f.music_spectral_flatness = if f.music_spectral_flatness.is_nan() {
            0.0
        } else {
            f.music_spectral_flatness.clamp(0.0, 1.0)
        };
        f.min_segment_ms = f.min_segment_ms.min(60_000);
        f.max_segment_ms = f.max_segment_ms.clamp(f.min_segment_ms.max(1), 600_000);

        let a = &mut self.audio;
        a.channels = a.channels.min(2);
        // 0 disables the ingest listener; anything else is a real port.
        if a.ingest_port > 0 && a.ingest_port < 1_024 {
            a.ingest_port = default_ingest_port();
        }

        let t = &mut self.audio_test;
        t.quiet_ms = clamp_audio_test_ms(Some(t.quiet_ms), default_audio_test_quiet_ms());
        t.speech_ms = clamp_audio_test_ms(Some(t.speech_ms), default_audio_test_speech_ms());

        self.llm.speech_noise_threshold =
            clamp_speech_noise_threshold(self.llm.speech_noise_threshold);
        self.llm.hotwords.truncate(MAX_HOTWORDS);
        for w in &mut self.llm.hotwords {
            *w = w.trim().to_string();
        }
        self.llm
            .hotwords
            .retain(|w| !w.is_empty() && w.len() <= MAX_HOTWORD_LEN);
    }

    /// Reject a config that must not be applied at all.
    ///
    /// Called after [`Config::clamp`] and before anything is written to disk or
    /// handed to the pipeline, so a rejected request never mutates state and
    /// never writes a file.
    pub fn validate(&self, config_path: &Path) -> std::result::Result<(), ValidationErrors> {
        let mut errors = Vec::new();

        if self.server.port == 0 {
            errors.push("server.port 不能为 0（随机端口会使管理页地址和 Origin 校验失效）".into());
        }

        // --- audio.sample_rate: no zero / one-sample / overflow / huge alloc ---
        // `0` is rejected outright, not treated as "device default": every
        // producer now emits a fixed internal rate, so a 0 here would make the
        // value the VAD is told disagree with the rate the frames were made at
        // (P0-04). `1` would make `frame_ms` a division by a nonsense rate and a
        // resampler ratio of 1:16000; `u32::MAX` would overflow the frame
        // arithmetic. All three are refused before anything is stored.
        let rate = self.audio.sample_rate;
        if !(AUDIO_SAMPLE_RATE_MIN..=AUDIO_SAMPLE_RATE_MAX).contains(&rate) {
            errors.push(format!(
                "audio.sample_rate={rate} 超出允许范围（{AUDIO_SAMPLE_RATE_MIN}–{AUDIO_SAMPLE_RATE_MAX} Hz，且必须显式填写，0 不再表示“设备默认”）"
            ));
        }
        // --- audio.channels --------------------------------------------------
        if self.audio.channels > 2 {
            errors.push(format!(
                "audio.channels={} 超出允许范围（0–2）",
                self.audio.channels
            ));
        }
        // --- audio.mode ------------------------------------------------------
        if !matches!(self.audio.mode.as_str(), "system" | "device" | "obs_filter") {
            errors.push(format!(
                "audio.mode={} 不是受支持的模式（system/device/obs_filter）",
                self.audio.mode
            ));
        }
        // --- overlay.display_delay_ms / clear_after_ms -----------------------
        // `clamp` already normalised these, so nothing to reject.

        // --- string lengths --------------------------------------------------
        let mut too_long = |name: &str, value: &str, max: usize, errors: &mut Vec<String>| {
            if value.len() > max {
                errors.push(format!("{name} 过长（{} 字节，上限 {max}）", value.len()));
            }
        };
        too_long("llm.model", &self.llm.model, 200, &mut errors);
        too_long("llm.provider", &self.llm.provider, 64, &mut errors);
        too_long("llm.target_lang", &self.llm.target_lang, 16, &mut errors);
        too_long(
            "llm.transcription_model",
            &self.llm.transcription_model,
            200,
            &mut errors,
        );
        too_long("llm.workspace_id", &self.llm.workspace_id, 128, &mut errors);
        too_long(
            "llm.api_key",
            &self.llm.api_key,
            MAX_STRING_LEN,
            &mut errors,
        );
        if let Some(e) = &self.llm.endpoint {
            too_long("llm.endpoint", e, MAX_STRING_LEN, &mut errors);
        }
        if let Some(p) = &self.llm.system_prompt {
            too_long("llm.system_prompt", p, MAX_SYSTEM_PROMPT_LEN, &mut errors);
        }
        too_long("audio.device", &self.audio.device, 512, &mut errors);
        too_long("obs.host", &self.obs.host, 255, &mut errors);
        too_long(
            "obs.password",
            &self.obs.password,
            MAX_STRING_LEN,
            &mut errors,
        );
        too_long(
            "overlay.font_family",
            &self.overlay.font_family,
            512,
            &mut errors,
        );
        too_long(
            "overlay.font_color",
            &self.overlay.font_color,
            32,
            &mut errors,
        );
        too_long(
            "overlay.background_color",
            &self.overlay.background_color,
            32,
            &mut errors,
        );
        too_long("server.host", &self.server.host, 255, &mut errors);
        if self.overlay.layout.len() > 32 {
            errors.push("overlay.layout 过长".into());
        }

        // --- recording_dir: no traversal, no UNC, no escaping the config dir --
        if let Err(e) = resolve_recording_dir(config_path, &self.recording_dir) {
            errors.push(e);
        }
        // --- retention: bounded, so a typo cannot delete a decade of sessions --
        // 0 = keep forever (the shipped default). Anything past ~10 years is a
        // mistake rather than an intent, so it is refused instead of honoured.
        if self.retention_days > 3_650 {
            errors.push(format!(
                "retention_days={} 超出合理范围（0 = 永久保留，最大 3650 天）",
                self.retention_days
            ));
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(ValidationErrors(errors))
        }
    }

    /// `clamp` + `validate`, the two steps every externally supplied config
    /// must pass before it is stored or written.
    pub fn normalise(&mut self, config_path: &Path) -> std::result::Result<(), ValidationErrors> {
        self.clamp();
        self.validate(config_path)
    }
}

/// Resolve `recording_dir` against the config's directory, refusing anything
/// that would write outside it (P0-04).
///
/// Empty keeps the `<config dir>/recordings` default; a relative path is
/// resolved from the config directory as long as it contains no `..` component;
/// an absolute path is accepted only when it is inside that directory. Windows
/// UNC paths (`\\server\share`) are always refused: they write the session's
/// subtitles to a network share that the local user does not control.
pub fn resolve_recording_dir(
    config_path: &Path,
    recording_dir: &str,
) -> std::result::Result<PathBuf, String> {
    let base = config_path.parent().unwrap_or_else(|| Path::new("."));
    let raw = recording_dir.trim();
    if raw.is_empty() {
        return checked_recording_path(base, &base.join("recordings"), recording_dir);
    }
    if raw.starts_with("\\\\") || raw.starts_with("//") {
        return Err(format!("recording_dir 不能是 UNC 网络路径：{raw}"));
    }
    // A Windows drive-relative path (`C:foo`) is neither absolute nor safely
    // relative: reject it rather than guessing which directory it means.
    let bytes = raw.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && !raw.starts_with("\\\\") {
        let absolute = Path::new(raw).is_absolute();
        if !absolute {
            return Err(format!("recording_dir 不能是驱动器相对路径：{raw}"));
        }
    }
    if raw.chars().any(|c| c == '\0') {
        return Err("recording_dir 含非法字符".into());
    }

    let requested = Path::new(raw);
    if requested.is_absolute() {
        // The target may not exist yet, so canonicalise the deepest existing
        // ancestor and re-append the missing tail. Both sides go through the
        // SAME canonicalisation: `std::fs::canonicalize` on Windows returns a
        // verbatim path (`\\?\C:\…`) for a path that exists but a plain one
        // (`C:\…`) for a path that does not, and `Path::starts_with` compares
        // components — so `\\?\C:\a` does NOT start with `C:\a`. Comparing a
        // canonicalised target against a plain base therefore reported *every*
        // out-of-tree absolute path as inside the tree, which is the opposite of
        // what this check is for.
        let base_abs = canonicalize_allow_missing(base);
        let resolved = canonicalize_allow_missing(requested);
        if path_is_within(&resolved, &base_abs) {
            return Ok(requested.to_path_buf());
        }
        return Err(format!(
            "recording_dir 的绝对路径必须位于配置目录 {} 之内：{raw}",
            base.display()
        ));
    }

    for component in requested.components() {
        match component {
            std::path::Component::ParentDir => {
                return Err(format!("recording_dir 不允许包含 .. 上级目录：{raw}"));
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                return Err(format!("recording_dir 不是合法的相对路径：{raw}"));
            }
            _ => {}
        }
    }
    checked_recording_path(base, &base.join(requested), recording_dir)
}

fn checked_recording_path(
    base: &Path,
    requested: &Path,
    raw: &str,
) -> std::result::Result<PathBuf, String> {
    if path_is_within(
        &canonicalize_allow_missing(requested),
        &canonicalize_allow_missing(base),
    ) {
        Ok(requested.to_path_buf())
    } else {
        Err(format!("recording_dir 经符号链接解析后超出配置目录：{raw}"))
    }
}

/// Canonicalise as much of `path` as exists, then re-append the missing tail.
fn canonicalize_allow_missing(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    // How many trailing components of `path` do not exist on disk yet.
    let mut missing = 0usize;
    loop {
        if let Ok(canonical) = std::fs::canonicalize(&existing) {
            let mut out = canonical;
            let parts: Vec<&std::ffi::OsStr> = path
                .components()
                .skip(path.components().count().saturating_sub(missing))
                .map(|c| c.as_os_str())
                .collect();
            for part in parts {
                out.push(part);
            }
            return out;
        }
        let Some(parent) = existing.parent() else {
            // Reached a root that does not canonicalise (a non-existent drive,
            // say). Fall back to the path as given; `path_is_within` still
            // compares it component-wise, so this cannot turn a rejection into
            // an acceptance.
            return path.to_path_buf();
        };
        if parent == existing {
            return path.to_path_buf();
        }
        existing = parent.to_path_buf();
        missing += 1;
        if missing > 256 {
            // Defensive: no real path is this deep.
            return path.to_path_buf();
        }
    }
}

/// True when `candidate` is `base` itself or lives underneath it.
///
/// Compares the canonical `\\?\`-stripped forms so a verbatim path and a plain
/// path for the same file compare equal. Component-wise, so `C:\data-evil` is
/// NOT considered inside `C:\data`.
fn path_is_within(candidate: &Path, base: &Path) -> bool {
    let strip = |p: &Path| -> PathBuf {
        let text = p.to_string_lossy();
        match text.strip_prefix(r"\\?\") {
            Some(rest) => PathBuf::from(rest),
            None => p.to_path_buf(),
        }
    };
    strip(candidate).starts_with(strip(base))
}

impl Config {
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if path.exists() {
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("read config {}", path.display()))?;
            let cfg: Config =
                toml::from_str(&raw).with_context(|| format!("parse config {}", path.display()))?;
            Ok(cfg)
        } else {
            // First run. Prefer the embedded default template (so the binary
            // is truly self-contained), but fall back to the programmatic
            // default if the embedded asset is missing for some reason.
            let raw = crate::embedded::DEFAULT_CONFIG;
            let cfg: Config = toml::from_str(raw).unwrap_or_else(|_| Config::default());
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            atomic_write(path, raw.as_bytes())?;
            Ok(cfg)
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        atomic_write(path, toml::to_string_pretty(self)?.as_bytes())?;
        Ok(())
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}
