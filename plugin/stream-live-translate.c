/*
 * Stream Live Translate — OBS thin-shell plugin.
 *
 * Responsibilities:
 *   1. Launch (and later terminate) the bundled Rust engine that lives in
 *      this plugin's data directory (data/engine/stream-live-translate).
 *   2. Register the "Live Subtitle Capture" audio filter. The filter taps
 *      the audio of whatever OBS source it is attached to, mixes down to
 *      mono, and streams 16 kHz s16le PCM to the engine's local ingest TCP
 *      port (default 8788).
 *
 * Everything else (VAD / music detection, language detection, LLM, the
 * subtitle overlay and the admin panel) lives in the engine.
 */

#ifdef _WIN32
/* Must come before any windows.h (pulled in by libobs headers). */
#include <winsock2.h>
#include <ws2tcpip.h>
#endif

#include <obs-module.h>
#include <media-io/audio-io.h>
#include <util/platform.h>

#include "version.h"

#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <string.h>
#include <math.h>
#include <time.h>

#ifdef _WIN32
#include <windows.h>
#pragma comment(lib, "ws2_32")
typedef SOCKET slt_sock_t;
#define SLT_INVALID_SOCK INVALID_SOCKET
#else
#include <pthread.h>
#include <unistd.h>
#include <signal.h>
#include <errno.h>
#include <fcntl.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <sys/socket.h>
#include <sys/select.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <arpa/inet.h>
typedef int slt_sock_t;
#define SLT_INVALID_SOCK (-1)
#endif

/* libobs exports this, but the header that declares it (util/threading.h)
 * unconditionally pulls in <pthread.h> and breaks a standalone MSVC build — the
 * same reason os_event_* is forward-declared below. Declared with its real
 * prototype rather than left implicit: an implicit declaration assumes
 * `int f()`, which MSVC flags (C4013) and which is exactly the kind of mismatch
 * that only fails when the calling convention or return type actually differs. */
extern void os_set_thread_name(const char *name);

/* Forward declaration: `slt_connect` below closes a half-open socket on its
 * error paths, but `slt_close` is defined further down. Without this the call
 * was an implicit declaration (`int slt_close(...)`), which then *conflicted*
 * with the real `static void slt_close(...)` definition — the plugin did not
 * compile at all under MSVC (C2371: redefinition with a different base type). */
static void slt_close(slt_sock_t s);

#define SLT_MODULE_NAME "stream-live-translate"
#define SLT_INGEST_PORT_DEFAULT 8788
#define SLT_INGEST_RATE 16000
#define SLT_RMS_GATE 0.0005f

/* Ingest handshake (audit P0-05). The wire header is now
 *
 *     4 bytes   magic "SLTA"
 *     u32 LE    sample rate
 *     u32 LE    format (0 = mono s16le)
 *     32 bytes  nonce: 32 ASCII hex characters, compared byte for byte and
 *               case-sensitively by the engine, zero padded if shorter
 *
 * Before this header, the engine sends "SLTS" plus SHA-256("SLTS" || nonce).
 * After validating the header it sends the fixed 8-byte acknowledgement
 * "SLTAOK01". Only then does the plugin send continuous mono s16le PCM.
 *
 * The nonce is generated once per OBS session in obs_module_load(), handed to
 * the engine on its command line (--ingest-nonce) and echoed back on every
 * connect. A process that merely accepts the connection on the ingest port
 * cannot produce it, so it can never receive audio. */
#define SLT_NONCE_BYTES 32u
#define SLT_HEADER_BYTES (12u + SLT_NONCE_BYTES)
#define SLT_SERVER_HELLO_BYTES (4u + SLT_NONCE_BYTES)
#define SLT_ACCEPT_ACK_BYTES 8u
_Static_assert(sizeof("SLTS") - 1u == 4u, "server hello magic drift");
_Static_assert(sizeof("SLTAOK01") - 1u == SLT_ACCEPT_ACK_BYTES,
	       "ingest ACK length drift");

/* Hard ceiling on the capture sample rate we are willing to resample. It
 * exists only to bound the resampler's output buffer (see
 * resample_and_send): 384 kHz at 16 kHz output means at most 24 output
 * samples per input sample, so a 16 KiB chunk can never produce more than
 * 192 KiB. Real OBS audio outputs are 44.1/48/96 kHz. */
#define SLT_MAX_IN_RATE 384000u

/* Sockets are non-blocking; every wait is done in short bounded slices so
 * that the `stop` event is observed promptly. */
#define SLT_POLL_SLICE_MS 50u

/* Whole-operation budgets for the (already sliced) socket waits. A healthy
 * loopback connect/send is sub-millisecond; these only bound a wedged or
 * unreachable peer. */
#define SLT_CONNECT_TIMEOUT_MS 2000u
#define SLT_HANDSHAKE_TIMEOUT_MS 3000u
#define SLT_SEND_TIMEOUT_MS 3000u

/* How long obs_module_unload() gives the sender thread to notice `stop` and
 * return before it is force-terminated. The sender loop's longest bounded
 * wait is one SLT_SEND_TIMEOUT_MS slice chain plus a 500 ms idle wait. */
#define SLT_JOIN_TIMEOUT_MS 5000u

/* OBS's util/threading.h unconditionally includes <pthread.h> (OBS builds
 * with pthreads on every platform), which breaks standalone MSVC builds of
 * this plugin. We only need the os_event API, which libobs exports, so
 * forward-declare it instead of including the header. */
struct os_event_data;
typedef struct os_event_data os_event_t;
enum os_event_type {
	OS_EVENT_TYPE_AUTO,
	OS_EVENT_TYPE_MANUAL,
};
extern int os_event_init(os_event_t **event, enum os_event_type type);
extern void os_event_destroy(os_event_t *event);
extern int os_event_timedwait(os_event_t *event, unsigned long milliseconds);
extern int os_event_try(os_event_t *event);
extern int os_event_signal(os_event_t *event);

/* OBS's util/threading.h does not expose a plain mutex type (only os_event /
 * os_sem), so wrap the native primitives directly. */
#ifdef _WIN32
typedef CRITICAL_SECTION slt_mutex_t;
#else
typedef pthread_mutex_t slt_mutex_t;
#endif

static void slt_mutex_init(slt_mutex_t *m)
{
#ifdef _WIN32
	InitializeCriticalSection(m);
#else
	pthread_mutex_init(m, NULL);
#endif
}

static void slt_mutex_destroy(slt_mutex_t *m)
{
#ifdef _WIN32
	DeleteCriticalSection(m);
#else
	pthread_mutex_destroy(m);
#endif
}

static void slt_mutex_lock(slt_mutex_t *m)
{
#ifdef _WIN32
	EnterCriticalSection(m);
#else
	pthread_mutex_lock(m);
#endif
}

static void slt_mutex_unlock(slt_mutex_t *m)
{
#ifdef _WIN32
	LeaveCriticalSection(m);
#else
	pthread_mutex_unlock(m);
#endif
}

/* ---------------------------------------------------------------------- */
/* Portable socket error classification                                    */
/* ---------------------------------------------------------------------- */

/* True for the "not ready yet" errors a non-blocking socket reports. */
static bool slt_sock_err_would_block(void)
{
#ifdef _WIN32
	int e = WSAGetLastError();
	return e == WSAEWOULDBLOCK || e == WSAEINPROGRESS ||
	       e == WSAEALREADY || e == WSAEINTR;
#else
	return errno == EAGAIN || errno == EWOULDBLOCK || errno == EINPROGRESS ||
	       errno == EALREADY || errno == EINTR;
#endif
}

/* True while a non-blocking connect() is still in flight. */
static bool slt_sock_err_in_progress(void)
{
#ifdef _WIN32
	int e = WSAGetLastError();
	return e == WSAEWOULDBLOCK || e == WSAEINPROGRESS || e == WSAEALREADY;
#else
	return errno == EINPROGRESS || errno == EALREADY || errno == EWOULDBLOCK;
#endif
}

/* Read-and-clear the pending socket error (0 == connected). */
static int slt_sock_take_error(slt_sock_t s)
{
	int err = 0;
#ifdef _WIN32
	int len = (int)sizeof(err);
	if (getsockopt(s, SOL_SOCKET, SO_ERROR, (char *)&err, &len) != 0)
		return WSAGetLastError();
#else
	socklen_t len = (socklen_t)sizeof(err);
	if (getsockopt(s, SOL_SOCKET, SO_ERROR, &err, &len) != 0)
		return errno;
#endif
	return err;
}

/* ---------------------------------------------------------------------- */
/* Ingest nonce (audit P0-05)                                              */
/* ---------------------------------------------------------------------- */

/* 32 ASCII hex characters + NUL. Written by obs_module_load() before the
 * sender thread exists and read (never written) by the sender thread
 * afterwards, so no lock is needed; the thread creation is the barrier. */
#define SLT_NONCE_LEN SLT_NONCE_BYTES

static char g_nonce[SLT_NONCE_LEN + 1u];
static bool g_nonce_valid = false;
_Static_assert(sizeof(g_nonce) == SLT_NONCE_BYTES + 1u,
	       "nonce buffer must hold exactly 32 hex chars plus NUL");

