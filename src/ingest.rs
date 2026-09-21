//! OBS plugin audio ingest.
//!
//! When the engine runs inside the OBS plugin package, audio does not come
//! from cpal loopback: the C shell plugin attaches an audio filter to the
//! user's OBS source and streams the captured PCM to us over a local TCP
//! connection.
//!
//! Wire protocol (all integers little-endian):
//!
//! ```text
//!   4 bytes   magic "SLTA"
//!   u32       sample rate of the payload (the plugin always sends 16000)
//!   u32       format: 0 = mono s16le
//!   ...       continuous mono s16le PCM samples
//! ```
//!
//! Received audio is resampled to the configured pipeline rate, chopped into
//! ~20 ms frames and pushed into the same channel the cpal capturer uses, so
//! VAD / music detection / LLM downstream are completely unchanged.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use parking_lot::Mutex;
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, info, warn};

use crate::audio::PcmSender;
use crate::AppState;

const MAGIC: &[u8; 4] = b"SLTA";
const FORMAT_I16_MONO: u32 = 0;
/// One pipeline frame: 20 ms at 16 kHz.
const FRAME_SAMPLES_16K: usize = 320;

/// Generation counter for the ingest registry.
///
/// Why a generation is needed: `try_start` calls `register()` (which installs
/// the NEW pipeline's sender) *before* it stores the new `PipelineInner`, and
/// storing that inner drops the OLD one — whose `_ingest_guard` used to clear
/// `SENDER` unconditionally on drop. So the old guard wiped out the sender that
/// had just been registered, and in `obs_filter` mode the new pipeline never
/// received another frame: the first-frame gate timed out, the retry registered
/// again and was wiped again, i.e. a permanent self-lock.
///
/// Every registration now carries the generation that was current when it was
/// installed, and dropping it only clears the slot if that generation is still
/// the active one.
static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
static SENDER: Mutex<Option<(u64, PcmSender)>> = Mutex::new(None);

fn current_generation() -> u64 {
    GENERATION.load(std::sync::atomic::Ordering::SeqCst)
}

/// Retire the active registration without installing a new one. Bumping the
/// generation is what makes any stale guard harmless afterwards.
fn retire_current() {
    GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    *SENDER.lock() = None;
}

/// Guard returned by [`register`]; unregisters the sender when dropped, but only
/// if no newer registration has replaced it in the meantime.
pub struct Registration {
    generation: u64,
}

impl Drop for Registration {
    fn drop(&mut self) {
        let mut slot = SENDER.lock();
        if slot.as_ref().is_some_and(|(g, _)| *g == self.generation) {
            *slot = None;
        }
    }
}

/// Register the raw-PCM sender of the currently active pipeline so ingest
/// connections can feed frames into it.
pub fn register(tx: PcmSender) -> Registration {
    let generation = GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    *SENDER.lock() = Some((generation, tx));
    Registration { generation }
}

/// End the current finite input. This is used when a replay sender closes its
/// TCP stream: dropping the last raw sender lets the VAD and provider writer
/// finish normally instead of leaving a live websocket waiting forever.
fn close_input() {
    retire_current();
}

fn try_send_frame(frame: Vec<i16>) {
    if let Some((_, tx)) = SENDER.lock().as_ref() {
        // Best-effort send; drop on backpressure (same policy as cpal path).
        let _ = tx.try_send(frame);
    }
}

