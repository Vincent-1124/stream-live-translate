// Headless regression tests for audit item P2-04: the admin panel used to treat
// a legal `0` / `false` / `""` as "no value" and write the default back.
//
// Three confirmed cases (all fixed in `admin/app.js`):
//
//   * `overlay.bg_opacity = 0` (0 = fully transparent, `src/config.rs` documents
//     it and only clamps the value down to <= 100) -> the save path's
//     `parseInt(...) || 75` wrote 75 back;
//   * `overlay.border_radius = 0` -> the load path's `|| 8` showed 8, and the
//     next save persisted 8;
//   * `filter.silence_rms = 0` -> the load path's `rms > 0 ? rms : 0.012`
//     showed 0.012, switched the preset back to a named one and persisted it.
//
// WHY 0 IS THE RIGHT READING FOR `silence_rms`
//   `src/vad.rs::Vad::decide()` classifies a frame as `Silence` only when
//   `rms < self.cfg.silence_rms`, and RMS is never negative, so `silence_rms = 0`
//   is exactly "the silence gate never matches" = the gate is disabled. It is
//   accepted by the server contract: `FilterConfig` documents the field as
//   "RMS threshold (0.0-1.0) below which we treat the segment as silence" and
//   `Config::clamp()` normalises it with `clamp(0.0, 1.0)` — a clamp, not a
//   rejection, and never a floor of the default. So 0 must round-trip exactly;
//   the panel must NOT silently re-arm the gate with 0.012.
//
// The suite drives the REAL `admin/app.js` in the shared DOM shim and models the
// server (merge-then-clamp, like `post_config` -> `Config::normalise`) so every
// case exercises the whole load -> save -> reload path:
//
//   load  : GET /api/config -> fillForm()          (assert the control shows 0)
//   save  : collectPatch() -> POST /api/config     (assert the body carries 0)
//   reload: the panel re-GETs after saving, and a second boot = a page reload
//           (assert the control still shows 0 and the second save is identical)
//
// It imports the harness from `admin-preset-cases.mjs` (which is also the entry
// point: `node tests/admin-preset-cases.mjs` runs both suites).

import assert from "node:assert/strict";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { baseConfig, bootPanel, getPanelSource, setPanelSource } from "./admin-preset-cases.mjs";

export const results = [];
function check(name, fn) {
  return Promise.resolve()
    .then(fn)
    .then(() => { results.push({ name, ok: true }); })
    .catch((error) => {
      results.push({ name, ok: false, error: error && error.stack ? error.stack : String(error) });
    });
}

// ---------------------------------------------------------------------------
// Fake server
// ---------------------------------------------------------------------------

/**
 * Merged config the panel boots with, section by section, so a case only has to
 * name the fields it cares about.
 */
export function cfgWith({ llm = {}, audio = {}, filter = {}, obs = {}, overlay = {}, top = {} } = {}) {
  const base = baseConfig();
  return {
    ...base,
    llm: { ...base.llm, ...llm },
    audio: { ...base.audio, ...audio },
    filter: { ...base.filter, ...filter },
    obs: { ...base.obs, ...obs },
    overlay: { ...base.overlay, ...overlay },
    ...top,
  };
}

/** `merge_json` from `src/server.rs`: deep merge, arrays and nulls replace. */
function mergeJson(base, patch) {
  if (!patch || typeof patch !== "object" || Array.isArray(patch)) return patch;
  const merged = { ...(base && typeof base === "object" && !Array.isArray(base) ? base : {}) };
  for (const [key, value] of Object.entries(patch)) merged[key] = mergeJson(merged[key], value);
  return merged;
}

const clamp = (v, lo, hi) => Math.min(hi, Math.max(lo, v));

/**
 * The `Config::clamp()` rules that apply to what the panel posts/reads, mirrored
 * from `src/config.rs`.  Note what is NOT here: nothing turns a legal `0` into
 * a default — `bg_opacity` is only capped at 100 (0 = fully transparent),
 * `border_radius` / `bg_width` / `bg_height` are only capped from above (0 = a
 * real value / "auto"), `silence_rms` is clamped into [0, 1] and
 * `display_delay_ms` keeps its special 0 = "no buffer".
 */