/* Best-effort 16 random bytes. Order of preference:
 *   1. Windows: BCryptGenRandom, loaded at runtime with GetProcAddress so the
 *      plugin needs no new import library / dependency (kernel32 is already
 *      linked); falls back to SystemFunction036 in advapi32.dll, which is
 *      also already loaded by OBS.
 *      macOS: arc4random_buf().
 *      Other POSIX: getentropy(), then /dev/urandom (all are in libc; no new
 *      dependency).
 * There is deliberately no predictable fallback: without a CSPRNG the plugin
 * fails closed instead of starting an ingest session with forgeable identity.
 * Returns false when no system CSPRNG is available. */
static bool slt_random_bytes(uint8_t *out, size_t len)
{
#ifdef _WIN32
	typedef LONG(WINAPI * slt_bcrypt_gen_t)(void *, unsigned char *,
						unsigned long, unsigned long);
	typedef BOOLEAN(WINAPI * slt_rtl_gen_t)(unsigned char *, unsigned long);

	HMODULE bcrypt = LoadLibraryA("bcrypt.dll");
	if (bcrypt) {
		slt_bcrypt_gen_t gen = (slt_bcrypt_gen_t)(void *)GetProcAddress(
			bcrypt, "BCryptGenRandom");
		/* BCRYPT_USE_SYSTEM_PREFERRED_RNG == 0x00000002 */
		if (gen && gen(NULL, out, (unsigned long)len, 2ul) == 0)
			return true;
	}

	HMODULE advapi = LoadLibraryA("advapi32.dll");
	if (advapi) {
		slt_rtl_gen_t gen = (slt_rtl_gen_t)(void *)GetProcAddress(
			advapi, "SystemFunction036");
		if (gen && gen(out, (unsigned long)len))
			return true;
	}
	return false;
#elif defined(__APPLE__)
	/* arc4random_buf() is provided by libSystem on every supported macOS
	 * version and cannot fail, so no fallback is necessary here. */
	arc4random_buf(out, len);
	return true;
#else
	/* getentropy() is available in glibc >= 2.25. Declaring it weak means
	 * an older libc still links and simply falls through to
	 * /dev/urandom instead of failing to load the plugin. */
#if defined(__GLIBC__)
	extern int getentropy(void *buffer, size_t length) __attribute__((weak));
	if (getentropy && getentropy(out, len) == 0)
		return true;
#else
	if (getentropy(out, len) == 0)
		return true;
#endif

	int fd = open("/dev/urandom", O_RDONLY);
	if (fd >= 0) {
		size_t got = 0;
		while (got < len) {
			ssize_t r = read(fd, out + got, len - got);
			if (r <= 0)
				break;
			got += (size_t)r;
		}
		close(fd);
		if (got == len)
			return true;
	}
	return false;
#endif
}

/* SHA-256 of the one fixed 36-byte server-proof message, "SLTS" || nonce.
 * Keeping this one-block implementation local avoids a new plugin dependency;
 * the Rust peer uses the standard sha2 crate for the same bytes. */
static uint32_t slt_rotr32(uint32_t x, unsigned n)
{
	return (x >> n) | (x << (32u - n));
}

static void slt_server_proof(const uint8_t nonce[SLT_NONCE_BYTES],
			     uint8_t out[32])
{
	static const uint32_t k[64] = {
		0x428a2f98u, 0x71374491u, 0xb5c0fbcfu, 0xe9b5dba5u,
		0x3956c25bu, 0x59f111f1u, 0x923f82a4u, 0xab1c5ed5u,
		0xd807aa98u, 0x12835b01u, 0x243185beu, 0x550c7dc3u,
		0x72be5d74u, 0x80deb1feu, 0x9bdc06a7u, 0xc19bf174u,
		0xe49b69c1u, 0xefbe4786u, 0x0fc19dc6u, 0x240ca1ccu,
		0x2de92c6fu, 0x4a7484aau, 0x5cb0a9dcu, 0x76f988dau,
		0x983e5152u, 0xa831c66du, 0xb00327c8u, 0xbf597fc7u,
		0xc6e00bf3u, 0xd5a79147u, 0x06ca6351u, 0x14292967u,
		0x27b70a85u, 0x2e1b2138u, 0x4d2c6dfcu, 0x53380d13u,
		0x650a7354u, 0x766a0abbu, 0x81c2c92eu, 0x92722c85u,
		0xa2bfe8a1u, 0xa81a664bu, 0xc24b8b70u, 0xc76c51a3u,
		0xd192e819u, 0xd6990624u, 0xf40e3585u, 0x106aa070u,
		0x19a4c116u, 0x1e376c08u, 0x2748774cu, 0x34b0bcb5u,
		0x391c0cb3u, 0x4ed8aa4au, 0x5b9cca4fu, 0x682e6ff3u,
		0x748f82eeu, 0x78a5636fu, 0x84c87814u, 0x8cc70208u,
		0x90befffau, 0xa4506cebu, 0xbef9a3f7u, 0xc67178f2u,
	};
	uint8_t block[64] = {0};
	uint32_t w[64];
	uint32_t h[8] = {0x6a09e667u, 0xbb67ae85u, 0x3c6ef372u,
			 0xa54ff53au, 0x510e527fu, 0x9b05688cu,
			 0x1f83d9abu, 0x5be0cd19u};

	memcpy(block, "SLTS", 4);
	memcpy(block + 4, nonce, SLT_NONCE_BYTES);
	block[36] = 0x80u;
	block[62] = 0x01u; /* 36 bytes == 288 bits == 0x0120 */
	block[63] = 0x20u;
	for (size_t i = 0; i < 16; i++)
		w[i] = ((uint32_t)block[i * 4] << 24) |
		       ((uint32_t)block[i * 4 + 1] << 16) |
		       ((uint32_t)block[i * 4 + 2] << 8) |
		       (uint32_t)block[i * 4 + 3];
	for (size_t i = 16; i < 64; i++) {
		uint32_t s0 = slt_rotr32(w[i - 15], 7) ^
			      slt_rotr32(w[i - 15], 18) ^ (w[i - 15] >> 3);
		uint32_t s1 = slt_rotr32(w[i - 2], 17) ^
			      slt_rotr32(w[i - 2], 19) ^ (w[i - 2] >> 10);
		w[i] = w[i - 16] + s0 + w[i - 7] + s1;
	}

	uint32_t a = h[0], b = h[1], c = h[2], d = h[3];
	uint32_t e = h[4], f = h[5], g = h[6], x = h[7];
	for (size_t i = 0; i < 64; i++) {
		uint32_t s1 = slt_rotr32(e, 6) ^ slt_rotr32(e, 11) ^
			      slt_rotr32(e, 25);
		uint32_t ch = (e & f) ^ ((~e) & g);
		uint32_t t1 = x + s1 + ch + k[i] + w[i];
		uint32_t s0 = slt_rotr32(a, 2) ^ slt_rotr32(a, 13) ^
			      slt_rotr32(a, 22);
		uint32_t maj = (a & b) ^ (a & c) ^ (b & c);
		uint32_t t2 = s0 + maj;
		x = g; g = f; f = e; e = d + t1;
		d = c; c = b; b = a; a = t1 + t2;
	}
	h[0] += a; h[1] += b; h[2] += c; h[3] += d;
	h[4] += e; h[5] += f; h[6] += g; h[7] += x;
	for (size_t i = 0; i < 8; i++) {
		out[i * 4] = (uint8_t)(h[i] >> 24);
		out[i * 4 + 1] = (uint8_t)(h[i] >> 16);
		out[i * 4 + 2] = (uint8_t)(h[i] >> 8);
		out[i * 4 + 3] = (uint8_t)h[i];
	}
}

static bool slt_server_proof_selftest(void)
{
	static const char nonce[] = "0123456789abcdef0123456789abcdef";
	_Static_assert(sizeof(nonce) - 1u == SLT_NONCE_BYTES,
		       "server proof test nonce length drift");
	static const uint8_t expected[32] = {
		0x29, 0x89, 0x15, 0xcc, 0x82, 0xc8, 0x95, 0x66,
		0xf2, 0x31, 0x3f, 0x40, 0xfe, 0x68, 0xae, 0x97,
		0x68, 0x1c, 0x0d, 0x77, 0x24, 0x9a, 0xcd, 0x77,
		0xa4, 0x1e, 0xbc, 0xf5, 0x8e, 0xd0, 0x1f, 0xac,
	};
	uint8_t actual[32];
	uint8_t difference = 0;
	slt_server_proof((const uint8_t *)nonce, actual);
	for (size_t i = 0; i < sizeof(actual); i++)
		difference |= actual[i] ^ expected[i];
	return difference == 0;
}

/* Fill g_nonce with 32 lowercase ASCII hex characters. Fails closed when the
 * system CSPRNG is unavailable. */
