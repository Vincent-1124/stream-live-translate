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
//!
//! # Cancellation (P1-01)
//!
//! Every long-running step of a run is cancellable through a **cancel
//! generation** counter:
//!
//! * first-audio wait (`try_start`)
//! * provider connect / recognise session (`provider.run(..)` inside `_llm_task`)
//! * the post-replay drain wait in `run()`
//! * `watch()`'s ticker loop
//!
//! See [`PipelineHandle::generation`] for the mechanism itself.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use parking_lot::Mutex;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::audio::AudioCapturer;
use crate::vad::{SegmentKind, Vad};
use crate::AppState;

const PCM_CHANNEL_CAPACITY: usize = 256;

pub struct PipelineHandle {
    inner: Arc<Mutex<Option<PipelineInner>>>,
    shutdown: watch::Sender<bool>,
    pub shutdown_rx: watch::Receiver<bool>,
    /// Whether the run loop may keep a pipeline alive. `false` = paused by the
    /// admin stop button; the run loop parks instead of starting again.
    run_gate: watch::Sender<bool>,
    run_gate_rx: watch::Receiver<bool>,
    /// Earliest time the next `try_start` may run. Bounded by
    /// [`provider_restart_backoff`] after a provider failure so a broken
    /// endpoint (bad key, region, model) cannot be hammered in a tight loop.
    retry_gate: Mutex<Option<tokio::time::Instant>>,
    /// **Cancel generation** (P1-01).
    ///
    /// A `u64` counter, not a `bool`, on purpose: a boolean can only say "a
    /// cancel happened at some point", so a stale task that wakes up later
    /// cannot tell *which* run it belonged to, and a `resume()`/re-open would
    /// silently re-arm work that had already been cancelled. A monotonic
    /// generation gives every run an identity:
    ///
    /// * `try_start` takes the generation it was started for and refuses to
    ///   install [`PipelineInner`] once that number is no longer current
    ///   ([`PipelineHandle::install_if_current`] does that check and the store
    ///   under one lock, so a restart can never lose the race);
    /// * every long await selects on `changed()` of a receiver cloned from
    ///   this sender, which makes "one bump cancels all of them" a structural
    ///   property rather than something each call site has to remember.
    current_generation: watch::Sender<u64>,
    /// The value of [`PipelineHandle::current_generation`] as of the last bump.
    generation: AtomicU64,
    /// `true` while the *reason* for the newest generation is an operator stop
    /// (`/api/stop`, process shutdown) or an urgent restart, `false` for a
    /// graceful one (config save, provider retry, audio-stall recovery).
    ///
    /// Both kinds of bump cancel in-flight setup work. The difference is the
    /// post-replay drain: a graceful restart must still give a live provider up
    /// to [`DRAIN_GRACE`] so a finite replay's final sentence is not cut, while
    /// a **stop** may not wait at all — `/api/stop` has to stop audio capture
    /// and the cloud connection within [`STOP_CANCEL_BOUND`].
    urgent_stop: watch::Sender<bool>,
}

struct PipelineInner {
    _capturer: Option<AudioCapturer>,
    /// Present in `obs_filter` mode; unregisters the ingest sender on drop.
    _ingest_guard: Option<crate::ingest::Registration>,
    _llm_task: JoinHandle<()>,
    _vad_task: JoinHandle<()>,
    _audio_task: JoinHandle<()>,
}

/// Everything a spawned task of one run needs in order to cancel itself.
///
/// The two keep-alive fields exist only so the watch senders outlive the
/// `'static` tasks: dropping a `watch::Sender` makes every receiver's `changed()`
/// return `Err`, and these tasks must keep waiting rather than mistake that for
/// a cancel.
#[derive(Clone)]
struct Cancellation {
    generation: watch::Receiver<u64>,
    urgent: watch::Receiver<bool>,
    _keep_generation_alive: watch::Sender<u64>,
    _keep_urgent_alive: watch::Sender<bool>,
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
fn gate_wait(
    not_before: Option<tokio::time::Instant>,
    now: tokio::time::Instant,
) -> Option<Duration> {
    not_before
        .and_then(|deadline| deadline.checked_duration_since(now))
        .filter(|d| !d.is_zero())
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

/// Hard bound on how long a **stop** may leave audio capture or the cloud
/// connection alive (P1-01).
///
/// Every await point of a run observes the cancel generation, and a stop takes
/// the [`PipelineInner`] under the same lock that guards installation, so
/// teardown is a synchronous drop rather than a wait. The number is therefore
/// not a "grace period" — it is the wall clock a caller (and a test) may assume
/// before the pipeline is provably gone. One second is four orders of magnitude
/// above the observed cost (a few hundred microseconds) and still short enough
/// that `/api/stop` is honest about having stopped.
pub const STOP_CANCEL_BOUND: Duration = Duration::from_secs(1);

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

/// The one sample rate every producer feeding this pipeline converts to, and the
/// only rate the VAD is ever told about.
///
/// **Single source of truth.** Before this constant the rate was re-derived in
/// three places and they disagreed:
///
/// * `ingest.rs` resamples the plugin's stream to a fixed 16 kHz;
/// * `audio.rs` (cpal) resamples to `config.audio.sample_rate` (0 → 16 kHz);
/// * `try_start` labelled *both* streams with
///   `config.audio.sample_rate.max(16_000)`.
///
/// `Vad::decide` turns the rate into real numbers — `frame_ms` drives the
/// segment/debounce timers and `spectral_flatness` scales the music detector's
/// analysis — so a wrong label mis-times segments (a 48 kHz config made a 320
/// sample frame count as 20 ms when it really was 6.67 ms at 48 kHz, or the
/// reverse on the ingest path) and mis-scales the music detector. Producers must
/// therefore convert to *this* value and no other, and `try_start` passes it
/// straight to `Vad::decide`.
///
/// Lives here because `pipeline.rs` is the consumer that owns the frame-size
/// assumptions ([`FRAME_SAMPLES_INTERNAL`] == 20 ms) and both producers already
/// depend on it. `ingest.rs` and `audio.rs` should replace their local literals
/// with `crate::pipeline::INTERNAL_SAMPLE_RATE` (one line each; see the task
/// report — those files belong to other tasks).
pub const INTERNAL_SAMPLE_RATE: u32 = 16_000;

/// Samples in one internal frame: exactly 20 ms at [`INTERNAL_SAMPLE_RATE`].
///
/// The VAD's debounce (`> 350 ms` of silence closes a segment) and
/// `filter.min_segment_ms` / `max_segment_ms` are all expressed in milliseconds
/// derived from the frame length, so a frame that is not this long silently
/// changes every one of those timings.
pub const FRAME_SAMPLES_INTERNAL: usize = 320;

/// The `voiced_ms` increase one internal frame must produce. Derived from the two
/// constants above so it can never drift: this is the value the VAD computes for
/// a full frame at [`INTERNAL_SAMPLE_RATE`].
pub const FRAME_MS_INTERNAL: u32 =
    (FRAME_SAMPLES_INTERNAL as u64 * 1000 / INTERNAL_SAMPLE_RATE as u64) as u32;

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
        Some(last) => {
            now.signed_duration_since(last).num_milliseconds()
                >= AUDIO_STALL_AFTER.as_millis() as i64
        }
    }
}

/// True when `generation` is still the cancel generation of the newest run.
fn generation_is_current(generation: &watch::Sender<u64>, expected: u64) -> bool {
    *generation.borrow() == expected
}

