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

fn default_speech_noise_threshold() -> f32 { 0.0 }
fn default_semantic_punctuation() -> bool { true }

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
            Duration::from_millis(clamp_audio_test_ms(Some(self.quiet_ms), default_audio_test_quiet_ms())),
            Duration::from_millis(clamp_audio_test_ms(Some(self.speech_ms), default_audio_test_speech_ms())),
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
    /// Sample rate. 0 = device default.
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
    /// Max caption lines. Live subtitles are capped at 2: a full page is
    /// replaced by the next page instead of growing taller. Values above 2 are
    /// clamped by the overlay, and 1 = strict single line with an ellipsis.
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
                font_family: "Noto Sans CJK SC, Microsoft YaHei, PingFang SC, sans-serif"
                    .into(),
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
            audio_test: AudioTestConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        clamp_clear_after_ms, clamp_display_delay_ms, clamp_speech_noise_threshold,
        clamp_speech_noise_threshold_flagged, default_speech_noise_threshold, AudioTestConfig,
        Config, AUDIO_TEST_MAX_SEGMENT_MS, AUDIO_TEST_MIN_SEGMENT_MS, CLEAR_AFTER_MAX_MS,
        CLEAR_AFTER_MIN_MS, DISPLAY_DELAY_MAX_MS, DISPLAY_DELAY_MIN_MS,
        SPEECH_NOISE_THRESHOLD_MAX, SPEECH_NOISE_THRESHOLD_MIN,
    };
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
        assert_eq!(clamp_display_delay_ms(Some(back.overlay.display_delay_ms)), 0);
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
        assert_eq!(clamp_speech_noise_threshold(9.0), SPEECH_NOISE_THRESHOLD_MAX);
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
        assert_eq!(
            clamp_speech_noise_threshold_flagged(f32::NAN),
            (0.0, true)
        );
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
            assert_eq!(clamp_speech_noise_threshold(back.llm.speech_noise_threshold), value);
        }
    }

    #[test]
    fn audio_test_durations_are_clamped() {
        let cfg = AudioTestConfig { quiet_ms: 0, speech_ms: 999_999 };
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
        assert_eq!(cfg.overlay.display_delay_ms, default.overlay.display_delay_ms);
        assert_eq!(cfg.overlay.clear_after_ms, default.overlay.clear_after_ms);
        assert_eq!(cfg.audio_test.quiet_ms, default.audio_test.quiet_ms);
        assert_eq!(cfg.audio_test.speech_ms, default.audio_test.speech_ms);
        assert_eq!(cfg.recording_dir, default.recording_dir);
        // `max_lines` above 2 is silently clamped by the overlay, so shipping a
        // template that advertises more would be a lie.
        assert!(cfg.overlay.max_lines <= 2, "template must not advertise >2 lines");
    }
}

impl Config {
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if path.exists() {
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("read config {}", path.display()))?;
            let cfg: Config = toml::from_str(&raw)
                .with_context(|| format!("parse config {}", path.display()))?;
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
            std::fs::write(path, raw)?;
            Ok(cfg)
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(path, toml::to_string_pretty(self)?)?;
        Ok(())
    }
}
