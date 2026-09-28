//! Cross-platform system audio loopback capture.
//!
//! Strategy:
//!   * Windows: WASAPI loopback from the default output render device (cpal).
//!   * macOS:   CoreAudio aggregate device via cpal (loopback requires either
//!              a preinstalled "Multi-Output Device" + Soundflower/BlackHole
//!              OR ScreenCaptureKit). We try the standard cpal loopback flow
//!              first and fall back to ScreenCaptureKit when
//!              `audio.use_screen_capture_kit` is true.
//!   * Linux:   ALSA (cpal's backend). System audio is exposed by
//!              PulseAudio/PipeWire as a `*.monitor` source through the
//!              alsa-plugins bridge; we prefer it and fall back to the
//!              default input (usually a microphone) with a warning.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream, StreamConfig};
use parking_lot::Mutex;
use tracing::{info, warn};

use crate::config::AudioConfig;

pub type PcmSender = tokio::sync::mpsc::Sender<Vec<i16>>;
pub type PcmReceiver = tokio::sync::mpsc::Receiver<Vec<i16>>;

#[derive(Debug, Clone)]
pub struct CaptureSpec {
    pub sample_rate: u32,
    pub channels: u16,
}

pub struct CaptureHandle {
    pub stream: Stream,
    pub spec: CaptureSpec,
    healthy: Arc<AtomicBool>,
}

// SAFETY: cpal::Stream internally stores raw pointers and is therefore !Send
// by default, but all stream operations are synchronized inside cpal (the
// audio callback runs on cpal's own thread). Moving/dropping the handle
// across threads — required because it lives in the shared AppState behind
// a Mutex — is safe on all supported backends.
unsafe impl Send for CaptureHandle {}
unsafe impl Sync for CaptureHandle {}

pub struct AudioCapturer {
    cfg: AudioConfig,
    state: Arc<Mutex<Option<CaptureHandle>>>,
}

impl AudioCapturer {
    pub fn new(cfg: AudioConfig) -> Self {
        Self {
            cfg,
            state: Arc::new(Mutex::new(None)),
        }
    }

    pub fn spec(&self) -> Option<CaptureSpec> {
        self.state.lock().as_ref().map(|h| h.spec.clone())
    }

    pub fn stop(&self) {
        if let Some(handle) = self.state.lock().take() {
            drop(handle.stream);
            info!("audio capture stopped");
        }
    }

    pub fn is_running(&self) -> bool {
        self.state.lock().is_some()
    }

    /// CPAL reports device loss asynchronously through the stream error
    /// callback.  Keep that signal available to the pipeline so it can stop
    /// reporting an unplugged receiver as an active input and retry it.
    pub fn is_healthy(&self) -> bool {
        self.state
            .lock()
            .as_ref()
            .is_some_and(|handle| handle.healthy.load(Ordering::Acquire))
    }

    /// Start a new capture stream. The audio frames (downsampled to mono PCM
    /// s16le) are pushed into `tx`.
    pub fn start(&self, tx: PcmSender) -> Result<()> {
        self.stop();

        let host = cpal::default_host();
        let device =
            pick_device(&host, &self.cfg).with_context(|| "no suitable audio device found")?;
        let device_name = device.name().unwrap_or_else(|_| "<unnamed>".into());
        info!(device = %device_name, "audio device selected");

        let supported = device
            .default_input_config()
            .or_else(|_| device.default_output_config())
            .map_err(|e| anyhow!("device has no usable config: {e}"))?;

        // Open the device using a format it actually advertises.  Forcing a
        // 16 kHz stream on a 48 kHz-only wireless receiver makes cpal fail
        // before we can resample it.  The callback converts native audio to
        // the fixed mono pipeline rate below.
        let config: StreamConfig = supported.config();
        let input_rate = config.sample_rate.0;
        // Every producer feeds the pipeline's single internal format. The
        // configured value is validated for compatibility, but must never
        // relabel native-rate PCM or change what VAD/providers receive.
        let output_rate = crate::pipeline::INTERNAL_SAMPLE_RATE;

        let sample_format = supported.sample_format();
        let healthy = Arc::new(AtomicBool::new(true));
        let stream = match sample_format {
            SampleFormat::F32 => {
                build_stream::<f32>(&device, &config, tx.clone(), output_rate, healthy.clone())
            }
            SampleFormat::I16 => {
                build_stream::<i16>(&device, &config, tx.clone(), output_rate, healthy.clone())
            }
            SampleFormat::U16 => {
                build_stream::<u16>(&device, &config, tx.clone(), output_rate, healthy.clone())
            }
            other => {
                return Err(anyhow!(
                    "unsupported sample format {other:?}; please file an issue"
                ));
            }
        }
        .with_context(|| "failed to build audio stream")?;

        stream
            .play()
            .with_context(|| "failed to start audio stream")?;

        *self.state.lock() = Some(CaptureHandle {
            stream,
            spec: CaptureSpec {
                sample_rate: output_rate,
                channels: 1,
            },
            healthy,
        });

        info!(
            input_rate,
            input_channels = config.channels,
            output_rate,
            output_channels = 1,
            format = ?sample_format,
            "audio capture started"
        );
        Ok(())
    }
}

