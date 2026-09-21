// Loads the REAL overlay/app.js into a Node VM with the browser shim around it.
//
// Nothing here re-implements overlay logic: the production IIFE is read from
// disk and executed, then driven through the stub WebSocket.  `loadOverlay`
// accepts a `transform` hook so a negative control can perturb the source in
// memory (never on disk) and prove the assertions actually bite.

import fs from "node:fs";
import path from "node:path";
import vm from "node:vm";
import { fileURLToPath } from "node:url";

import {
  DEFAULT_METRICS,
  createDocument,
  createGetComputedStyle,
  createLocalStorage,
  createWebSocketStub,
  lineBreaks,
  lineModel,
  textWidth,
} from "./dom-shim.mjs";
import { FakeClock } from "./fake-clock.mjs";

export const TESTS_DIR = path.dirname(fileURLToPath(import.meta.url));
export const REPO_ROOT = path.resolve(TESTS_DIR, "..");
export const OVERLAY_DIR = path.join(REPO_ROOT, "overlay");
export const APP_JS_PATH = path.join(OVERLAY_DIR, "app.js");
export const INDEX_HTML_PATH = path.join(OVERLAY_DIR, "index.html");

export const DEFAULT_OVERLAY_CONFIG = Object.freeze({
  font_size: 48,
  max_lines: 2,
  display_delay_ms: 750,
  clear_after_ms: 4000,
  animation: "fade",
  position: "bottom",
  layout: "single",
});

export function readOverlaySource() {
  return fs.readFileSync(APP_JS_PATH, "utf8");
}

export function readOverlayHtml() {
  return fs.readFileSync(INDEX_HTML_PATH, "utf8");
}

const delay = () => new Promise((resolve) => setImmediate(resolve));

/** Drain microtasks (the overlay's init() awaits fetch(...)). */
export async function flushMicrotasks(rounds = 12) {
  for (let i = 0; i < rounds; i++) await delay();
}

/**
 * Minimal Node-side test runner.  Order is deterministic: no fake clock ticks
 * between tests unless the test asks for them.  Failures whose stable `caseId`
 * is listed in KNOWN_FAILURES are still reported in full, but counted
 * separately so the baseline run can distinguish "real code is broken" from
 * "the test harness is broken".
 */
export class Runner {
  constructor(title, knownFailures = {}) {
    this.title = title;
    this.knownFailures = knownFailures;
    this.passed = [];
    this.failed = [];
    this.known = [];
    this.records = [];
  }

  async run(name, caseId, fn) {
    try {
      await fn();
      this.passed.push(name);
      this.records.push({ name, caseId, status: "pass" });
    } catch (err) {
      const known = this.knownFailures[caseId];
      if (known) {
        this.known.push({ name, caseId, err, reason: known });
        this.records.push({ name, caseId, status: "known-failing" });
      } else {
        this.failed.push({ name, caseId, err });
        this.records.push({ name, caseId, status: "fail" });
      }
    }
  }

  /** Failing case ids, for the negative control's diff. */
  failingIds() {
    return this.failed.map((f) => f.caseId);
  }

  report() {
    const line = "-".repeat(72);
    console.log(`\n${line}\n${this.title}\n${line}`);
    for (const name of this.passed) console.log(`  PASS  ${name}`);
    for (const { name, err } of this.failed) {
      console.log(`  FAIL  ${name}`);
      console.log(indent(err && err.stack ? err.stack : String(err), "        "));
    }
    for (const { name, err, reason } of this.known) {
      console.log(`  KNOWN-FAILING  ${name}`);
      console.log(indent(err && err.message ? err.message : String(err), "        "));
      console.log(indent(`why this is accepted for now: ${reason}`, "        "));
    }
    console.log(
      `\n${this.passed.length} passed, ${this.failed.length} failed, ${this.known.length} known-failing` +
        (this.known.length ? " (real-code defect, documented — see tests/README.md)" : " (none)")
    );
    return this.failed.length;
  }
}

function indent(s, pad) {
  return String(s)
    .split("\n")
    .map((l) => pad + l)
    .join("\n");
}

