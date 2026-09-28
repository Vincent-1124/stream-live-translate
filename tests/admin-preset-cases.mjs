// Headless regression tests for the admin panel's filter presets.
//
// Why this exists: the panel's 「过滤预设」 used to only write the LOCAL
// `filter.silence_rms`, which cannot change what the cloud transcribes
// (`src/pipeline.rs` still forwards Speech *and* Silence frames; only Music is
// dropped).  It now also writes the CLOUD `llm.speech_noise_threshold`, which is
// sent in `run-task`.  A silent regression here would put the panel back to
// "the knob does nothing", so the wiring is asserted rather than eyeballed:
//
//   1. the panel renders an input with min=-1 max=1 step=0.1;
//   2. selecting a preset sets BOTH values to that preset's pair;
//   3. the saved patch (`collectPatch()` → POST /api/config body) carries
//      `llm.speech_noise_threshold`;
//   4. out-of-range typed values are clamped to [-1, 1];
//   5. a hand-edited value falls back to the 「自定义」 preset instead of being
//      snapped back to a preset on the next save.
//
// Uses the shared DOM shim (`dom-shim.mjs`) plus a small extension for the two
// things the panel does that the overlay never did: `querySelectorAll` on the
// preset button container, and attribute setters (`setAttribute`/`disabled`).

import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import vm from "node:vm";
import { fileURLToPath } from "node:url";

import { createDocument, createLocalStorage, createWebSocketStub } from "./dom-shim.mjs";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const REPO = path.resolve(HERE, "..");

const PANEL_JS = fs.readFileSync(path.join(REPO, "admin", "app.js"), "utf8");
const PANEL_HTML = fs.readFileSync(path.join(REPO, "admin", "index.html"), "utf8");

/**
 * The source `bootPanel()` actually executes.  Identical to the file on disk
 * unless a negative control swaps in a deliberately re-broken variant with
 * [`setPanelSource`] — the file itself is never written.
 */
let panelSource = PANEL_JS;

/** Negative-control hook: run every later `bootPanel()` from this source. */
export function setPanelSource(source) {
  panelSource = String(source);
}

/** The source currently under test (pristine unless a control swapped it). */
export function getPanelSource() {
  return panelSource;
}

export const results = [];

/// Let pending microtasks run.
///
/// `check()` used to be synchronous, so every `check(name, async () => …)` in
/// this file had its assertions **silently discarded**: the callback returned a
/// pending promise, the case was recorded `ok`, and the footer exited before the
/// assertion ever ran. Draining the queue after each case is what makes those
/// seven cases real — and it is how the wiring defect below was found.
async function drainMicrotasks(rounds = 60) {
  for (let i = 0; i < rounds; i++) await Promise.resolve();
}

function check(name, fn) {
  try {
    const value = fn();
    if (value && typeof value.then === "function") {
      // Async case: record it as failed unless the promise settles.
      results.push(
        value.then(
          () => ({ name, ok: true }),
          (error) => ({
            name,
            ok: false,
            error: error && error.stack ? error.stack : String(error),
          }),
        ),
      );
      return;
    }
    results.push({ name, ok: true });
  } catch (error) {
    results.push({ name, ok: false, error: error && error.stack ? error.stack : String(error) });
  }
}

// ---------------------------------------------------------------------------
// Panel environment
// ---------------------------------------------------------------------------

/** Default `/api/config` payload the panel boots with. */
export function baseConfig(overrides = {}) {
  return {
    server: { host: "127.0.0.1", port: 8897, static_dir: "dist" },
    llm: {
      provider: "bailian-fun-asr",
      api_key: "",
      api_key_set: true,
      model: "fun-asr-realtime",
      endpoint: null,
      target_lang: "zh",
      translate_chinese: false,
      system_prompt: null,
      segment_ms: 0,
      transcribe: false,
      transcription_model: "",
      gateway_text: false,
      workspace_id: "",
      speech_noise_threshold: 0.0,
      semantic_punctuation_enabled: true,
      hotwords: [],
      ...(overrides.llm || {}),
    },
    audio: {
      mode: "obs_filter",
      use_screen_capture_kit: true,
      device: "",
      sample_rate: 16000,
      channels: 1,
      ingest_port: 8898,
    },
    filter: { silence_rms: 0.012, music_spectral_flatness: 0.55, min_segment_ms: 350, max_segment_ms: 8000 },
    obs: { auto_connect: true, host: "127.0.0.1", port: 4455, password: "", register_dock: true },
    overlay: {
      font_family: "sans-serif",
      font_size: 48,
      font_color: "#FFFFFF",
      background_color: "#000000",
      background_opacity: 0.55,
      bg_width: 0,
      bg_height: 0,
      border_radius: 8,
      bg_opacity: 75,
      max_lines: 2,
      display_delay_ms: 750,
      clear_after_ms: 4000,
      position: "bottom",
      layout: "single",
      animation: "typewriter",
      mirror_to_text_source: false,
    },
    recording_dir: "",
    audio_test: { quiet_ms: 3000, speech_ms: 10000 },
    hotword_status: { count: 0, rounds: 0, delivered: false, delivered_count: 0, delivered_rounds: 0, skipped_unchanged: 0 },
    ...(overrides.top || {}),
  };
}