static bool slt_nonce_generate(void)
{
	static const char hex[] = "0123456789abcdef";
	/* Two hex characters are emitted per random byte. */
	uint8_t raw[SLT_NONCE_BYTES / 2u];
	_Static_assert(sizeof(raw) * 2u == SLT_NONCE_LEN,
		       "nonce entropy must fit the hex buffer exactly");
	memset(g_nonce, 0, sizeof(g_nonce));
	g_nonce_valid = false;
	if (!slt_random_bytes(raw, sizeof(raw)))
		return false;

	for (size_t i = 0; i < sizeof(raw); i++) {
		g_nonce[i * 2u] = hex[(raw[i] >> 4) & 0x0Fu];
		g_nonce[i * 2u + 1u] = hex[raw[i] & 0x0Fu];
	}
	g_nonce[SLT_NONCE_LEN] = '\0';
	g_nonce_valid = true;
	return true;
}

/* ---------------------------------------------------------------------- */
/* Shared sender state                                                     */
/* ---------------------------------------------------------------------- */

/* Ring buffer of s16le mono PCM captured at the OBS audio output rate.
 * 2 MiB holds ~22 s at 48 kHz; overflow drops the newest data, which only
 * happens if the engine is unreachable for a long time anyway. */
#define SLT_RING_BYTES (2 * 1024 * 1024)

/* Cross-thread ownership rules (there must be no unsynchronised read/write
 * pair anywhere):
 *
 *   mutex  protects every mutable field below. The critical sections are all
 *          short (index arithmetic / a few scalar copies); the lock is NEVER
 *          held across a socket call, an event wait or a blog() call.
 *
 *   ring/head/tail/used
 *          written by the OBS audio-filter thread(s) in filter_audio, read
 *          and drained by the sender thread. Mutex. This was already so.
 *
 *   port           written by the filter thread (filter_create/filter_update),
 *                  read by the sender thread. Mutex.
 *   reconnect_requested
 *                  a plain bool, written by the filter thread, read/cleared by
 *                  the sender thread. Mutex. It used to be `volatile bool`,
 *                  which is not an atomic and gave no happens-before edge;
 *                  `volatile` was dropped so a bare access is a compile error.
 *
 *   in_rate        written by the filter thread when it detects a genuine rate
 *                  change, read by the sender thread. Mutex.
 *   resample_pos / resample_carry / resample_have_carry
 *                  the persistent resampler state. The sender thread is the
 *                  owner (it advances it); the filter thread only *resets* it
 *                  when the input rate changes or a new stream starts, under
 *                  the mutex. resample_gen is the reset epoch: the sender
 *                  snapshots it, and writes its advanced state back only if
 *                  the epoch is unchanged, so a reset that lands mid-send is
 *                  never clobbered by stale state.
 *
 *   peer_rejected  written by the sender thread when the ingest handshake is
 *                  refused, read by the filter thread (to skip capture) and
 *                  by the UI thread (filter_ingest_status_text). Mutex.
 *
 *   sock           owned exclusively by the sender thread (connect, handshake,
 *                  send, close). It is deliberately NOT reachable from the
 *                  filter thread any more, which removes the old cross-thread
 *                  create/close races on it.
 *
 *   engine_proc / job / engine_pid
 *                  owned exclusively by the sender thread (engine_alive /
 *                  engine_ensure_running / engine_spawn), except that
 *                  engine_terminate() touches them from obs_module_unload
 *                  *after* the sender thread has been joined and terminated.
 *                  No lock is needed under that ordering rule; engine_spawn()
 *                  is likewise only called from the sender thread.
 *
 *   wake / stop    plain os_events; os_event_* is internally synchronised.
 *   g_sender_started / g_thread
 *                  written by the loader thread before the sender thread
 *                  exists and read by the unloader thread after signalling
 *                  `stop`; the os_event signal/join supplies the ordering.
 */
struct slt_sender {
	slt_mutex_t mutex;
	os_event_t *wake;
	os_event_t *stop;
	uint8_t *ring;
	size_t head; /* write position */
	size_t tail; /* read position  */
	size_t used;

	uint32_t port;
	bool reconnect_requested;
	uint32_t in_rate; /* sample rate the ring is filled at */

	/* Exact linear-resampler state, carried across calls (see
	 * resample_and_send). resample_pos is the offset of the next output
	 * grid point relative to the current chunk's in[0], in input samples;
	 * resample_carry is the input sample immediately before in[0]. The grid
	 * is derivable from the integer output counter, so it cannot drift, and
	 * only a whole-step offset is carried, so a chunk boundary cannot shift
	 * it. The pair is what makes the output independent of how the ring is
	 * drained. */
	double resample_pos;
	int16_t resample_carry;
	bool resample_have_carry;
	uint32_t resample_gen; /* reset epoch, see above */

	bool peer_rejected; /* ingest peer failed the identity check */

#ifdef _WIN32
	HANDLE engine_proc;
	HANDLE job;
	WSADATA wsa;
#else
	pid_t engine_pid;
#endif
};

static struct slt_sender g_sender;
static volatile bool g_sender_started = false;
#ifdef _WIN32
static HANDLE g_thread;
#else
static pthread_t g_thread;
#endif

/* ---------------------------------------------------------------------- */
/* Ring buffer helpers (caller holds g_sender.mutex)                       */
/* ---------------------------------------------------------------------- */

static void ring_push(const uint8_t *data, size_t len)
{
	if (len == 0)
		return;
	if (len > SLT_RING_BYTES - g_sender.used) {
		/* Engine is not draining; drop instead of building latency. */
		return;
	}
	size_t first = SLT_RING_BYTES - g_sender.head;
	if (first > len)
		first = len;
	memcpy(g_sender.ring + g_sender.head, data, first);
	if (len > first)
		memcpy(g_sender.ring, data + first, len - first);
	g_sender.head = (g_sender.head + len) % SLT_RING_BYTES;
	g_sender.used += len;
}

static size_t ring_pop(uint8_t *out, size_t max_len)
{
	size_t len = g_sender.used < max_len ? g_sender.used : max_len;
	if (len == 0)
		return 0;
	size_t first = SLT_RING_BYTES - g_sender.tail;
	if (first > len)
		first = len;
	memcpy(out, g_sender.ring + g_sender.tail, first);
	if (len > first)
		memcpy(out + first, g_sender.ring, len - first);
	g_sender.tail = (g_sender.tail + len) % SLT_RING_BYTES;
	g_sender.used -= len;
	return len;
}

/* ---------------------------------------------------------------------- */
/* Portable TCP helper                                                     */
/*                                                                         */
/* The socket is put in NON-BLOCKING mode and every wait is done in short  */
/* bounded slices, with three independent escape hatches:                  */
/*                                                                         */
/*   1. the `stop` event, so obs_module_unload() can always reclaim the    */
/*      sender thread;                                                     */
/*   2. a per-operation total deadline (SLT_CONNECT_TIMEOUT_MS /           */
/*      SLT_SEND_TIMEOUT_MS), so a peer that accepts the connection and    */
/*      then never reads cannot wedge the thread;                          */
/*   3. a per-slice wait, so the loop can never block indefinitely.        */
/*                                                                         */
/* Why not SO_SNDTIMEO: it only bounds a single send() call, and a         */
/* blocked send() is not interruptible by an event, so `stop` could not    */
/* wake the thread. Why not a second thread or closesocket() from the      */
/* unloader to break the send: closesocket() on a socket another thread is */
/* blocked in is undefined on Windows and causes fd reuse races on POSIX,   */
/* and the unloader must not touch state the sender still owns.            */
/* Why select() on both platforms rather than WSAPoll/WSAEventSelect:      */
/* select() is the one readiness API that is identical in shape on Winsock */
/* and on POSIX (poll() does not exist in Winsock, WSAPoll does not exist  */
/* on POSIX), so this file needs exactly one implementation.               */
/* ---------------------------------------------------------------------- */

static uint64_t slt_now_ms(void)
{
#ifdef _WIN32
	return (uint64_t)GetTickCount64();
#else
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return (uint64_t)ts.tv_sec * 1000u + (uint64_t)(ts.tv_nsec / 1000000);
#endif
}

/* True once obs_module_unload() has asked the sender thread to stop. The
 * stop event is a manual-reset event, so the signal stays latched. */
static bool slt_stop_requested(void)
{
	if (!g_sender.stop)
		return false;
	return os_event_try(g_sender.stop) == 0;
}

/* Sleep for at most SLT_POLL_SLICE_MS, but return early if `stop` is
 * signalled. Used to pace the polling loop on the POSIX side. */
static void slt_abort_wait_tick(void)
{
	if (!g_sender.stop) {
#ifdef _WIN32
		Sleep(SLT_POLL_SLICE_MS);
#else
		struct timespec ts;
		ts.tv_sec = 0;
		ts.tv_nsec = (long)SLT_POLL_SLICE_MS * 1000000L;
		nanosleep(&ts, NULL);
#endif
		return;
	}
	os_event_timedwait(g_sender.stop, SLT_POLL_SLICE_MS);
}

/* Wait until the socket is ready in the given direction, the `stop` event is
 * signalled, the total deadline passes, or the wait would exceed the
 * remaining budget. Returns 1 ready, 0 timeout/not-ready, -1 error. */