/** Assertion helpers with explicit expected/actual output. */
export const assert = {
  ok(cond, msg, actual) {
    if (!cond) throw new Error(`${msg}\n  expected: truthy\n  actual:   ${fmt(actual)}`);
  },
  eq(actual, expected, msg) {
    if (actual !== expected) {
      throw new Error(`${msg}\n  expected: ${fmt(expected)}\n  actual:   ${fmt(actual)}`);
    }
  },
  lte(actual, expected, msg) {
    if (!(actual <= expected)) {
      throw new Error(`${msg}\n  expected: <= ${fmt(expected)}\n  actual:   ${fmt(actual)}`);
    }
  },
  gt(actual, expected, msg) {
    if (!(actual > expected)) {
      throw new Error(`${msg}\n  expected: > ${fmt(expected)}\n  actual:   ${fmt(actual)}`);
    }
  },
};

function fmt(v) {
  if (typeof v === "string") {
    return v.length > 200 ? `${JSON.stringify(v.slice(0, 200))}… (len ${v.length})` : JSON.stringify(v);
  }
  return JSON.stringify(v);
}

/**
 * Boot the real overlay inside the shim.
 *
 * @param {object} [opts]
 * @param {object} [opts.overlayConfig]  config the fake /api/config returns
 * @param {number} [opts.viewportWidth]  browser-source width for the shim
 * @param {object} [opts.metrics]        layout metric overrides
 * @param {string} [opts.search]         location.search
 * @param {string} [opts.hash]           location.hash
 * @param {(src: string) => string} [opts.transform]  in-memory source patch
 */