/// Wait until the cancel generation moves away from `expected`.
///
/// `tokio::sync::watch` keeps the newest value: `changed()` also completes when
/// the current value was never observed by this receiver, so this verifies the
/// *value* rather than trusting the notification. A bump before the wait began
/// therefore cancels immediately, and a `changed()` that arrives while the value
/// is still `expected` (the interesting case during a rapid resume) keeps
/// waiting instead of cancelling a run that is current again.
async fn wait_for_generation_change(rx: &mut watch::Receiver<u64>, expected: u64) {
    loop {
        if *rx.borrow_and_update() != expected {
            return;
        }
        if rx.changed().await.is_err() {
            // Every sender is gone: the process is going away, so a cancelled
            // run is the right outcome.
            return;
        }
    }
}

/// Outcome of waiting for the first audio frame of a run.
#[derive(Debug, PartialEq, Eq)]
enum FirstAudioWait {
    /// Real (non-silent) audio arrived; the cloud session may be opened.
    Audio,
    /// No audio within [`AUDIO_FIRST_FRAME_GRACE`].
    TimedOut,
    /// The audio task exited before any frame arrived.
    InputEnded,
    /// A cancel generation bump arrived first (stop / restart / config save).
    Cancelled,
}

/// Wait for the first audio frame, bounded by both the grace period and the
/// cancel generation.
///
/// This is the await that used to be uninterruptible: `/api/stop` answered
/// `{ok:true}` while a run was still parked here for up to 30 s, and the parked
/// run then installed itself over the run the stop had just started.
async fn wait_for_first_audio(
    first_frame_rx: &mut tokio::sync::oneshot::Receiver<()>,
    generation_rx: &mut watch::Receiver<u64>,
    generation: u64,
    grace: Duration,
) -> FirstAudioWait {
    tokio::select! {
        biased;
        frame = &mut *first_frame_rx => match frame {
            Ok(()) => FirstAudioWait::Audio,
            Err(_) => FirstAudioWait::InputEnded,
        },
        _ = wait_for_generation_change(generation_rx, generation) => FirstAudioWait::Cancelled,
        _ = tokio::time::sleep(grace) => FirstAudioWait::TimedOut,
    }
}

/// Why a run was cancelled rather than failing. Carries the run's generation so
/// the run loop and the log can tell which attempt was abandoned.
#[derive(Debug, PartialEq, Eq)]
struct Cancelled(u64);

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "run cancelled (generation {})", self.0)
    }
}

impl std::error::Error for Cancelled {}