static int slt_sock_wait(slt_sock_t s, bool want_write, uint64_t deadline)
{
	for (;;) {
		if (slt_stop_requested())
			return 0;
		uint64_t now = slt_now_ms();
		if (now >= deadline)
			return 0;
		uint64_t remaining = deadline - now;
		uint64_t slice = SLT_POLL_SLICE_MS;
		if (slice > remaining)
			slice = remaining;

		fd_set fds;
		struct timeval tv;
		FD_ZERO(&fds);
		FD_SET(s, &fds);
		tv.tv_sec = (long)(slice / 1000u);
		tv.tv_usec = (long)((slice % 1000u) * 1000u);
		int rc = select((int)(s + 1), want_write ? NULL : &fds,
				want_write ? &fds : NULL, NULL, &tv);
		if (rc > 0)
			return 1;
		if (rc == 0)
			continue; /* slice expired, re-check stop/deadline */
		if (!slt_sock_err_would_block()) {
			/* select() itself failed (e.g. WSAENOTSOCK after the peer
			 * reset). Not retryable. */
			return -1;
		}
		slt_abort_wait_tick();
	}
}

/* Bounded connect + handshake. */
static slt_sock_t slt_connect(uint16_t port)
{
	slt_sock_t s = socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
	if (s == SLT_INVALID_SOCK)
		return SLT_INVALID_SOCK;

#ifndef _WIN32
#ifdef SO_NOSIGPIPE
	/* macOS / BSD sockets have no MSG_NOSIGNAL; instead disable SIGPIPE
	 * per-socket so a broken connection can't kill the plugin process. */
	int nosig = 1;
	setsockopt(s, SOL_SOCKET, SO_NOSIGPIPE, &nosig, sizeof(nosig));
#endif
#endif

	struct sockaddr_in addr;
	memset(&addr, 0, sizeof(addr));
	addr.sin_family = AF_INET;
	addr.sin_port = htons(port);
	addr.sin_addr.s_addr = htonl(INADDR_LOOPBACK);

#ifdef _WIN32
	u_long nb = 1;
	if (ioctlsocket(s, FIONBIO, &nb) != 0) {
		closesocket(s);
		return SLT_INVALID_SOCK;
	}
#else
	int flags = fcntl(s, F_GETFL, 0);
	if (flags < 0 || fcntl(s, F_SETFL, flags | O_NONBLOCK) < 0) {
		close(s);
		return SLT_INVALID_SOCK;
	}
#endif

	const uint64_t deadline = slt_now_ms() + SLT_CONNECT_TIMEOUT_MS;
	if (connect(s, (struct sockaddr *)&addr, sizeof(addr)) != 0) {
		if (!slt_sock_err_in_progress()) {
			slt_close(s);
			return SLT_INVALID_SOCK;
		}
		if (slt_sock_wait(s, true, deadline) != 1) {
			slt_close(s);
			return SLT_INVALID_SOCK;
		}
		/* select() reporting writable only means "the connect
		 * finished"; SO_ERROR says whether it succeeded. */
		if (slt_sock_take_error(s) != 0) {
			slt_close(s);
			return SLT_INVALID_SOCK;
		}
	}

	int one = 1;
	setsockopt(s, IPPROTO_TCP, TCP_NODELAY, (const char *)&one,
		   sizeof(one));
	return s;
}

static void slt_close(slt_sock_t s)
{
	if (s == SLT_INVALID_SOCK)
		return;
#ifdef _WIN32
	closesocket(s);
#else
	close(s);
#endif
}

/* Bounded, cancellable send-all. Returns false on error, on the total
 * deadline expiring, or as soon as `stop` is signalled. */
static bool slt_send_all(slt_sock_t s, const uint8_t *buf, size_t len)
{
	size_t off = 0;
	const uint64_t deadline = slt_now_ms() + SLT_SEND_TIMEOUT_MS;

	if (s == SLT_INVALID_SOCK)
		return false;

	while (off < len) {
		if (slt_stop_requested())
			return false;
		if (slt_now_ms() >= deadline) {
			blog(LOG_WARNING,
			     "[SLT] ingest send timed out after %u ms (%zu of %zu bytes left); dropping the connection",
			     (unsigned)SLT_SEND_TIMEOUT_MS, len - off, len);
			return false;
		}

#ifdef _WIN32
		int n = send(s, (const char *)buf + off, (int)(len - off), 0);
#else
		/* MSG_NOSIGNAL is a Linux/glibc extension and is undefined on
		 * macOS / BSD; there we rely on SO_NOSIGPIPE (set in
		 * slt_connect) to avoid SIGPIPE. */
#ifdef MSG_NOSIGNAL
		ssize_t n = send(s, buf + off, len - off, MSG_NOSIGNAL);
#else
		ssize_t n = send(s, buf + off, len - off, 0);
#endif
#endif
		if (n > 0) {
			off += (size_t)n;
			continue;
		}
		if (n < 0 && slt_sock_err_would_block()) {
			/* Send buffer full: the peer accepted but is not
			 * reading. Wait for writability in bounded slices so
			 * `stop` can still abort us. */
			int w = slt_sock_wait(s, true, deadline);
			if (w == 1)
				continue;
			if (w < 0)
				blog(LOG_WARNING,
				     "[SLT] ingest socket wait failed; dropping the connection");
			return false;
		}
		return false; /* hard socket error */
	}
	return true;
}

/* Bounded, cancellable receive-exactly. Handshake reads use their own short
 * deadline so a fake service that accepts but stays silent cannot hold the
 * sender thread or obtain audio. */
static bool slt_recv_all(slt_sock_t s, uint8_t *buf, size_t len)
{
	size_t off = 0;
	const uint64_t deadline = slt_now_ms() + SLT_HANDSHAKE_TIMEOUT_MS;

	if (s == SLT_INVALID_SOCK)
		return false;

	while (off < len) {
		if (slt_stop_requested() || slt_now_ms() >= deadline)
			return false;
#ifdef _WIN32
		int n = recv(s, (char *)buf + off, (int)(len - off), 0);
#else
		ssize_t n = recv(s, buf + off, len - off, 0);
#endif
		if (n > 0) {
			off += (size_t)n;
			continue;
		}
		if (n == 0)
			return false;
		if (slt_sock_err_would_block()) {
			if (slt_sock_wait(s, false, deadline) == 1)
				continue;
		}
		return false;
	}
	return true;
}

/* ---------------------------------------------------------------------- */
/* Ingest peer identity                                                    */
/* ---------------------------------------------------------------------- */

/* The ONLY place that decides whether a freshly connected ingest peer is the
 * engine we expect. The engine proves knowledge of the launch nonce before the
 * plugin sends its header, so a process squatting on the port cannot learn the
 * nonce and never receives PCM. */
static bool slt_peer_is_trusted(slt_sock_t s, uint16_t port)
{
	UNUSED_PARAMETER(port);

	if (!g_nonce_valid) {
		blog(LOG_WARNING,
		     "[SLT] refusing to stream: no ingest nonce is available, so the engine cannot be authenticated");
		return false;
	}

	uint8_t hello[SLT_SERVER_HELLO_BYTES];
	uint8_t expected[32];
	if (!slt_recv_all(s, hello, sizeof(hello)))
		return false;
	if (memcmp(hello, "SLTS", 4) != 0)
		return false;
	slt_server_proof((const uint8_t *)g_nonce, expected);
	uint8_t difference = 0;
	for (size_t i = 0; i < sizeof(expected); i++)
		difference |= hello[4 + i] ^ expected[i];
	return difference == 0;
}

/* Build the 44-byte ingest header: the original 12 bytes followed by the
 * 32-byte ASCII hex nonce. The first 12 bytes are byte-identical to what the
 * engine has always parsed, which is what keeps the wire format additive. */
static void slt_build_header(uint8_t out[SLT_HEADER_BYTES])
{
	memcpy(out, "SLTA", 4);
	const uint32_t rate = SLT_INGEST_RATE;
	const uint32_t fmt = 0; /* mono s16le */
	memcpy(out + 4, &rate, 4);
	memcpy(out + 8, &fmt, 4);

	memset(out + 12, 0, SLT_NONCE_BYTES);
	if (g_nonce_valid)
		memcpy(out + 12, g_nonce, SLT_NONCE_LEN);
}

/* Connect to the engine ingest port, send the 44-byte SLTA header (12 bytes
 * layout + 32-byte nonce) and require the peer to pass the identity check.
 * Returns the connected socket, or SLT_INVALID_SOCK (the socket is closed on
 * every failure path).
 *
 * Note that engine_already_running() is deliberately NOT used as the identity
 * proof: all it establishes is that *something* accepts a TCP connection on
 * the port. */