fn pick_device(host: &cpal::Host, cfg: &AudioConfig) -> Result<cpal::Device> {
    if !cfg.device.is_empty() {
        if let Some(d) = host
            .devices()?
            .into_iter()
            .find(|d| d.name().map(|n| n == cfg.device).unwrap_or(false))
        {
            return Ok(d);
        }
        warn!(
            requested = %cfg.device,
            "requested device not found, falling back to default"
        );
    }
    // Prefer an output device on platforms that support loopback (Windows,
    // macOS), otherwise fall back to the default input.
    if let Some(d) = host.default_output_device() {
        if host.id().name().to_string().contains("WASAPI") {
            return Ok(d);
        }
    }
    // Linux 没有 WASAPI 那种环回接口：系统声音是作为 PulseAudio /
    // PipeWire 的「monitor 源」暴露的，名字里通常带 `.monitor`。cpal 走
    // ALSA（经 alsa-plugins 的 pulse 插件能看到这些源），但默认输入设备
    // 是麦克风 —— 不特殊处理的话 Linux 上会静默录错设备，用户只知道
    // 「没字幕」，很难排查。
    #[cfg(target_os = "linux")]
    {
        if let Some(d) = pick_linux_monitor(host) {
            let name = d.name().unwrap_or_else(|_| "<unnamed>".into());
            info!(device = %name, "using system audio monitor source");
            return Ok(d);
        }
        warn!(
            "no PulseAudio/PipeWire monitor source found; \
             falling back to the default input device (likely a microphone). \
             To capture system audio on Linux, select a *.monitor device above."
        );
    }
    host.default_input_device()
        .ok_or_else(|| anyhow!("no input or output device available"))
}

/// Pick the PulseAudio/PipeWire monitor source that carries system audio.
/// Prefers an exact `*.monitor` name, then any device mentioning monitor /
/// loopback. Returns None when the ALSA plugin bridge isn't available.
#[cfg(target_os = "linux")]
fn pick_linux_monitor(host: &cpal::Host) -> Option<cpal::Device> {
    let mut fallback = None;
    for dev in host.devices().ok()?.into_iter() {
        let Ok(name) = dev.name() else { continue };
        // Only devices we can actually open as an input are usable.
        if dev.default_input_config().is_err() {
            continue;
        }
        let lower = name.to_lowercase();
        if lower.ends_with(".monitor") {
            return Some(dev);
        }
        if fallback.is_none() && (lower.contains("monitor") || lower.contains("loopback")) {
            fallback = Some(dev);
        }
    }
    fallback
}

fn build_stream<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    tx: PcmSender,
    output_rate: u32,
    healthy: Arc<AtomicBool>,
) -> Result<Stream>
where
    T: cpal::Sample + cpal::SizedSample + Send + 'static,
    f32: cpal::FromSample<T>,
{
    let err_tx = tx.clone();
    let channels = config.channels as usize;
    let input_rate = config.sample_rate.0;
    let mut resampler = StreamResampler::new(input_rate, output_rate);
    let stream = device.build_input_stream(
        config,
        move |data: &[T], _info| {
            let mut out = Vec::with_capacity(data.len() / channels.max(1));
            for frame in data.chunks(channels.max(1)) {
                let mut acc = 0.0f32;
                for s in frame {
                    acc += s.to_sample::<f32>();
                }
                let mono = acc / channels.max(1) as f32;
                out.push((mono.clamp(-1.0, 1.0) * i16::MAX as f32) as i16);
            }
            // Best-effort send; drop on backpressure.
            let mut converted = Vec::new();
            resampler.push(&out, &mut converted);
            if !converted.is_empty() {
                let _ = err_tx.try_send(converted);
            }
        },
        move |err| {
            healthy.store(false, Ordering::Release);
            tracing::error!(error = %err, "audio stream error");
        },
        None,
    )?;
    Ok(stream)
}

