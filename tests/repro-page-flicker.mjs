#!/usr/bin/env node
// REPRODUCTION for the long-sentence page flicker (user report #2):
//   「当一句话比较长时出第二段字幕的时候第一段字幕可能会和第二段字幕交替闪烁」
//
//   node tests/repro-page-flicker.mjs            # all scenarios
//   node tests/repro-page-flicker.mjs S2         # one scenario
//
// It drives the REAL overlay/app.js (via tests/overlay-harness.mjs) with
// cumulative `replace:true` frames — the protocol Bailian Fun-ASR uses (see
// src/llm.rs -> SubtitleEvent::Replace) — and records EVERY page handed to
// displayShown() through an in-memory probe, so "what was rendered" is measured
// instead of inferred.
//
// Each rendered page's start offset is exact by construction:
//     offset = pageStart_after_render - rendered.length
// because pickShown() advances `pageStart` to the END of the page it returns.
//
// The invariant is the one the user can see: while ONE long sentence pages
// forward, the rendered page must never move BACKWARDS to an earlier page.
// A backwards move is exactly "第一段/第二段交替闪烁": page 2, then page 1,
// then page 2 again.
//
// Exit code 1 when any scenario jumps backwards (the defect), 0 when clean.

import { loadOverlay } from "./overlay-harness.mjs";

const DISPLAY_DELAY_MS = 750;

const PROBE = (src) => {
  const anchor = "  function displayShown(shown) {";
  if (!src.includes(anchor)) throw new Error("displayShown anchor not found in overlay/app.js");
  let out = src.replace(
    anchor,
    "  const __rendered = [];\n  function displayShown(shown) {\n    __rendered.push(shown);"
  );
  const tail = "  init();\n})();";
  if (!out.includes(tail)) throw new Error("tail anchor not found in overlay/app.js");
  out = out.replace(
    tail,
    "  init();\n  globalThis.__probe = function () { return { pageStart: pageStart, partialBuffer: partialBuffer, currentText: currentText, rendered: __rendered.slice() }; };\n})();"
  );
  return out;
};

// A long sentence that needs several two-line pages (a two-line page is 62 CJK
// chars under the shim geometry, so this is ~3 pages of 125 chars).
const BASE =
  "直播字幕遇到特别长的句子时必须持续换页而不能依赖滚动，所以这一段刻意写得比两行长得多：" +
  "它要确认翻过去的内容不会在下一帧整段重新出现，也要确认全程没有任何一帧同时显示三行，" +
  "并且换页之后仍能一直读到句尾，如果句子还能更长分页逻辑也应当继续推进到最后一页。";

/// Cumulative frames that only ever grow (the happy path: no re-decoding).
const growthFrames = (text) => Array.from({ length: text.length }, (_, i) => text.slice(0, i + 1));

/// Cumulative frames where the recognizer RE-DECODES the tail every 6th frame
/// (a homophone correction / punctuation change near the end).  Frame N is then
/// neither an extension nor a trim of frame N-1 — exactly what real ASR
/// revisions do and what the overlay's own replay samples never do.
function tailRevisionFrames(text) {
  const frames = [];
  for (let i = 0; i < text.length; i++) {
    const head = text.slice(0, i + 1);
    frames.push(i > 8 && i % 6 === 0 ? head.slice(0, -1) + "。" : head);
  }
  return frames;
}

/// Cumulative frames where a word in the MIDDLE of the open sentence is
/// corrected (insertion before the current page cursor), which shifts every
/// offset after the insertion point.
function midRevisionFrames(text) {
  const frames = [];
  for (let i = 0; i < text.length; i++) {
    let head = text.slice(0, i + 1);
    if (i === 100) head = head.slice(0, 40) + "（已更正）" + head.slice(40);
    frames.push(head);
  }
  return frames;
}

const SCENARIOS = {
  S1: { title: "S1 cumulative growth, no re-decoding (baseline — must stay clean)", animation: "fade", frames: growthFrames },
  S2: { title: "S2 cumulative growth + tail re-decode every 6th frame (real ASR)", animation: "fade", frames: tailRevisionFrames },
  S3: { title: "S3 cumulative growth + a mid-sentence correction at frame 101", animation: "fade", frames: midRevisionFrames },
  S4: { title: "S4 same as S2 but on the typewriter path (the shipped default)", animation: "typewriter", frames: tailRevisionFrames },
};

