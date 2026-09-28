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
//!   32 bytes  nonce: ASCII hex, zero padded (P0-05)
//!   server -> client  8 bytes "SLTAOK01" after validating the header
//!   ...       continuous mono s16le PCM samples
//! ```
//!
//! Before the client sends that header, the engine sends `"SLTS"` followed by
//! SHA-256(`"SLTS" || nonce`). The plugin verifies this server proof first, so
//! a process squatting on the port never sees the nonce or any PCM. The fixed
//! ACK then proves that the engine accepted the client's header.
//!
//! The nonce is the handshake identity of the plugin that started this engine
//! (`--ingest-nonce` / `SLT_INGEST_NONCE`). When it is configured, a peer that
//! does not present it gets no audio at all and its connection is dropped: a
//! process that merely squats on the ingest port must not be able to push audio
//! into the pipeline. When the engine was started by hand (replay/testing) the
//! nonce is absent and any peer is accepted, which is logged at info level.
//!
//! Received audio is resampled to the pipeline's internal 16 kHz rate, chopped
//! into ~20 ms frames and pushed into the same channel the cpal capturer uses,
//! so VAD / music detection / LLM downstream are completely unchanged.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, info, warn};

use crate::audio::{PcmSender, StreamResampler};
use crate::AppState;

const MAGIC: &[u8; 4] = b"SLTA";
const FORMAT_I16_MONO: u32 = 0;
/// One pipeline frame: exactly 20 ms at [`PIPELINE_RATE_16K`].
///
/// Derived from `pipeline.rs` rather than repeated, so the frame geometry the VAD
/// and the providers assume cannot drift from what this producer emits.
const FRAME_SAMPLES_16K: usize = crate::pipeline::FRAME_SAMPLES_INTERNAL;
/// The fixed-width nonce field that follows the three prefix words.
const NONCE_LEN: usize = 32;
/// magic + rate + format + nonce.
const HEADER_LEN: usize = 12 + NONCE_LEN;
const SERVER_HELLO_MAGIC: &[u8; 4] = b"SLTS";
const SERVER_HELLO_LEN: usize = SERVER_HELLO_MAGIC.len() + NONCE_LEN;
const ACCEPT_ACK: &[u8; 8] = b"SLTAOK01";
/// Live ingest connections allowed at once. The plugin opens one; the second
/// slot covers a plugin restart overlapping the old socket. Anything beyond
/// that is a flood (or a squatter) and is rejected instead of being allowed to
/// spawn unbounded tasks.
const MAX_CONNECTIONS: usize = 2;
/// A peer that connects and then sends nothing must not own a slot forever.
const HEADER_TIMEOUT: Duration = Duration::from_secs(5);
/// Currently open ingest connections.
static CONNECTIONS: AtomicUsize = AtomicUsize::new(0);
/// Whether the "no nonce configured" notice has already been emitted.
static UNAUTHENTICATED_LOGGED: AtomicBool = AtomicBool::new(false);

