//! Pipeline: capture audio -> VAD/music filter -> LLM provider.
//!
//! Data flow:
//!
//!   audio_capturer --[raw pcm]--> mpsc raw_rx --vad_filter--> mpsc speech_rx --> LLM
//!
//! The VAD filter is a small task that buffers raw PCM frames, runs an
//! energy + spectral-flatness test, and only forwards speech segments to
//! the LLM.  We also keep a per-frame RMS in a shared ring buffer so the
//! admin panel can render a live "input level" meter.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::audio::AudioCapturer;
use crate::vad::{SegmentKind, Vad};
use crate::AppState;

const PCM_CHANNEL_CAPACITY: usize = 256;

pub struct PipelineHandle {
    inner: Arc<Mutex<Option<PipelineInner>>>,
    shutdown: tokio::sync::watch::Sender<bool>,
    pub shutdown_rx: tokio::sync::watch::Receiver<bool>,
    /// Whether the run loop may keep a pipeline alive. `false` = paused by the
    /// admin stop button; the run loop parks instead of starting again.
    run_gate: tokio::sync::watch::Sender<bool>,
    run_gate_rx: tokio::sync::watch::Receiver<bool>,
    /// Earliest time the next `try_start` may run. Bounded by
    /// [`provider_restart_backoff`] after a provider failure so a broken
    /// endpoint (bad key, region, model) cannot be hammered in a tight loop.
    restart_not_before: Mutex<Option<tokio::time::Instant>>,
}

struct PipelineInner {
    _capturer: Option<AudioCapturer>,
    /// Present in `obs_filter` mode; unregisters the ingest sender on drop.
    _ingest_guard: Option<crate::ingest::Registration>,
    _llm_task: tokio::task::JoinHandle<()>,
    _vad_task: tokio::task::JoinHandle<()>,
    _audio_task: tokio::task::JoinHandle<()>,
}

/// Retry spacing after a provider session ended, chosen from whether the session
/// ever produced real audio. A session that was working and then died is worth
/// retrying quickly (network blip, server-side session limit); one that never
/// got anywhere is more likely a configuration error, so back off further.
/// `attempts` grows per consecutive failure and is capped, so a permanently dead
/// endpoint settles at [`PROVIDER_BACKOFF_MAX`] instead of retrying forever at a
/// fixed short interval.
fn provider_restart_backoff(attempts: u32) -> Duration {
    let base: u64 = if attempts == 0 { 2 } else { 5 };
    let secs = base.saturating_mul(1u64 << attempts.min(4));
    Duration::from_secs(secs.min(PROVIDER_BACKOFF_MAX.as_secs()))
}

/// Upper bound for [`provider_restart_backoff`]. A permanently dead endpoint
/// settles here (one attempt per 80 s) rather than retrying forever every 2 s.
pub const PROVIDER_BACKOFF_MAX: Duration = Duration::from_secs(80);

/// Remaining wait before the next start. `None` once the gate is open.
fn gate_wait(not_before: Option<tokio::time::Instant>, now: tokio::time::Instant) -> Option<Duration> {
    not_before.and_then(|deadline| deadline.checked_duration_since(now)).filter(|d| !d.is_zero())
}

/// How long the input may go without a single frame before it counts as gone.
/// Comfortably longer than any real capture block (OBS sends ~20 ms chunks) and
/// far shorter than the cloud's empty-stream timeout, so a silent input is
/// detected long before a session opened on it can be killed by the server.
pub const AUDIO_STALL_AFTER: Duration = Duration::from_millis(2_000);

/// How long a restart waits for a still-live provider session to finish on its
/// own before the pipeline is torn down anyway.
///
/// This MUST cover the provider's own bounded drain, which is 10 s (see the
/// Bailian reader in `llm.rs`). The previous value was 2 s, so `try_start` — and
/// with it `PipelineInner::drop` → `_llm_task.abort()` — ran 8 s before the drain
/// window closed, and a final sentence arriving after 2 s was still lost: the
/// drain fix had not achieved its own goal.
///
/// The wait ends as soon as the provider finishes, so a normal restart is never
/// delayed by this value; it only bounds a provider that is genuinely stuck.
pub const DRAIN_GRACE: Duration = Duration::from_secs(12);