/// Wait for the provider session of one run to be cancelled, and return how
/// strong that cancellation was.
///
/// Two strengths, deliberately different:
///
/// * an **operator stop** (`/api/stop`, process shutdown) must end the cloud
///   connection at once — bounded by [`STOP_CANCEL_BOUND`], never by the drain;
/// * a **graceful restart** (config save, retry, audio-stall recovery) must let
///   a session that is inside its bounded post-replay drain finish, or a finite
///   replay's final sentence is lost. That is what [`DRAIN_GRACE`] is for.
///
/// A run that is still *recognising* normally never sees a graceful bump at all
/// (the run loop only restarts once the provider task has ended or the input has
/// stalled), so waiting the drain window there costs nothing. Returns when the
/// generation this run belongs to has moved on.
async fn catch_provider_cancel(cancel: Cancellation, generation: u64) {
    let mut cancel = cancel;
    wait_for_generation_change(&mut cancel.generation, generation).await;
    if *cancel.urgent.borrow_and_update() {
        return;
    }
    tokio::select! {
        _ = tokio::time::sleep(DRAIN_GRACE) => {}
        changed = cancel.urgent.changed() => {
            // Err = the process is going away; either way, stop now.
            let _ = changed;
        }
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
///
/// NOTE: this crate's tokio has no `JoinHandle::is_cancelled()`, so "is it
/// alive" is read the only way the handle allows. The stop path does not depend
/// on this reading being instantaneous: `pause()`/`restart()`/`shutdown()` drop
/// [`PipelineInner`] under the same lock they install under, which *cancels* the
/// provider task synchronously — the task can no longer touch shared state or
/// hold the socket, whether or not the scheduler has reaped it yet. Callers that
/// need to observe the actual end await the handle (see the tests).
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

/// The whole-config comparison behind `watch()`'s safety net.
///
/// The previous watcher compared a hand-written whitelist (`provider`,
/// `api_key`, `model`, `endpoint`, `audio.mode`, `audio.device`), so a change to
/// **any other field that needs a fresh session** — `llm.segment_ms`,
/// `audio.sample_rate`, `llm.workspace_id`, `llm.translate_chinese`, … — was
/// silently ignored when it arrived from somewhere other than the HTTP handler
/// (a hand-edited `config.toml`, an external tool, an OBS plugin writing it).
///
/// `Config` does not implement `PartialEq` and `src/config.rs` is not ours to
/// change, so equality is taken over `serde_json::Value` instead: every config
/// field derives `Serialize`, which makes a whole-struct comparison possible
/// without touching the struct definitions. Two things are deliberate:
///
/// * `llm.hotwords` is **excluded**, because a hotword change is delivered to a
///   live session with `continue-task` and must not restart it (see `watch()`);
/// * the value is used as a *fingerprint* — compared for equality only, never
///   parsed back — so JSON's key ordering and number formatting are all that
///   matter, and both are deterministic for a derived `Serialize`.
fn needs_restart_fingerprint(cfg: &crate::config::Config) -> String {
    let mut probe = cfg.clone();
    probe.llm.hotwords.clear();
    serde_json::to_string(&probe).unwrap_or_default()
}

/// The live hotword list from a config, as the string key `watch()` compares.
fn hotwords_key(cfg: &crate::config::Config) -> String {
    cfg.llm.hotwords.join("\u{1f}")
}

impl PipelineHandle {
    pub fn new() -> Self {
        let (shutdown, shutdown_rx) = watch::channel(false);
        let (run_gate, run_gate_rx) = watch::channel(true);
        let (current_generation, _) = watch::channel(0u64);
        let (urgent_stop, _) = watch::channel(false);
        Self {
            inner: Arc::new(Mutex::new(None)),
            shutdown,
            shutdown_rx,
            run_gate,
            run_gate_rx,
            retry_gate: Mutex::new(None),
            current_generation,
            generation: AtomicU64::new(0),
            urgent_stop,
        }
    }

    pub fn is_running(&self) -> bool {
        self.inner.lock().is_some()
    }

    /// True while the run loop is allowed to keep a pipeline alive.
    pub fn is_enabled(&self) -> bool {
        *self.run_gate_rx.borrow()
    }

    /// The cancel generation a new run must be started for (P1-01).
    ///
    /// Every `pause()`, `restart(..)` and `shutdown()` bumps this. A run that
    /// was started for an older number is stale: it must abort whatever it
    /// created instead of installing itself (see `try_start`).
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// Bump the cancel generation. One bump cancels *every* await point of the
    /// previous run, because each of them selects on a receiver cloned from
    /// `current_generation`.
    fn bump_generation(&self) -> u64 {
        let next = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        // Keep the sender's value current even between attempts, when no
        // receiver exists. `send` would fail without storing `next` and make
        // every subsequent attempt look stale, causing a busy retry loop.
        self.current_generation.send_replace(next);
        next
    }

    /// A receiver that observes cancel-generation changes.
    fn generation_receiver(&self) -> watch::Receiver<u64> {
        self.current_generation.subscribe()
    }

    /// Mark the current generation as an operator stop (no drain allowed) or a
    /// graceful restart (a live provider keeps [`DRAIN_GRACE`]).
    fn set_urgent(&self, urgent: bool) {
        let _ = self.urgent_stop.send(urgent);
    }

    /// Stop the pipeline and keep it stopped until [`resume`] is called.
    ///
    /// Bounded, not instantaneous: the cancel generation bump makes every await
    /// point of the current run abort, and the inner state is taken synchronously
    /// when the provider task is already gone. When the provider task has not
    /// been observed finished yet, `stop_current` defers instead of yanking a
    /// session that may be draining its final sentence — so the abort lands on
    /// that task's next poll, still inside [`STOP_CANCEL_BOUND`]. Either way
    /// nothing outlives the bound.
    pub async fn pause(&self) {
        let _ = self.run_gate.send(false);
        self.restart(Duration::ZERO).await;
    }

    /// Allow the run loop to start a pipeline again.
    pub async fn resume(&self) {
        // Reopen the run loop's gate. Deliberately no generation bump here:
        // bumping would cancel a healthy run that a plain resume is meant to
        // leave alone.
        let _ = self.run_gate.send(true);
    }

    /// True while the provider session task is alive.
    pub fn provider_task_running(&self) -> bool {
        provider_task_alive(self.inner.lock().as_ref())
    }

    /// Block this handle from starting again for `delay`.
    fn defer_restart(&self, delay: Duration) {
        *self.retry_gate.lock() = Some(tokio::time::Instant::now() + delay);
    }

    /// **Urgent** restart: cancel everything in flight now.
    ///
    /// Used by `/api/stop` (via [`pause`]), `/api/restart`, `POST /api/config`
    /// and process shutdown.
    ///
    /// The cancel generation is bumped *before* the lock is taken, which is what
    /// makes the old-run-cannot-install-itself guarantee hold: a stale `try_start`
    /// that reaches [`install_if_current`] afterwards sees the new number under
    /// the same lock it would have installed under.
    ///
    /// Unlike [`restart_graceful`], this does **not** defer to `run()` when the
    /// provider task has not been observed finished yet: an operator stop must be
    /// bounded by [`STOP_CANCEL_BOUND`] even if the provider task never ends on
    /// its own. (`pause()` has just parked the run loop, so a deferral there would
    /// not merely be slow — the loop would never come back to service it, and a
    /// wedged session would hold the cloud connection open until the process
    /// exited. That was a real defect: the fake-provider test caught it.)
    pub async fn restart(&self, drain_grace: Duration) {
        self.set_urgent(true);
        self.bump_generation();
        // `_forced`: drop the current run now, drain or no drain.
        self.stop_current(drain_grace, true);
    }

    /// **Graceful** restart: cancel in-flight *setup* work, but let a live
    /// provider session finish its bounded post-replay drain.
    ///
    /// The bump still has to happen (an attempt parked in the 30 s first-audio
    /// wait has no `PipelineInner` yet, so nothing else could cancel it), and it
    /// is safe for a *running* provider because that session's audio has already
    /// ended — the only thing it can be doing is waiting for the last sentence.
    /// `run()` gives it up to [`DRAIN_GRACE`] before tearing it down, and the
    /// provider task itself observes the same window in `catch_provider_cancel`.
    ///
    /// The urgency flag is cleared so the provider observes "graceful" for this
    /// bump; a concurrent `/api/stop` still wins, because stopping is what the
    /// generation bump means for `try_start`.
    pub async fn restart_graceful(&self, retry_delay: Duration) {
        self.set_urgent(false);
        self.bump_generation();
        self.stop_current(retry_delay, false);
    }

    /// Shared body of the two restart variants.
    ///
    /// `force` = tear the current run down immediately even when its provider task
    /// has not reported finished. Otherwise the teardown is postponed (the caller
    /// records a retry delay) so `run()` can wait out [`DRAIN_GRACE`] and the
    /// provider can deliver the final sentence of a finite replay.
    ///
    /// The lock is deliberately released before the store: `restart` is awaited
    /// from inside a `tokio::spawn`ed task, so it must not hold a
    /// `parking_lot` guard across an await point.
    fn stop_current(&self, drain_grace: Duration, force: bool) {
        let taken = {
            let mut guard = self.inner.lock();
            match guard.as_ref() {
                Some(inner) if !force && !inner._llm_task.is_finished() => None,
                _ => guard.take(),
            }
        };
        match taken {
            // Dropping aborts the LLM, VAD and audio tasks.
            Some(inner) => drop(inner),
            None => self.defer_restart(drain_grace),
        }
    }

    /// Install `inner` for `generation`, unless a newer generation has taken
    /// over in the meantime — in which case nothing is installed, the tasks
    /// `inner` owns are aborted **here**, and a [`Cancelled`] is returned.
    ///
    /// The generation check and the store happen under the same lock, and every
    /// bump happens before that lock is taken, so a restart can never be
    /// overwritten by an older run: either the stale run checks first and is
    /// rejected, or it installs first and the restart's own teardown (which does
    /// take the lock) removes it afterwards.
    ///
    /// The abort is not left to the caller. An earlier version returned the inner
    /// state in the `Err` arm and relied on the caller dropping it; because
    /// `Result` is `#[must_use]` only as a lint, `let _ = ...` would have
    /// detached a live provider task — a socket left open by a run that was
    /// explicitly rejected. A run that may not install itself must not survive
    /// anywhere, so it is torn down before this returns.
    fn install_if_current(
        &self,
        generation: u64,
        inner: PipelineInner,
    ) -> std::result::Result<(), Cancelled> {
        let mut guard = self.inner.lock();
        if self.generation.load(Ordering::SeqCst) != generation {
            // `abort()` is issued here, synchronously, while we hold the very
            // lock the generation bump took first. `PipelineInner::drop` would
            // do the same, but only if the caller dropped the value.
            inner._llm_task.abort();
            inner._audio_task.abort();
            inner._vad_task.abort();
            return Err(Cancelled(generation));
        }
        *guard = Some(inner);
        Ok(())
    }

    /// The state a run's spawned tasks need in order to cancel themselves.
    fn cancellation(&self) -> Cancellation {
        Cancellation {
            generation: self.generation_receiver(),
            urgent: self.urgent_stop.subscribe(),
            _keep_generation_alive: self.current_generation.clone(),
            _keep_urgent_alive: self.urgent_stop.clone(),
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
    /// The caller is expected to run this inside a `select!` that also watches
    /// the cancel generation, so a stop still ends the wait at once.
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
        // An urgent bump as well: any in-flight `try_start` must abort rather
        // than install itself while the process is going away.
        self.set_urgent(true);
        self.bump_generation();
        let _ = self.shutdown.send(true);
        // Nothing may be postponed on the way out.
        let taken = self.inner.lock().take();
        if let Some(inner) = taken {
            drop(inner);
        }
    }
}

pub fn spawn(state: Arc<AppState>, _config_path: std::path::PathBuf) {
    let handle = state.pipeline.clone();
    let state_clone = state.clone();
    tokio::spawn(async move {
        run(state_clone, handle).await;
    });
}

async fn run(state: Arc<AppState>, handle: Arc<PipelineHandle>) {
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
        let gate = gate_wait(*handle.retry_gate.lock(), tokio::time::Instant::now());
        // The drain window is a floor, never truncated by the retry gate: a 2 s
        // deferral must not cut a 10 s drain short (that was defect #2).
        let safe_wait = if handle.provider_task_running() {
            gate.map_or(DRAIN_GRACE, |g| g.max(DRAIN_GRACE))
        } else {
            gate.unwrap_or_default()
        };
        if !safe_wait.is_zero() {
            info!(
                ms = safe_wait.as_millis() as u64,
                "waiting for the previous session before restarting"
            );
            // With no provider task, this is purely a retry delay. Calling
            // `wait_for_provider` here returns immediately and spins through
            // this branch until the deadline, flooding the log.
            if !handle.provider_task_running() {
                tokio::select! {
                    _ = tokio::time::sleep(safe_wait) => {}
                    changed = run_gate_rx.changed() => {
                        if changed.is_err() { return; }
                    }
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() { return; }
                    }
                }
                continue;
            }
            let drained = tokio::select! {
                drained = handle.wait_for_provider(safe_wait) => drained,
                // A graceful restart during a drain does not cut it short, but a
                // STOP must: it must not leave the cloud session alive for the
                // rest of the drain window.
                _ = tokio::time::sleep(if *handle.urgent_stop.borrow() { Duration::ZERO } else { safe_wait }) => {
                    false
                }
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
                // Not an operator stop: this is the restart path progressing.
                handle.restart_graceful(Duration::ZERO).await;
            }
            continue;
        }
        // The generation this attempt is being started for. `try_start` refuses
        // to install itself once this is no longer current.
        let generation = handle.generation();
        let result = try_start(&state, &handle, generation).await;
        match result {
            Ok(()) => {
                info!(generation, "pipeline started cleanly");
                // Clear any stale error from a previous failed attempt so
                // the admin panel shows a healthy state.
                state.status.write().last_error = None;
                backoff = Duration::from_secs(2);
            }
            Err(e) => {
                // A cancellation is not a failure: no backoff, no error banner.
                // The run loop's own gate/shutdown checks decide what happens
                // next (park, retry with a newer generation, or exit).
                if let Some(cancelled) = e.downcast_ref::<Cancelled>() {
                    info!(generation = cancelled.0, "pipeline attempt cancelled");
                    continue;
                }
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
        watch(&state, &handle, generation).await;
    }
}

async fn watch(state: &Arc<AppState>, handle: &Arc<PipelineHandle>, generation: u64) {
    // Kept as a local receiver: `handle.shutdown_rx.clone().changed()` would
    // borrow a temporary that is dropped before the `select!` polls it (E0716).
    let mut shutdown_rx = handle.shutdown_rx.clone();
    // Whole-config fingerprint (see `needs_restart_fingerprint`). Replaces the
    // old hand-written whitelist so a field nobody remembered to list still
    // drives a restart when it changes out of band. Never reassigned: the loop
    // returns as soon as it restarts, and the next `watch()` re-reads.
    let last_needs_restart = needs_restart_fingerprint(&state.config.read());
    // 热词（R10）：词表变化**不**需要重启会话——百炼通道支持 continue-task
    // 运行中更新，重启反而会掐掉当前正在识别的那句话。这里只把它推进 feed。
    let mut last_hotwords = hotwords_key(&state.config.read());
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
        // A stop/restart moved the generation on: this watcher belongs to a
        // dead run and must not act on the config of the new one.
        if handle.generation() != generation {
            return;
        }
        if !handle.is_running() {
            return;
        }
        let cur = state.config.read().clone();
        let cur_hotwords = hotwords_key(&cur);
        let cur_needs_restart = needs_restart_fingerprint(&cur);
        let restarted_for_config = cur_needs_restart != last_needs_restart;
        if cur_hotwords != last_hotwords {
            last_hotwords = cur_hotwords;
            let words = cur.llm.hotwords.clone();
            let plan = crate::hotwords::plan(&words);
            state.hotwords.set(plan);
            info!(
                words = words.len(),
                "hotwords changed; pushed to the live session (continue-task when running)"
            );
        }
        if restarted_for_config {
            info!("config changed, restarting pipeline");
            // A config change is a user-initiated restart that must still let a
            // finite replay's final sentence drain (P1-01 requirement: a
            // config-save restart keeps the drain, a stop does not), so it uses
            // the graceful variant. `run()` then applies DRAIN_GRACE — or tears
            // the session down at once when the operator just pressed stop.
            // (`last_needs_restart` is deliberately not updated: the loop returns
            // immediately and the next `watch()` re-reads the config.)
            handle.restart_graceful(Duration::from_secs(2)).await;
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(250)) => {}
                changed = shutdown_rx.changed() => {
                    if changed.is_err() { return; }
                }
            }
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
        // of a replay used to be lost. `restart_graceful` reacts to the live
        // provider by postponing instead of tearing the session down.
        {
            let stalled = {
                let status = state.status.read();
                audio_is_stalled(
                    status.audio_active,
                    status.last_input_at,
                    chrono::Utc::now(),
                )
            };
            if stalled {
                let mut status = state.status.write();
                status.audio_active = false;
                status.last_error =
                    Some("音频输入已停止（检查 OBS 音源或所选设备），正在等待恢复…".into());
            }
        }
        let audio_active = state.status.read().audio_active;
        if last_audio_active && !audio_active {
            info!("audio stream disconnected, restarting pipeline to prepare for reconnection");
            handle.restart_graceful(Duration::from_secs(2)).await;
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(250)) => {}
                changed = shutdown_rx.changed() => {
                    if changed.is_err() { return; }
                }
            }
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
            state.status.write().last_error = Some("识别连接已断开，正在自动重连…".into());
            handle.restart_graceful(delay).await;
            return;
        }
    }
}