/** Select ids the panel reads, in markup order. */
const SELECT_IDS = ["audio-mode", "filter-preset", "target_lang", "ov-display-delay",
                    "ov-clear-after", "ov-position", "ov-animation", "segment_ms"];

/**
 * The `<option value>` list every `<select>` in `admin/index.html` offers.
 *
 * A real browser's `select.value` drops a value the markup does not offer and
 * `setSelectValue()` chooses among `sel.options`; a stub with `options = []`
 * can model neither, so the select-related zero/fallback bugs would be
 * invisible to the suite.
 */
export function parseSelectOptions(html = PANEL_HTML) {
  const out = {};
  const blocks = html.matchAll(/<select\b[^>]*\bid="([^"]+)"[^>]*>([\s\S]*?)<\/select>/g);
  for (const [, id, body] of blocks) {
    out[id] = [...body.matchAll(/<option\b[^>]*\bvalue="([^"]*)"/g)].map((m) => m[1]);
  }
  return out;
}

/**
 * Boot `admin/app.js` against the DOM shim.
 *
 * Returns the document, the recorded `fetch` calls and the panel's own
 * `collectPatch()` result (captured by intercepting the save POST).
 *
 * @param {object}   [opts]
 * @param {object}   [opts.config]         payload for `GET /api/config`
 * @param {object}   [opts.statusOverrides] extra `/api/status` fields
 * @param {object}   [opts.configStore]    optional fake server: `{ read(), write(patch) }`.
 *   When given, GET returns `read()` and POST is handed to `write()`, so a save
 *   followed by the panel's own re-`loadConfig()` (or a second boot, i.e. a page
 *   reload) round-trips through the stored value instead of a frozen fixture.
 * @param {boolean}  [opts.selectOptions]  populate `sel.options` from the markup
 *   and make `sel.value` behave like a browser (a value the markup does not
 *   offer leaves the select blank). Default true.
 */