static slt_sock_t slt_connect_engine(uint16_t port, bool *out_rejected)
{
	*out_rejected = false;

	slt_sock_t s = slt_connect(port);
	if (s == SLT_INVALID_SOCK)
		return SLT_INVALID_SOCK;

	if (!slt_peer_is_trusted(s, port)) {
		/* Stay silent: neither the nonce header nor PCM is disclosed to an
		 * untrusted listener. */
		*out_rejected = true;
		slt_close(s);
		return SLT_INVALID_SOCK;
	}

	uint8_t header[SLT_HEADER_BYTES];
	slt_build_header(header);
	if (!slt_send_all(s, header, sizeof(header))) {
		slt_close(s);
		return SLT_INVALID_SOCK;
	}

	uint8_t ack[SLT_ACCEPT_ACK_BYTES];
	if (!slt_recv_all(s, ack, sizeof(ack)) ||
	    memcmp(ack, "SLTAOK01", sizeof(ack)) != 0) {
		*out_rejected = true;
		slt_close(s);
		return SLT_INVALID_SOCK;
	}

	return s;
}

/* ---------------------------------------------------------------------- */
/* Engine process management                                               */
/* ---------------------------------------------------------------------- */

/* NOTE (audit P0-05): this is NOT an identity check. It only proves that
 * *something* accepts a TCP connection on the port — a port squatter, another
 * app or a stale engine all satisfy it. It is still useful to avoid spawning a
 * duplicate engine, but the plugin must never treat it as proof that the peer
 * is ours: that proof is slt_peer_is_trusted(), applied in
 * slt_connect_engine() before a single audio byte is sent. */
static bool engine_already_running(uint16_t port)
{
	slt_sock_t s = slt_connect(port);
	if (s == SLT_INVALID_SOCK)
		return false;
	slt_close(s);
	return true;
}

static void engine_spawn(void)
{
#ifdef _WIN32
	const char *exe = obs_module_file("engine/stream-live-translate.exe");
#else
	const char *exe = obs_module_file("engine/stream-live-translate");
#endif
	if (!exe) {
		blog(LOG_WARNING, "[SLT] bundled engine binary not found");
		return;
	}

	if (engine_already_running(SLT_INGEST_PORT_DEFAULT)) {
		blog(LOG_INFO,
		     "[SLT] engine already running, not spawning a second one");
		bfree((void *)exe);
		return;
	}

	char exe_copy[1024];
	snprintf(exe_copy, sizeof(exe_copy), "%s", exe);

#ifdef _WIN32
	/* The nonce travels on the command line. It is visible in the process
	 * list of this machine only, which is the same trust domain the whole
	 * plugin/engine pair already runs in; the handshake exists to stop an
	 * unrelated process that merely squats on the ingest port from being
	 * fed audio, not to defend against a local attacker reading our own
	 * process memory. */
	char cmdline[2048];
	snprintf(cmdline, sizeof(cmdline),
		 "\"%s\" --audio-mode obs_filter --ingest-nonce %s", exe_copy,
		 g_nonce_valid ? g_nonce : "");

	/* Run the engine from its own directory and capture its console
	 * output into engine.log next to the binary: the engine has no
	 * console window, so without this a startup crash would be
	 * completely silent. */
	char exedir[1024];
	snprintf(exedir, sizeof(exedir), "%s", exe_copy);
	char *sl = strrchr(exedir, '\\');
	if (!sl)
		sl = strrchr(exedir, '/');
	if (sl)
		*sl = '\0';

	char logpath[1200];
	snprintf(logpath, sizeof(logpath), "%s\\engine.log", exedir);

	SECURITY_ATTRIBUTES sa;
	memset(&sa, 0, sizeof(sa));
	sa.nLength = sizeof(sa);
	sa.bInheritHandle = TRUE;
	HANDLE logfile = CreateFileA(logpath, GENERIC_WRITE, FILE_SHARE_READ,
				     &sa, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL,
				     NULL);

	STARTUPINFOA si;
	PROCESS_INFORMATION pi;
	memset(&si, 0, sizeof(si));
	si.cb = sizeof(si);
	memset(&pi, 0, sizeof(pi));
	if (logfile != INVALID_HANDLE_VALUE) {
		si.dwFlags = STARTF_USESTDHANDLES;
		si.hStdInput = GetStdHandle(STD_INPUT_HANDLE);
		si.hStdOutput = logfile;
		si.hStdError = logfile;
	}

	if (!CreateProcessA(NULL, cmdline, NULL, NULL, TRUE, CREATE_NO_WINDOW,
			    NULL, exedir, &si, &pi)) {
		blog(LOG_WARNING, "[SLT] failed to spawn engine (err %lu)",
		     GetLastError());
		if (logfile != INVALID_HANDLE_VALUE)
			CloseHandle(logfile);
		bfree((void *)exe);
		return;
	}
	if (logfile != INVALID_HANDLE_VALUE)
		CloseHandle(logfile);
	CloseHandle(pi.hThread);
	g_sender.engine_proc = pi.hProcess;

	/* Kill the engine automatically if OBS crashes or is killed. */
	g_sender.job = CreateJobObjectA(NULL, NULL);
	if (g_sender.job) {
		JOBOBJECT_EXTENDED_LIMIT_INFORMATION info;
		memset(&info, 0, sizeof(info));
		info.BasicLimitInformation.LimitFlags =
			JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
		SetInformationJobObject(g_sender.job,
					JobObjectExtendedLimitInformation,
					&info, sizeof(info));
		AssignProcessToJobObject(g_sender.job, pi.hProcess);
	}

	blog(LOG_INFO, "[SLT] engine spawned (pid %lu)",
	     GetProcessId(pi.hProcess));
#else
	pid_t pid = fork();
	if (pid < 0) {
		blog(LOG_WARNING, "[SLT] fork failed: %s", strerror(errno));
		bfree((void *)exe);
		return;
	}
	if (pid == 0) {
		setsid();
		/* argv is inherited across exec, so the nonce string stays
		 * valid in the child without any copying. */
		execl(exe_copy, exe_copy, "--audio-mode", "obs_filter",
		      "--ingest-nonce", g_nonce_valid ? g_nonce : "",
		      (char *)NULL);
		_exit(127);
	}
	g_sender.engine_pid = pid;
	blog(LOG_INFO, "[SLT] engine spawned (pid %d)", (int)pid);
#endif

	bfree((void *)exe);
}

static void engine_terminate(void)
{
#ifdef _WIN32
	if (g_sender.engine_proc) {
		TerminateProcess(g_sender.engine_proc, 0);
		CloseHandle(g_sender.engine_proc);
		g_sender.engine_proc = NULL;
	}
	if (g_sender.job) {
		CloseHandle(g_sender.job); /* kills remaining job members */
		g_sender.job = NULL;
	}
#else
	if (g_sender.engine_pid > 0) {
		kill(g_sender.engine_pid, SIGTERM);
		g_sender.engine_pid = -1;
	}
#endif
}

static bool engine_alive(void)
{
#ifdef _WIN32
	if (!g_sender.engine_proc)
		return false;
	return WaitForSingleObject(g_sender.engine_proc, 0) == WAIT_TIMEOUT;
#else
	if (g_sender.engine_pid <= 0)
		return false;
	int st;
	pid_t r = waitpid(g_sender.engine_pid, &st, WNOHANG);
	if (r == 0)
		return true; /* still running */
	g_sender.engine_pid = -1; /* exited and reaped */
	return false;
#endif
}

/* Called periodically from the sender thread: if the engine died (e.g.
 * crashed at startup), respawn it instead of silently streaming nowhere.
 *
 * engine_proc/job/engine_pid belong to the sender thread (see the ownership
 * table), so no lock is needed here; obs_module_unload() only touches them
 * after the sender thread has been joined and terminated. `last_try` is a
 * function-local static and likewise only ever runs on the sender thread. */
static void engine_ensure_running(void)
{
#ifdef _WIN32
	static DWORD last_try = 0;
	DWORD now = GetTickCount();
#else
	static time_t last_try = 0;
	time_t now = time(NULL);
#endif
	if (engine_alive())
		return;
#ifdef _WIN32
	if (now - last_try < 10000)
		return;
	last_try = now;
	if (g_sender.engine_proc) {
		CloseHandle(g_sender.engine_proc);
		g_sender.engine_proc = NULL;
	}
#else
	if (now - last_try < 10)
		return;
	last_try = now;
#endif
	blog(LOG_WARNING, "[SLT] engine is not running; (re)spawning it");
	engine_spawn();
}

/* ---------------------------------------------------------------------- */
/* Resampler (linear, fully persistent across calls)                       */
/* ---------------------------------------------------------------------- */

/* Resample one drained chunk to 16 kHz s16le and send it over `s`.
 *
 * State carried across calls (all under g_sender.mutex, see the ownership
 * table above). The anchor of the resampling grid is the input sample
 * immediately before this chunk, `resample_carry`, and the grid itself is
 * carried as a whole-step offset `resample_pos`:
 *
 *   resample_carry   the previous chunk's last input sample. Without it, the
 *                    first output sample of a chunk was interpolated against
 *                    in[1] instead of the true preceding sample, so the output
 *                    depended on where the ring happened to be drained. With
 *                    it the interpolation is continuous across boundaries.
 *   resample_pos     the offset (< one grid step) of the next output grid
 *                    point from this chunk's in[0]. Only the offset is
 *                    carried - never a running position - so a chunk boundary
 *                    cannot shift the grid, and the position is recomputed
 *                    from the integer output counter so it cannot drift
 *                    (see the derivation in the function body).
 *
 * Only a change of the input rate or the beginning of a new stream resets
 * this state; a reconnect must NOT (the audio thread keeps filling the ring
 * at the same rate, so throwing the phase away would put a discontinuity into
 * the stream).
 *
 * Returns the number of input samples consumed (always n), or 0 if the
 * stream could not be fully sent. The caller only uses 0 as "the connection
 * is broken".
 */