export function clampLikeServer(cfg) {
  const ov = cfg.overlay;
  ov.font_size = clamp(ov.font_size, 8, 400);
  ov.bg_width = Math.min(16384, Math.max(0, ov.bg_width));
  ov.bg_height = Math.min(16384, Math.max(0, ov.bg_height));
  ov.border_radius = Math.min(512, Math.max(0, ov.border_radius));
  ov.bg_opacity = Math.min(100, Math.max(0, ov.bg_opacity));
  ov.max_lines = clamp(ov.max_lines, 1, 4);
  ov.display_delay_ms = ov.display_delay_ms === 0 ? 0 : clamp(ov.display_delay_ms, 500, 1000);
  ov.clear_after_ms = clamp(ov.clear_after_ms ?? 4000, 1000, 15000);

  const f = cfg.filter;
  f.silence_rms = isFinite(f.silence_rms) ? clamp(f.silence_rms, 0, 1) : 0;
  f.music_spectral_flatness = isFinite(f.music_spectral_flatness)
    ? clamp(f.music_spectral_flatness, 0, 1)
    : 0;
  f.min_segment_ms = Math.min(60000, f.min_segment_ms);
  f.max_segment_ms = clamp(f.max_segment_ms, Math.max(1, f.min_segment_ms), 600000);

  cfg.llm.speech_noise_threshold = isFinite(cfg.llm.speech_noise_threshold)
    ? clamp(cfg.llm.speech_noise_threshold, -1, 1)
    : 0;
  return cfg;
}

/**
 * Stand-in for the Rust engine's storage: what POST /api/config writes is what
 * the next GET /api/config (and therefore the next page load) returns.
 */
export function createConfigStore(initial) {
  let current = clampLikeServer(structuredClone(initial));
  const saved = [];
  return {
    read() {
      // `get_config` hands the panel the values the overlay will really use.
      const out = structuredClone(current);
      out.overlay.clear_after_ms = clamp(out.overlay.clear_after_ms ?? 4000, 1000, 15000);
      out.overlay.display_delay_ms =
        out.overlay.display_delay_ms === 0 ? 0 : clamp(out.overlay.display_delay_ms, 500, 1000);
      out.llm.speech_noise_threshold = clamp(out.llm.speech_noise_threshold, -1, 1);
      out.llm.api_key_set = !!initial.llm.api_key_set;
      out.llm.speech_noise_threshold_clamped = false;
      return out;
    },
    write(patch) {
      saved.push(patch);
      current = clampLikeServer(mergeJson(current, patch));
      return { ok: true };
    },
    saved,
    raw: () => structuredClone(current),
  };
}

// ---------------------------------------------------------------------------
// Driving one full round trip
// ---------------------------------------------------------------------------

/** Snapshot the controls a case cares about. */
function snapshot(panel, ids) {
  const out = {};
  for (const id of ids) {
    const el = panel.el(id);
    out[id] = el
      ? { value: el.value, checked: el.checked, text: el.textContent, display: el.style.display }
      : null;
  }
  return out;
}

/** Click 保存配置 and drain the microtask queue the handler awaits through. */
async function save(panel) {
  panel.el("save-btn").dispatchEvent({ type: "click" });
  for (let i = 0; i < 200; i++) await Promise.resolve();
  const posts = panel.saveCalls();
  assert.equal(posts.length, 1, `应发出 1 次 POST /api/config，实际 ${posts.length}`);
  return posts[0].body;
}

/** Type into a control the way a user would (value change + input event). */
function typeValue(panel, id, value) {
  const el = panel.el(id);
  el.value = String(value);
  // The shim does not synthesise `target`, and `ov-bg-opacity`'s listener reads
  // `e.target.value`, so pass the element the way a real dispatch would.
  el.dispatchEvent({ type: "input", target: el });
}