/// How long the pipeline waits for the first *non-silent* audio before giving up
/// on this attempt. Opening the cloud session before any real audio exists is
/// what produced the bare "request timeout after 23 seconds" failures: the
/// engine connected the WebSocket and sent `run-task`, then had nothing to send,
/// and the server dropped the idle task.
///
/// This gates on ENERGY, not on "a frame arrived": both the OBS plugin (with its
/// default `gate_silence=false`) and WASAPI keep delivering digital silence, so
/// counting frames alone would still open a session against a silent source.
///
/// 30 s is generous for OBS to start pushing through the ingest port, and short
/// enough that a genuinely dead source is reported rather than hanging.
pub const AUDIO_FIRST_FRAME_GRACE: Duration = Duration::from_secs(30);

/// Frames at or below this RMS count as digital silence for the first-audio
/// gate. Below the quietest speech the guided test accepts (0.005) and above
/// pure-zero padding.
pub const AUDIO_GATE_RMS_FLOOR: f32 = 0.001;

/// True when a frame is loud enough to mean "the source is actually playing".
fn frame_opens_audio_gate(rms: f32) -> bool {
    rms > AUDIO_GATE_RMS_FLOOR
}

/// True when the input was reported active but has produced no frame for
/// [`AUDIO_STALL_AFTER`]. Without this the status stayed `audio_active = true`
/// forever after the source went quiet, because the mpsc channel only closes
/// when the process holding the sender exits — not when OBS stops feeding it.
fn audio_is_stalled(
    audio_active: bool,
    last_input_at: Option<chrono::DateTime<chrono::Utc>>,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    if !audio_active {
        return false;
    }
    match last_input_at {
        // Active but nothing captured yet: `audio_active` is only set together
        // with a timestamp, so this cannot happen; treat it as live.
        None => false,
        Some(last) => now.signed_duration_since(last).num_milliseconds()
            >= AUDIO_STALL_AFTER.as_millis() as i64,
    }
}

impl Drop for PipelineInner {
    fn drop(&mut self) {
        // Dropping a JoinHandle only detaches the task; without abort() the
        // old LLM/audio tasks (and the open WebSocket session) would leak
        // and keep running across restarts.
        //
        // The one deliberate exception is the post-replay drain: a finite
        // ingest stream drops the raw sender first, which closes `speech_rx`
        // and lets the provider writer send `finish-task` and keep reading the
        // final sentence. `PipelineHandle::restart` therefore postpones the
        // teardown while that provider task is still alive — see its docs.
        self._llm_task.abort();
        self._audio_task.abort();
        self._vad_task.abort();
    }
}

/// True while the provider session task is still alive. Used to tell a session
/// that is finishing its bounded drain (audio gone, task alive) from one that
/// has actually died (task finished).
fn provider_task_alive(inner: Option<&PipelineInner>) -> bool {
    inner.is_some_and(|inner| !inner._llm_task.is_finished())
}

/// Stop the current pipeline run and prevent the run loop from starting it
/// again, without ending the run loop itself.
///
/// Needed because a "stop" that only drops the inner state is immediately
/// undone by the next `try_start` — including a first-audio wait that nothing
/// could interrupt, so `/api/restart` and a config save used to return `ok`
/// while the previous attempt was still parked. `resume` re-arms it.
pub const PIPELINE_STOPPED_MESSAGE: &str = "管线已由管理页停止；点击「重启管线」可重新启动";

impl PipelineHandle {
    pub fn new() -> Self {
        let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
        let (run_gate, run_gate_rx) = tokio::sync::watch::channel(true);
        Self {
            inner: Arc::new(Mutex::new(None)),
            shutdown,
            shutdown_rx,
            run_gate,
            run_gate_rx,
            restart_not_before: Mutex::new(None),
        }
    }