static size_t resample_and_send(slt_sock_t s, const int16_t *in, size_t n)
{
	bool identity;
	uint32_t gen;
	uint32_t in_rate;
	int16_t carry = 0;
	bool have_carry;

	if (n < 2 || s == SLT_INVALID_SOCK)
		return 0; /* one sample yields no interpolatable output */

	/* Snapshot the whole resampler state in one critical section; the
	 * audio thread may reset any of it (rate change) at any moment. */
	slt_mutex_lock(&g_sender.mutex);
	in_rate = g_sender.in_rate;
	identity = (in_rate == SLT_INGEST_RATE);
	gen = g_sender.resample_gen;
	carry = g_sender.resample_carry;
	have_carry = g_sender.resample_have_carry;
	slt_mutex_unlock(&g_sender.mutex);

	if (identity) {
		/* No interpolation, so there is no resampler state that
		 * matters. The old code reset the position to 0 on this path
		 * (and on every reconnect), which threw the fractional phase
		 * away for no reason; a rate change is the only thing that
		 * may invalidate the phase. */
		return slt_send_all(s, (const uint8_t *)in,
				    n * sizeof(int16_t))
			       ? n
			       : 0;
	}

	/* --- grid arithmetic -------------------------------------------------
	 * The output grid is x_k = (k + 1) * step input samples, k = 0, 1, 2,
	 * ...: the same walk the original code used (it interpolated between
	 * in[i0 - 1] and in[i0] with i0 = (size_t)((k + 1) * step) and
	 * t = frac), anchored so that the first output is exactly in[0].
	 *
	 * `p` is a DOUBLE, but it is recomputed from the integer counter
	 * `produced` on every iteration instead of being accumulated with
	 * `p += step`. That matters: an accumulator drifts by one ULP per
	 * sample, and over a few hundred thousand samples the drift changed
	 * both the phase and the number of emitted samples depending on how the
	 * ring happened to be drained. Recomputing keeps the phase a pure
	 * function of the stream position.
	 *
	 * What crosses a call boundary is only `next_p`, the offset of the next
	 * grid point from the next chunk's in[0], reduced into [0, step). A
	 * bounded offset (never a running position) is what keeps the grid
	 * stable: the next chunk continues at the grid point the previous one
	 * stopped before, instead of restarting the phase. */
	double next_p;
	if (have_carry)
		next_p = g_sender.resample_pos; /* offset of the next grid point */
	else
		next_p = 0.0; /* brand-new stream: first output at in[0] */

	/* Input samples per output sample (>= 1 for a downsample). */
	const double step = (double)in_rate / (double)SLT_INGEST_RATE;

	/* Size the output buffer so that no write can exceed it: within a chunk
	 * of n samples the grid points are p <= n - 1 and they are spaced by
	 * step, so at most ceil((n - 1) * 16000 / in_rate) + 1 are emitted.
	 * Computed in integers because the floating-point estimate n / step was
	 * too small for several real rates (e.g. 44.1 kHz). The old
	 * `int16_t out[4096]` overflowed the stack for a 16 KiB chunk (8192
	 * samples) at any step < 2, e.g. an 8 kHz input, where this bound is
	 * 2n. */
	const uint64_t cap64 =
		(((uint64_t)n - 1u) * (uint64_t)SLT_INGEST_RATE) /
			(uint64_t)in_rate +
		2u;
	const size_t cap = (size_t)cap64;
	int16_t *out = bmalloc(cap * sizeof(int16_t));
	if (!out) {
		blog(LOG_WARNING, "[SLT] out of memory in the resampler");
		return 0;
	}

	/* The carried sample is the input sample immediately before in[0]; for
	 * the very first call of a stream there is none, so fall back to in[0]
	 * (a zero-order hold that only affects a single output sample). */
	const double prev = have_carry ? (double)carry : (double)in[0];

	size_t produced = 0;
	for (;;) {
		const double p = next_p + (double)produced * step;
		if (p > (double)(n - 1u))
			break;
		const size_t i0 = (size_t)p;
		const double t = p - (double)i0;
		const int16_t a = (i0 == 0) ? (int16_t)prev : in[i0 - 1];
		const int16_t b = in[i0];
		double v = (1.0 - t) * (double)a + t * (double)b;
		if (!(v >= -32768.0))
			v = -32768.0; /* also catches NaN */
		else if (v > 32767.0)
			v = 32767.0;
		out[produced++] = (int16_t)v;
	}

	/* Rebase the next grid point onto the next chunk, whose in[0] is n
	 * samples further along, and fold it into [0, step) with fmod. The fold
	 * is a no-op on the grid (a whole number of steps is subtracted) and it
	 * is what bounds the carried value, so it can never grow or go
	 * negative. */
	{
		double carried =
			next_p + (double)produced * step - (double)n;
		carried = fmod(carried, step);
		if (carried < 0.0)
			carried += step;
		next_p = carried;
	}

	const bool ok = slt_send_all(s, (const uint8_t *)out,
				     produced * sizeof(int16_t));
	bfree(out);

	if (ok) {
		slt_mutex_lock(&g_sender.mutex);
		if (g_sender.resample_gen == gen) {
			/* Write back only if no rate change reset the state
			 * while we were working; otherwise our state belongs
			 * to the previous rate and must be discarded. */
			g_sender.resample_pos = next_p;
			g_sender.resample_carry = in[n - 1];
			g_sender.resample_have_carry = true;
		}
		slt_mutex_unlock(&g_sender.mutex);
	}
	return ok ? n : 0;
}

/* ---------------------------------------------------------------------- */
/* Sender thread                                                           */
/* ---------------------------------------------------------------------- */

/* The socket is a loop-local (not a struct field) because the sender thread
 * owns it outright: the filter thread must never create, use or close it.
 * `stop` is checked at every step, including inside the cancelable send, so
 * the loop always returns within a few hundred milliseconds of the signal. */
static void sender_loop(void)
{
	uint8_t *chunk = bmalloc(16 * 1024);
	slt_sock_t sock = SLT_INVALID_SOCK;
	bool connected_once = false;
	bool logged_reject = false;

	if (!chunk) {
		blog(LOG_WARNING, "[SLT] sender thread has no buffer");
		return;
	}

	for (;;) {
		if (slt_stop_requested())
			break;

		engine_ensure_running();

		/* Filter settings asked for a reconnect (port change). Read
		 * the flag and the new port together, under the mutex. */
		slt_mutex_lock(&g_sender.mutex);
		bool reconnect = g_sender.reconnect_requested;
		uint32_t port = g_sender.port;
		g_sender.reconnect_requested = false;
		slt_mutex_unlock(&g_sender.mutex);

		if (reconnect && sock != SLT_INVALID_SOCK) {
			slt_close(sock);
			sock = SLT_INVALID_SOCK;
		}

		/* Ensure we are connected to the engine ingest port. */
		if (sock == SLT_INVALID_SOCK) {
			bool rejected = false;
			bool log_accept = false;
			bool log_reject = false;
			sock = slt_connect_engine((uint16_t)port, &rejected);

			/* All shared state is updated in ONE critical section
			 * and every log() call happens outside it: the mutex is
			 * never held across a call that can block. */
			slt_mutex_lock(&g_sender.mutex);
			g_sender.peer_rejected = rejected;
			if (rejected) {
				/* Only a brand-new stream or a genuine rate
				 * change may reset the resampler state (see
				 * filter_audio); a rejected peer is neither,
				 * so the state is deliberately left alone. */
				if (!logged_reject) {
					logged_reject = true;
					log_reject = true;
				}
			} else if (sock != SLT_INVALID_SOCK) {
				/* A brand-new stream starts at the first
				 * successful connection. From here on the
				 * resampler state is carried across
				 * reconnects so the phase stays continuous. */
				if (!connected_once) {
					connected_once = true;
					/* New stream: start the grid at the
					 * origin (the first output is in[0]).
					 * Clearing have_carry makes the next
					 * call take the fresh-stream path. */
					g_sender.resample_pos = 0.0;
					g_sender.resample_have_carry = false;
					g_sender.resample_gen++;
					log_accept = true;
				} else if (logged_reject) {
					log_accept = true;
				}
				logged_reject = false;
			}
			slt_mutex_unlock(&g_sender.mutex);

			if (log_reject)
				blog(LOG_WARNING,
				     "[SLT] ingest peer on port %u was rejected by the identity check; audio is suppressed until it is accepted",
				     port);
			else if (log_accept)
				blog(LOG_INFO,
				     "[SLT] connected to engine ingest port %u",
				     port);
		}

		/* Wait for audio (or reconnect timeout). */
		os_event_timedwait(g_sender.wake, 500);

		if (slt_stop_requested())
			break;
		if (sock == SLT_INVALID_SOCK)
			continue;

		/* Drain the ring buffer and stream it out. */
		for (;;) {
			if (slt_stop_requested())
				goto done;

			size_t len;
			slt_mutex_lock(&g_sender.mutex);
			len = ring_pop(chunk, 16 * 1024);
			slt_mutex_unlock(&g_sender.mutex);
			if (len == 0)
				break;
			/* len is always even: mono frames are pushed whole.
			 * Drop a stray trailing byte if one ever appears. */
			size_t samples = len / 2;
			if (samples > 0 &&
			    resample_and_send(sock, (const int16_t *)chunk,
					      samples) == 0) {
				/* The mutual handshake already completed before this
				 * first PCM send. A failure here is therefore either a
				 * disconnect or shutdown, never an implicit identity
				 * verdict. */
				if (slt_stop_requested()) {
					/* Shutting down: not an identity
					 * failure, and the log would be noise. */
					slt_close(sock);
					sock = SLT_INVALID_SOCK;
					goto done;
				}
				blog(LOG_WARNING,
				     "[SLT] ingest send failed after authenticated handshake, reconnecting");
				slt_close(sock);
				sock = SLT_INVALID_SOCK;
				break;
			}
		}
	}

done:
	bfree(chunk);
	slt_close(sock);
}