export async function bootPanel({
  config = baseConfig(),
  statusOverrides = {},
  configStore = null,
  selectOptions = true,
} = {}) {
  const doc = createDocument({ html: PANEL_HTML, clock: undefined });
  const previewViewport = doc.getElementById("preview-obs-viewport");
  if (previewViewport) previewViewport.parentElement = { clientWidth: 1000 };

  // --- the shim extensions the panel needs ---------------------------------
  // The overlay never sets attributes or disables a control, so the shim has no
  // `setAttribute`.  The panel does both (`aria-pressed`, `disabled`).
  const extend = (el) => {
    if (el.__adminExtended) return el;
    el.__adminExtended = true;
    el.attributes = new Map();
    el.setAttribute = (name, value) => { el.attributes.set(String(name), String(value)); };
    el.getAttribute = (name) => (el.attributes.has(String(name)) ? el.attributes.get(String(name)) : null);
    el.removeAttribute = (name) => { el.attributes.delete(String(name)); };
    el.disabled = false;
    el.hidden = false;
    el.closest = (sel) => {
      if (!sel) return null;
      const want = String(sel).replace(/^\./, "");
      if (el.id && (sel === "#" + el.id || sel.includes("#" + el.id))) return el;
      if (sel.startsWith(".") && el.classList.contains(want)) return el;
      return null;
    };
    return el;
  };
  doc._elements.forEach(extend);
  // `<select>.options` is iterated by fillForm (`[...sel.options]`) and by
  // `setSelectValue`, so select stubs need an option list.  With
  // `selectOptions` (the default) it is the real one parsed out of the markup,
  // and `value` is a browser-like accessor: assigning a value no `<option>`
  // offers leaves the select BLANK — the failure mode `setSelectValue()` exists
  // to prevent.  A stub with `options = []` and no accessor accepts anything,
  // which hides exactly the bugs this harness is supposed to catch.
  const markupOptions = selectOptions ? parseSelectOptions() : null;
  for (const id of SELECT_IDS) {
    const sel = doc.getElementById(id);
    if (!sel) continue;
    const values = markupOptions && markupOptions[id] ? [...markupOptions[id]] : [];
    sel.options = values.map((value) => ({ value, textContent: value }));
    if (values.length === 0) continue;
    let current = "";
    Object.defineProperty(sel, "value", {
      get: () => current,
      set: (v) => { const s = String(v); current = values.includes(s) ? s : ""; },
      enumerable: true,
      configurable: true,
    });
    // `fillForm` appends an option for a value the markup does not offer (e.g. a
    // hand-edited audio.mode); a real browser would then have it in `options`.
    const appendChild = sel.appendChild.bind(sel);
    sel.appendChild = (child) => {
      appendChild(child);
      const v = child && child.value !== undefined ? String(child.value) : "";
      if (v && !values.includes(v)) values.push(v);
      return child;
    };
  }
  const origCreate = doc.createElement;
  doc.createElement = (tag) => extend(origCreate(tag));
  doc.querySelectorAll = (sel) => {
    if (sel === "#noise-preset-buttons .btn") {
      const box = doc.getElementById("noise-preset-buttons");
      return box ? box.children : [];
    }
    return [];
  };
  doc.addEventListener = () => {};

  const calls = [];
  const debugErrors = [];
  const status = {
    running: true,
    audio_active: true,
    input_level: 0.01,
    llm_connected: true,
    obs_connected: true,
    last_error: null,
    obs_error: null,
    last_subtitle_at: null,
    bind_url: "http://127.0.0.1:8897",
    obs_dock_url: null,
    config_path: "config.toml",
    audio_mode_forced: "obs_filter",
    audio_test_quiet_ms: 3000,
    audio_test_speech_ms: 10000,
    speech_noise_threshold: Number(config.llm.speech_noise_threshold) || 0,
    speech_noise_threshold_clamped: false,
    hotwords: { count: 0, rounds: 0, delivered: false },
    ...statusOverrides,
  };

  const fetchImpl = async (rawUrl, opts = {}) => {
    const url = String(rawUrl);
    const body = opts.body ? JSON.parse(opts.body) : null;
    calls.push({ url, method: opts.method || "GET", body });
    let payload = {};
    if (url.endsWith("/api/config") && (opts.method || "GET") === "GET") {
      payload = configStore ? configStore.read() : config;
    } else if (url.endsWith("/api/status")) {
      payload = status;
    } else if (url.endsWith("/api/recordings")) {
      payload = { jsonl_path: "recordings/x.jsonl" };
    } else if (url.endsWith("/api/subtitles")) {
      payload = { history: [] };
    } else if (url.endsWith("/api/devices")) {
      payload = [];
    } else if (url.endsWith("/api/config") && opts.method === "POST") {
      if (configStore) configStore.write(body);
      payload = { ok: true };
    }
    return { ok: true, status: 200, json: async () => payload, text: async () => "" };
  };

  // `vm` keeps the panel's `$()` closure bound to THIS document, even while the
  // test drives several panels in one process.
  const wsInstances = [];
  const sandbox = {
    console: {
      log: (...a) => { if (process.env.ADMIN_TEST_DEBUG) console.error("[panel log]", ...a); },
      warn: (...a) => { if (process.env.ADMIN_TEST_DEBUG) console.error("[panel warn]", ...a); },
      error: (...a) => { debugErrors.push(a.map(String).join(" ")); },
      info: () => {},
      debug: () => {},
    },
    document: doc,
    navigator: { userAgent: "node-vm-admin-tests", language: "zh-CN" },
    location: { protocol: "http:", host: "127.0.0.1:8897", search: "", href: "http://127.0.0.1:8897/admin" },
    localStorage: createLocalStorage(),
    getComputedStyle: () => ({ fontSize: "48px", lineHeight: "60px", getPropertyValue: () => "" }),
    WebSocket: createWebSocketStub({ instances: wsInstances }),
    fetch: fetchImpl,
    // Timers never fire: every assertion in this file is synchronous.
    setTimeout: () => 0,
    clearTimeout: () => {},
    setInterval: () => 0,
    clearInterval: () => {},
    queueMicrotask: (fn) => queueMicrotask(fn),
    URLSearchParams,
    URL,
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
  sandbox.window.addEventListener = () => {};
  sandbox.window.removeEventListener = () => {};

  const context = vm.createContext(sandbox);
  vm.runInContext(panelSource, context, { filename: "admin/app.js" });
  for (let i = 0; i < 200; i++) await Promise.resolve();

  return {
    doc,
    sandbox,
    wsInstances,
    calls,
    debugErrors,
    el: (id) => doc.getElementById(id),
    saveCalls: () => calls.filter((c) => c.method === "POST" && c.url.endsWith("/api/config")),
  };
}

// ---------------------------------------------------------------------------
// Cases
// ---------------------------------------------------------------------------

const PRESET_EXPECTATIONS = [
  { preset: "soft", threshold: 0.0, rms: 0.007 },
  { preset: "balanced", threshold: 0.3, rms: 0.012 },
  { preset: "straight", threshold: 0.6, rms: 0.012 },
  { preset: "strong", threshold: 0.9, rms: 0.012 },
];

export async function run() {
  results.length = 0;
  // `check()` records an async case as a promise; `run()` must await them all or
  // the assertions are discarded again (that was the original defect).
  const settle = async () => {
    await drainMicrotasks();
    const pending = results.filter((entry) => entry && typeof entry.then === "function");
    if (pending.length) {
      const settled = await Promise.all(pending);
      // Replace the promise entries with their settled result, in place, so the
      // reported order still matches the source order.
      let i = 0;
      for (let idx = 0; idx < results.length; idx++) {
        if (results[idx] && typeof results[idx].then === "function") {
          results[idx] = settled[i++];
        }
      }
    }
  };

  const panel = await bootPanel({
    config: baseConfig({ llm: { speech_noise_threshold: 0.9 } }),
  });

  check("阈值输入是 type=number min=-1 max=1 step=0.1", () => {
    const html = PANEL_HTML;
    assert.match(
      html,
      /id="speech-noise-threshold"[^>]*type="number"[^>]*min="-1"[^>]*max="1"[^>]*step="0\.1"/,
      "index.html 里的阈值输入必须带 min/max/step",
    );
  });

  check("启动后回填配置里的生效值（0.9）", () => {
    assert.equal(panel.el("speech-noise-threshold").value, "0.9");
    assert.equal(panel.el("filter-preset").value, "strong");
  });

  check("四个快捷档位按钮都渲染出来了", () => {
    const buttons = panel.doc.querySelectorAll("#noise-preset-buttons .btn");
    assert.deepEqual(
      buttons.map((b) => b.textContent),
      ["0.0 关", "0.3 中等", "0.6 较强", "0.9 强"],
    );
  });

  check("界面上没有残留的零散旧控件名", () => {
    assert.ok(!PANEL_HTML.includes("人声过滤预设"), "旧标题应已替换");
    assert.ok(!PANEL_JS.includes("PRESET_RMS"), "旧的 PRESET_RMS 应已删除");
  });

  for (const spec of PRESET_EXPECTATIONS) {
    // A fresh panel per preset: `boot()` is a one-shot IIFE.
    // eslint-disable-next-line no-await-in-loop
    const p = await bootPanel({ config: baseConfig({ llm: { speech_noise_threshold: 0.9 } }) });
    check(`选择「${spec.preset}」同时写入阈值 ${spec.threshold} 与本地静音 ${spec.rms}`, () => {
      const select = p.el("filter-preset");
      select.value = spec.preset;
      select.dispatchEvent({ type: "change" });
      assert.equal(p.el("speech-noise-threshold").value, String(spec.threshold));
      assert.equal(p.el("silence-rms").value, String(spec.rms));
    });
  }

  check("保存时 patch 里带 llm.speech_noise_threshold（强过滤 → 0.9）", async () => {
    const p = await bootPanel({ config: baseConfig({ llm: { speech_noise_threshold: 0.0 } }) });
    const select = p.el("filter-preset");
    select.value = "strong";
    select.dispatchEvent({ type: "change" });
    // Trigger the save button's handler through the panel's own binding.
    const saveBtn = p.el("save-btn");
    assert.ok(saveBtn, "save-btn 必须存在");
    saveBtn.dispatchEvent({ type: "click" });
    for (let i = 0; i < 40; i++) await Promise.resolve();
    const posts = p.saveCalls();
    assert.equal(posts.length, 1, `应发出 1 次 POST /api/config，实际 ${posts.length}`);
    assert.equal(posts[0].body.llm.speech_noise_threshold, 0.9);
    assert.equal(posts[0].body.filter.silence_rms, 0.012);
  });

  check("手填越界值会被钳到 [-1, 1]", async () => {
    const p = await bootPanel({});
    const input = p.el("speech-noise-threshold");
    input.value = "5";
    input.dispatchEvent({ type: "input" });
    assert.equal(input.value, "1");
    input.value = "-9";
    input.dispatchEvent({ type: "input" });
    assert.equal(input.value, "-1");
  });

  check("手改阈值后预设回到「自定义」，且本地静音阈值不被连带改掉", async () => {
    const p = await bootPanel({ config: baseConfig({ llm: { speech_noise_threshold: 0.3 } }) });
    assert.equal(p.el("filter-preset").value, "balanced");
    const rmsBefore = p.el("silence-rms").value;
    const input = p.el("speech-noise-threshold");
    input.value = "0.45";
    input.dispatchEvent({ type: "input" });
    assert.equal(p.el("filter-preset").value, "custom");
    assert.equal(p.el("silence-rms").value, rmsBefore);
  });

  check("「自定义」档保存时用手改的阈值，而不是回落到预设值", async () => {
    const p = await bootPanel({ config: baseConfig({ llm: { speech_noise_threshold: 0.45 } }) });
    assert.equal(p.el("filter-preset").value, "custom");
    p.el("save-btn").dispatchEvent({ type: "click" });
    for (let i = 0; i < 40; i++) await Promise.resolve();
    const posts = p.saveCalls();
    assert.equal(posts.length, 1);
    assert.equal(posts[0].body.llm.speech_noise_threshold, 0.45);
  });

  check("「当前生效值」把服务端值和未保存的输入区分开", async () => {
    const p = await bootPanel({
      config: baseConfig({ llm: { speech_noise_threshold: 0.3 } }),
      statusOverrides: { speech_noise_threshold: 0.3 },
    });
    const status = p.el("noise-threshold-status");
    assert.ok(status.textContent.includes("当前生效值 0.3"), status.textContent);
    const input = p.el("speech-noise-threshold");
    input.value = "0.6";
    input.dispatchEvent({ type: "input" });
    assert.ok(
      status.textContent.includes("尚未生效"),
      `改成 0.6 后应提示尚未生效，实际：${status.textContent}`,
    );
  });

  check("非百炼通道明确提示该阈值不会下发", async () => {
    const p = await bootPanel({ config: baseConfig({ llm: { provider: "mock", speech_noise_threshold: 0.9 } }) });
    const status = p.el("noise-threshold-status");
    assert.ok(status.textContent.includes("不会下发"), status.textContent);
    assert.equal(p.el("noise-provider-hint").hidden, false);
  });

  check("输入框清空时不会谎报「输入框 0 尚未生效」", async () => {
    const p = await bootPanel({
      config: baseConfig({ llm: { speech_noise_threshold: 0.9 } }),
      statusOverrides: { speech_noise_threshold: 0.9 },
    });
    const input = p.el("speech-noise-threshold");
    input.value = "";
    input.dispatchEvent({ type: "input" });
    const status = p.el("noise-threshold-status");
    assert.ok(!status.textContent.includes("尚未生效"), `清空输入时不应提示尚未生效：${status.textContent}`);
    assert.ok(status.textContent.includes("当前生效值 0.9"), status.textContent);
  });

  // The final preset case pushes a `filter-preset` change whose save assertion
  // runs after the current microtask, so give it its slice before returning.
  await drainMicrotasks();
  await settle();

  return results;
}

// Allow `node tests/admin-preset-cases.mjs` for a direct run.
//
// History: `check()` used to be synchronous, so every `check(name, async () => …)`
// in this file had its assertions **silently discarded** — the case was recorded
// `ok` the moment the async callback returned a pending promise, and the footer's
// `process.exit()` discarded the rest. Making the harness await-aware exposed a
// real wiring defect it had been hiding: `collectPatch()` posted the cloud
// threshold as `filter.speech_noise_threshold` while the server reads
// `llm.speech_noise_threshold`, so choosing a 过滤预设 never reached the cloud.
// Both are fixed: the panel now sends `llm.speech_noise_threshold`, and this
// suite actually asserts the async cases.
if (process.argv[1] && path.resolve(process.argv[1]) === path.resolve(fileURLToPath(import.meta.url))) {
  const list = await run();
  let failed = 0;
  for (const r of list) {
    if (r.ok) {
      console.log(`  ok   ${r.name}`);
    } else {
      failed++;
      console.log(`  FAIL ${r.name}\n${r.error}`);
    }
  }
  console.log(`\n${list.length - failed}/${list.length} passed`);
  process.exit(failed ? 1 : 0);
}