export async function loadOverlay(opts = {}) {
  const transform = opts.transform || ((s) => s);
  const source = transform(readOverlaySource());
  const html = readOverlayHtml();

  const clock = new FakeClock();
  const document = createDocument({
    html,
    clock,
    viewportWidth: opts.viewportWidth,
    metrics: opts.metrics,
  });

  const sockets = [];
  const WebSocketStub = createWebSocketStub({ instances: sockets });
  const localStorage = createLocalStorage();
  const consoleCalls = { log: [], warn: [], error: [] };
  const windowListeners = new Map();

  const overlayConfig = { ...DEFAULT_OVERLAY_CONFIG, ...(opts.overlayConfig || {}) };
  const location = {
    href: "http://127.0.0.1:8080/overlay",
    protocol: "http:",
    host: "127.0.0.1:8080",
    hostname: "127.0.0.1",
    pathname: "/overlay",
    search: opts.search ?? "",
    hash: opts.hash ?? "",
    origin: "http://127.0.0.1:8080",
  };
  const navigator = { userAgent: "node-vm-overlay-tests", language: "zh-CN" };

  const sandbox = {
    console: {
      log: (...a) => consoleCalls.log.push(a.join(" ")),
      warn: (...a) => consoleCalls.warn.push(a.join(" ")),
      error: (...a) => consoleCalls.error.push(a.join(" ")),
      info: () => {},
      debug: () => {},
    },
    document,
    navigator,
    location,
    localStorage,
    getComputedStyle: createGetComputedStyle(document),
    WebSocket: WebSocketStub,
    fetch: async () => ({
      ok: true,
      status: 200,
      json: async () => ({ overlay: overlayConfig }),
      text: async () => "",
    }),
    setTimeout: (...a) => clock.setTimeout(...a),
    clearTimeout: (...a) => clock.clearTimeout(...a),
    setInterval: (...a) => clock.setInterval(...a),
    clearInterval: (...a) => clock.clearInterval(...a),
    queueMicrotask: (fn) => queueMicrotask(fn),
    URLSearchParams,
    URL,
    TextEncoder,
    TextDecoder,
    Promise,
    JSON,
    Math,
    Date,
    Object,
    Array,
    String,
    Number,
    Boolean,
    RegExp,
    Error,
    TypeError,
    Set,
    Map,
    isFinite,
    parseInt,
    parseFloat,
  };
  sandbox.window = sandbox;
  sandbox.self = sandbox;
  sandbox.globalThis = sandbox;
  sandbox.window.addEventListener = (type, fn) => {
    if (!windowListeners.has(type)) windowListeners.set(type, []);
    windowListeners.get(type).push(fn);
  };
  sandbox.window.removeEventListener = () => {};

  const context = vm.createContext(sandbox);
  const initPromise = vm.runInContext(source, context, { filename: APP_JS_PATH });

  // init() awaits loadConfig()/fetch before connectWS(); drain microtasks so
  // the stub WebSocket exists before the first test drives it.
  await flushMicrotasks();
  if (initPromise && typeof initPromise.then === "function") {
    await Promise.race([initPromise, flushMicrotasks(30)]);
  }
  await flushMicrotasks(6);

  const captionEl = document.getElementById("caption");
  const lineEl = document.getElementById("caption-line");
  if (!lineEl) throw new Error("shim is missing #caption-line");
  if (!sockets.length) throw new Error("overlay never constructed a WebSocket after init()");

  const ws = sockets[0];
  ws.open();
  await flushMicrotasks(4);

  const metrics = document.__metrics;
  const lineHeightPx = metrics.fontSize * metrics.lineHeight;
  const maxHeightPx = DEFAULT_OVERLAY_CONFIG.max_lines * lineHeightPx;

  const api = {
    document,
    clock,
    sockets,
    ws,
    localStorage,
    consoleCalls,
    overlayConfig,
    metrics,
    lineHeightPx,
    maxHeightPx,
    captionEl,
    lineEl,
    sandbox,
    initPromise,

    get text() { return lineEl.textContent; },
    get isShown() { return captionEl.classList.contains("show"); },
    get hasEmptyClass() { return captionEl.classList.contains("empty"); },

    /** Measured scrollHeight of the current DOM text (shim layout model). */
    measuredHeight(text = lineEl.textContent) {
      const prev = lineEl.textContent;
      lineEl.textContent = text;
      const h = lineEl.scrollHeight;
      lineEl.textContent = prev;
      return h;
    },

    /** Independent line count for arbitrary text under the current geometry. */
    lineCount(text) {
      return lineModel(text, {
        metrics,
        fontSize: metrics.fontSize,
        lineHeight: metrics.lineHeight,
        nowrap: lineEl.classList.contains("single-line"),
      }).lines;
    },

    /** Per-line ranges for arbitrary text (diagnostics). */
    lineBreaks(text) {
      const geom = api.geometry();
      return lineBreaks(text, geom.usableWidth, metrics.fontSize, metrics, lineEl.classList.contains("single-line"));
    },

    /**
     * Diagnostic for the two-line cap: which characters spill past line 2.
     */
    overflowReport(text) {
      const breaks = api.lineBreaks(text);
      return {
        lines: breaks.length,
        height: api.measuredHeight(text),
        extra: breaks.slice(2).map((b) => text.slice(b.start, b.end)),
      };
    },

    geometry() {
      return {
        ...lineModel("", { metrics, fontSize: metrics.fontSize, lineHeight: metrics.lineHeight }),
        viewportWidth: metrics.viewportWidth,
        maxCaptionWidth: metrics.maxCaptionWidth,
        horizontalPadding: metrics.horizontalPadding,
        cjkRatio: metrics.cjkRatio,
        otherRatio: metrics.otherRatio,
        spaceRatio: metrics.spaceRatio,
      };
    },

    textWidth(text) { return textWidth(text, metrics.fontSize, metrics); },

    /** Lines needed if `text` were rendered in full (no pagination). */
    fullRenderLines(text) { return api.lineCount(text); },

    emit(payload) { ws.emit(payload); },
    emitPartial(text, replace = false) {
      ws.emit(replace ? { type: "partial", text, replace: true } : { type: "partial", text });
    },
    emitFinal(text) { ws.emit({ type: "final", text }); },
    emitCleared() { ws.emit({ type: "cleared" }); },
    tick(ms) { clock.tick(ms); return api; },

    /**
     * Pending one-shot timers.  The overlay keeps a 10 s WebSocket ping
     * *interval* alive, so `pendingCount` alone would mix the two; the buffer
     * assertions only care about one-shot deadlines.
     */
    pendingTimeouts() {
      return clock.pending().filter((t) => t.kind === "timeout");
    },
    pendingIntervals() {
      return clock.pending().filter((t) => t.kind === "interval");
    },

    /**
     * Virtual ms remaining until the queued display buffer fires, or null when
     * nothing is buffered.  The display timer is the earliest pending one-shot
     * timer; the silence-clear timer lives `clear_after_ms` away.
     */
    displayDeadline() {
      const timers = api.pendingTimeouts();
      if (!timers.length) return null;
      return timers[0].at - clock.now;
    },
    resize() {
      for (const fn of windowListeners.get("resize") || []) fn({ type: "resize" });
    },
    storageEvent(key, newValue) {
      for (const fn of windowListeners.get("storage") || []) fn({ key, newValue });
    },
  };

  return api;
}

export { DEFAULT_METRICS };