#ifdef _WIN32
static DWORD WINAPI sender_thread(LPVOID param)
{
	UNUSED_PARAMETER(param);
	os_set_thread_name("slt-sender");
	sender_loop();
	return 0;
}
#else
static void *sender_thread(void *param)
{
	UNUSED_PARAMETER(param);
	os_set_thread_name("slt-sender");
	sender_loop();
	return NULL;
}
#endif

/* ---------------------------------------------------------------------- */
/* Audio filter                                                            */
/* ---------------------------------------------------------------------- */

struct slt_filter {
	obs_source_t *source;
	bool enabled;
	bool gate_silence;
	uint32_t port;
};

/* --- filter-visible ingest error state ---------------------------------- *
 * This OBS SDK's obs_source_info has no get_error() callback, so the error
 * has to be visible in the filter's properties panel instead: get_properties
 * is rebuilt from scratch on every call, so it reads the current verdict and
 * shows an OBS_TEXT_INFO_ERROR row. The verdict itself lives in
 * g_sender.peer_rejected (mutex-protected, written only by the sender
 * thread) and is also logged loudly via blog(LOG_WARNING). */

static const char *filter_get_name(void *unused)
{
	UNUSED_PARAMETER(unused);
	return obs_module_text("Filter");
}

static const char *const SLT_ERR_REJECTED =
	"引擎身份校验失败：端口上的进程不是预期的 Stream Live Translate 引擎，已停止发送音频。"
	" / Ingest peer failed the identity check; audio sending is stopped: the "
	"process listening on this port is not the Stream Live Translate engine.";

static const char *const SLT_ERR_BAD_PORT =
	"端口无效：必须在 1024-65535 之间。 / Invalid port: it must be between "
	"1024 and 65535.";

/* Read the sender thread's ingest verdict under the mutex. */
static bool peer_read_rejected(void)
{
	slt_mutex_lock(&g_sender.mutex);
	const bool rejected = g_sender.peer_rejected;
	slt_mutex_unlock(&g_sender.mutex);
	return rejected;
}

static bool peer_read_rejected_safe(void)
{
	/* During module unload the mutex may already be gone; the ownership
	 * table guarantees peer_rejected is only touched under it, so there is
	 * nothing to read then either. */
	if (!g_sender_started)
		return false;
	return peer_read_rejected();
}

static const char *filter_ingest_status_text(uint32_t port)
{
	if (port < 1024)
		return SLT_ERR_BAD_PORT;
	if (peer_read_rejected_safe())
		return SLT_ERR_REJECTED;
	return NULL;
}

/* The one place that changes the ingest port and/or flushes the ring. Both
 * `port` and `reconnect_requested` are updated in a single critical section
 * so the sender thread can never observe a new port without the matching
 * reconnect request; `reconnect_requested` is set last. */
static void sender_request_reconnect(uint32_t port, bool flush_ring)
{
	slt_mutex_lock(&g_sender.mutex);
	g_sender.port = port;
	if (flush_ring)
		g_sender.head = g_sender.tail = g_sender.used = 0;
	g_sender.reconnect_requested = true;
	slt_mutex_unlock(&g_sender.mutex);
}

static void filter_defaults(obs_data_t *s)
{
	obs_data_set_default_bool(s, "enabled", true);
	obs_data_set_default_bool(s, "gate_silence", false);
	obs_data_set_default_int(s, "port", SLT_INGEST_PORT_DEFAULT);
}

static obs_properties_t *filter_properties(void *data)
{
	/* `data` is the filter instance when OBS has one, NULL when it only
	 * wants the defaults (get_properties2 semantics). The status row below
	 * needs no filter state, only the shared sender state. */
	struct slt_filter *f = data;
	const uint32_t port = f ? f->port : SLT_INGEST_PORT_DEFAULT;

	obs_properties_t *p = obs_properties_create();
	obs_properties_add_bool(p, "enabled", obs_module_text("Enabled"));
	obs_properties_add_bool(p, "gate_silence",
				obs_module_text("GateSilence"));
	obs_properties_add_int(p, "port", obs_module_text("Port"), 1024,
			       65535, 1);

	/* Visible ingest error state (see the note above). OBS_TEXT_INFO rows
	 * are read-only, so nothing new is written to the settings object. */
	const char *err = filter_ingest_status_text(port);
	if (err) {
		obs_property_t *prop = obs_properties_add_text(
			p, "ingest_status", err, OBS_TEXT_INFO);
		obs_property_set_long_description(prop, err);
	}
	return p;
}

static void *filter_create(obs_data_t *settings, obs_source_t *source)
{
	struct slt_filter *f = bzalloc(sizeof(*f));
	f->source = source;
	f->enabled = obs_data_get_bool(settings, "enabled");
	f->gate_silence = obs_data_get_bool(settings, "gate_silence");
	f->port = (uint32_t)obs_data_get_int(settings, "port");

	/* Reading g_sender.port without the mutex was one half of the data
	 * race; the sender thread (and every other filter instance) can be
	 * writing it right now. */
	uint32_t current_port;
	slt_mutex_lock(&g_sender.mutex);
	current_port = g_sender.port;
	slt_mutex_unlock(&g_sender.mutex);

	if (f->port != current_port)
		sender_request_reconnect(f->port, false);

	blog(LOG_INFO, "[SLT] filter attached to source \"%s\"",
	     obs_source_get_name(source));
	return f;
}

static void filter_destroy(void *data)
{
	struct slt_filter *f = data;
	bfree(f);
}

static void filter_update(void *data, obs_data_t *settings)
{
	struct slt_filter *f = data;
	f->enabled = obs_data_get_bool(settings, "enabled");
	f->gate_silence = obs_data_get_bool(settings, "gate_silence");
	uint32_t port = (uint32_t)obs_data_get_int(settings, "port");

	slt_mutex_lock(&g_sender.mutex);
	uint32_t current_port = g_sender.port;
	slt_mutex_unlock(&g_sender.mutex);

	if (port != current_port) {
		/* Port changed: flush the stale-port audio and ask the
		 * sender thread to reconnect, both under the same rule. */
		sender_request_reconnect(port, true);
	}
}

static struct obs_audio_data *filter_audio(void *data,
					   struct obs_audio_data *audio)
{
	struct slt_filter *f = data;
	if (!f->enabled || !audio || audio->frames == 0)
		return audio;

	/* Capture audio only once the peer is trusted: a rejected or
	 * unverified peer must not receive audio, and there is no point
	 * filling the ring for nobody. The error is surfaced in the filter's
	 * properties panel (see filter_ingest_status_text) and in the log. */
	if (peer_read_rejected())
		return audio;

	const uint32_t frames = audio->frames;
	/* struct obs_audio_data has no channel-layout field; derive the channel
	 * count from the active audio output configuration instead. */
	const struct audio_output_info *aoi =
		audio_output_get_info(obs_get_audio());
	const size_t channels = aoi ? get_audio_channels(aoi->speakers) : 0;
	if (channels == 0) {
		/* OBS audio output not ready yet; skip this frame silently. */
		return audio;
	}
	if (channels > MAX_AV_PLANES) {
		blog(LOG_WARNING,
		     "[SLT] unsupported channel count %zu (max %d)",
		     channels, MAX_AV_PLANES);
		return audio;
	}

	/* OBS audio filters receive planar float samples. */
	const float *planes[MAX_AV_PLANES];
	for (size_t c = 0; c < channels; c++) {
		planes[c] = (const float *)audio->data[c];
		if (!planes[c])
			return audio;
	}