    pub fn is_running(&self) -> bool {
        self.inner.lock().is_some()
    }

    /// True while the run loop is allowed to keep a pipeline alive.
    pub fn is_enabled(&self) -> bool {
        *self.run_gate_rx.borrow()
    }

    /// Stop the pipeline and keep it stopped until [`resume`] is called.
    pub async fn pause(&self) {
        let _ = self.run_gate.send(false);
        self.restart(Duration::ZERO).await;
    }

    /// Allow the run loop to start a pipeline again.
    pub async fn resume(&self) {
        let _ = self.run_gate.send(true);
    }

    /// True while the provider session task is alive.
    fn provider_task_running(&self) -> bool {
        provider_task_alive(self.inner.lock().as_ref())
    }

    /// Block this handle from starting again for `delay`.
    fn defer_restart(&self, delay: Duration) {
        *self.restart_not_before.lock() = Some(tokio::time::Instant::now() + delay);
    }

    /// Stop the current run; the outer run-loop starts a new pipeline with the
    /// (possibly updated) config. Used by `/api/restart`, the config-change
    /// watcher and the provider-death recovery path.
    ///
    /// If a provider session is still alive its audio has already ended, so it is
    /// inside the bounded drain that delivers the last sentence. Tearing it down
    /// here would abort that drain, so the teardown is postponed and the caller
    /// is expected to call [`wait_for_provider`] before starting again. With no
    /// live provider the state is dropped immediately, so a stop still takes
    /// effect at once.
    ///
    /// The lock is deliberately released before the store: `restart` is awaited
    /// from inside a `tokio::spawn`ed task, so it must not hold a
    /// `parking_lot` guard across an await point.
    pub async fn restart(&self, drain_grace: Duration) {
        let taken = {
            let mut guard = self.inner.lock();
            match guard.as_ref() {
                Some(inner) if !inner._llm_task.is_finished() => None,
                _ => guard.take(),
            }
        };
        match taken {
            Some(inner) => drop(inner),
            None => {
                if self.inner.lock().is_some() {
                    self.defer_restart(drain_grace);
                }
            }
        }
    }