async function runScenario(key) {
  const s = SCENARIOS[key];
  const ov = await loadOverlay({ transform: PROBE, overlayConfig: { animation: s.animation } });

  const offsets = [];
  const renderedTexts = [];
  const jumps = [];

  let logCursor = 0;
  let sentFrames = 0;

  for (const text of s.frames(BASE)) {
    ov.emitPartial(text, true);
    sentFrames++;
    ov.tick(DISPLAY_DELAY_MS); // let the page buffer release
    // Drain the typewriter so the recorded page is the whole page (the probe
    // records displayShown()'s argument, which is the page, not the typed
    // prefix — draining only keeps the DOM comparable).
    for (let i = 0; i < 200 && ov.pendingTimeouts().length > 1; i++) ov.tick(16);
    const st = ov.sandbox.__probe();
    const fresh = st.rendered.slice(logCursor);
    logCursor = st.rendered.length;
    if (fresh.length === 0) continue; // nothing new was rendered this frame
    const page = fresh[fresh.length - 1];
    // Every rendered page is handed to displayShown() as a slice of the line
    // renderShow() just published as `currentText`, so its offset in that line
    // IS the page offset.  (pageStart cannot be used: pickShown() only advances
    // it when it actually cuts a page.)
    const at = page === "" ? st.pageStart : st.currentText.indexOf(page);
    offsets.push({ frame: sentFrames, at, page, pageStart: st.pageStart, text: st.currentText });
    renderedTexts.push(page);
  }

  let previous = 0;
  let previousFrame = 0;
  for (const o of offsets) {
    if (o.at < previous) {
      jumps.push({ ...o, from: previous, fromFrame: previousFrame });
    }
    previous = o.at;
    previousFrame = o.frame;
  }
  return { key, title: s.title, offsets, jumps };
}

const only = process.argv[2];
const keys = only ? [only] : Object.keys(SCENARIOS);
for (const k of keys) {
  if (!SCENARIOS[k]) {
    console.error(`unknown scenario ${k}; known: ${Object.keys(SCENARIOS).join(", ")}`);
    process.exit(2);
  }
}

console.log("=".repeat(78));
console.log("page-flicker reproduction — REAL overlay/app.js, cumulative replace frames");
console.log("=".repeat(78));
console.log(`sentence: ${BASE.length} chars (two-line page under the shim = 62 chars)`);
console.log("invariant: a long sentence that pages forward must never render an EARLIER page again\n");

let totalJumps = 0;
for (const k of keys) {
  const r = await runScenario(k);
  const distinct = [...new Set(r.offsets.map((o) => o.at))];
  console.log("-".repeat(78));
  console.log(r.title);
  console.log(
    `  rendered pages=${r.offsets.length}  distinct page offsets=${distinct.length} [${distinct.join(", ")}]`
  );
  // Run-length view of the rendered page offset over time.
  const runs = [];
  for (const o of r.offsets) {
    const last = runs[runs.length - 1];
    if (last && last.value === o.at) last.to = o.frame;
    else runs.push({ value: o.at, from: o.frame, to: o.frame });
  }
  console.log(
    "  rendered page offset over time: " +
      runs.slice(0, 30).map((x) => `${x.value}${x.from === x.to ? "" : `(f${x.from}-${x.to})`}`).join(" -> ") +
      (runs.length > 30 ? ` … (${runs.length} runs)` : "")
  );
  console.log(
    "  sequence at the paging transition: " +
      r.offsets.slice(55, 75).map((o) => o.at).join(",")
  );
  if (r.jumps.length) {
    console.log(`  BACKWARD PAGE MOVES: ${r.jumps.length}`);
    for (const j of r.jumps.slice(0, 5)) {
      console.log(
        `    frame ${j.frame}: page start ${j.from} -> ${j.at}   ` +
          `rendered ${JSON.stringify(j.page.slice(0, 16))}… (${j.page.length} chars)`
      );
    }
    if (r.jumps.length > 5) console.log(`    … ${r.jumps.length - 5} more`);
  } else {
    console.log("  BACKWARD PAGE MOVES: 0");
  }
  totalJumps += r.jumps.length;
  console.log("");
}

console.log("=".repeat(78));
if (totalJumps > 0) {
  console.log(
    `RESULT: FAIL — ${totalJumps} backward page move(s): the caption falls back to page 1 ` +
      "mid-sentence and returns to page 2 = the reported 第一段/第二段交替闪烁."
  );
  process.exit(1);
}
console.log("RESULT: OK — a paging sentence never renders an earlier page again.");
process.exit(0);