	const float inv_ch = 1.0f / (float)channels;
	int16_t *mono = bmalloc(frames * sizeof(int16_t));
	if (!mono)
		return audio;
	double sq_sum = 0.0;

	for (uint32_t i = 0; i < frames; i++) {
		float acc = 0.0f;
		for (size_t c = 0; c < channels; c++)
			acc += planes[c][i];
		float v = acc * inv_ch;
		sq_sum += (double)v * v;
		if (v > 1.0f)
			v = 1.0f;
		else if (v < -1.0f)
			v = -1.0f;
		mono[i] = (int16_t)(v * 32767.0f);
	}

	const float rms = (float)sqrt(sq_sum / frames);
	if (!f->gate_silence || rms >= SLT_RMS_GATE) {
		uint32_t rate = audio_output_get_sample_rate(obs_get_audio());
		if (rate == 0)
			rate = 48000;
		/* Bound the rate so the resampler's output buffer stays small;
		 * nothing above 384 kHz is a real capture device. */
		if (rate > SLT_MAX_IN_RATE)
			rate = SLT_MAX_IN_RATE;

		bool rate_changed = false;
		slt_mutex_lock(&g_sender.mutex);
		if (rate != g_sender.in_rate) {
			/* A GENUINE sample-rate change is the only thing that
			 * may invalidate the resampler phase: restart the
			 * resampler (position + carry) and drop the ring,
			 * which still holds audio captured at the old rate.
			 * resample_gen tells the sender thread to discard any
			 * state it is computing for the previous rate. */
			g_sender.in_rate = rate;
			g_sender.resample_pos = 0.0;
			g_sender.resample_carry = 0;
			g_sender.resample_have_carry = false;
			g_sender.resample_gen++;
			g_sender.head = g_sender.tail = g_sender.used = 0;
			rate_changed = true;
		}
		ring_push((const uint8_t *)mono, frames * sizeof(int16_t));
		slt_mutex_unlock(&g_sender.mutex);

		if (rate_changed)
			blog(LOG_INFO, "[SLT] audio sample rate set to %u",
			     rate);
		os_event_signal(g_sender.wake);
	}

	bfree(mono);
	return audio;
}

static struct obs_source_info filter_info = {
	.id = "stream_live_translate_capture",
	.type = OBS_SOURCE_TYPE_FILTER,
	.output_flags = OBS_SOURCE_AUDIO,
	.get_name = filter_get_name,
	.create = filter_create,
	.destroy = filter_destroy,
	.get_defaults = filter_defaults,
	.get_properties = filter_properties,
	.update = filter_update,
	.filter_audio = filter_audio,
};

/* ---------------------------------------------------------------------- */
/* Module lifecycle                                                        */
/* ---------------------------------------------------------------------- */

bool obs_module_load(void)
{
#ifdef _WIN32
	if (WSAStartup(MAKEWORD(2, 2), &g_sender.wsa) != 0)
		return false;
#endif
	if (!slt_server_proof_selftest()) {
		blog(LOG_ERROR,
		     "[SLT] ingest authentication self-test failed; refusing to start");
#ifdef _WIN32
		WSACleanup();
#endif
		return false;
	}

	/* Generate the ingest nonce BEFORE spawning (and respawning) the
	 * engine: the engine receives it on its command line and requires it
	 * back in the ingest handshake, so a spawn without it would produce an
	 * engine that rejects every connection. */
	if (!slt_nonce_generate()) {
		blog(LOG_ERROR,
		     "[SLT] no system CSPRNG is available; refusing to start because ingest authentication cannot be secured");
#ifdef _WIN32
		WSACleanup();
#endif
		return false;
	}
	blog(LOG_INFO,
	     "[SLT] generated a %u-char ingest nonce from the system CSPRNG",
	     (unsigned)SLT_NONCE_LEN);

	g_sender.ring = bmalloc(SLT_RING_BYTES);
	g_sender.head = g_sender.tail = g_sender.used = 0;
	g_sender.port = SLT_INGEST_PORT_DEFAULT;
	g_sender.reconnect_requested = false;
	g_sender.in_rate = 48000;
	g_sender.resample_pos = 0.0;
	g_sender.resample_carry = 0;
	g_sender.resample_have_carry = false;
	g_sender.resample_gen = 0;
	g_sender.peer_rejected = false;
#ifndef _WIN32
	g_sender.engine_pid = -1;
#endif

	if (!g_sender.ring) {
		blog(LOG_WARNING,
		     "[SLT] could not allocate the %d KiB capture ring; the filter will not stream",
		     (int)(SLT_RING_BYTES / 1024));
		return false;
	}

	slt_mutex_init(&g_sender.mutex);
	os_event_init(&g_sender.wake, OS_EVENT_TYPE_AUTO);
	/* MANUAL reset on purpose: a single os_event_signal() latches, so the
	 * cancellation checks inside slt_send_all()/slt_sock_wait() cannot
	 * "consume" the stop request before the sender loop sees it. */
	os_event_init(&g_sender.stop, OS_EVENT_TYPE_MANUAL);

	engine_spawn();

#ifdef _WIN32
	g_thread = CreateThread(NULL, 0, sender_thread, NULL, 0, NULL);
	g_sender_started = g_thread != NULL;
#else
	g_sender_started =
		pthread_create(&g_thread, NULL, sender_thread, NULL) == 0;
#endif

	obs_register_source(&filter_info);
	blog(LOG_INFO, "[SLT] Stream Live Translate plugin loaded (v%s)",
	     SLT_VERSION);
	return true;
}

void obs_module_unload(void)
{
	/* The teardown below may only touch g_sender.* once the sender thread
	 * has provably stopped touching it. `stop` is a manual-reset event, so
	 * a single os_event_signal() latches and every cancellation check
	 * inside the thread - including the ones inside a blocked socket send -
	 * sees it. */
	enum {
		SLT_STOP_COOPERATIVE, /* the thread returned on its own */
		SLT_STOP_FORCED,      /* TerminateThread() confirmed it is gone */
		SLT_STOP_FAILED,      /* it may still be running: free nothing */
	} stopped = SLT_STOP_COOPERATIVE;

	if (g_sender_started) {
		os_event_signal(g_sender.stop);

#ifdef _WIN32
		if (WaitForSingleObject(g_thread, SLT_JOIN_TIMEOUT_MS) !=
		    WAIT_OBJECT_0) {
			/* Still alive despite the signal. NEVER free the mutex,
			 * events or ring buffer under a running thread: that is
			 * a use-after-free. */
			blog(LOG_WARNING,
			     "[SLT] sender thread did not stop within %u ms; force-terminating it",
			     (unsigned)SLT_JOIN_TIMEOUT_MS);
			if (TerminateThread(g_thread, 1)) {
				stopped = SLT_STOP_FORCED;
				blog(LOG_WARNING,
				     "[SLT] sender thread was force-terminated; audio still buffered in the ring is lost");
			} else {
				stopped = SLT_STOP_FAILED;
				blog(LOG_WARNING,
				     "[SLT] TerminateThread failed (err %lu); the thread may still be running, so nothing is freed",
				     GetLastError());
			}
		}
		CloseHandle(g_thread);
		g_thread = NULL;
#else
		/* There is no portable pthread_timedjoin_np, and the sender loop
		 * is now bounded everywhere (its longest wait is one
		 * SLT_SEND_TIMEOUT_MS chain plus the 500 ms idle wait), so a
		 * plain, unbounded join is guaranteed to terminate. */
		pthread_join(g_thread, NULL);
#endif
		g_sender_started = false;
	}

	if (stopped == SLT_STOP_FAILED) {
		/* Keep every object the thread can still reach alive: the
		 * events, the mutex, the ring buffer AND the engine handle (the
		 * live thread still dereferences engine_proc). Do not
		 * WSACleanup() either. A handful of leaked objects is strictly
		 * better than a use-after-free, and a later module load
		 * re-initialises all of it. */
		blog(LOG_WARNING,
		     "[SLT] unload incomplete: the sender thread could not be stopped; its objects and the engine handle are intentionally leaked");
		return;
	}

	engine_terminate();

	if (stopped == SLT_STOP_COOPERATIVE) {
		os_event_destroy(g_sender.stop);
		os_event_destroy(g_sender.wake);
		slt_mutex_destroy(&g_sender.mutex);
		bfree(g_sender.ring);
	} else {
		/* SLT_STOP_FORCED: the thread is provably gone, but there is no
		 * way to know which lock it held when it was killed, so the
		 * primitives and the ring are leaked rather than destroyed. The
		 * engine was terminated above, which is what actually matters
		 * for the user. */
		blog(LOG_WARNING,
		     "[SLT] sender mutex/events/ring intentionally leaked after a force-kill");
	}
	g_sender.stop = NULL;
	g_sender.wake = NULL;
	g_sender.ring = NULL;

#ifdef _WIN32
	WSACleanup();
#endif
}

OBS_DECLARE_MODULE()
OBS_MODULE_USE_DEFAULT_LOCALE(SLT_MODULE_NAME, "en-US")