/// The rate of everything this module *emits*, i.e. of `frame_buf` and of every
/// frame handed to the pipeline. Payload bytes arrive at the rate the plugin
/// declares in the header (always called `in_rate`, never `rate`); the two must
/// not be confused, which is why this constant carries its value in its name.
///
/// Ingest always resamples to 16 kHz instead of `config.audio.sample_rate`:
/// everything downstream is built on 320 samples == 20 ms (the VAD derives
/// segment durations from it, `min_segment_ms` / `max_segment_ms` are compared
/// against it) and the OBS plugin itself always sends 16 kHz. Scaling frames by
/// a config value here was the pre-fix behaviour that produced frames which were
/// not 20 ms long and mixed input-rate with output-rate samples in one buffer.
/// Taken from [`crate::pipeline::INTERNAL_SAMPLE_RATE`] — the single place the
/// internal rate is defined, and the same value `try_start` hands to `Vad::decide`
/// — so this producer and the consumer can never disagree about what a frame is.
const PIPELINE_RATE_16K: u32 = crate::pipeline::INTERNAL_SAMPLE_RATE;

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
                Ok((stream, peer)) => {
                    // Refuse to grow one task per connection: a flood (or a
                    // squatter reconnecting in a loop) is rejected here, before
                    // any header is read.
                    let Some(slot) = ConnectionSlot::try_acquire() else {
                        warn!(
                            %peer,
                            max = MAX_CONNECTIONS,
                            "rejecting ingest connection: too many open ingest connections"
                        );
                        drop(stream);
                        continue;
                    };
                    let state = state.clone();
                    tokio::spawn(async move {
                        // Held for the lifetime of the connection.
                        let _slot = slot;
                        if let Err(e) = handle_conn(state, stream, peer).await {
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

/// Counts one live ingest connection; released when the connection task ends
/// (including on panic, so a stuck peer cannot leak a slot permanently).
struct ConnectionSlot;

impl ConnectionSlot {
    /// `None` when [`MAX_CONNECTIONS`] are already open.
    fn try_acquire() -> Option<Self> {
        let mut current = CONNECTIONS.load(std::sync::atomic::Ordering::Acquire);
        loop {
            if current >= MAX_CONNECTIONS {
                return None;
            }
            match CONNECTIONS.compare_exchange_weak(
                current,
                current + 1,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            ) {
                Ok(_) => return Some(Self),
                Err(observed) => current = observed,
            }
        }
    }
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        CONNECTIONS.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
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

async fn handle_conn(state: Arc<AppState>, mut stream: TcpStream, peer: SocketAddr) -> Result<()> {
    let _ = stream.set_nodelay(true);
    stream
        .writable()
        .await
        .context("ingest socket not writable")?;

    // --- handshake ----------------------------------------------------
    // Read the header *before* touching any pipeline state, so a rejected peer
    // never even marks the input as active.
    let expected_nonce = state.ingest_nonce.clone();
    if expected_nonce.is_none()
        && !UNAUTHENTICATED_LOGGED.swap(true, std::sync::atomic::Ordering::AcqRel)
    {
        info!(
            "ingest nonce not configured (--ingest-nonce / SLT_INGEST_NONCE): \
             the ingest port accepts unauthenticated audio from any local process"
        );
    }

    // Prove the server knows the launch nonce before the plugin discloses it in
    // its header. This is what keeps a port-squatting fake service from ever
    // receiving either the nonce or PCM.
    write_server_hello(&mut stream, expected_nonce.as_deref(), HEADER_TIMEOUT).await?;

    let header = read_header(&mut stream, HEADER_TIMEOUT).await?;
    if !nonce_allowed(expected_nonce.as_deref(), &header.nonce) {
        // Fail closed: not one PCM sample reaches the pipeline, and dropping
        // the socket here ends the connection.
        warn!(
            %peer,
            "rejecting ingest connection: nonce mismatch; no audio accepted"
        );
        return Err(anyhow!("ingest nonce mismatch"));
    }
    write_accept_ack(&mut stream, HEADER_TIMEOUT).await?;

    let in_rate = header.in_rate;
    {
        let mut s = state.status.write();
        s.audio_active = true;
        s.last_error = None;
    }
    info!(%peer, rate = in_rate, "OBS filter audio stream connected");

    let result = pump_audio(&mut stream, in_rate).await;
    close_input();
    {
        let mut s = state.status.write();
        s.audio_active = false;
    }
    info!("OBS filter audio stream disconnected");
    result
}

fn nonce_field(nonce: Option<&str>) -> [u8; NONCE_LEN] {
    let mut field = [0u8; NONCE_LEN];
    if let Some(nonce) = nonce {
        let bytes = nonce.as_bytes();
        let len = bytes.len().min(NONCE_LEN);
        field[..len].copy_from_slice(&bytes[..len]);
    }
    field
}

fn server_proof(nonce: Option<&str>) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(SERVER_HELLO_MAGIC);
    hash.update(nonce_field(nonce));
    hash.finalize().into()
}

async fn write_server_hello(
    stream: &mut TcpStream,
    nonce: Option<&str>,
    timeout: Duration,
) -> Result<()> {
    let mut hello = [0u8; SERVER_HELLO_LEN];
    hello[..SERVER_HELLO_MAGIC.len()].copy_from_slice(SERVER_HELLO_MAGIC);
    hello[SERVER_HELLO_MAGIC.len()..].copy_from_slice(&server_proof(nonce));
    tokio::time::timeout(timeout, stream.write_all(&hello))
        .await
        .map_err(|_| anyhow!("timed out writing the ingest server proof"))?
        .context("write ingest server proof")
}

async fn write_accept_ack(stream: &mut TcpStream, timeout: Duration) -> Result<()> {
    tokio::time::timeout(timeout, stream.write_all(ACCEPT_ACK))
        .await
        .map_err(|_| anyhow!("timed out writing the ingest acceptance ACK"))?
        .context("write ingest acceptance ACK")
}

/// The validated fixed header (`HEADER_LEN` bytes) that opens an ingest stream.
#[derive(Debug, Clone, Copy)]
struct IngestHeader {
    in_rate: u32,
    nonce: [u8; NONCE_LEN],
}

/// Read and validate the fixed ingest header (magic, rate, format, nonce).
///
/// The whole header must arrive within `timeout`: a peer that connects and then
/// sends nothing — or stalls half way — is disconnected instead of holding its
/// connection slot forever.
async fn read_header(stream: &mut TcpStream, timeout: Duration) -> Result<IngestHeader> {
    let mut header = [0u8; HEADER_LEN];
    tokio::time::timeout(timeout, stream.read_exact(&mut header))
        .await
        .map_err(|_| anyhow!("timed out reading the ingest header"))?
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
    let mut nonce = [0u8; NONCE_LEN];
    nonce.copy_from_slice(&header[12..HEADER_LEN]);
    Ok(IngestHeader { in_rate, nonce })
}

/// Whether the received nonce field identifies the plugin that started us.
///
/// `None` means the engine was started without a nonce (hand-run replay or a
/// test): any peer is accepted on purpose, and the caller logs that fact once at
/// info level. Otherwise the meaningful prefix of the configured nonce must
/// match byte for byte (case sensitive) and the remaining bytes are ignored, as
/// the wire format zero-pads the field.
fn nonce_allowed(expected: Option<&str>, received: &[u8; NONCE_LEN]) -> bool {
    let Some(expected) = expected else {
        return true;
    };
    let want = expected.as_bytes();
    let len = want.len().min(NONCE_LEN);
    received[..len] == want[..len]
}

/// Stream mono s16le PCM from `stream` into the registered pipeline sender as
/// whole 20 ms frames at [`PIPELINE_RATE_16K`].
async fn pump_audio(stream: &mut TcpStream, in_rate: u32) -> Result<()> {
    // Two buffers, in two *different* sample rates — that separation is the
    // whole point:
    //   * `input_buf` holds samples exactly as the plugin sent them (in_rate).
    //     It is handed to the streaming resampler and cleared on every read.
    //   * `frame_buf` holds resampled samples at PIPELINE_RATE_16K. Only whole
    //     FRAME_SAMPLES_16K frames leave it; the incomplete remainder waits for
    //     the next read.
    // The old code pushed the *resampled* remainder back into the pre-resample
    // buffer, so it was resampled a second time and the 20 ms frame grid moved
    // by a chunk-dependent amount on every TCP read.
    let mut resampler = StreamResampler::new(in_rate, PIPELINE_RATE_16K);
    let mut read_buf = vec![0u8; 8 * 1024];
    // A sample split across two reads: the low byte has arrived, the high byte
    // comes with the next read.
    let mut leftover_byte: Option<u8> = None;
    let mut input_buf: Vec<i16> = Vec::with_capacity(read_buf.len() / 2 + 1);
    let mut frame_buf: Vec<i16> = Vec::with_capacity(FRAME_SAMPLES_16K * 2);

    loop {
        let n = stream.read(&mut read_buf).await.context("ingest read")?;
        if n == 0 {
            break;
        }

        // 1) raw bytes -> input-rate samples, carrying a half sample across reads.
        let mut bytes: &[u8] = &read_buf[..n];
        if let Some(prev) = leftover_byte.take() {
            input_buf.push(i16::from_le_bytes([prev, bytes[0]]));
            bytes = &bytes[1..];
        }
        input_buf.extend(
            bytes
                .chunks_exact(2)
                .map(|pair| i16::from_le_bytes([pair[0], pair[1]])),
        );
        if bytes.len() % 2 == 1 {
            leftover_byte = Some(*bytes.last().unwrap());
        }

        // 2) input-rate samples -> PIPELINE_RATE_16K samples. The resampler
        //    keeps its phase and the samples the next output sample still needs,
        //    so this is bit-identical to resampling the whole stream in one call
        //    no matter how the TCP reads were split.
        resampler.push(&input_buf, &mut frame_buf);
        input_buf.clear();

        // 3) emit whole 20 ms frames; keep the remainder for the next read.
        let complete = frame_buf.len() - frame_buf.len() % FRAME_SAMPLES_16K;
        for frame in frame_buf[..complete].chunks_exact(FRAME_SAMPLES_16K) {
            try_send_frame(frame.to_vec());
        }
        frame_buf.drain(..complete);
    }

    // A finite stream can end while the interpolator is waiting for its final
    // right-hand sample. Clamp that tail exactly once, then emit any newly
    // completed 20 ms frame. An incomplete final frame is deliberately dropped:
    // every downstream consumer requires exactly FRAME_SAMPLES_16K samples.
    resampler.finish(&mut frame_buf);
    let complete = frame_buf.len() - frame_buf.len() % FRAME_SAMPLES_16K;
    for frame in frame_buf[..complete].chunks_exact(FRAME_SAMPLES_16K) {
        try_send_frame(frame.to_vec());
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
        assert!(
            rx.try_recv().is_err(),
            "stale guard drop must not re-register"
        );
    }

    // --- P1-02: the whole ingest path, over a real loopback socket --------

    /// Deterministic LCG (no `rand` dependency, same bytes on every run).
    fn lcg(state: &mut u64) -> u64 {
        *state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        *state
    }

    /// ~1 s of deterministic PCM: tone + full-range dither.
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

    fn pcm_bytes(samples: &[i16]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    /// The wire header: magic + u32 rate + u32 format + 32-byte zero-padded
    /// ASCII hex nonce (P0-05). Keep this in step with `read_header`.
    fn current_header(rate: u32, nonce: &[u8; NONCE_LEN]) -> Vec<u8> {
        let mut header = Vec::with_capacity(HEADER_LEN);
        header.extend_from_slice(MAGIC);
        header.extend_from_slice(&rate.to_le_bytes());
        header.extend_from_slice(&FORMAT_I16_MONO.to_le_bytes());
        header.extend_from_slice(nonce);
        assert_eq!(header.len(), HEADER_LEN, "magic + rate + format + nonce");
        header
    }

    /// The 32-byte nonce field carrying `token`, zero padded.
    fn nonce_field(token: &str) -> [u8; NONCE_LEN] {
        assert!(token.len() <= NONCE_LEN, "a test nonce must fit the field");
        super::nonce_field(Some(token))
    }

    /// What one ingest connection produced.
    struct IngestRun {
        frames: Vec<Vec<i16>>,
        /// The server got past the handshake into `pump_audio`, i.e. PCM could
        /// reach the pipeline at all.
        pumped: bool,
        /// For a *rejected* handshake: the client observed the server closing the
        /// connection. Always `true` for a pumped run, whose pump only ends when
        /// the client half-closes.
        closed: bool,
    }

    /// Drive the real ingest TCP path over loopback, mirroring `handle_conn`
    /// exactly: read the header, apply the fail-closed nonce check, and only
    /// then pump. (Only the `state.status` bookkeeping is not exercised; no
    /// runtime behaviour depends on it.)
    async fn ingest_over_tcp(
        expected_nonce: Option<&str>,
        sent_nonce: &[u8; NONCE_LEN],
        in_rate: u32,
        writes: Vec<Vec<u8>>,
    ) -> IngestRun {
        // Plenty of room: 1 s of audio is 50 frames and `try_send_frame` drops
        // on backpressure, which would make a frame comparison meaningless.
        let (tx, mut rx) = mpsc::channel::<Vec<i16>>(4_096);
        let guard = register(tx);

        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind loopback ingest port");
        let addr = listener.local_addr().expect("loopback address");
        let expected_nonce = expected_nonce.map(str::to_owned);
        let expected_hello = server_proof(expected_nonce.as_deref());
        let pumped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let pumped_by_server = pumped.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _peer) = listener.accept().await.expect("accept ingest client");
            write_server_hello(&mut stream, expected_nonce.as_deref(), HEADER_TIMEOUT)
                .await
                .expect("write server proof");
            let header = read_header(&mut stream, HEADER_TIMEOUT)
                .await
                .expect("read ingest header");
            assert_eq!(
                header.in_rate, in_rate,
                "the header rate must survive the round trip"
            );
            if !nonce_allowed(expected_nonce.as_deref(), &header.nonce) {
                // Rejected: return before `pump_audio`, exactly like handle_conn.
                return;
            }
            write_accept_ack(&mut stream, HEADER_TIMEOUT)
                .await
                .expect("write acceptance ACK");
            pumped_by_server.store(true, std::sync::atomic::Ordering::Release);
            pump_audio(&mut stream, header.in_rate)
                .await
                .expect("pump audio");
        });

        let mut client = TcpStream::connect(addr).await.expect("connect to ingest");
        let mut hello = [0u8; SERVER_HELLO_LEN];
        client
            .read_exact(&mut hello)
            .await
            .expect("read server proof");
        assert_eq!(&hello[..4], SERVER_HELLO_MAGIC);
        assert_eq!(&hello[4..], expected_hello);
        client
            .write_all(&current_header(in_rate, sent_nonce))
            .await
            .expect("write header");
        let mut ack = [0u8; ACCEPT_ACK.len()];
        let accepted = client.read_exact(&mut ack).await.is_ok() && ack == *ACCEPT_ACK;
        if accepted {
            for write in writes {
                // The assertions are about delivered frames, not about the client.
                if client.write_all(&write).await.is_err() {
                    break;
                }
                // Let the receiver drain this write before sending the next one.
                // TCP is a byte stream: without this the kernel coalesces the writes
                // and the pump sees the same full 8 KiB reads on every run, so the
                // test would never reach a chunk boundary at all (verified: it then
                // passes even on the broken code).
                tokio::task::yield_now().await;
            }
        }
        let _ = client.shutdown().await;
        server.await.expect("ingest server task");

        let pumped = pumped.load(std::sync::atomic::Ordering::Acquire);
        let closed = if pumped {
            true
        } else {
            // The server dropped the socket; a read must now see EOF (or a
            // reset, because unread PCM was discarded).
            let mut probe = [0u8; 1];
            !matches!(
                tokio::time::timeout(Duration::from_secs(2), client.read(&mut probe)).await,
                Ok(Ok(n)) if n > 0
            )
        };
        drop(client);
        drop(guard);

        let mut frames = Vec::new();
        while let Ok(frame) = rx.try_recv() {
            frames.push(frame);
        }
        IngestRun {
            frames,
            pumped,
            closed,
        }
    }

    /// Frames only, for the tests that use the production nonce configuration
    /// (no nonce) and therefore always expect the stream to be accepted.
    async fn frames_over_tcp(in_rate: u32, writes: Vec<Vec<u8>>) -> Vec<Vec<i16>> {
        let run = ingest_over_tcp(None, &[0u8; NONCE_LEN], in_rate, writes).await;
        assert!(run.pumped, "an unauthenticated run must still be accepted");
        run.frames
    }

    /// Split a payload the way a real TCP client does: mostly large writes plus
    /// 1-byte writes (which cut a 16-bit sample in half) and odd-size writes
    /// (which cut a frame off the 320-sample grid). The awkward sizes are
    /// forced at the front so they always happen, whatever the payload length.
    fn randomised_writes(payload: &[u8], seed: u64) -> Vec<Vec<u8>> {
        let mut s = seed;
        let mut sizes: Vec<usize> = vec![7, 1, 3, 1, 5, 2];
        let mut planned: usize = sizes.iter().sum();
        while planned < payload.len() {
            let n = ((lcg(&mut s) >> 33) as usize % 1_000) + 1;
            sizes.push(n);
            planned += n;
        }
        let mut writes = Vec::new();
        let mut written = 0usize;
        for size in sizes {
            if written >= payload.len() {
                break;
            }
            let end = (written + size).min(payload.len());
            writes.push(payload[written..end].to_vec());
            written = end;
        }
        writes
    }

    /// The audit requirement: how the TCP stream happened to be split must not
    /// change the frames the pipeline sees. Before the fix the resampler
    /// restarted at phase 0 on every read *and* the resampled tail was fed back
    /// into the input buffer, so every write pattern produced a different
    /// frame sequence.
    #[test]
    fn pump_audio_frames_are_write_boundary_invariant() {
        let _serial = registry_guard();
        retire_current();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime");

        let in_rate = 44_100u32;
        let pcm = deterministic_pcm(in_rate as usize / 2); // 0.5 s, keeps the sleeps short
        let payload = pcm_bytes(&pcm);

        // One single write of the whole stream is the reference.
        let whole = rt.block_on(frames_over_tcp(in_rate, vec![payload.clone()]));
        assert!(
            whole.len() >= 24,
            "0.5 s at 16 kHz should give ~25 frames, got {}",
            whole.len()
        );
        assert!(
            whole.iter().all(|f| f.len() == FRAME_SAMPLES_16K),
            "every emitted frame must be exactly {FRAME_SAMPLES_16K} samples"
        );

        let mut seed = 0xA5A5_1234_5678_9ABCu64;
        for trial in 0..6 {
            let writes = randomised_writes(&payload, seed);
            seed = lcg(&mut seed);
            assert!(
                writes.iter().any(|w| w.len() == 1),
                "the randomised split must include a 1-byte write"
            );
            let chunked = rt.block_on(frames_over_tcp(in_rate, writes.clone()));
            assert_eq!(
                whole,
                chunked,
                "the frame sequence changed with the TCP write sizes (trial {trial}, \
                 {} writes)",
                writes.len()
            );
        }
    }

    /// Odd sample counts and totals that are not a multiple of 320 must not
    /// panic and must never emit a short frame.
    #[test]
    fn odd_length_input_and_tail_frames() {
        let _serial = registry_guard();
        retire_current();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime");

        // Identity path: 481 samples = one 320 frame + a 161-sample remainder
        // that must stay buffered (harmless: the stream just ends).
        let pcm = deterministic_pcm(481);
        let writes = randomised_writes(&pcm_bytes(&pcm), 0x1234_5678);
        assert!(writes.iter().any(|w| w.len() == 1));
        let frames = rt.block_on(frames_over_tcp(16_000, writes));
        assert_eq!(frames.len(), 1, "481 samples is one full frame plus a tail");
        assert_eq!(frames[0].len(), FRAME_SAMPLES_16K);
        assert_eq!(frames[0], pcm[..FRAME_SAMPLES_16K].to_vec());

        // 160 samples at 8 kHz become exactly one 320-sample frame. `push`
        // alone can only emit 318 because its last two samples need the finite
        // stream clamp; EOF must call `finish` or the entire frame disappears.
        let pcm = deterministic_pcm(160);
        let frames = rt.block_on(frames_over_tcp(8_000, vec![pcm_bytes(&pcm)]));
        assert_eq!(
            frames.len(),
            1,
            "EOF must flush the resampler before deciding whether a full frame exists"
        );
        assert_eq!(frames[0].len(), FRAME_SAMPLES_16K);

        // Resampling path with an odd sample count: compare against the
        // streaming resampler fed the whole payload at once.
        let in_rate = 44_100u32;
        let pcm = deterministic_pcm(7_001);
        let mut expected = Vec::new();
        StreamResampler::new(in_rate, PIPELINE_RATE_16K).push(&pcm, &mut expected);
        let writes = randomised_writes(&pcm_bytes(&pcm), 0xFEED_BEEF);
        let frames = rt.block_on(frames_over_tcp(in_rate, writes));

        assert!(
            frames.iter().all(|f| f.len() == FRAME_SAMPLES_16K),
            "no short frame may be emitted"
        );
        assert_eq!(
            frames.len(),
            expected.len() / FRAME_SAMPLES_16K,
            "only whole 20 ms frames may leave the pump"
        );
        assert_eq!(
            frames.iter().flatten().copied().collect::<Vec<i16>>(),
            expected[..frames.len() * FRAME_SAMPLES_16K].to_vec(),
            "the emitted samples must be the resampled stream, in order"
        );
    }

    // --- P0-05: the nonce handshake --------------------------------------

    /// The nonce is an opaque byte string: every byte of the configured token
    /// matters, nothing past it does, and no nonce at all means "accept"
    /// (documented hand-run replay mode).
    #[test]
    fn nonce_comparison_is_prefix_exact_over_the_configured_token() {
        let token = "0123456789abcdef0123456789abcdef";
        assert!(nonce_allowed(Some(token), &nonce_field(token)));

        // A single flipped byte anywhere in the token must reject.
        for i in [0usize, 15, 31] {
            let mut bad = nonce_field(token);
            bad[i] ^= 0x01;
            assert!(!nonce_allowed(Some(token), &bad), "byte {i} must matter");
        }
        // Case matters: this is a byte string, not a parsed number.
        assert!(
            !nonce_allowed(Some(token), &nonce_field(&token.to_uppercase())),
            "the comparison must be case sensitive"
        );
        // Bytes past the token are padding and are ignored.
        let mut padded = nonce_field("deadbeef");
        padded[8..].copy_from_slice(&[0xAB; NONCE_LEN - 8]);
        assert!(
            nonce_allowed(Some("deadbeef"), &padded),
            "the zero-padded tail of the field must be ignored"
        );
        // A token longer than the field only constrains the field.
        let long = "a".repeat(NONCE_LEN + 8);
        assert!(nonce_allowed(Some(&long), &nonce_field(&long[..NONCE_LEN])));
        // No configured nonce: any peer is accepted on purpose.
        assert!(nonce_allowed(None, &[0xFF; NONCE_LEN]));
    }

    #[test]
    fn server_proof_and_accept_ack_are_distinct_protocol_steps() {
        let token = "0123456789abcdef0123456789abcdef";
        let proof_hex = server_proof(Some(token))
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(
            proof_hex, "298915cc82c89566f2313f40fe68ae97681c0d77249acd77a41ebcf58ed01fac",
            "the Rust proof must stay in lockstep with the plugin's SHA-256 self-test"
        );
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime");
        rt.block_on(async {
            let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
            let addr = listener.local_addr().expect("address");
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept");
                write_server_hello(&mut stream, Some(token), HEADER_TIMEOUT)
                    .await
                    .expect("server proof");
                let header = read_header(&mut stream, HEADER_TIMEOUT)
                    .await
                    .expect("header after proof");
                assert!(nonce_allowed(Some(token), &header.nonce));
                write_accept_ack(&mut stream, HEADER_TIMEOUT)
                    .await
                    .expect("accept ACK");
            });

            let mut client = TcpStream::connect(addr).await.expect("connect");
            let mut hello = [0u8; SERVER_HELLO_LEN];
            client.read_exact(&mut hello).await.expect("read proof");
            assert_eq!(&hello[..4], SERVER_HELLO_MAGIC);
            assert_eq!(&hello[4..], &server_proof(Some(token)));
            client
                .write_all(&current_header(16_000, &nonce_field(token)))
                .await
                .expect("write authenticated header");
            let mut ack = [0u8; ACCEPT_ACK.len()];
            client.read_exact(&mut ack).await.expect("read ACK");
            assert_eq!(&ack, ACCEPT_ACK);
            server.await.expect("server task");
        });
    }

    /// The whole point of P0-05: a peer that cannot present the configured
    /// nonce receives no audio and loses the connection. A peer that does
    /// present it is served normally.
    #[test]
    fn a_wrong_nonce_gets_no_audio_and_a_closed_connection() {
        let _serial = registry_guard();
        retire_current();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime");

        let in_rate = 44_100u32;
        let payload = pcm_bytes(&deterministic_pcm(4_410)); // 0.1 s
        let token = "0123456789abcdef0123456789abcdef";

        // Sanity: through the very same path, the right nonce delivers frames.
        let good = rt.block_on(ingest_over_tcp(
            Some(token),
            &nonce_field(token),
            in_rate,
            vec![payload.clone()],
        ));
        assert!(good.pumped, "a matching nonce must be accepted");
        assert!(
            !good.frames.is_empty(),
            "a matching nonce must deliver audio frames"
        );

        // Wrong nonce: the connection must not reach `pump_audio` at all, so not
        // one PCM byte can be delivered, and it must be closed.
        let wrong = rt.block_on(ingest_over_tcp(
            Some(token),
            &nonce_field("ffffffff"),
            in_rate,
            randomised_writes(&payload, 0x7),
        ));
        assert!(
            !wrong.pumped,
            "a mismatching nonce must be rejected before any audio is pumped"
        );
        assert!(
            wrong.frames.is_empty(),
            "no PCM frame at all may reach the pipeline on a nonce mismatch"
        );
        assert!(
            wrong.closed,
            "the rejected connection must be closed, not left hanging"
        );

        // No nonce configured (hand-run replay/testing): accepted deliberately.
        let unauthenticated = rt.block_on(ingest_over_tcp(
            None,
            &nonce_field("this-is-ignored"),
            in_rate,
            vec![payload.clone()],
        ));
        assert!(
            unauthenticated.pumped,
            "without a configured nonce any peer is accepted"
        );
        assert_eq!(
            unauthenticated.frames, good.frames,
            "the unauthenticated stream is the same audio"
        );
    }

    /// A peer that connects and then sends nothing must not hold its slot: the
    /// header read is bounded.
    #[test]
    fn a_silent_peer_cannot_hold_the_connection_slot() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime");
        let timed_out = rt.block_on(async {
            let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
            let addr = listener.local_addr().expect("address");
            let server = tokio::spawn(async move {
                let (mut stream, _peer) = listener.accept().await.expect("accept");
                // Shorter than HEADER_TIMEOUT so the suite stays quick; the
                // production call passes HEADER_TIMEOUT.
                read_header(&mut stream, Duration::from_millis(50))
                    .await
                    .is_err()
            });
            // Connect and then say nothing at all.
            let _client = TcpStream::connect(addr).await.expect("connect");
            server.await.expect("server task")
        });
        assert!(timed_out, "a header that never arrives must time out");
    }

    /// The connection cap: the third simultaneous ingest connection is refused
    /// (and logged), so a flood cannot spawn unbounded tasks.
    #[test]
    fn ingest_connections_are_capped() {
        let held: Vec<ConnectionSlot> = (0..MAX_CONNECTIONS)
            .map(|_| ConnectionSlot::try_acquire().expect("a slot within the cap"))
            .collect();
        assert!(
            ConnectionSlot::try_acquire().is_none(),
            "a connection beyond the cap must be refused"
        );
        drop(held);
        let reused = ConnectionSlot::try_acquire().expect("a released slot is reusable");
        drop(reused);
        assert_eq!(
            CONNECTIONS.load(std::sync::atomic::Ordering::Acquire),
            0,
            "every slot must be returned"
        );
    }
}