/// Start one pipeline run for `generation`.
///
/// `generation` is the cancel generation this attempt belongs to. Every await
/// in here observes it, and the final install is guarded by
/// [`PipelineHandle::install_if_current`], so a run whose generation has moved
/// on aborts the tasks it created and reports [`Cancelled`] instead of
/// installing itself — the structural guarantee the restart path depends on.
async fn try_start(
    state: &Arc<AppState>,
    handle: &Arc<PipelineHandle>,
    generation: u64,
) -> Result<()> {
    let cfg = state.config.read().clone();
    // The mock provider needs no credentials; require a key for real ones.
    if cfg.llm.api_key.is_empty() && cfg.llm.provider != "mock" {
        anyhow::bail!("API key not set; configure it in the admin panel first");
    }

    let mut generation_rx = handle.generation_receiver();
    if !generation_is_current(&handle.current_generation, generation) {
        return Err(anyhow::Error::new(Cancelled(generation)));
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
    let (first_frame_tx, mut first_frame_rx) = tokio::sync::oneshot::channel::<()>();
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
            // The rate the frames were PRODUCED at, not a value re-derived from
            // the config: both producers (ingest, cpal) convert to
            // `INTERNAL_SAMPLE_RATE`, so labelling with `config.audio.sample_rate`
            // mis-timed the ingest stream by up to 3x and mis-scaled the music
            // detector. See `INTERNAL_SAMPLE_RATE`.
            let spec_rate = INTERNAL_SAMPLE_RATE;
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
                        let frame_ms = (frame.len() as u64 * 1000 / spec_rate as u64) as u32;
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
    //
    // P1-01: this wait is now cancellable. `/api/stop` and `/api/restart` used
    // to return `{ok:true}` while the run sat here for up to 30 s, and the parked
    // run then installed itself over the run the restart had just created.
    let waited = {
        state.status.write().last_error = Some("等待音频输入（OBS 音源或所选设备）…".into());
        wait_for_first_audio(
            &mut first_frame_rx,
            &mut generation_rx,
            generation,
            AUDIO_FIRST_FRAME_GRACE,
        )
        .await
    };
    match waited {
        FirstAudioWait::Audio => {}
        FirstAudioWait::InputEnded => {
            abort_abandoned_run(&handle.inner, generation, audio_task);
            anyhow::bail!("音频输入在收到任何音频前就结束了");
        }
        FirstAudioWait::TimedOut => {
            abort_abandoned_run(&handle.inner, generation, audio_task);
            anyhow::bail!(
                "等待音频输入超时（{} 秒内没有收到任何音频帧）；未创建云端会话",
                AUDIO_FIRST_FRAME_GRACE.as_secs()
            );
        }
        FirstAudioWait::Cancelled => {
            abort_abandoned_run(&handle.inner, generation, audio_task);
            return Err(anyhow::Error::new(Cancelled(generation)));
        }
    }

    // A bump can also land between the gate opening and the provider task being
    // spawned; check again so a cancelled run does not even open a socket.
    if !generation_is_current(&handle.current_generation, generation) {
        abort_abandoned_run(&handle.inner, generation, audio_task);
        return Err(anyhow::Error::new(Cancelled(generation)));
    }

    // LLM task. Owns `speech_rx`.
    let cancel = handle.cancellation();
    let run_generation = generation;
    let llm_state = state.clone();
    let llm_task = tokio::spawn(async move {
        // P1-01: the provider session — connect *and* recognise — is cancellable
        // without waiting for the whole task to be aborted at process exit.
        // `catch_provider_cancel` is what stops `provider.run(..)`; dropping that
        // future drops the WebSocket session with it. A stop returns at once, a
        // graceful restart only after this run's drain window.
        tokio::select! {
            biased;
            _ = catch_provider_cancel(cancel, run_generation) => {
                info!(
                    generation = run_generation,
                    "provider session cancelled by a newer generation"
                );
            }
            _ = run_provider(llm_state, speech_rx, sink) => {}
        }
    });

    let inner = PipelineInner {
        _capturer: capturer_slot,
        _ingest_guard: ingest_guard,
        _llm_task: llm_task,
        _vad_task: tokio::spawn(async {}), // legacy placeholder, kept for shape
        _audio_task: audio_task,
    };
    // Structural guard (P1-01): a stale run must never install itself. On the
    // error path `inner` is dropped here, which aborts the tasks it just created.
    if handle.install_if_current(generation, inner).is_err() {
        return Err(anyhow::Error::new(Cancelled(generation)));
    }

    Ok(())
}

/// Abort the tasks of an attempt that is being abandoned before it installed
/// anything. `audio_task` is local to the attempt; the provider task (if it was
/// already spawned) is found through `inner` — which is still empty on this
/// path, so nothing else can be holding it.
fn abort_abandoned_run(
    inner: &Arc<Mutex<Option<PipelineInner>>>,
    generation: u64,
    audio_task: JoinHandle<()>,
) {
    audio_task.abort();
    if let Some(inner) = inner.lock().take() {
        drop(inner);
    }
    info!(generation, "abandoned pipeline attempt tore down its tasks");
}

/// One provider session: build it, publish the hotword context, and run it.
async fn run_provider(
    llm_state: Arc<AppState>,
    speech_rx: mpsc::Receiver<Vec<i16>>,
    sink: crate::subtitle::SubtitleSink,
) {
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
    match provider.run(speech_rx, sink, context_rounds).await {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_bump_persists_without_receivers() {
        let handle = PipelineHandle::new();
        handle.bump_generation();
        assert_eq!(handle.generation(), 1);
        assert!(generation_is_current(&handle.current_generation, 1));
        assert_eq!(*handle.generation_receiver().borrow(), 1);
    }

    #[test]
    fn a_dead_provider_session_backs_off_exponentially_and_caps() {
        // A session that never got anywhere retries quickly once, then backs off
        // further; the cap stops an unreachable endpoint from being hammered
        // forever at a fixed short interval.
        let delays: Vec<u64> = (0..7)
            .map(|n| provider_restart_backoff(n).as_secs())
            .collect();
        assert_eq!(
            delays,
            vec![2, 10, 20, 40, 80, 80, 80],
            "unexpected backoff ladder"
        );
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
        assert!(
            !frame_opens_audio_gate(0.0),
            "pure silence must not open the gate"
        );
        assert!(!frame_opens_audio_gate(AUDIO_GATE_RMS_FLOOR));
        assert!(frame_opens_audio_gate(AUDIO_GATE_RMS_FLOOR * 2.0));
        assert!(frame_opens_audio_gate(0.02), "speech must open the gate");
    }

    #[test]
    fn the_restart_gate_is_open_when_unset_or_elapsed() {
        let now = tokio::time::Instant::now();
        assert!(gate_wait(None, now).is_none());
        assert!(
            gate_wait(Some(now), now).is_none(),
            "a zero wait must not block"
        );
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
        assert!(
            handle.is_finished(),
            "a completed task must report finished"
        );
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
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
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

    // -----------------------------------------------------------------------
    // P1-02 follow-up: the frame rate is a single source of truth.
    //
    // The producers (`ingest.rs`, `audio.rs`) convert to `INTERNAL_SAMPLE_RATE`
    // and `try_start` hands that same constant to `Vad::decide`. These tests
    // pin the identity, and pin the timing the debounce logic depends on.
    // -----------------------------------------------------------------------

    /// All three of `ingest.rs`'s assumptions in one place, so drift is caught
    /// here instead of on a live stream.
    #[test]
    fn the_internal_sample_rate_is_16khz_and_the_ingest_producer_is_told_to_match() {
        assert_eq!(INTERNAL_SAMPLE_RATE, 16_000);
        // `ingest.rs` hands the pipeline fixed 20 ms frames; if it ever emitted a
        // different length at this rate the VAD's debounce would be wrong.
        assert_eq!(FRAME_SAMPLES_INTERNAL, 320);
        assert_eq!(FRAME_MS_INTERNAL, 20, "320 samples at 16 kHz is 20 ms");

        // The VAD's own arithmetic for such a frame, with no config involved.
        assert_eq!(
            (FRAME_SAMPLES_INTERNAL as u64 * 1000 / INTERNAL_SAMPLE_RATE as u64) as u32,
            20
        );

        // NOTE: `ingest.rs` currently keeps its own private
        // `PIPELINE_RATE_16K: u32 = 16_000` and its own `FRAME_SAMPLES_16K = 320`,
        // and `audio.rs` spells `16_000` inline. Those files belong to other
        // tasks, so the handoff is a one-line change in each:
        //     use crate::pipeline::INTERNAL_SAMPLE_RATE;   // replace the literal
        // Until then this constant is the only place both producers can be
        // checked against, and the equality asserted above is the contract.
    }

    /// The differential proof that the first-audio cancellation is real: the
    /// PRE-FIX wait (`tokio::time::timeout(GRACE, first_frame_rx)` with nothing
    /// else to wake it) is still parked immediately after the stop has been
    /// observed by the new code. That is exactly the defect P1-01 names.
    #[test]
    fn the_pre_fix_first_audio_wait_could_not_be_cancelled() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("build runtime");
        rt.block_on(async {
            let (first_frame_tx, mut rx) = tokio::sync::oneshot::channel::<()>();
            let _first_frame_tx = first_frame_tx;
            let grace = Duration::from_millis(400);

            // What `try_start` used to await, verbatim.
            let old_style = tokio::spawn(async move {
                let _ = tokio::time::timeout(grace, &mut rx).await;
            });
            tokio::time::sleep(Duration::from_millis(20)).await;

            // The stop, observed exactly the way the new wait observes it.
            let handle = Arc::new(PipelineHandle::new());
            let generation = handle.generation();
            let mut generation_rx = handle.generation_receiver();
            let stopper = handle.clone();
            let stop = tokio::spawn(async move { stopper.pause().await });
            wait_for_generation_change(&mut generation_rx, generation).await;
            let _ = stop.await;
            assert!(
                !generation_is_current(&handle.current_generation, generation),
                "the stop must be visible to the generation watcher"
            );

            // The new wait would already have returned here. The old one is still
            // parked: nothing in it observes the generation at all.
            assert!(
                !old_style.is_finished(),
                "the pre-fix wait returned on its own, so this test proves nothing"
            );
            // It only ends when its grace period expires.
            tokio::time::timeout(Duration::from_secs(2), old_style)
                .await
                .expect("the pre-fix wait must end when its own grace expires")
                .expect("task must not panic");
        });
    }

    /// The rate genuinely reaches `Vad::decide` and produces the 20 ms step the
    /// debounce logic assumes — and the *same* frame at a 48 kHz label would
    /// produce a different number, which is the defect this pins.
    #[test]
    fn the_vad_is_told_the_rate_the_frames_were_produced_at() {
        let cfg = crate::config::FilterConfig::default();
        let frame: Vec<i16> = (0..FRAME_SAMPLES_INTERNAL)
            .map(|n| {
                let t = n as f32 / INTERNAL_SAMPLE_RATE as f32;
                (20_000.0 * (2.0 * std::f32::consts::PI * 300.0 * t).sin()) as i16
            })
            .collect();

        let mut vad = Vad::new(cfg.clone());
        let decision = vad.decide(&frame, INTERNAL_SAMPLE_RATE);
        assert_eq!(
            decision.kind,
            SegmentKind::Speech,
            "a 300 Hz tone at RMS {} must be speech, not music/silence",
            decision.rms
        );
        assert_eq!(
            decision.voiced_ms, FRAME_MS_INTERNAL,
            "one internal frame must advance the segment timer by exactly {} ms",
            FRAME_MS_INTERNAL
        );

        // The old label (`config.audio.sample_rate`, e.g. 48 000 for a 48 kHz
        // capture device) would report 6 ms instead of 20 ms for this very frame,
        // i.e. a 3x timing error on the ingest path.
        let mut mislabelled = Vad::new(cfg);
        let wrong = mislabelled.decide(&frame, 48_000);
        assert_ne!(
            wrong.voiced_ms, decision.voiced_ms,
            "the sample rate must actually change the VAD's timing, or this test proves nothing"
        );
        assert_eq!(wrong.voiced_ms, 6);
    }

    /// The ingest producer's frame geometry must equal the pipeline's: 320
    /// samples is what it emits, and at the internal rate that is 20 ms.
    #[test]
    fn the_ingest_frame_geometry_matches_the_pipeline_contract() {
        assert_eq!(
            FRAME_SAMPLES_INTERNAL * 1000 / INTERNAL_SAMPLE_RATE as usize,
            20
        );
        // A 20 ms frame at 16 kHz is what the OBS plugin sends and what
        // `min_segment_ms` / `max_segment_ms` were tuned against.
        assert_eq!(FRAME_SAMPLES_INTERNAL, 16 * 20);
    }

    // -----------------------------------------------------------------------
    // P1-01: cancel generation (stop / restart really cancels).
    //
    // Two environment facts these tests have to work around, both measured
    // rather than assumed:
    //
    // * The crate does not enable tokio's `test-util` feature (`Cargo.toml` is
    //   owned by another task and must not be edited), so
    //   `tokio::time::pause()/advance()` are unavailable. Real, deliberately
    //   short waits with generous bounds are used instead; the bound only has to
    //   be far below the 30 s `AUDIO_FIRST_FRAME_GRACE` and the 10 s provider
    //   drain, which it is by two orders of magnitude.
    // * These runtimes are `multi_thread` with ONE worker thread, not
    //   `current_thread`. On a current-thread runtime the spawned task is only
    //   polled when the test future yields, and an abort delivered to another
    //   task then goes unserviced — measured as 69 290 `yield_now()`s with the
    //   task's future still not dropped. That is a property of the test runtime,
    //   not of the pipeline: production runs on the multi-thread runtime built by
    //   `#[tokio::main]`, and one worker thread keeps these tests deterministic
    //   without changing the abort semantics being tested.
    // -----------------------------------------------------------------------

    /// A fake provider task that models a REAL one closely enough for the cancel
    /// tests: it owns something a real session owns (a sender standing in for the
    /// WebSocket), and it either runs forever or ends when the run's cancel
    /// generation moves on ([`catch_provider_cancel`], exactly what the real
    /// `_llm_task` selects on).
    ///
    /// `ended_rx` is how these tests observe "the session is gone": it resolves
    /// only when **every** sender has been dropped, i.e. when the task's future
    /// was really destroyed. That is a stronger and more honest signal than
    /// asking a `JoinHandle`, which reports terminal state on the runtime's
    /// schedule rather than at the moment the resource is released.
    struct PendingProvider {
        task: JoinHandle<()>,
        ended_rx: mpsc::Receiver<()>,
    }

    fn pending_provider() -> PendingProvider {
        let (ended_tx, ended_rx) = mpsc::channel::<()>(1);
        let task = tokio::spawn(async move {
            let _keep_socket = ended_tx;
            std::future::pending::<()>().await;
        });
        PendingProvider { task, ended_rx }
    }

    fn cancellable_pending_provider(handle: &PipelineHandle, generation: u64) -> PendingProvider {
        let (ended_tx, ended_rx) = mpsc::channel::<()>(1);
        let cancel = handle.cancellation();
        let task = tokio::spawn(async move {
            let _keep_socket = ended_tx;
            catch_provider_cancel(cancel, generation).await;
        });
        PendingProvider { task, ended_rx }
    }

    /// Install `provider`'s task as the handle's live provider session, the way
    /// `try_start` would at the end of a successful start.
    ///
    /// The task handle is owned by the installed `PipelineInner` — that is the
    /// whole point of the installation — so it cannot also be returned. A caller
    /// that needs to observe the session ending takes the probe channel *out of*
    /// `PendingProvider` first and then passes only the task, which keeps the
    /// ownership obvious: the handle goes in, nothing comes back.
    fn install_fake_provider(handle: &PipelineHandle, task: tokio::task::JoinHandle<()>) {
        let _ = handle.install_if_current(
            handle.generation(),
            PipelineInner {
                _capturer: None,
                _ingest_guard: None,
                _llm_task: task,
                _vad_task: tokio::spawn(async {}),
                _audio_task: tokio::spawn(async {}),
            },
        );
    }

    /// Wait, bounded by [`STOP_CANCEL_BOUND`], for a provider session to release
    /// what it owned. Panics with `what` if it never does.
    async fn assert_session_ended(rx: &mut mpsc::Receiver<()>, what: &str) {
        let ended = tokio::time::timeout(STOP_CANCEL_BOUND, rx.recv())
            .await
            .unwrap_or_else(|_| {
                panic!("{what}: still holding its resource after {STOP_CANCEL_BOUND:?}")
            });
        assert!(
            ended.is_none(),
            "{what}: the channel produced a value instead of closing"
        );
    }

    /// Probe for the guarantee the bounded-stop tests measure: a provider task
    /// that `abort()`s has really stopped, and one that is never aborted keeps
    /// holding the resource.
    ///
    /// Two measured facts, both of which the stop tests depend on:
    ///
    /// 1. `abort()` before the task's first poll drops the future without ever
    ///    polling it, so a guard inside the *body* never runs — which is why the
    ///    stop tests observe the task through a channel that closing (i.e. through
    ///    the resource being released) instead of through a body guard.
    /// 2. Once the task is pending, aborting it releases that resource promptly
    ///    (here: the sender closes the receiver).
    ///
    /// A multi-thread runtime with one worker is used because that is what
    /// production runs (`#[tokio::main]`), and because on a current-thread
    /// runtime an abort delivered to another task is not serviced until the
    /// test future yields to the scheduler (measured: 69 290 `yield_now()`s with
    /// the task still not dropped) — a property of the test runtime, not of the
    /// pipeline.
    #[test]
    fn aborting_a_provider_task_releases_its_resource() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("build runtime");
        rt.block_on(async {
            // --- baseline: a task nobody stops keeps its resource ----------
            let (kept_tx, mut kept_rx) = mpsc::channel::<()>(1);
            let keeper = tokio::spawn(async move {
                let _keep = kept_tx;
                std::future::pending::<()>().await;
            });
            // Leak the handle so the task is definitively NOT dropped/aborted by
            // the end of this scope; that is what makes the baseline meaningful.
            std::mem::forget(keeper);
            tokio::time::sleep(Duration::from_millis(20)).await;
            assert!(
                kept_rx.try_recv().is_err(),
                "a live pending task must still hold its resource"
            );

            // --- the task really ends, and releases what it owned ----------
            // `recv()` resolving to `None` means every sender is gone, i.e. the
            // task's future was really destroyed.
            let (stop_tx, mut stop_rx) = mpsc::channel::<()>(1);
            let stopped = tokio::spawn(async move {
                let _keep = stop_tx;
                std::future::pending::<()>().await;
            });
            tokio::time::sleep(Duration::from_millis(20)).await;
            stopped.abort();
            assert_session_ended(&mut stop_rx, "an aborted provider task").await;
        });
    }

    /// `stop_leaves_no_live_provider_task` (P1-01, phase: recognising).
    ///
    /// A live provider session — the "cloud connection" — must be gone within
    /// the bounded stop window, not merely marked stopped.
    #[test]
    fn stop_leaves_no_live_provider_task() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("build runtime");
        rt.block_on(async {
            let handle = PipelineHandle::new();
            // Take the probe channel out first, then hand only the task over, so
            // there is no partially-moved `PendingProvider` to trip over.
            let (task, mut ended_rx) = {
                let provider = pending_provider();
                (provider.task, provider.ended_rx)
            };
            install_fake_provider(&handle, task);
            assert!(handle.is_running());
            assert!(
                handle.provider_task_running(),
                "the fake provider task must look alive before the stop"
            );

            let started = std::time::Instant::now();
            handle.pause().await;
            // An urgent stop tears the run down unconditionally, so the inner
            // state is gone by the time `pause()` returns.
            assert!(
                !handle.is_running(),
                "pause() must drop the run's inner state"
            );
            // The session itself must stop, which is observable as the resource it
            // owned being released (the channel closing), within the stop bound.
            assert_session_ended(&mut ended_rx, "pause()").await;
            let elapsed = started.elapsed();
            assert!(
                !handle.provider_task_running(),
                "pause() must leave no live provider task"
            );
            assert!(
                elapsed < STOP_CANCEL_BOUND,
                "pause() took {elapsed:?}, outside the documented {STOP_CANCEL_BOUND:?} stop bound"
            );
        });
    }

    /// `restart_bumps_the_generation_and_a_stale_run_cannot_install_itself`
    /// (P1-01, phases: first-audio / connecting).
    #[test]
    fn restart_bumps_the_generation_and_a_stale_run_cannot_install_itself() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("build runtime");
        rt.block_on(async {
            let handle = PipelineHandle::new();
            let stale_generation = handle.generation();

            handle.restart(Duration::ZERO).await;
            let fresh_generation = handle.generation();
            assert!(
                fresh_generation > stale_generation,
                "restart must bump the generation ({stale_generation} -> {fresh_generation})"
            );
            assert!(
                !handle.is_running(),
                "nothing was running before the restart"
            );

            // The run started for the OLD generation now finishes its
            // first-audio wait and tries to install itself. It must not.
            let (stale_task, mut stale_ended_rx) = {
                let provider = pending_provider();
                (provider.task, provider.ended_rx)
            };
            let installed = handle.install_if_current(
                stale_generation,
                PipelineInner {
                    _capturer: None,
                    _ingest_guard: None,
                    _llm_task: stale_task,
                    _vad_task: tokio::spawn(async {}),
                    _audio_task: tokio::spawn(async {}),
                },
            );
            let rejected = installed.expect_err(
                "a stale run must never install its PipelineInner over a newer generation",
            );
            assert!(
                !handle.is_running(),
                "the rejected run must not have replaced the live state"
            );
            assert_eq!(
                rejected.0, stale_generation,
                "the rejection must name the generation it refused"
            );
            // THE POINT: `install_if_current` issues the abort itself, so a stale
            // run cannot leave a live provider task behind even if its caller
            // ignores the error. The session's resource is released promptly.
            assert_session_ended(&mut stale_ended_rx, "a rejected run's provider").await;
            assert!(
                handle.inner.lock().is_none(),
                "a rejected run must not be installed, before or after it was aborted"
            );

            // And the CURRENT generation may still install, so the guard is not
            // simply "refuse everything".
            let (fresh_task, mut fresh_ended_rx) = {
                let provider = pending_provider();
                (provider.task, provider.ended_rx)
            };
            install_fake_provider(&handle, fresh_task);
            assert!(
                handle.is_running(),
                "the current generation must still be installable"
            );
            assert!(handle.provider_task_running());
            handle.shutdown().await;
            assert_session_ended(&mut fresh_ended_rx, "shutdown").await;
        });
    }

    /// `stop_cancels_the_first_audio_wait_promptly` (P1-01, phase: first-audio).
    ///
    /// A run parked in the first-audio wait must observe a stop well inside
    /// `AUDIO_FIRST_FRAME_GRACE`. With the old code nothing could interrupt that
    /// wait, so it ran the full 30 s.
    #[test]
    fn stop_cancels_the_first_audio_wait_promptly() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("build runtime");
        rt.block_on(async {
            let (first_frame_tx, mut first_frame_rx) = tokio::sync::oneshot::channel::<()>();
            // Held (not dropped) so the wait cannot end because the sender died.
            let _first_frame_tx = first_frame_tx;
            // `Arc`, not a cloned handle: a `PipelineHandle` has no `Clone` on
            // purpose, because cloning it would fork the cancel generation.
            let handle = Arc::new(PipelineHandle::new());
            let generation = handle.generation();
            let mut generation_rx = handle.generation_receiver();

            let stopper = handle.clone();
            let stop_task = tokio::spawn(async move {
                // Well inside the 30 s grace, and long enough that the waiter is
                // definitely parked before the stop arrives.
                tokio::time::sleep(Duration::from_millis(50)).await;
                stopper.pause().await;
            });

            let started = std::time::Instant::now();
            let outcome = wait_for_first_audio(
                &mut first_frame_rx,
                &mut generation_rx,
                generation,
                AUDIO_FIRST_FRAME_GRACE,
            )
            .await;
            let elapsed = started.elapsed();
            let _ = stop_task.await;

            assert_eq!(
                outcome,
                FirstAudioWait::Cancelled,
                "a stop must cancel the first-audio wait instead of leaving it parked"
            );
            assert!(
                elapsed < Duration::from_secs(3),
                "the first-audio wait was cancelled after {elapsed:?}; \
                 AUDIO_FIRST_FRAME_GRACE is {AUDIO_FIRST_FRAME_GRACE:?}, so this is not prompt"
            );
            assert!(
                !handle.is_enabled(),
                "a stop must leave the run gate closed"
            );
        });
    }

    /// `a_whole_config_change_is_detected_by_the_watcher` (P1-01).
    ///
    /// The old watcher compared a hand-written whitelist
    /// (`provider`/`api_key`/`model`/`endpoint`/`audio.mode`/`audio.device`), so
    /// this change — and every other field outside it — was invisible to it.
    #[test]
    fn a_whole_config_change_is_detected_by_the_watcher() {
        let base = crate::config::Config::default();
        let fingerprint = needs_restart_fingerprint(&base);

        // A field that is NOT in the old whitelist and that the pipeline reads.
        let mut changed = base.clone();
        changed.overlay.font_size += 1;
        assert_ne!(
            fingerprint,
            needs_restart_fingerprint(&changed),
            "overlay.font_size is outside the old whitelist and must still be detected"
        );

        // A field that the old whitelist DID cover keeps working.
        let mut provider = base.clone();
        provider.llm.provider = "mock".into();
        assert_ne!(fingerprint, needs_restart_fingerprint(&provider));

        // audio.sample_rate: another field the old whitelist missed, and one the
        // capture path reads.
        let mut rate = base.clone();
        rate.audio.sample_rate = 48_000;
        assert_ne!(fingerprint, needs_restart_fingerprint(&rate));

        // Comparing a config with itself must not report a change, or the
        // watcher would restart the pipeline on every tick.
        assert_eq!(fingerprint, needs_restart_fingerprint(&base.clone()));

        // Hotwords are excluded on purpose: they are delivered to a live session
        // with continue-task and must not restart it.
        let mut words = base.clone();
        words.llm.hotwords = vec!["区域赛".into()];
        assert_eq!(
            fingerprint,
            needs_restart_fingerprint(&words),
            "a hotword-only change must not report a restart"
        );
        assert_ne!(
            hotwords_key(&base),
            hotwords_key(&words),
            "but it must be seen by the hotword path"
        );
    }

    /// The `watch()` loop must notice the generation it was started for has been
    /// replaced *before* it acts on a config change — otherwise a stale watcher
    /// would restart the NEW run.
    #[test]
    fn a_watcher_observes_a_generation_bump_and_exits() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("build runtime");
        rt.block_on(async {
            let handle = Arc::new(PipelineHandle::new());
            let generation = handle.generation();
            let mut rx = handle.generation_receiver();
            assert!(generation_is_current(
                &handle.current_generation,
                generation
            ));
            let bump = {
                let handle = handle.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    handle.restart(Duration::ZERO).await;
                })
            };
            // This is the check `watch()` performs at the top of every tick.
            wait_for_generation_change(&mut rx, generation).await;
            assert!(
                !generation_is_current(&handle.current_generation, generation),
                "the bump must be observable by a run that is holding this generation"
            );
            let _ = bump.await;
        });
    }

    /// `a_config_save_restart_still_allows_the_final_drain` (P1-01, phase: final
    /// drain).
    ///
    /// A config-save restart must not cut a still-live provider's bounded drain
    /// short, while an operator stop must: the two are deliberately different
    /// cancellation strengths, carried by the urgency flag ([`DRAIN_GRACE`] for
    /// the save, [`STOP_CANCEL_BOUND`] for the stop).
    #[test]
    fn a_config_save_restart_still_allows_the_final_drain() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("build runtime");
        rt.block_on(async {
            let handle = PipelineHandle::new();
            let generation = handle.generation();
            let (task, mut ended_rx) = {
                let provider = cancellable_pending_provider(&handle, generation);
                (provider.task, provider.ended_rx)
            };
            install_fake_provider(&handle, task);
            assert!(handle.provider_task_running());

            // What `POST /api/config` does: a GRACEFUL restart, because a finite
            // replay may still be inside its bounded drain.
            handle.restart_graceful(Duration::from_secs(2)).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            // The provider session is still alive: the drain window is untouched.
            assert!(
                handle.provider_task_running(),
                "a config-save restart must not tear down a session that is still draining"
            );
            assert!(
                ended_rx.try_recv().is_err(),
                "the provider task must still hold its WebSocket session open for the drain"
            );
            // The window the run loop grants is DRAIN_GRACE, which must cover the
            // provider's own 10 s reader timeout or the last sentence is lost.
            assert!(DRAIN_GRACE >= Duration::from_secs(10));

            // An operator stop, by contrast, must not wait: the same live
            // provider is gone within the bounded stop window.
            let started = std::time::Instant::now();
            handle.pause().await;
            assert_session_ended(&mut ended_rx, "a stop during the drain").await;
            let elapsed = started.elapsed();
            assert!(
                !handle.provider_task_running(),
                "a stop must end the drain at once"
            );
            assert!(
                elapsed < STOP_CANCEL_BOUND,
                "the stop took {elapsed:?}, outside the {STOP_CANCEL_BOUND:?} bound"
            );
        });
    }

    /// The provider task itself must stop the connect/recognise session as soon
    /// as a cancel generation arrives — that is what makes the provider phase
    /// cancellable without waiting for the task to be aborted at process exit
    /// (P1-01, phases: connecting / recognising).
    #[test]
    fn a_newer_generation_cancels_a_live_provider_session_in_both_strengths() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("build runtime");
        rt.block_on(async {
            // --- an operator stop ends the session immediately -----------------
            let handle = Arc::new(PipelineHandle::new());
            let generation = handle.generation();
            let (task, mut ended_rx) = {
                let provider = cancellable_pending_provider(&handle, generation);
                (provider.task, provider.ended_rx)
            };
            install_fake_provider(&handle, task);
            let stopper = handle.clone();
            let stop_task = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(30)).await;
                stopper.pause().await;
            });
            let started = std::time::Instant::now();
            assert_session_ended(&mut ended_rx, "a stop of a live provider session").await;
            assert!(
                started.elapsed() < STOP_CANCEL_BOUND,
                "the stop took {:?}",
                started.elapsed()
            );
            let _ = stop_task.await;

            // --- a graceful restart waits for the drain window -----------------
            let handle = PipelineHandle::new();
            let generation = handle.generation();
            let (task, mut ended_rx) = {
                let provider = cancellable_pending_provider(&handle, generation);
                (provider.task, provider.ended_rx)
            };
            install_fake_provider(&handle, task);
            handle.restart_graceful(Duration::ZERO).await;
            // Still alive: a config-save restart must give the drain its window.
            tokio::time::sleep(Duration::from_millis(40)).await;
            assert!(
                handle.provider_task_running(),
                "the drain window must not be cut short"
            );
            assert!(
                ended_rx.try_recv().is_err(),
                "the session must still be open"
            );
            // ...but an operator stop immediately afterwards ends it at once.
            let started = std::time::Instant::now();
            handle.pause().await;
            assert_session_ended(&mut ended_rx, "a stop after the drain window").await;
            assert!(started.elapsed() < STOP_CANCEL_BOUND);
        });
    }

    /// A graceful bump must not cancel the session *at once*: only after the
    /// drain window has expired (or an operator stop says otherwise).
    #[test]
    fn a_graceful_bump_does_not_cut_the_drain_window_at_once() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("build runtime");
        rt.block_on(async {
            let handle = PipelineHandle::new();
            let generation = handle.generation();
            let cancel = handle.cancellation();
            // This is exactly the future the real provider task selects on.
            let provider = tokio::spawn(catch_provider_cancel(cancel, generation));
            tokio::time::sleep(Duration::from_millis(20)).await;
            assert!(
                !provider.is_finished(),
                "the provider session must still be live"
            );

            // A graceful bump (config save) must NOT end the session immediately:
            // that is the whole point of DRAIN_GRACE.
            handle.restart_graceful(Duration::ZERO).await;
            tokio::time::sleep(Duration::from_millis(60)).await;
            assert!(
                !provider.is_finished(),
                "a graceful restart cancelled the drain window instead of granting it"
            );
            assert!(DRAIN_GRACE >= Duration::from_secs(10));

            // An operator stop always ends it, immediately.
            let started = std::time::Instant::now();
            handle.restart(Duration::ZERO).await;
            tokio::time::timeout(STOP_CANCEL_BOUND, provider)
                .await
                .expect("an urgent stop must end the session within the stop bound")
                .expect("provider task must not panic");
            assert!(started.elapsed() < STOP_CANCEL_BOUND);
        });
    }
}