/**
 * load -> save -> (the panel's own re-GET) -> page reload -> save again.
 *
 * @param {object} seed     stored config (as if hand-edited in config.toml)
 * @param {object} [opts]
 * @param {string[]} [opts.ids]    controls to snapshot before/after the saves
 * @param {Function} [opts.mutate] runs against the first panel before saving
 */
async function roundTrip(seed, { ids = [], mutate } = {}) {
  const store = createConfigStore(seed);
  const first = await bootPanel({ config: store.read(), configStore: store });
  const loaded = snapshot(first, ids);
  if (mutate) await mutate(first);
  const body1 = await save(first);
  const afterSave = snapshot(first, ids);
  const second = await bootPanel({ config: store.read(), configStore: store });
  const reloaded = snapshot(second, ids);
  const body2 = await save(second);
  return { store, first, second, loaded, afterSave, reloaded, body1, body2 };
}

/**
 * Every stage of the round trip must still carry `expected`.
 *
 * @param {string|null} expected `null` skips the value checks (a checkbox).
 * @param {boolean} [opts.checked] also assert `.checked` at every stage.
 */
function assertRoundTrip(rt, id, expected, { checked } = {}) {
  const show = (stage) => (rt[stage][id] ? rt[stage][id].value : "<missing>");
  if (expected !== null) {
    assert.equal(show("loaded"), expected, `加载后 ${id} 应是 ${expected}，实际 ${show("loaded")}`);
    assert.equal(show("afterSave"), expected, `保存并回读后 ${id} 应仍是 ${expected}，实际 ${show("afterSave")}`);
    assert.equal(show("reloaded"), expected, `重新打开页面后 ${id} 应是 ${expected}，实际 ${show("reloaded")}`);
  }
  if (checked !== undefined) {
    for (const stage of ["loaded", "afterSave", "reloaded"]) {
      assert.equal(rt[stage][id].checked, checked, `${stage}: ${id}.checked 应是 ${checked}`);
    }
  }
}

/** The 静音阈值 number the live readout shows (`#input-level-rms`). */
function readoutThreshold(text) {
  const m = /静音阈值 ([0-9.]+)/.exec(text || "");
  return m ? m[1] : null;
}

// ---------------------------------------------------------------------------
// Negative control
// ---------------------------------------------------------------------------

/**
 * Negative control (the style `tests/run-negative-control.mjs` uses for the
 * overlay): put ONE pre-fix idiom back into the panel source **in memory** and
 * prove the round-trip assertion for that field fails again.  Without this, a
 * green suite only proves "the assertion ran", not "the fix is what makes it
 * pass" — see the async `check()` note in the parent suite.
 *
 * The file on disk is never written; `setPanelSource()` only swaps what the VM
 * executes.
 */
function mutate(field, find, replace) {
  return {
    field,
    apply(source) {
      if (!source.includes(find)) {
        throw new Error(`负向对照的替换目标不存在（admin/app.js 已变化）：${find}`);
      }
      return source.replace(find, replace);
    },
  };
}

const PREFIX_MUTATIONS = [
  mutate(
    "bg_opacity 保存时的 `parseInt(...) || 75`",
    'bg_opacity: Math.min(100, Math.max(0, num("ov-bg-opacity", 75))),',
    'bg_opacity: parseInt($("ov-bg-opacity").value, 10) || 75,',
  ),
  mutate(
    "border_radius 加载时的 `|| 8`",
    'loadNum("ov-border-radius", cfg.overlay.border_radius, 8);',
    '$("ov-border-radius").value = cfg.overlay.border_radius || 8;',
  ),
  mutate(
    "silence_rms 加载时的 `rms > 0 ? rms : 0.012`",
    "    const rmsValue = Math.min(1, Math.max(0,\n      cfgNum(cfg.filter ? cfg.filter.silence_rms : undefined, 0.012)));",
    "    const rmsValue = (() => { const r = Number(cfg.filter && cfg.filter.silence_rms); return isFinite(r) && r > 0 ? r : 0.012; })();",
  ),
  mutate(
    "endpoint 加载时的默认端点预填",
    "if (prefillEndpoint && providerType !== \"mock\" && !$(\"endpoint\").value) {",
    "if (providerType !== \"mock\" && !$(\"endpoint\").value) {",
  ),
];