/// List available input/output devices for the admin panel.
pub fn list_devices() -> Result<Vec<DeviceInfo>> {
    let host = cpal::default_host();
    let mut out = Vec::new();
    for dev in host.devices()? {
        let name = dev.name().unwrap_or_else(|_| "<unnamed>".into());
        let supports_input = dev.default_input_config().is_ok();
        let supports_output = dev.default_output_config().is_ok();
        out.push(DeviceInfo {
            name,
            supports_input,
            supports_output,
        });
    }
    Ok(out)
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DeviceInfo {
    pub name: String,
    pub supports_input: bool,
    pub supports_output: bool,
}

/// Stateful streaming resampler for mono s16le PCM.
///
/// [`resample_mono`] is stateless: every call starts again at input index 0
/// with phase 0, so resampling a byte stream chunk by chunk does *not* produce
/// the same samples as resampling the same bytes in one piece — the phase is
/// lost at every chunk boundary. `StreamResampler` carries everything the next
/// output sample needs across calls:
///
/// * the interpolation phase (as the exact output counter `emitted`, never as
///   an accumulated float, so chunk boundaries cannot make it drift), and
/// * the input samples around that phase (`window`, addressed by the absolute
///   input index `base`). A window is kept rather than a single previous
///   sample because with a mid-sample phase the next output sample may need up
///   to `from / to` samples that arrived in an earlier chunk.
///
/// The guarantee is exact: pushing one long buffer once yields bit-identical
/// output to pushing the same buffer in any sequence of smaller chunks. The
/// arithmetic below is deliberately the same expression the old stateless body
/// used (`pos = i as f64 / ratio`, `t = (pos - i0) as f32`), so one whole-buffer
/// call also stays bit-identical to the previous implementation.
pub struct StreamResampler {
    from: u32,
    to: u32,
    /// `to / from`, the same f64 the legacy loop used.
    ratio: f64,
    /// Index of the next output sample; *this* is the interpolation phase.
    emitted: u64,
    /// Total input samples pushed so far.
    seen: u64,
    /// Buffered input samples `window[0..] == global input [base..seen)`.
    window: Vec<i16>,
    base: u64,
}

impl StreamResampler {
    pub fn new(from: u32, to: u32) -> Self {
        Self {
            from,
            to,
            ratio: to as f64 / from as f64,
            emitted: 0,
            seen: 0,
            window: Vec::new(),
            base: 0,
        }
    }

    /// `from == to` (or `from == 0`, which the old code treated as identity):
    /// samples pass through untouched.
    fn passthrough(&self) -> bool {
        self.from == 0 || self.from == self.to
    }

    /// `to == 0`: no output at all (the old code produced an empty Vec).
    fn silent(&self) -> bool {
        self.from != 0 && self.from != self.to && self.to == 0
    }

    /// Feed the next chunk of input-rate samples; resampled output is appended
    /// to `out`. Input that cannot be turned into a final output sample yet is
    /// retained for the next call (see also [`StreamResampler::finish`]).
    pub fn push(&mut self, input: &[i16], out: &mut Vec<i16>) {
        if input.is_empty() {
            return;
        }
        self.seen += input.len() as u64;
        if self.passthrough() {
            out.extend_from_slice(input);
            return;
        }
        self.window.extend_from_slice(input);
        if self.silent() {
            self.reset_window();
            return;
        }
        // Only samples that still exist in a whole-buffer resampling of
        // everything seen so far may be emitted; that is exactly the length the
        // old `out_len` computation produced.
        let count = self.output_len();
        loop {
            if self.emitted >= count as u64 {
                break;
            }
            let pos = self.emitted as f64 / self.ratio;
            let i0 = pos.floor() as u64;
            // Both interpolation endpoints must be present *and final*. If the
            // right-hand neighbour is still missing we stop: emitting it now
            // with a clamped neighbour would make the value depend on where the
            // chunk boundary happened to fall. `finish` handles the tail.
            if i0 + 2 > self.seen {
                break;
            }
            let t = (pos - i0 as f64) as f32;
            let a = self.window[(i0 - self.base) as usize];
            let b = self.window[(i0 + 1 - self.base) as usize];
            out.push(((1.0 - t) * a as f32 + t * b as f32) as i16);
            self.emitted += 1;
        }
        self.retire();
    }

    /// Flush the tail of a finite stream: the last output samples whose
    /// right-hand neighbour does not exist, which the old implementation
    /// produced by clamping the interpolation to the final input sample. Call
    /// this once, after the last [`StreamResampler::push`]; it does not affect
    /// the chunk-boundary guarantee, which only concerns `push`.
    pub fn finish(&mut self, out: &mut Vec<i16>) {
        if self.passthrough() || self.silent() || self.seen == 0 {
            self.reset_window();
            return;
        }
        let count = self.output_len();
        let last = self.seen - 1;
        while (self.emitted as usize) < count {
            let pos = self.emitted as f64 / self.ratio;
            let i0 = pos.floor() as u64;
            let i1 = (i0 + 1).min(last);
            let t = (pos - i0 as f64) as f32;
            let a = self.window[(i0 - self.base) as usize];
            let b = self.window[(i1 - self.base) as usize];
            out.push(((1.0 - t) * a as f32 + t * b as f32) as i16);
            self.emitted += 1;
        }
        self.reset_window();
    }

    /// Output samples the data seen so far can support. Byte-for-byte the
    /// expression the old loop used for `out_len`, so a whole-buffer call keeps
    /// its previous length.
    fn output_len(&self) -> usize {
        (self.seen as f64 * self.ratio) as usize
    }

    /// Drop buffered input that the next output sample cannot reach any more.
    /// This keeps the window bounded by roughly one chunk plus `from / to`
    /// samples instead of growing with the stream.
    fn retire(&mut self) {
        let next = self.emitted as f64 / self.ratio;
        let keep = (next.floor() as u64).min(self.seen);
        if keep > self.base {
            let drop = (keep - self.base) as usize;
            if drop >= self.window.len() {
                self.window.clear();
            } else {
                self.window.drain(..drop);
            }
            self.base = keep;
        }
    }

    fn reset_window(&mut self) {
        self.window.clear();
        self.base = self.seen;
    }
}

/// Helper: resample a chunk of mono s16le PCM from `from_rate` to `to_rate`.
/// Uses simple linear interpolation; good enough for VAD and ASR.
///
/// This is the one-shot form of [`StreamResampler`], so both paths share a
/// single interpolation implementation and one whole-buffer call keeps exactly
/// the output (values and length) it had before.
pub fn resample_mono(input: &[i16], from_rate: u32, to_rate: u32) -> Vec<i16> {
    let mut resampler = StreamResampler::new(from_rate, to_rate);
    let mut out = Vec::new();
    resampler.push(input, &mut out);
    resampler.finish(&mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic LCG (no `rand`, so every run and every platform feeds the
    /// resampler exactly the same bytes).
    fn lcg(state: &mut u64) -> u64 {
        *state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        *state
    }

    /// ~1 s of deterministic PCM: a tone (so phase errors are visible in the
    /// sample values) plus full-range LCG dither (so the interpolation also
    /// sees awkward odd values).
    fn deterministic_pcm(len: usize) -> Vec<i16> {
        let mut s = 0x0BAD_C0DE_1234_5678u64;
        (0..len)
            .map(|i| {
                let noise = ((lcg(&mut s) >> 48) as i64) - 32_768;
                let tone = ((i as f64) * 0.045).sin() * 20_000.0;
                (tone as i64 + noise).clamp(i16::MIN as i64, i16::MAX as i64) as i16
            })
            .collect()
    }

    /// The pre-fix implementation, kept as the reference the new
    /// `resample_mono` must still match bit-for-bit.
    fn legacy_resample_mono(input: &[i16], from_rate: u32, to_rate: u32) -> Vec<i16> {
        if from_rate == 0 || from_rate == to_rate || input.is_empty() {
            return input.to_vec();
        }
        let ratio = to_rate as f64 / from_rate as f64;
        let out_len = (input.len() as f64 * ratio) as usize;
        let mut out = Vec::with_capacity(out_len);
        for i in 0..out_len {
            let src = i as f64 / ratio;
            let i0 = src.floor() as usize;
            let i1 = (i0 + 1).min(input.len() - 1);
            let t = (src - i0 as f64) as f32;
            let v = (1.0 - t) * input[i0] as f32 + t * input[i1] as f32;
            out.push(v as i16);
        }
        out
    }

    #[test]
    fn resamples_48khz_mono_to_16khz_duration() {
        let input: Vec<i16> = (0..4_800).map(|n| n as i16).collect();
        let output = resample_mono(&input, 48_000, 16_000);
        assert_eq!(output.len(), 1_600);
        assert_eq!(output[0], input[0]);
    }

    /// The one-shot entry point must keep producing exactly what the old
    /// stateless loop produced — values *and* length — for a whole-buffer call.
    #[test]
    fn resample_mono_is_unchanged_for_a_whole_buffer() {
        let rates = [
            (44_100u32, 16_000u32),
            (48_000, 16_000),
            (22_050, 16_000),
            (8_000, 16_000),
            (16_000, 8_000),
            (22_050, 44_100),
            (16_000, 16_000),
            (48_000, 48_000),
            (0, 16_000),
            (16_000, 0),
            (0, 0),
        ];
        for (from, to) in rates {
            for len in [0usize, 1, 2, 3, 5, 7, 101, 319, 320, 321, 999, 4_800, 8_000] {
                let input = deterministic_pcm(len);
                assert_eq!(
                    resample_mono(&input, from, to),
                    legacy_resample_mono(&input, from, to),
                    "whole-buffer resample changed (from={from} to={to} len={len})"
                );
            }
        }
    }

    /// P1-02 bug 1: the phase is lost at every chunk boundary, so the output
    /// depends on how the stream happened to be split. The streaming resampler
    /// must be immune to that: whole vs. many random chunk sizes vs. one sample
    /// at a time all have to concatenate to exactly the same samples.
    #[test]
    fn resampling_is_chunk_boundary_invariant() {
        for (from, to) in [
            (44_100u32, 16_000u32),
            (48_000, 16_000),
            (8_000, 16_000),  // upsampling
            (16_000, 16_000), // identity
        ] {
            let input = deterministic_pcm(from as usize);
            let mut whole = Vec::new();
            StreamResampler::new(from, to).push(&input, &mut whole);
            assert!(!whole.is_empty(), "from={from} to={to} produced nothing");

            let mut seed = 0x5EED_1234_5678_9ABCu64;
            for trial in 0..6 {
                let mut resampler = StreamResampler::new(from, to);
                let mut chunked = Vec::new();
                let mut fed = 0usize;
                while fed < input.len() {
                    let size = ((lcg(&mut seed) >> 33) as usize % 997) + 1;
                    let end = (fed + size).min(input.len());
                    resampler.push(&input[fed..end], &mut chunked);
                    fed = end;
                }
                assert_eq!(
                    whole, chunked,
                    "random chunking changed the output (from={from} to={to} trial={trial})"
                );
            }

            // The pathological split of the old code: 1 sample per call.
            let mut resampler = StreamResampler::new(from, to);
            let mut single = Vec::new();
            for sample in &input {
                resampler.push(std::slice::from_ref(sample), &mut single);
            }
            assert_eq!(
                whole, single,
                "one-sample pushes changed the output (from={from} to={to})"
            );
        }
    }

    /// The test above is only meaningful because the old stateless call really
    /// was chunk-dependent: this is the P1-02 defect in its simplest form
    /// (per-chunk `floor(len * ratio)` loses almost every sample).
    #[test]
    fn the_old_stateless_call_is_chunk_dependent() {
        let input = deterministic_pcm(44_100);
        let whole = resample_mono(&input, 44_100, 16_000);
        let per_chunk: Vec<i16> = input
            .chunks(1)
            .flat_map(|c| resample_mono(c, 44_100, 16_000))
            .collect();
        assert_ne!(
            whole, per_chunk,
            "the per-chunk one-shot resample must be chunk-dependent"
        );
    }

    /// Odd chunk lengths and a total that is not a multiple of the 20 ms frame
    /// size must not panic and must never lose or invent samples.
    #[test]
    fn odd_length_pushes_keep_every_sample_in_order() {
        let input = deterministic_pcm(7_001); // odd total, chunked in odd sizes
        let mut whole = Vec::new();
        StreamResampler::new(44_100, 16_000).push(&input, &mut whole);

        let mut resampler = StreamResampler::new(44_100, 16_000);
        let mut chunked = Vec::new();
        for chunk in input.chunks(3) {
            resampler.push(chunk, &mut chunked);
        }
        assert_eq!(whole, chunked);
        // 7_001 * 16_000 / 44_100 truncates to 2_540 samples, i.e. 7 full
        // frames plus a 300-sample remainder the caller has to hold back.
        assert_eq!(whole.len(), 2_540);
        assert_eq!(whole.len() % 320, 300);
    }
}

#[cfg(target_os = "macos")]
pub mod macos {
    //! On Apple Silicon, ScreenCaptureKit can capture *any* system audio
    //! (including the OBS monitor output) without installing a virtual audio
    //! device. The user grants permission once. We expose a stub here that
    //! the audio module consults when cpal loopback isn't available.
    use super::*;

    pub fn is_supported() -> bool {
        // The actual SCK bindings are out of scope for this template; the
        // admin panel surfaces a clear error instructing the user to either
        // install BlackHole (free, signed) or grant Screen Recording perms.
        true
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
compile_error!("only Windows, macOS, and Linux are supported");

// (No `linux` helper module: cpal 0.15 on Linux talks to ALSA directly, and
// there is nothing to start up front. The monitor-source preference lives in
// `pick_linux_monitor` above.)