    /// Wait for a still-live provider session to finish its own bounded drain,
    /// up to `max_wait`. Returns immediately when nothing is running.
    ///
    /// This is what actually buys the drain its window: the wait is bounded by
    /// the provider's own 10 s reader timeout, and because it *waits* instead of
    /// aborting at a deadline, a final sentence that arrives late is still
    /// delivered rather than cut off.
    ///
    /// Returns `true` when the provider is gone (drained or already finished).
    pub async fn wait_for_provider(&self, max_wait: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + max_wait;
        loop {
            if !self.provider_task_running() {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Permanent shutdown (process exit). The outer run-loop sees the
    /// shutdown flag and terminates for good.
    pub async fn shutdown(&self) {
        let _ = self.shutdown.send(true);
        // Nothing may be postponed on the way out.
        let taken = self.inner.lock().take();
        if let Some(inner) = taken {
            drop(inner);
        }
    }
}

pub fn spawn(state: Arc<AppState>, config_path: std::path::PathBuf) {
    let handle = state.pipeline.clone();
    let state_clone = state.clone();
    tokio::spawn(async move {
        run(state_clone, config_path, handle).await;
    });
}

async fn run(
    state: Arc<AppState>,
    _config_path: std::path::PathBuf,
    handle: Arc<PipelineHandle>,
) {
    let mut backoff = Duration::from_secs(2);
    let mut shutdown_rx = handle.shutdown_rx.clone();
    let mut run_gate_rx = handle.run_gate_rx.clone();
    loop {
        if *shutdown_rx.borrow() {
            return;
        }
        // Paused from the admin panel: park here instead of starting a pipeline.
        // This is a real stop (unlike merely dropping the inner state, which the
        // very next `try_start` undid).
        if !*run_gate_rx.borrow() {
            tokio::select! {
                changed = run_gate_rx.changed() => {
                    if changed.is_err() { return; }
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() { return; }
                }
            }
            continue;
        }
        // A restart requested while the previous provider was still draining
        // must not yank the socket: WAIT for the drain to finish (the provider
        // ends its own session when the final sentence arrives) instead of
        // aborting it at a deadline. `wait_for_provider` returns as soon as the
        // task is gone, so an ordinary restart is not delayed.
        //
        // The gate value is copied out on its own line so the `parking_lot`
        // guard is released before the await below (the future must stay Send).
        let gate = gate_wait(*handle.restart_not_before.lock(), tokio::time::Instant::now());
        // The drain window is a floor, never truncated by the retry gate: a 2 s
        // deferral must not cut a 10 s drain short (that was defect #2).
        let safe_wait = if handle.provider_task_running() {
            gate.map_or(DRAIN_GRACE, |g| g.max(DRAIN_GRACE))
        } else {
            gate.unwrap_or_default()
        };
        if !safe_wait.is_zero() {
            info!(ms = safe_wait.as_millis() as u64, "waiting for the previous session before restarting");
            let drained = tokio::select! {
                drained = handle.wait_for_provider(safe_wait) => drained,
                changed = run_gate_rx.changed() => {
                    if changed.is_err() { return; }
                    false
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() { return; }
                    false
                }
            };
            // `drained == false` means the provider outlived the wait and there
            // was something to drain; tear it down so the loop can progress.
            if !drained && handle.provider_task_running() {
                warn!(
                    ms = safe_wait.as_millis() as u64,
                    "provider did not finish within the drain window; tearing it down"
                );
                handle.restart(Duration::ZERO).await;
            }
            continue;
        }
        let result = try_start(&state, &handle).await;
        match result {
            Ok(()) => {
                info!("pipeline started cleanly");
                // Clear any stale error from a previous failed attempt so
                // the admin panel shows a healthy state.
                state.status.write().last_error = None;
                backoff = Duration::from_secs(2);
            }
            Err(e) => {
                // `{e:#}` flattens the whole anyhow cause chain: the Display of
                // an anyhow::Error on its own prints only the OUTERMOST context
                // ("connect to Bailian Fun-ASR"), which hid a TLS root cause
                // (`SEC_E_NO_CREDENTIALS`) during a real debugging session.
                warn!(error = %format!("{e:#}"), "pipeline failed");
                {
                    let mut s = state.status.write();
                    // {:#} flattens the full anyhow cause chain so the admin
                    // panel shows e.g. "connect to qwen realtime: HTTP 401".
                    s.last_error = Some(format!("{e:#}"));
                    s.audio_active = false;
                    s.llm_connected = false;
                }
                // A stop must not wait for the current retry delay.  This
                // also prevents a shutdown from issuing one final reconnect.
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() { return; }
                    }
                }
                backoff = (backoff * 2).min(Duration::from_secs(10));
                continue;
            }
        }
        watch(&state, &handle).await;
    }
}

async fn watch(state: &Arc<AppState>, handle: &Arc<PipelineHandle>) {
    let mut last_provider = state.config.read().llm.provider.clone();
    let mut last_api_key = state.config.read().llm.api_key.clone();
    let mut last_model = state.config.read().llm.model.clone();
    let mut last_endpoint = state.config.read().llm.endpoint.clone();
    let mut last_audio_mode = state.config.read().audio.mode.clone();
    let mut last_device = state.config.read().audio.device.clone();
    // 热词（R10）：词表变化**不**需要重启会话——百炼通道支持 continue-task
    // 运行中更新，重启反而会掐掉当前正在识别的那句话。这里只把它推进 feed。
    let mut last_hotwords = state.config.read().llm.hotwords.clone();
    let mut last_audio_active = state.status.read().audio_active;
    // Distinguishes "handshake still in flight" from "session was up and died".
    // Creating a Bailian session can take many seconds, so restarting on
    // `!llm_connected` alone would kill a perfectly healthy connect attempt.
    let mut saw_llm_connected = state.status.read().llm_connected;
    let mut ticker = tokio::time::interval(Duration::from_secs(2));
    loop {
        ticker.tick().await;
        if *handle.shutdown_rx.borrow() {
            return;
        }
        if !handle.is_running() {
            return;
        }
        let cur = state.config.read().clone();
        if cur.llm.hotwords != last_hotwords {
            last_hotwords = cur.llm.hotwords.clone();
            let plan = crate::hotwords::plan(&last_hotwords);
            state.hotwords.set(plan);
            info!(
                words = last_hotwords.len(),
                "hotwords changed; pushed to the live session (continue-task when running)"
            );
        }
        if cur.llm.provider != last_provider
            || cur.llm.api_key != last_api_key
            || cur.llm.model != last_model
            || cur.llm.endpoint != last_endpoint
            || cur.audio.mode != last_audio_mode
            || cur.audio.device != last_device
        {
            info!("config changed, restarting pipeline");
            last_provider = cur.llm.provider.clone();
            last_api_key = cur.llm.api_key.clone();
            last_model = cur.llm.model.clone();
            last_endpoint = cur.llm.endpoint.clone();
            last_audio_mode = cur.audio.mode.clone();
            last_device = cur.audio.device.clone();
            // A config change is user-initiated, so only give a still-running
            // provider a short grace period to finish; otherwise stop at once.
            let grace = if handle.provider_task_running() {
                Duration::from_secs(2)
            } else {
                Duration::ZERO
            };
            handle.restart(grace).await;
            tokio::time::sleep(Duration::from_millis(250)).await;
            return;
        }
        // CPAL reports a receiver/device failure asynchronously.  Reflect that
        // in the status and restart so an unplugged wireless receiver is not
        // presented as healthy until it can be opened again.
        let capture_failed = handle
            .inner
            .lock()
            .as_ref()
            .and_then(|inner| inner._capturer.as_ref())
            .is_some_and(|capturer| !capturer.is_healthy());
        if capture_failed {
            let mut status = state.status.write();
            status.audio_active = false;
            status.last_error = Some("audio input device disconnected; retrying".into());
        }
        // Detect audio stream disconnect (e.g., paused live stream in OBS).
        // When audio goes from active to inactive, restart the pipeline so it
        // can reconnect when audio resumes.
        //
        // Exception: a finite replay closes its TCP stream while the provider
        // is still draining the last sentence. Aborting here cut the 10 s drain
        // down to at most one ticker interval, which is how the final subtitle
        // of a replay used to be lost. `restart` reacts to the live provider by
        // postponing instead of tearing the session down.
        {
            let stalled = {
                let status = state.status.read();
                audio_is_stalled(status.audio_active, status.last_input_at, chrono::Utc::now())
            };
            if stalled {
                let mut status = state.status.write();
                status.audio_active = false;
                status.last_error = Some("音频输入已停止（检查 OBS 音源或所选设备），正在等待恢复…".into());
            }
        }
        let audio_active = state.status.read().audio_active;
        if last_audio_active && !audio_active {
            info!("audio stream disconnected, restarting pipeline to prepare for reconnection");
            handle.restart(Duration::from_secs(2)).await;
            tokio::time::sleep(Duration::from_millis(250)).await;
            return;
        }
        last_audio_active = audio_active;

        // Cloud session died while the loop is still alive. Without this the
        // pipeline stayed "running" with a dead socket and never produced
        // another subtitle, because the run loop only wakes on `try_start`
        // failure or an audio drop. A session that has not connected yet is
        // left to the connect grace period (`saw_llm_connected` is still false).
        let llm_connected = state.status.read().llm_connected;
        if llm_connected {
            saw_llm_connected = true;
            // A session that reached the provider is "healthy" as far as the
            // retry budget is concerned, so the next failure starts short again.
            state
                .provider_failures
                .store(0, std::sync::atomic::Ordering::Relaxed);
        } else if saw_llm_connected && !handle.provider_task_running() {
            let attempts = state
                .provider_failures
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let delay = provider_restart_backoff(attempts);
            warn!(
                attempt = attempts + 1,
                delay_s = delay.as_secs(),
                "provider session ended while the pipeline was running; restarting"
            );
            state.status.write().last_error =
                Some("识别连接已断开，正在自动重连…".into());
            handle.restart(delay).await;
            return;
        }
    }
}

async fn try_start(state: &Arc<AppState>, handle: &Arc<PipelineHandle>) -> Result<()> {
    let cfg = state.config.read().clone();
    // The mock provider needs no credentials; require a key for real ones.
    if cfg.llm.api_key.is_empty() && cfg.llm.provider != "mock" {
        anyhow::bail!("API key not set; configure it in the admin panel first");
    }

    let (raw_tx, mut raw_rx) = mpsc::channel::<Vec<i16>>(PCM_CHANNEL_CAPACITY);
    let (speech_tx, speech_rx) = mpsc::channel::<Vec<i16>>(PCM_CHANNEL_CAPACITY);

    let use_obs_filter = cfg.audio.mode == "obs_filter";
    let mut capturer_slot: Option<AudioCapturer> = None;
    let mut ingest_guard: Option<crate::ingest::Registration> = None;
    if use_obs_filter {
        // Audio arrives over the local ingest TCP port from the OBS plugin
        // filter; no cpal capture needed.
        ingest_guard = Some(crate::ingest::register(raw_tx.clone()));
        info!(
            port = cfg.audio.ingest_port,
            "audio input: OBS filter ingest (waiting for the plugin to stream audio)"
        );
    } else {
        let capturer = AudioCapturer::new(cfg.audio.clone());
        capturer
            .start(raw_tx)
            .map_err(|e| anyhow::anyhow!("audio start failed: {e}"))?;
        capturer_slot = Some(capturer);
    }
    {
        // `audio_active` is deliberately NOT set here. Starting a capturer (or
        // registering the ingest sender) only means we are *willing* to accept
        // audio; a cloud session opened on that basis begins transcribing an
        // empty stream and is torn down by the server after ~23 s ("request
        // timeout"), which then looked like a mysterious provider failure.
        // The VAD task flips this to true on the first real frame instead, and
        // `watch()` flips it back when frames stop arriving.
        let mut s = state.status.write();
        s.last_error = None;
    }
    let sink = state.subtitle.sink();

    // Signals that real (non-silent) audio has arrived. The cloud session waits
    // on this so it is never opened against a silent input — the OBS plugin and
    // WASAPI both keep delivering digital-silence frames, so counting frames
    // alone would open a session on a silent source.
    let (first_frame_tx, first_frame_rx) = tokio::sync::oneshot::channel::<()>();
    let mut first_frame_tx = Some(first_frame_tx);

    // VAD filter task.
    let vad_state = state.clone();
    let vad_cfg = cfg.filter.clone();
    let audio_task = tokio::spawn(async move {
        let mut vad = Vad::new(vad_cfg);
        let mut last_kind = SegmentKind::Silence;
        let mut silence_debounce_ms: u32 = 0;
        loop {
            let frame = match raw_rx.recv().await {
                Some(f) => f,
                None => {
                    info!("audio channel closed, marking audio inactive");
                    vad_state.status.write().audio_active = false;
                    return;
                }
            };
            let spec_rate = vad_state
                .config
                .read()
                .audio
                .sample_rate
                .max(16_000);
            let decision = vad.decide(&frame, spec_rate);
            // First frame loud enough to mean the source is really playing:
            // let the provider connect now.
            if frame_opens_audio_gate(decision.rms) {
                if let Some(tx) = first_frame_tx.take() {
                    let _ = tx.send(());
                }
            }
            // Expose the selected capture source before VAD filtering for
            // microphone selection and quiet-speech calibration. The first real
            // frame is what marks the input as live (see `try_start`).
            {
                let mut status = vad_state.status.write();
                status.audio_active = true;
                status.last_input_at = Some(chrono::Utc::now());
                status.input_level = decision.rms;
                status.input_frames = status.input_frames.saturating_add(1);
                status.input_rms_sum += decision.rms as f64;
                status.input_peak = status.input_peak.max(decision.rms);
            }
            match decision.kind {
                SegmentKind::Speech => {
                    let _ = speech_tx.try_send(frame.clone());
                    last_kind = SegmentKind::Speech;
                }
                SegmentKind::Music => {
                    last_kind = SegmentKind::Music;
                }
                SegmentKind::Silence => {
                    let _ = speech_tx.try_send(frame.clone());
                    if matches!(last_kind, SegmentKind::Speech) {
                        let frame_ms = (frame.len() as u64 * 1000
                            / spec_rate as u64) as u32;
                        silence_debounce_ms = silence_debounce_ms.saturating_add(frame_ms);
                        if silence_debounce_ms > 350 {
                            last_kind = SegmentKind::Silence;
                            silence_debounce_ms = 0;
                        }
                    } else {
                        last_kind = SegmentKind::Silence;
                    }
                }
            }
        }
    });

    // Gate the cloud session on real audio. Opening it earlier is what produced
    // the bare "request timeout after 23 seconds" failures: the engine connected
    // and sent `run-task`, then had nothing to send, and the server dropped the
    // idle task. A silent source now simply stays in the retry loop instead.
    {
        state.status.write().last_error =
            Some("等待音频输入（OBS 音源或所选设备）…".into());
        match tokio::time::timeout(AUDIO_FIRST_FRAME_GRACE, first_frame_rx).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                // Sender dropped: the audio task exited before any frame.
                audio_task.abort();
                anyhow::bail!("音频输入在收到任何音频前就结束了");
            }
            Err(_) => {
                audio_task.abort();
                anyhow::bail!(
                    "等待音频输入超时（{} 秒内没有收到任何音频帧）；未创建云端会话",
                    AUDIO_FIRST_FRAME_GRACE.as_secs()
                );
            }
        }
    }

    // LLM task. Owns `speech_rx`.
    let llm_state = state.clone();
    let llm_task = tokio::spawn(async move {
        let provider = match crate::llm::build(&llm_state.config.read().llm) {
            Ok(p) => p,
            Err(e) => {
                let mut s = llm_state.status.write();
                s.last_error = Some(format!("{e:#}"));
                s.llm_connected = false;
                return;
            }
        };
        // 热词（R10）：会话开始时的上下文用当前词表快照构造；运行中的更新
        // 由 provider 订阅同一个 feed 后以 continue-task 下发。
        let plan = llm_state.hotwords.plan();
        let context_rounds = plan.rounds.clone();
        provider.set_hotwords(llm_state.hotwords.clone());
        provider.set_hotword_status(llm_state.hotword_status.clone());
        {
            let mut s = llm_state.status.write();
            s.llm_connected = true;
        }
        match provider.run(speech_rx, sink.clone(), context_rounds).await {
            Ok(_) => {
                let mut s = llm_state.status.write();
                s.llm_connected = false;
            }
            Err(e) => {
                // Full cause chain, not just the outer context — see the note
                // on the `pipeline failed` log above.
                warn!(error = %format!("{e:#}"), "LLM session ended");
                let mut s = llm_state.status.write();
                s.llm_connected = false;
                s.last_error = Some(format!("{e:#}"));
            }
        }
    });

    *handle.inner.lock() = Some(PipelineInner {
        _capturer: capturer_slot,
        _ingest_guard: ingest_guard,
        _llm_task: llm_task,
        _vad_task: tokio::spawn(async move {}), // legacy placeholder
        _audio_task: audio_task,
    });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dead_provider_session_backs_off_exponentially_and_caps() {
        // A session that never got anywhere retries quickly once, then backs off
        // further; the cap stops an unreachable endpoint from being hammered
        // forever at a fixed short interval.
        let delays: Vec<u64> = (0..7).map(|n| provider_restart_backoff(n).as_secs()).collect();
        assert_eq!(delays, vec![2, 10, 20, 40, 80, 80, 80], "unexpected backoff ladder");
        assert!(
            delays.windows(2).all(|w| w[0] <= w[1]),
            "backoff must not shrink as attempts accumulate: {delays:?}"
        );
    }

    #[test]
    fn the_drain_window_covers_the_provider_reader_timeout() {
        // The Bailian reader keeps draining for 10s after `finish-task`; the
        // restart window must not be shorter or the final sentence is aborted.
        assert!(
            DRAIN_GRACE >= Duration::from_secs(10),
            "DRAIN_GRACE ({DRAIN_GRACE:?}) must cover the provider's 10s drain"
        );
    }

    #[test]
    fn digital_silence_does_not_open_the_audio_gate() {
        // Both the OBS plugin (gate_silence defaults to false) and WASAPI keep
        // sending silent frames, so the gate must look at energy.
        assert!(!frame_opens_audio_gate(0.0), "pure silence must not open the gate");
        assert!(!frame_opens_audio_gate(AUDIO_GATE_RMS_FLOOR));
        assert!(frame_opens_audio_gate(AUDIO_GATE_RMS_FLOOR * 2.0));
        assert!(frame_opens_audio_gate(0.02), "speech must open the gate");
    }

    #[test]
    fn the_restart_gate_is_open_when_unset_or_elapsed() {
        let now = tokio::time::Instant::now();
        assert!(gate_wait(None, now).is_none());
        assert!(gate_wait(Some(now), now).is_none(), "a zero wait must not block");
        assert!(
            gate_wait(Some(now - Duration::from_millis(1)), now).is_none(),
            "an elapsed deadline must not block"
        );
    }

    #[test]
    fn the_restart_gate_reports_the_remaining_wait() {
        let now = tokio::time::Instant::now();
        let wait = gate_wait(Some(now + Duration::from_millis(750)), now)
            .expect("a future deadline must block");
        assert!(wait <= Duration::from_millis(750));
        assert!(wait > Duration::from_millis(700), "wait was {wait:?}");
    }

    #[test]
    fn a_finished_provider_task_is_not_treated_as_alive() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("build runtime");
        let handle = rt.block_on(async {
            let task = tokio::spawn(async {});
            // Let the spawned task actually complete before we look at it.
            tokio::time::sleep(Duration::from_millis(10)).await;
            task
        });
        assert!(handle.is_finished(), "a completed task must report finished");
    }

    #[test]
    fn a_pipeline_with_no_inner_state_has_no_live_provider() {
        assert!(!provider_task_alive(None));
    }

    /// The admin stop button must hold the run loop off, and `/api/restart`
    /// must be able to lift that hold again — a one-way stop would make the
    /// restart endpoint a no-op.
    #[test]
    fn pausing_holds_the_run_loop_and_resuming_releases_it() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime");
        rt.block_on(async {
            let handle = PipelineHandle::new();
            assert!(handle.is_enabled(), "a new pipeline starts enabled");
            handle.pause().await;
            assert!(!handle.is_enabled(), "pause must close the run gate");
            handle.resume().await;
            assert!(handle.is_enabled(), "resume must reopen the run gate");
        });
    }

    #[test]
    fn an_input_that_stops_sending_frames_is_not_reported_as_active() {
        let now = chrono::Utc::now();
        assert!(!audio_is_stalled(false, None, now));
        assert!(
            !audio_is_stalled(true, Some(now - chrono::Duration::milliseconds(500)), now),
            "a recent frame means the input is live"
        );
        assert!(
            audio_is_stalled(true, Some(now - chrono::Duration::milliseconds(2_100)), now),
            "2 s without a frame means the input is gone"
        );
    }

    #[test]
    fn an_active_input_without_a_timestamp_is_left_alone() {
        // Defensive: `audio_active` is only ever set together with a timestamp,
        // so this combination must not trigger a spurious restart.
        assert!(!audio_is_stalled(true, None, chrono::Utc::now()));
    }
}