/** Assert that `run` rejects (the mutation really did break the round trip). */
async function assertPrefixBreaks(name, mutation, body) {
  const pristine = getPanelSource();
  setPanelSource(mutation.apply(pristine));
  try {
    await assert.rejects(
      body,
      () => true,
      `${name}：把旧写法放回去后，往返用例仍然通过 —— 用例没有真正覆盖这个缺陷`,
    );
  } finally {
    setPanelSource(pristine);
  }
}

// ---------------------------------------------------------------------------
// Cases
// ---------------------------------------------------------------------------

/** 0 = fully transparent: `src/config.rs` caps bg_opacity at 100, never floors it. */
const OPACITY_IDS = ["ov-bg-opacity", "ov-opacity-display"];
const ZERO_OVERLAY_IDS = ["ov-border-radius", "ov-bg-width", "ov-bg-height"];

/**
 * One negative control per fixed defect: the pre-fix idiom is put back in
 * memory and the matching round-trip assertion must fail again.
 */
const NEGATIVE_CONTROLS = [
  {
    mutation: PREFIX_MUTATIONS[0],
    run: async () => {
      const rt = await roundTrip(cfgWith({ overlay: { bg_opacity: 0 } }), { ids: OPACITY_IDS });
      assert.equal(rt.loaded["ov-bg-opacity"].value, "0");
      assert.equal(rt.body1.overlay.bg_opacity, 0, "bg_opacity 0 在保存时被写成了 75");
      assert.equal(rt.reloaded["ov-bg-opacity"].value, "0");
    },
  },
  {
    mutation: PREFIX_MUTATIONS[1],
    run: async () => {
      const rt = await roundTrip(cfgWith({ overlay: { border_radius: 0 } }),
        { ids: ["ov-border-radius"] });
      assert.equal(rt.loaded["ov-border-radius"].value, "0", "border_radius 0 在加载时被显示成 8");
      assert.equal(rt.body1.overlay.border_radius, 0);
    },
  },
  {
    mutation: PREFIX_MUTATIONS[2],
    run: async () => {
      const rt = await roundTrip(cfgWith({ filter: { silence_rms: 0 } }),
        { ids: ["silence-rms", "filter-preset"] });
      assert.equal(rt.loaded["silence-rms"].value, "0", "silence_rms 0 在加载时被显示成 0.012");
      assert.equal(rt.body1.filter.silence_rms, 0);
    },
  },
  {
    mutation: PREFIX_MUTATIONS[3],
    run: async () => {
      const rt = await roundTrip(
        cfgWith({ llm: { provider: "openai-realtime", endpoint: null } }),
        { ids: ["endpoint"] },
      );
      assert.equal(rt.loaded.endpoint.value, "", "空 endpoint 在加载时被预填成了默认 URL");
      assert.equal(rt.body1.llm.endpoint, null);
    },
  },
];