/// Run the ingest TCP server. Re-binds automatically when the configured
/// port changes.
pub async fn serve(state: Arc<AppState>) {
    loop {
        let port = state.config.read().audio.ingest_port;
        if port == 0 {
            // Ingest disabled; poll for config changes.
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }
        match TcpListener::bind(("127.0.0.1", port)).await {
            Ok(listener) => {
                info!(port, "OBS audio ingest listening on 127.0.0.1");
                run_listener(&state, listener, port).await;
            }
            Err(e) => {
                warn!(port, error = %e, "failed to bind ingest port");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

async fn run_listener(state: &Arc<AppState>, listener: TcpListener, port: u16) {
    loop {
        tokio::select! {
            accept = listener.accept() => match accept {
                Ok((stream, _peer)) => {
                    let state = state.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_conn(state, stream).await {
                            debug!(error = %e, "ingest connection ended");
                        }
                    });
                }
                Err(e) => {
                    warn!(error = %e, "ingest accept failed");
                }
            },
            _ = watch_port_change(state, port) => {
                info!(port, "ingest port changed, re-binding");
                return;
            }
        }
    }
}

async fn watch_port_change(state: &Arc<AppState>, port: u16) {
    let mut ticker = tokio::time::interval(Duration::from_secs(2));
    loop {
        ticker.tick().await;
        if state.config.read().audio.ingest_port != port {
            return;
        }
    }
}

async fn handle_conn(state: Arc<AppState>, mut stream: TcpStream) -> Result<()> {
    let _ = stream.set_nodelay(true);
    stream
        .writable()
        .await
        .context("ingest socket not writable")?;

    // --- fixed header -------------------------------------------------
    let mut header = [0u8; 12];
    stream
        .read_exact(&mut header)
        .await
        .context("read ingest header")?;
    if &header[0..4] != MAGIC {
        return Err(anyhow!("bad ingest magic"));
    }
    let in_rate = u32::from_le_bytes(header[4..8].try_into().unwrap());
    let format = u32::from_le_bytes(header[8..12].try_into().unwrap());
    if format != FORMAT_I16_MONO {
        return Err(anyhow!("unsupported ingest format {format}"));
    }
    if !(8_000..=384_000).contains(&in_rate) {
        return Err(anyhow!("implausible ingest sample rate {in_rate}"));
    }

    {
        let mut s = state.status.write();
        s.audio_active = true;
        s.last_error = None;
    }
    info!(rate = in_rate, "OBS filter audio stream connected");

    let result = pump_audio(&state, &mut stream, in_rate).await;
    close_input();
    {
        let mut s = state.status.write();
        s.audio_active = false;
    }
    info!("OBS filter audio stream disconnected");
    result
}

async fn pump_audio(
    state: &Arc<AppState>,
    stream: &mut TcpStream,
    in_rate: u32,
) -> Result<()> {
    let out_rate = {
        let r = state.config.read().audio.sample_rate;
        if r == 0 {
            16_000
        } else {
            r
        }
    };
    let frame_samples = (out_rate as usize * FRAME_SAMPLES_16K) / 16_000;

    let mut read_buf = vec![0u8; 8 * 1024];
    let mut leftover_byte: Option<u8> = None;
    let mut sample_buf: Vec<i16> = Vec::with_capacity(frame_samples * 4);

    loop {
        let n = stream
            .read(&mut read_buf)
            .await
            .context("ingest read")?;
        if n == 0 {
            break;
        }

        let mut bytes: &[u8] = &read_buf[..n];
        if let Some(prev) = leftover_byte.take() {
            sample_buf.push(i16::from_le_bytes([prev, bytes[0]]));
            bytes = &bytes[1..];
        }
        let pairs = bytes.len() / 2;
        for pair in bytes[..pairs * 2].chunks_exact(2) {
            sample_buf.push(i16::from_le_bytes([pair[0], pair[1]]));
        }
        if bytes.len() % 2 == 1 {
            leftover_byte = Some(*bytes.last().unwrap());
        }

        let ready: Vec<i16> = if in_rate != out_rate {
            let out = crate::audio::resample_mono(&sample_buf, in_rate, out_rate);
            sample_buf.clear();
            out
        } else {
            std::mem::take(&mut sample_buf)
        };

        let mut frames = ready;
        let tail = frames.len() % frame_samples;
        let tail_samples: Vec<i16> = if tail > 0 {
            frames.split_off(frames.len() - tail)
        } else {
            Vec::new()
        };
        for chunk in frames.chunks(frame_samples) {
            try_send_frame(chunk.to_vec());
        }
        sample_buf.extend_from_slice(&tail_samples);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    /// The registry is process-global, so these tests must not interleave: one
    /// test's `retire_current()` would otherwise invalidate another's guard.
    static REGISTRY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn registry_guard() -> std::sync::MutexGuard<'static, ()> {
        REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn sender() -> (PcmSender, mpsc::Receiver<Vec<i16>>) {
        mpsc::channel::<Vec<i16>>(8)
    }

    /// The exact self-lock that made `obs_filter` pipelines go deaf:
    /// `try_start` registers the NEW sender, then drops the OLD `PipelineInner`
    /// (and with it the old guard). If the old guard clears the slot
    /// unconditionally, the new pipeline never receives another frame.
    #[test]
    fn dropping_a_stale_guard_keeps_the_new_registration() {
        let _serial = registry_guard();
        retire_current(); // start from a clean slate
        let (old_tx, _old_rx) = sender();
        let old_guard = register(old_tx);

        let (new_tx, mut new_rx) = sender();
        let _new_guard = register(new_tx);

        // The old pipeline is torn down after the new one registered.
        drop(old_guard);

        try_send_frame(vec![1, 2, 3]);
        assert!(
            new_rx.try_recv().is_ok(),
            "the new pipeline must still receive frames after the old guard is dropped"
        );
    }

    #[test]
    fn the_current_guard_still_unregisters() {
        let _serial = registry_guard();
        retire_current();
        let (tx, mut rx) = sender();
        let guard = register(tx);
        try_send_frame(vec![7]);
        assert!(rx.try_recv().is_ok(), "registered sender receives frames");

        drop(guard);
        try_send_frame(vec![8]);
        assert!(
            rx.try_recv().is_err(),
            "dropping the active guard must stop delivery"
        );
    }

    /// A finite replay ends by retiring the active input, which must also
    /// invalidate any guard that is still alive at that moment.
    #[test]
    fn retiring_the_input_invalidates_an_outstanding_guard() {
        let _serial = registry_guard();
        retire_current();
        let (tx, mut rx) = sender();
        let guard = register(tx);
        close_input();
        try_send_frame(vec![9]);
        assert!(rx.try_recv().is_err(), "retired input must not deliver");

        // The stale guard must not resurrect anything when it finally drops.
        drop(guard);
        try_send_frame(vec![10]);
        assert!(rx.try_recv().is_err(), "stale guard drop must not re-register");
    }
}