export async function run() {
  results.length = 0;

  await check("load→save→reload 保持 overlay.bg_opacity = 0（全透明）", async () => {
    const rt = await roundTrip(cfgWith({ overlay: { bg_opacity: 0 } }), { ids: OPACITY_IDS });
    assert.equal(rt.loaded["ov-bg-opacity"].value, "0", "加载后滑块应是 0");
    assert.equal(rt.loaded["ov-opacity-display"].text, "0%", "透明度的百分数应显示 0%");
    assert.equal(rt.body1.overlay.bg_opacity, 0, "保存的 patch 里 bg_opacity 必须是 0，不能是 75");
    assert.equal(rt.afterSave["ov-bg-opacity"].value, "0", "保存后回读仍应是 0");
    assert.equal(rt.reloaded["ov-bg-opacity"].value, "0", "重新打开页面后仍是 0");
    assert.equal(rt.body2.overlay.bg_opacity, 0, "第二次保存仍是 0");
    assert.equal(rt.store.raw().overlay.bg_opacity, 0, "服务端存的仍是 0");
  });

  await check("load→save→reload 保持 overlay.border_radius = 0（直角）", async () => {
    const rt = await roundTrip(cfgWith({ overlay: { border_radius: 0 } }),
      { ids: ["ov-border-radius"] });
    assertRoundTrip(rt, "ov-border-radius", "0");
    assert.equal(rt.body1.overlay.border_radius, 0, "保存的 patch 里 border_radius 必须是 0，不能是 8");
    assert.equal(rt.body2.overlay.border_radius, 0, "第二次保存仍是 0");
    assert.equal(rt.store.raw().overlay.border_radius, 0, "服务端存的仍是 0");
  });

  await check("load→save→reload 保持 filter.silence_rms = 0（关闭本地静音门限）", async () => {
    const rt = await roundTrip(
      cfgWith({ filter: { silence_rms: 0 }, llm: { speech_noise_threshold: 0.3 } }),
      { ids: ["silence-rms", "filter-preset"] },
    );
    assert.equal(rt.loaded["silence-rms"].value, "0", "加载后静音阈值应是 0，不能是 0.012");
    assert.equal(rt.loaded["filter-preset"].value, "custom",
      "0 不等于任何预设的静音阈值，预设应是「自定义」");
    assert.equal(rt.body1.filter.silence_rms, 0, "保存的 patch 里 silence_rms 必须是 0");
    assert.equal(rt.afterSave["silence-rms"].value, "0", "保存后回读仍应是 0");
    assert.equal(rt.reloaded["silence-rms"].value, "0", "重新打开页面后仍是 0");
    assert.equal(rt.body2.filter.silence_rms, 0, "第二次保存仍是 0");
    assert.equal(rt.store.raw().filter.silence_rms, 0, "服务端存的仍是 0（clamp 到 [0,1] 不会改成默认值）");
  });

  await check("load→save→reload 保持 overlay.bg_width / bg_height = 0（0 = 自动）", async () => {
    const rt = await roundTrip(cfgWith({ overlay: { bg_width: 0, bg_height: 0 } }),
      { ids: ZERO_OVERLAY_IDS });
    assertRoundTrip(rt, "ov-bg-width", "0");
    assertRoundTrip(rt, "ov-bg-height", "0");
    assert.equal(rt.body1.overlay.bg_width, 0);
    assert.equal(rt.body1.overlay.bg_height, 0);
    assert.equal(rt.body2.overlay.bg_width, 0);
    assert.equal(rt.body2.overlay.bg_height, 0);
  });

  await check("load→save→reload 保持 overlay.display_delay_ms = 0（无缓冲）", async () => {
    const rt = await roundTrip(cfgWith({ overlay: { display_delay_ms: 0 } }),
      { ids: ["ov-display-delay"] });
    assertRoundTrip(rt, "ov-display-delay", "0");
    assert.equal(rt.body1.overlay.display_delay_ms, 0,
      "clampDisplayDelay 必须保留 0 = 无缓冲");
    assert.equal(rt.body2.overlay.display_delay_ms, 0);
  });

  await check("load→save→reload 保持 llm.speech_noise_threshold = 0（关）与预设 soft", async () => {
    const rt = await roundTrip(
      cfgWith({ llm: { speech_noise_threshold: 0 }, filter: { silence_rms: 0.007 } }),
      { ids: ["speech-noise-threshold", "filter-preset"] },
    );
    assertRoundTrip(rt, "speech-noise-threshold", "0");
    assert.equal(rt.loaded["filter-preset"].value, "soft");
    // NOTE: as of this writing `collectPatch()` posts the cloud threshold under
    // `filter.` (that is where `filterPresetPatch()` returns it) while the
    // server reads `llm.speech_noise_threshold` — a separate wiring defect that
    // is OUT OF SCOPE for P2-04 and is reported to the parent.  This case only
    // pins P2-04's claim: whichever key carries the value must carry 0.
    const posted1 = rt.body1.llm.speech_noise_threshold ?? rt.body1.filter.speech_noise_threshold;
    const posted2 = rt.body2.llm.speech_noise_threshold ?? rt.body2.filter.speech_noise_threshold;
    assert.equal(posted1, 0, "阈值 0 不能在保存时被默认值替换");
    assert.equal(posted2, 0, "阈值 0 不能在第二次保存时被默认值替换");
  });

  await check("load→save→reload 保持 llm.segment_ms = 0（低延迟关闭，由勾选框承载）", async () => {
    const rt = await roundTrip(cfgWith({ llm: { segment_ms: 0 } }),
      { ids: ["low_latency", "segment_ms"] });
    assert.equal(rt.loaded.low_latency.checked, false, "segment_ms = 0 时勾选框应是未勾选");
    assert.equal(rt.reloaded.low_latency.checked, false);
    assert.equal(rt.body1.llm.segment_ms, 0, "未勾选低延迟时保存的必须是 0");
    assert.equal(rt.body2.llm.segment_ms, 0);
    assert.equal(rt.store.raw().llm.segment_ms, 0);
  });

  await check("load→save→reload 保持所有 false 勾选框为 false", async () => {
    const rt = await roundTrip(
      cfgWith({
        llm: { translate_chinese: false, transcribe: false, gateway_text: false },
        audio: { use_screen_capture_kit: false },
        obs: { auto_connect: false },
      }),
      { ids: ["translate_chinese", "transcribe", "gateway_text", "use_sck", "obs-auto"] },
    );
    for (const id of ["translate_chinese", "transcribe", "gateway_text", "use_sck", "obs-auto"]) {
      assertRoundTrip(rt, id, null, { checked: false });
    }
    assert.equal(rt.body1.llm.translate_chinese, false);
    assert.equal(rt.body1.llm.transcribe, false);
    assert.equal(rt.body1.llm.gateway_text, false);
    assert.equal(rt.body1.audio.use_screen_capture_kit, false);
    assert.equal(rt.body1.obs.auto_connect, false);
    assert.equal(rt.body2.llm.translate_chinese, false);
    assert.equal(rt.body2.audio.use_screen_capture_kit, false);
    assert.equal(rt.body2.obs.auto_connect, false);
  });

  await check("非零值不被改写：bg_opacity 100 / border_radius 24 / 其它非零原样往返", async () => {
    const rt = await roundTrip(
      cfgWith({
        overlay: {
          bg_opacity: 100, border_radius: 24, bg_width: 320, bg_height: 120,
          font_size: 72, max_lines: 4, display_delay_ms: 1000, clear_after_ms: 6000,
        },
        filter: { silence_rms: 0.02 },
        llm: { speech_noise_threshold: 0.45 },
      }),
      { ids: ["ov-bg-opacity", "ov-border-radius", "ov-bg-width", "ov-bg-height",
              "ov-size", "ov-max-lines", "silence-rms"] },
    );
    assertRoundTrip(rt, "ov-bg-opacity", "100");
    assertRoundTrip(rt, "ov-border-radius", "24");
    assertRoundTrip(rt, "ov-bg-width", "320");
    assertRoundTrip(rt, "ov-bg-height", "120");
    assertRoundTrip(rt, "ov-size", "72");
    assertRoundTrip(rt, "ov-max-lines", "4");
    assertRoundTrip(rt, "silence-rms", "0.02");
    assert.equal(rt.body1.overlay.bg_opacity, 100);
    assert.equal(rt.body1.overlay.border_radius, 24);
    assert.equal(rt.body2.overlay.bg_opacity, 100);
    assert.equal(rt.body2.overlay.border_radius, 24);
    assert.equal(rt.body1.filter.silence_rms, 0.02);
    assert.equal(rt.body2.filter.silence_rms, 0.02);
  });

  await check("已支持的 position / animation 原样往返（top / fade）", async () => {
    const rt = await roundTrip(cfgWith({ overlay: { position: "top", animation: "fade" } }),
      { ids: ["ov-position", "ov-animation"] });
    assertRoundTrip(rt, "ov-position", "top");
    assertRoundTrip(rt, "ov-animation", "fade");
    assert.equal(rt.body1.overlay.position, "top");
    assert.equal(rt.body1.overlay.animation, "fade");
    assert.equal(rt.body2.overlay.position, "top");
    assert.equal(rt.body2.overlay.animation, "fade");
  });

  await check("markup 不支持的 position / animation 回落到默认，而不是空字符串", async () => {
    const rt = await roundTrip(cfgWith({ overlay: { position: "left", animation: "zoom" } }),
      { ids: ["ov-position", "ov-animation"] });
    // 直接赋值会让 select 变空白，而空白会被当成 "" 保存回服务端。
    assertRoundTrip(rt, "ov-position", "bottom");
    assertRoundTrip(rt, "ov-animation", "typewriter");
    assert.equal(rt.body1.overlay.position, "bottom");
    assert.equal(rt.body1.overlay.animation, "typewriter");
    assert.equal(rt.body2.overlay.position, "bottom");
    assert.equal(rt.body2.overlay.animation, "typewriter");
  });

  await check("用户手填 0 的字段保存后仍是 0（bg_opacity / border_radius / silence_rms）", async () => {
    const rt = await roundTrip(
      cfgWith({ llm: { speech_noise_threshold: 0.45 }, filter: { silence_rms: 0.012 } }),
      {
        ids: ["ov-bg-opacity", "ov-border-radius", "silence-rms", "filter-preset"],
        mutate: async (panel) => {
          typeValue(panel, "ov-bg-opacity", 0);
          typeValue(panel, "ov-border-radius", 0);
          typeValue(panel, "silence-rms", 0);
        },
      },
    );
    assert.equal(rt.first.el("filter-preset").value, "custom");
    assert.equal(rt.body1.overlay.bg_opacity, 0, "手填 0 被写成了 75");
    assert.equal(rt.body1.overlay.border_radius, 0);
    assert.equal(rt.body1.filter.silence_rms, 0, "手填 0 被写成了 0.012");
    assert.equal(rt.body2.overlay.bg_opacity, 0);
    assert.equal(rt.body2.filter.silence_rms, 0);
    assert.equal(rt.store.raw().overlay.bg_opacity, 0);
  });

  await check("空输入框 ≠ 0：清空 silence_rms 后保存回落到 0.012，而不是关掉门限", async () => {
    const rt = await roundTrip(
      cfgWith({ llm: { speech_noise_threshold: 0.45 }, filter: { silence_rms: 0.012 } }),
      {
        ids: ["silence-rms"],
        mutate: async (panel) => {
          const el = panel.el("silence-rms");
          el.value = "";
          el.dispatchEvent({ type: "input" });
        },
      },
    );
    assert.equal(rt.body1.filter.silence_rms, 0.012,
      "空输入框没有值，不能当成用户要 0（Number(\"\") === 0）");
    assert.equal(rt.reloaded["silence-rms"].value, "0.012");
  });

  await check("endpoint 留空（null = 用内置默认）不会被加载路径填成具体 URL", async () => {
    const rt = await roundTrip(
      cfgWith({ llm: { provider: "openai-realtime", endpoint: null, model: "gpt-realtime" } }),
      { ids: ["endpoint"] });
    assert.equal(rt.loaded.endpoint.value, "", "加载后 Base URL 应保持留空");
    assert.equal(rt.body1.llm.endpoint, null,
      "留空的 Base URL 必须原样保存为 null，而不是被预填的默认地址替换");
    assert.equal(rt.reloaded.endpoint.value, "");
    assert.equal(rt.body2.llm.endpoint, null);
    assert.equal(rt.store.raw().llm.endpoint, null);
  });

  await check("切换服务商时仍会填入该服务商的默认端点（便利保留）", async () => {
    const panel = await bootPanel({
      config: cfgWith({ llm: { provider: "openai-realtime", endpoint: null } }),
    });
    assert.equal(panel.el("endpoint").value, "", "加载时不应预填");
    panel.el("provider-type").value = "glm";
    panel.el("provider-type").dispatchEvent({ type: "change" });
    assert.equal(panel.el("endpoint").value, "wss://open.bigmodel.cn/api/paas/v4/realtime",
      "显式切换服务商时仍应填入默认端点");
  });

  await check("实时的 RMS 读数用真实静音阈值（0.012 不被 parseInt 截成 0）", async () => {
    const panel = await bootPanel({ config: cfgWith({ filter: { silence_rms: 0.012 } }) });
    const readout = panel.el("input-level-rms").textContent;
    assert.equal(readoutThreshold(readout), "0.012",
      `读数里的静音阈值应是 0.012，实际：${readout}`);
    assert.ok(readout.includes("低于阈值"),
      `0.01 的输入应低于 0.012 的阈值（旧代码把阈值读成 0 而给不出比较），实际：${readout}`);
  });

  await check("silence_rms = 0 时读数明确显示 0（门限关闭），不谎报阈值", async () => {
    const panel = await bootPanel({ config: cfgWith({ filter: { silence_rms: 0 } }) });
    const readout = panel.el("input-level-rms").textContent;
    assert.equal(readoutThreshold(readout), "0", `实际：${readout}`);
  });

  await check("管理页预览跟随 OBS 的累计字幕翻页，不重置到第一页", async () => {
    const original = getPanelSource();
    assert.match(original, /\}\)\(\);\s*$/, "panel probe insertion point missing");
    setPanelSource(original.replace(/\}\)\(\);\s*$/,
      "  globalThis.__previewProbe = () => ({ pageStart: previewPageStart, text: previewText });\n})();\n"));
    try {
      const panel = await bootPanel({ config: cfgWith({ overlay: { max_lines: 2 } }) });
      const line = panel.el("preview-line");
      Object.defineProperty(line, "scrollHeight", {
        configurable: true,
        get: () => Math.ceil(line.textContent.length / 20) * 60,
      });
      const ws = panel.wsInstances[0];
      assert.ok(ws, "admin subtitle socket was not opened");
      ws.emit({ type: "partial", replace: true, text: "甲".repeat(60) });
      assert.equal(panel.sandbox.__previewProbe().pageStart, 40);
      ws.emit({ type: "partial", replace: true, text: "甲".repeat(65) });
      assert.equal(panel.sandbox.__previewProbe().pageStart, 40,
        "同一句的修订不应重置预览页码");
      assert.equal(line.textContent, "甲".repeat(25),
        "管理页应显示 OBS 当前尾页，而非重新显示首页");
    } finally {
      setPanelSource(original);
    }
  });

  await check("实时预览直接加载 OBS 的只读字幕页面和 1920×540 视口", async () => {
    const overlayUrl = `http://127.0.0.1:8897/overlay?token=${"b".repeat(64)}`;
    const panel = await bootPanel({ statusOverrides: { overlay_url: overlayUrl } });
    const frame = panel.el("preview-obs-frame");
    const viewport = panel.el("preview-obs-viewport");
    assert.equal(frame.src, overlayUrl);
    assert.equal(viewport.style.width, "864px");
    assert.equal(viewport.style.height, "243px");
    assert.equal(frame.style.transform, "scale(0.45)");
    frame.onload();
    assert.equal(viewport.hidden, false);
    assert.equal(panel.el("preview-stage").hidden, true);
  });

  // The suite must be able to fail: with each pre-fix idiom restored in memory,
  // the round trip above has to break again.
  for (const control of NEGATIVE_CONTROLS) {
    await check(`负向对照：${control.mutation.field} → 对应的往返用例必须失败`, async () => {
      await assertPrefixBreaks(control.mutation.field, control.mutation, control.run);
    });
  }

  return results;
}

// Allow `node tests/admin-roundtrip-cases.mjs` for a direct run.
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
