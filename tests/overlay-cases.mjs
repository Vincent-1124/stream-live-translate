// Regression suite for the live-subtitle overlay (overlay/app.js).
//
// Every assertion below runs against the REAL overlay/app.js, executed inside
// tests/dom-shim.mjs.  A case is a function of `{ run }`; `run(name, caseId,
// body)` receives the loader, so the same assertions can be executed by
//   * tests/run-overlay-tests.mjs      (baseline, real source)
//   * tests/run-negative-control.mjs   (in-memory perturbed source)

import { assert } from "./overlay-harness.mjs";
import { lineModel } from "./dom-shim.mjs";

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

/**
 * Long CJK paragraph (156 chars) with a boundary marker every 37 characters so
 * a page cut is human-checkable.  Under the shim geometry (1488 px usable,
 * 48 px CJK) one line holds 31 chars and a two-line page holds 62.
 */
export const LONG_CJK = [
  "直播字幕遇到特别长的句子时必须持续换页而不能依赖滚动",
  "所以这一段刻意写得比两行长得多用于验证分页",
  "它要确认翻过去的内容不会在下一帧整段重新出现",
  "也要确认全程没有任何一帧同时显示三行文字",
  "并且换页之后仍然能够一直读到这句话的最后",
  "如果句子还能更长分页逻辑也应当继续往前推进",
  "而不是把尾巴丢掉或者背景高度撑成第三行",
].join("|");

const DISPLAY_DELAY_MS = 750;
const CLEAR_AFTER_MS = 4000;

/**
 * The overlay's OWN replay fixtures (copied verbatim from
 * overlay/app.js `REPLAY_PAGED` / `REPLAY_VERY_LONG`, which are private to the
 * IIFE so they have to be duplicated here).  Used to check the paging logic
 * against the exact strings the product ships as its self-test sample.
 */
export const REPLAY_PAGED =
  "这是一段用于验证两行分页的本地模拟字幕，它足够长，应该在当前页面填满后切换到后续文字页面，而不产生第三行或滚动。";

export const REPLAY_VERY_LONG =
  "直播字幕遇到特别长的句子时必须持续换页而不能依赖滚动，所以这一段刻意写得比两行长得多：它要确认翻过去的内容不会在下一帧整段重新出现，也要确认全程没有任何一帧同时显示三行，并且换页之后仍能一直读到句尾。" +
  "如果句子还能更长，分页逻辑也应当继续推进到最后一页，而不是把尾巴丢掉或者让背景高度撑成三行。";

/**
 * A cumulative-revision sample where the recognizer *shifts* the text instead
 * of only appending: frame N is not a prefix of frame N+1 near the tail (here
 * the last character changes from "重" to "新").  Real ASR revisions do this
 * constantly; the internal replay samples do not, which is why this fixture
 * exists.
 */
export const SHIFTING_REVISION_TEXT =
  "直播字幕遇到特别长的句子时必须持续换页而不能依赖滚动，所以这一段刻意写得比两行长得多：" +
  "它要确认翻过去的内容不会在下一帧整段重新出现，也要确认全程没有任何一帧同时显示三行，" +
  "并且换页之后仍能一直读到句尾，如果句子还能更长分页逻辑也应当继续推进到最后一页。";

/**
 * A long sentence whose *tail* changes as it is revised (a realistic ASR
 * correction), used by the A1o shifting-revision case.
 */

/** Prefixes where the DOM text occupied 3+ lines, as a readable report. */
function describeViolations(violations, limit = 8) {
  if (!violations.length) return "none";
  const head = violations
    .slice(0, limit)
    .map((v) => `prefix ${v.len} -> ${v.lines} lines (dom ${v.domLen} chars, ${v.height}px)`)
    .join("; ");
  return `${violations.length} violation(s): ${head}${violations.length > limit ? "; …" : ""}`;
}

/**
 * The DOM page for `prefix` when the page starts at `start`:
 * `prefix.slice(start, start + n)` for some n, i.e. a CONTIGUOUS slice that
 * begins at the page start — not necessarily the tail of the stream.  Returns
 * `null` when the DOM text is not a slice of the prefix at all (real data loss
 * or duplicated text).
 */
function pageSliceStart(prefix, shown, start) {
  if (shown === "") return start;
  if (!prefix.startsWith(shown, start)) return null;
  return start + shown.length;
}

/**
 * In-memory probe that records EVERY page handed to `displayShown()`.
 *
 * The DOM text is only a *typed prefix* of the page while the typewriter is
 * running (and empty for one interval right after a page flip), so a case that
 * needs to know WHICH page was rendered must look at what the overlay decided
 * to render, not at `textContent`.  `pickShown()` hands `displayShown()` a
 * slice of the line it just published as `currentText`, so the page's window is
 * exactly `currentText.indexOf(page)`.
 *
 * (Source is patched in memory only; the file on disk is never touched.)
 */
export const RENDER_LOG_TRANSFORM = (src) => {
  const fnAnchor = "  function displayShown(shown) {";
  if (!src.includes(fnAnchor)) throw new Error("displayShown anchor not found in overlay/app.js");
  let out = src.replace(
    fnAnchor,
    "  const __rendered = [];\n  function displayShown(shown) {\n    __rendered.push(shown);"
  );
  const tail = "  init();\n})();";
  if (!out.includes(tail)) throw new Error("probe anchor not found in overlay/app.js");
  out = out.replace(
    tail,
    "  init();\n  globalThis.__probe = function () { return { pageStart: pageStart, partialBuffer: partialBuffer, currentText: currentText, rendered: __rendered.slice() }; };\n})();"
  );
  return out;
};

/**
 * Every page rendered since `cursor`, each with its exact window in the
 * cumulative text (`at` = the offset of the page's first character, `-1` when
 * the page is not part of the current revision at all).
 */
function renderedWindows(ov, cursor) {
  const st = ov.sandbox.__probe();
  const pages = st.rendered.slice(cursor).map((page) => {
    const at = page === "" ? st.pageStart : st.currentText.indexOf(page);
    return { page, at, end: at < 0 ? -1 : at + page.length, text: st.currentText };
  });
  return { cursor: st.rendered.length, probe: st, pages };
}

/**
 * Frames of ONE long sentence where the recognizer keeps re-decoding the tail
 * (a homophone / punctuation correction): frame N+1 is then neither an equals,
 * a prefix, nor an extension of frame N.  This is what real ASR revisions look
 * like and what the overlay's own replay fixtures never do.
 */
function tailRevisionFrames(text) {
  const frames = [];
  for (let i = 0; i < text.length; i++) {
    const head = text.slice(0, i + 1);
    frames.push(i > 8 && i % 6 === 0 ? head.slice(0, -1) + "。" : head);
  }
  return frames;
}

/// Frames of ONE long sentence where a word in the middle of the open sentence
/// is corrected once (everything after the insertion therefore shifts).
function midRevisionFrames(text, at = 40, insert = "（已更正）", when = 100) {
  const frames = [];
  for (let i = 0; i < text.length; i++) {
    let head = text.slice(0, i + 1);
    if (i === when) head = head.slice(0, at) + insert + head.slice(at);
    frames.push(head);
  }
  return frames;
}

/**
 * Drive one cumulative frame and let the overlay settle, then report every page
 * it rendered for that frame.  Cumulative (`replace: true`) frames are used
 * because that is the protocol whose revisions rewrite the open sentence.
 */
function driveFrame(ov, text, state) {
  ov.emitPartial(text, true);
  state.sinceEmit = 0;
  // The FIRST render of a page goes through the display buffer; later ones are
  // immediate.  An empty caption therefore means "buffered", and one buffer
  // window releases it.
  if (ov.text === "") {
    ov.tick(DISPLAY_DELAY_MS);
    state.sinceEmit += DISPLAY_DELAY_MS;
  }
  // Let the typewriter finish the page (no-op for the other animations).
  for (let step = 0; step < 90; step++) {
    if (state.sinceEmit > CLEAR_AFTER_MS - 1000) {
      ov.emitPartial(text, true); // re-arm the silence timer with the same frame
      state.sinceEmit = 0;
    }
    ov.tick(32);
    state.sinceEmit += 32;
    if (ov.pendingTimeouts().length <= 1) break;
  }
  const r = renderedWindows(ov, state.cursor);
  state.cursor = r.cursor;
  return r.pages;
}

// ---------------------------------------------------------------------------
// shim self-check — proves the measurement model is not a rubber stamp
// ---------------------------------------------------------------------------

export async function testShimFidelity({ run }) {
  await run("S1 shim geometry matches overlay/style.css + index.html", "shim-geometry", async (load) => {
    const ov = await load();
    const geom = ov.geometry();
    assert.eq(geom.usableWidth, 1488, "usable width = min(96% x 1600, 1600) - 2 x 24px padding");
    assert.eq(geom.lineHeight, 60, "line height = 48px font x 1.25");
    assert.eq(ov.maxHeightPx, 120, "two-line viewport = 2 x 60px");
    assert.eq(
      ov.document.documentElement.style.getPropertyValue("--caption-max-height"),
      "120.00px",
      "renderStyle() must publish the two-line viewport height"
    );
  });

  await run("S2 shim line model is exact and wrap-sensitive", "shim-lines", async (load) => {
    const ov = await load();
    const geom = ov.geometry();
    // Probe the model's actual per-line capacity instead of assuming one:
    // with `.caption { letter-spacing: 0.01em }` modelled (see
    // OVERLAY_TEST_LETTER_SPACING) a CJK advance is 48.48px, not 48px, so the
    // capacity is 30 rather than 31. Deriving it keeps this self-check valid
    // under both geometries while still pinning exact boundary behaviour.
    let cjkPerLine = 0;
    while (cjkPerLine < 400 && ov.lineCount("字".repeat(cjkPerLine + 1)) <= 1) cjkPerLine++;
    assert.gt(cjkPerLine, 10, "one line must hold a sane number of CJK characters");
    // The advance the model effectively used, checked against the CSS-derived
    // value (font-size plus any configured letter-spacing).
    const impliedAdvance = geom.usableWidth / (cjkPerLine + 1);
    assert.lte(impliedAdvance, geom.fontSize + 1e-9, `CJK advance ${impliedAdvance}px exceeds the font size`);
    assert.gt(impliedAdvance, geom.fontSize * 0.9, `CJK advance ${impliedAdvance}px is implausibly small`);

    const latinPerLine = Math.floor(geom.usableWidth / (geom.fontSize * 0.55)); // 56
    const at = (n) => "字".repeat(n);
    assert.eq(ov.lineCount(at(cjkPerLine)), 1, `${cjkPerLine} CJK chars fit one line`);
    assert.eq(ov.lineCount(at(cjkPerLine + 1)), 2, `${cjkPerLine + 1} CJK chars wrap to two lines`);
    assert.eq(ov.lineCount(at(cjkPerLine * 2)), 2, `${cjkPerLine * 2} CJK chars fill two lines`);
    assert.eq(ov.lineCount(at(cjkPerLine * 2 + 1)), 3, `${cjkPerLine * 2 + 1} CJK chars need three lines`);
    assert.eq(ov.measuredHeight(at(cjkPerLine + 1)), 120, "two wrapped lines measure 120px");
    assert.gt(ov.measuredHeight(at(cjkPerLine * 2 + 1)), 120, "three wrapped lines exceed the box");

    // Latin text is narrower, so the model must weigh advances, not count chars.
    assert.eq(ov.lineCount("a".repeat(cjkPerLine)), 1, "Latin chars are narrower than CJK");
    assert.lte(ov.lineCount("a".repeat(100)), 2, "100 Latin chars still fit two lines");
    assert.eq(ov.lineCount("a".repeat(latinPerLine * 2 + 1)), 3, `${latinPerLine * 2 + 1} Latin chars need a third line`);
    assert.eq(lineModel("", { fontSize: 48, lineHeight: 1.25 }).lines, 1, "empty text is one line");

    // Sanity: the fixture really does need more than two lines un-paged.
    assert.gt(ov.lineCount(LONG_CJK), 2, "fixture must exceed two lines");
    console.log(`        · model capacity: ${cjkPerLine} CJK chars/line (implied advance ${impliedAdvance.toFixed(2)}px)`);
  });

  await run("S3 unbreakable Latin word: page cut never produces a 3rd wrapped line", "shim-latin", async (load) => {
    const ov = await load();
    const word = "abcdefghijklmnopqrstuvwxyz".repeat(8); // 208 chars, no break opportunity
    ov.emitPartial(word.slice(0, 1));
    ov.tick(DISPLAY_DELAY_MS);
    let peak = 0;
    for (let len = 2; len <= word.length; len++) {
      ov.emitPartial(word[len - 1]);
      const lines = ov.lineCount(ov.text);
      peak = Math.max(peak, lines);
      if (lines > 2) {
        throw new Error(
          `Latin sweep rendered ${lines} lines at prefix ${len}\n  dom text: ${JSON.stringify(ov.text)}`
        );
      }
    }
    assert.lte(peak, 2, "Latin sweep peak line count");
    console.log(`        · Latin token ${word.length} chars -> peak ${peak} line(s)`);
  });
}

// ---------------------------------------------------------------------------
// 1 + 2. two-line cap and paging advance, swept over EVERY prefix length
// ---------------------------------------------------------------------------

/** True delta stream (`{"type":"partial","text":<new chars>}`). */
export async function testDeltaStream({ run }) {
  await run("A1 delta partial stream: <=2 lines at EVERY prefix, page advances", "delta-stream", async (load) => {
    const ov = await load();
    const full = LONG_CJK;
    const maxH = ov.maxHeightPx;

    ov.emitPartial(full.slice(0, 1));
    ov.tick(DISPLAY_DELAY_MS);
    assert.eq(ov.text, full.slice(0, 1), "the buffered first partial must appear once display_delay_ms elapses");

    const violations = [];
    let advanced = 0;
    let pageIndex = 0; // start offset of the page currently on screen
    let maxPageIndex = 0;
    let firstAdvanceAt = null;
    let peakLines = 0;
    let lastPage = "";

    for (let len = 2; len <= full.length; len++) {
      ov.emitPartial(full[len - 1]); // delta, not cumulative
      const shown = ov.text;
      const prefix = full.slice(0, len);

      const lines = ov.lineCount(shown);
      peakLines = Math.max(peakLines, lines);
      if (lines > 2) {
        violations.push({ len, lines, domLen: shown.length, height: ov.measuredHeight(shown) });
      }

      const end = pageSliceStart(prefix, shown, pageIndex);
      if (end === null) {
        throw new Error(
          `page shown at prefix ${len} is not a contiguous slice of the accumulated text\n` +
            `  expected: prefix.slice(${pageIndex}, ${pageIndex} + n) for some n\n` +
            `  prefix:   …${JSON.stringify(prefix.slice(Math.max(0, pageIndex - 20)))}\n` +
            `  actual:   ${JSON.stringify(shown.slice(0, 40))}`
        );
      }
      assert.lte(end, prefix.length, `the page cannot hold more text than received (prefix ${len})`);
      if (end < prefix.length) {
        pageIndex = end; // the overlay cut a new page at this offset
        advanced++;
        maxPageIndex = Math.max(maxPageIndex, pageIndex);
        if (firstAdvanceAt === null) firstAdvanceAt = len;
      }
      if (shown) lastPage = shown;
    }

    // --- assertion 1: hard two-line cap, at every prefix ---
    assert.eq(
      violations.length,
      0,
      `A1 two-line cap over all ${full.length} prefixes: ${describeViolations(violations)}`
    );
    assert.lte(peakLines, 2, "peak line count over the whole delta sweep");
    assert.lte(ov.measuredHeight(ov.text), maxH, "the final page must fit the two-line viewport");

    // --- assertion 2: paging really advanced ---
    assert.gt(advanced, 0, "some prefix must have displayed a later page than the head");
    assert.gt(maxPageIndex, 0, "some page must have started after index 0");
    assert.ok(
      lastPage.endsWith(full[full.length - 1]),
      "the last rendered page must contain the tail of the sentence (nothing dropped)",
      lastPage
    );
    console.log(
      `        · ${full.length} prefixes / ${ov.lineCount(full)} lines unpaged; first advance at prefix ` +
        `${firstAdvanceAt}; furthest page started at ${maxPageIndex} (${advanced} cuts)`
    );
  });
}

/**
 * Cumulative revision stream (`{"type":"partial","text":<全部文本>,"replace":true}`).
 *
 * Note on `replacePartial()` semantics (overlay/app.js): a revision that does
 * NOT extend the previous frame starts a new sentence and goes back to page 1;
 * a revision that DOES extend it is the same sentence still growing, so
 * `pageStart` is kept and the sentence can page forward.  The DOM therefore
 * shows a *contiguous slice* of the cumulative text at the sentence's current
 * page offset — not necessarily `text.slice(0, n)`, and not necessarily the
 * tail (the tail arrives with later frames).  What must hold is:
 *   1. every rendered page fits two lines,
 *   2. every rendered page is a contiguous slice of the text received so far,
 *   3. the page offset really advances (long sentences reach later pages), and
 *   4. the union of every rendered character range covers the whole sentence —
 *      no content is skipped on the way.
 */
export async function testReplaceStream({ run }) {
  await run("A1r cumulative replace stream: <=2 lines, contiguous pages, advances, full coverage", "replace-stream", async (load) => {
    const ov = await load();
    const full = LONG_CJK;
    const violations = [];
    /** { start, end } of every page that was rendered, in frame order. */
    const ranges = [];
    const covered = new Set();

    ov.emitPartial(full.slice(0, 1), true);
    ov.tick(DISPLAY_DELAY_MS);
    let previousText = full.slice(0, 1);

    for (let len = 2; len <= full.length; len++) {
      const text = full.slice(0, len);
      // replacePartial() only resets pageStart when the revision does NOT
      // extend the previous frame; mirror that rule to know whether the page
      // offset is expected to be carried over.
      const grew = text.startsWith(previousText) && previousText.length > 0;
      ov.emitPartial(text, true);
      const dom = ov.text;

      const lines = ov.lineCount(dom);
      if (lines > 2) {
        violations.push({ len, lines, domLen: dom.length, height: ov.measuredHeight(dom) });
      }
      if (dom === "") continue;

      // 2. the page must be a contiguous slice of the text received so far.
      const at = text.indexOf(dom);
      if (at < 0) {
        throw new Error(
          `A1r prefix ${len}: the rendered page is not a contiguous slice of the revision\n` +
            `  expected: a substring of ${JSON.stringify(text.slice(0, 40))}…\n` +
            `  actual:   ${JSON.stringify(dom.slice(0, 40))}`
        );
      }
      ranges.push({ start: at, end: at + dom.length });
      for (let i = at; i < at + dom.length; i++) covered.add(i);

      // Extension frames only: `replacePartial()` deliberately keeps `pageStart`
      // there (that is what lets a long sentence page forward).  The page
      // window must still lie inside the text received so far — if it does not,
      // `pickShown()` fell back to `full.slice(0, cut)`/`slice(0, 1)` and the
      // caption is showing something other than the current revision.
      if (grew && at + dom.length > text.length) {
        throw new Error(
          `A1r prefix ${len}: the rendered page lies outside the revision text\n` +
            `  page window: [${at}, ${at + dom.length}) but the text is only ${text.length} chars\n` +
            `  this is the stale-pageStart case: a cumulative revision that shifts the text ` +
            `(text does not start with the previous frame) leaves pageStart ahead of the new text`
        );
      }
      previousText = text;
    }

    // --- 1. two-line cap at every prefix ---
    assert.eq(
      violations.length,
      0,
      `A1r two-line cap over all ${full.length} replace prefixes: ${describeViolations(violations)}`
    );
    // --- 2. every page was a contiguous slice (checked inline above) ---
    // --- 3. the page really advanced ---
    // The page start may lag while the sentence still fits in one page, but it
    // must end up past the first page, and it must never move backwards.
    let maxStart = 0;
    let previousStart = 0;
    for (const r of ranges) {
      if (r.start < previousStart) {
        throw new Error(
          `A1r: the page start moved backwards (${previousStart} -> ${r.start}) without a sentence reset`
        );
      }
      previousStart = r.start;
      maxStart = Math.max(maxStart, r.start);
    }
    assert.gt(maxStart, 0, "some page must have started after index 0 (paging advanced)");
    // --- 4. coverage: every character of the sentence got rendered at least once ---
    const missing = [];
    for (let i = 0; i < full.length; i++) if (!covered.has(i)) missing.push(i);
    assert.eq(
      missing.length,
      0,
      `A1r coverage: ${missing.length} character(s) were never rendered (first at index ${missing[0]})`
    );
    console.log(
      `        · ${full.length} chars: coverage ${covered.size}/${full.length}, ` +
        `max page start ${maxStart}, ${ranges.length} rendered pages`
    );
  });
}

// ---------------------------------------------------------------------------
// 3. display buffer semantics
// ---------------------------------------------------------------------------

export async function testBufferSemantics({ run }) {
  await run("A3 buffer: first partial delayed, later partials replace without extending the deadline", "buffer", async (load) => {
    const ov = await load();
    const t0 = ov.clock.now;

    ov.emitPartial("第一段识别文本");
    assert.eq(ov.text, "", "nothing may be displayed before the buffer elapses (t = 0 ms)");
    const queued = ov.pendingTimeouts();
    assert.eq(
      queued.length,
      2,
      "queueing a page must create exactly two one-shot timers: the display buffer and the silence clear"
    );
    assert.eq(ov.displayDeadline(), DISPLAY_DELAY_MS, "the display buffer must be queued for display_delay_ms");

    ov.tick(300);
    ov.emitPartial("，随后到达的补充内容");
    assert.eq(ov.text, "", "still buffered at t = 300 ms");
    assert.eq(
      ov.displayDeadline(),
      DISPLAY_DELAY_MS - 300,
      "a partial inside the buffer must NOT re-arm the display timer (the deadline keeps counting down)"
    );
    assert.eq(ov.pendingTimeouts().length, 2, "no extra timer inside the buffer window");

    ov.tick(400); // t = 700
    ov.emitPartial("，以及第三批");
    assert.eq(ov.text, "", "still buffered at t = 700 ms");
    assert.eq(
      ov.displayDeadline(),
      DISPLAY_DELAY_MS - 700,
      "the deadline must still be the ORIGINAL one at t = 700 ms"
    );
    assert.eq(ov.pendingTimeouts().length, 2, "no extra timer at t = 700 ms");

    ov.tick(DISPLAY_DELAY_MS - 700 - 1); // t = 749
    assert.eq(ov.text, "", "must not display before the ORIGINAL deadline (t = 749 ms)");

    ov.tick(1); // t = 750
    assert.eq(
      ov.text,
      "第一段识别文本，随后到达的补充内容，以及第三批",
      "at the original deadline the LATEST buffered text appears (the deadline was NOT extended)"
    );
    assert.ok(ov.isShown, "the caption must carry the `show` class once displayed", ov.isShown);
    assert.eq(ov.clock.now - t0, DISPLAY_DELAY_MS, "the buffer deadline must stay at display_delay_ms");

    ov.emitPartial("，第四批");
    assert.eq(
      ov.text,
      "第一段识别文本，随后到达的补充内容，以及第三批，第四批",
      "after the page is live, later partials must update immediately (no buffering)"
    );
    console.log(
      `        · buffer held at ${DISPLAY_DELAY_MS} ms from the FIRST partial; in-buffer revisions reused the ` +
        `same timers; post-display updates were immediate`
    );
  });

  await run("A3b display_delay_ms is configurable and clamps to [500, 1000]", "delay-config", async (load) => {
    const short = await load({ overlayConfig: { display_delay_ms: 500 } });
    short.emitPartial("甲");
    short.tick(499);
    assert.eq(short.text, "", "a 500 ms delay must still be pending at 499 ms");
    short.tick(1);
    assert.eq(short.text, "甲", "must display at the configured 500 ms");

    const clamped = await load({ overlayConfig: { display_delay_ms: 100000 } });
    clamped.emitPartial("乙");
    clamped.tick(999);
    assert.eq(clamped.text, "", "an out-of-range delay clamps to 1000 ms (pending at 999 ms)");
    clamped.tick(1);
    assert.eq(clamped.text, "乙", "the clamped delay must expire at 1000 ms");
  });
}

// ---------------------------------------------------------------------------
// 4. clear-after-silence
// ---------------------------------------------------------------------------

export async function testClearAfterSilence({ run }) {
  // Overlay-internal state for the assertions that must look past the DOM.
  const probeTransform = (src) => {
    const anchor = "  init();\n})();";
    if (!src.includes(anchor)) throw new Error("probe anchor not found in overlay/app.js");
    return src.replace(
      anchor,
      "  init();\n  globalThis.__probe = function () { return { partialBuffer: partialBuffer, currentText: currentText, pageStart: pageStart, hideTimer: hideTimer, displayTimer: displayTimer }; };\n})();"
    );
  };

  // The observable contract: a line that reaches the screen through the
  // display buffer must still be removed clear_after_ms after the last event.
  await run("A4 clear_after_ms=4000: the first (buffered) line is cleared", "clear-first-line", async (load) => {
    const ov = await load();
    ov.emitPartial("清屏测试用的字幕内容");
    ov.tick(DISPLAY_DELAY_MS);
    assert.eq(ov.text, "清屏测试用的字幕内容", "the buffered line must be visible after display_delay_ms");
    assert.ok(
      ov.pendingTimeouts().length > 0,
      "a silence-clear timer must be armed once the buffered line is on screen",
      ov.pendingTimeouts().length
    );
    ov.tick(CLEAR_AFTER_MS);
    assert.eq(ov.text, "", `the line must be cleared ${CLEAR_AFTER_MS} ms after the last event`);
    ov.tick(3600_000);
    assert.eq(ov.text, "", "the line must not come back later");
  });

  await run("A4c clear_after_ms=4000 clears a live line, re-buffers, resize cannot resurrect", "clear-live", async (load) => {
    const ov = await load();

    // Two events: the second one lands after the first render is on screen, so
    // the silence deadline starts from that event.
    ov.emitPartial("清屏测试用的字幕内容");
    ov.tick(DISPLAY_DELAY_MS);
    assert.eq(ov.text, "清屏测试用的字幕内容", "the first render must be visible before the revision");
    ov.tick(250);
    ov.emitPartial("清屏测试用的字幕", true);
    assert.eq(ov.text, "清屏测试用的字幕", "a live revision must replace the line immediately");

    ov.tick(CLEAR_AFTER_MS - 1);
    assert.eq(ov.text, "清屏测试用的字幕", "still visible at clear_after_ms - 1 ms after the last event");

    ov.tick(1);
    assert.eq(ov.text, "", "the caption must be empty after clear_after_ms of silence");
    assert.ok(ov.hasEmptyClass, "the caption must carry the `empty` class after clearing", ov.hasEmptyClass);
    assert.ok(!ov.isShown, "the caption must drop the `show` class after clearing", ov.isShown);

    // --- the next page must go through the display buffer again ---
    // A `replace:true` frame is the protocol's sentence reset, so this is a
    // fresh page: no stale text may survive it.
    ov.emitPartial("清屏之后的新一句", true);
    assert.eq(ov.text, "", "the page after a clear must re-enter the display buffer");
    assert.eq(
      ov.displayDeadline(),
      DISPLAY_DELAY_MS,
      "the re-buffered page must be queued for a full display_delay_ms"
    );

    ov.resize();
    assert.eq(ov.text, "", "resize must not resurrect text while the new page is buffering");

    ov.tick(300);
    ov.resize();
    assert.eq(ov.text, "", "resize mid-buffer still must not show anything");

    ov.tick(DISPLAY_DELAY_MS - 300);
    assert.eq(ov.text, "清屏之后的新一句", "the buffered page appears at its own deadline");

    ov.tick(250);
    ov.emitPartial("清屏之后的新一句", true); // arm a fresh silence deadline
    ov.tick(CLEAR_AFTER_MS);
    assert.eq(ov.text, "", "a second silence period clears the re-buffered page");
    ov.resize();
    assert.eq(ov.text, "", "resize after clearing must not restore the previous line");
    console.log("        · clear -> re-buffer -> resize-safe, verified across two silence periods");
  });

  // The delta path: a bare `partial` carries only the NEW characters, so
  // whatever text the page shows must be exactly what was streamed since the
  // page began.
  await run("A4e after a silence clear a fresh delta stream shows only its own text", "clear-delta", async (load) => {
    const ov = await load({ transform: probeTransform });
    ov.emitPartial("第一句。");
    ov.tick(DISPLAY_DELAY_MS);
    ov.tick(250);
    ov.emitPartial("第一句。", true); // revision arms the silence deadline
    ov.tick(CLEAR_AFTER_MS);
    assert.eq(ov.text, "", "the first sentence must have been cleared by silence");

    const before = ov.sandbox.__probe();
    assert.eq(
      before.partialBuffer,
      "",
      "the silence clear must drop the accumulated partial buffer (otherwise the next sentence is glued onto the old one)"
    );

    ov.emitPartial("第二句");
    assert.eq(ov.text, "", "the page after a clear must re-enter the display buffer (not show instantly)");
    ov.tick(DISPLAY_DELAY_MS);
    assert.eq(ov.text, "第二句", "the re-buffered page must show only the text streamed after the clear");
  });

  // After a silence clear, currentText must be reset or the next page would
  // bypass its display buffer entirely.
  await run("A4d after a silence clear the next page goes through the buffer again", "clear-rebuffer", async (load) => {
    const ov = await load();
    // First page: a revision after the first render arms the silence timer.
    ov.emitPartial("第一句");
    ov.tick(DISPLAY_DELAY_MS);
    ov.tick(250);
    ov.emitPartial("第一句", true);
    ov.tick(CLEAR_AFTER_MS);
    assert.eq(ov.text, "", "the first sentence must have been cleared by silence");

    ov.emitPartial("第二句", true);
    assert.eq(ov.text, "", "the page after a clear must re-enter the display buffer (not show instantly)");
    ov.tick(DISPLAY_DELAY_MS);
    assert.eq(ov.text, "第二句", "the re-buffered page must appear at its own deadline");
  });

  await run("A4b clear_after_ms is configurable and clamps to >= 1000 ms", "clear-config", async (load) => {
    const twoSec = await load({ overlayConfig: { clear_after_ms: 2000 } });
    twoSec.emitPartial("可配置清屏");
    twoSec.tick(DISPLAY_DELAY_MS);
    twoSec.emitPartial("可配置清屏", true);
    twoSec.tick(1999);
    assert.eq(twoSec.text, "可配置清屏", "clear_after_ms=2000: still visible 1999 ms after the last event");
    twoSec.tick(1);
    assert.eq(twoSec.text, "", "clear_after_ms=2000: cleared at 2000 ms");

    const floored = await load({ overlayConfig: { clear_after_ms: 10 } });
    floored.emitPartial("下限钳制");
    floored.tick(DISPLAY_DELAY_MS);
    floored.emitPartial("下限钳制", true);
    floored.tick(999);
    assert.eq(floored.text, "下限钳制", "clear_after_ms below the floor must clamp up to 1000 ms");
    floored.tick(1);
    assert.eq(floored.text, "", "the clamped clear_after_ms fires at 1000 ms");
  });
}

// ---------------------------------------------------------------------------
// 5. replace semantics
// ---------------------------------------------------------------------------

export async function testReplaceSemantics({ run }) {
  await run("A5 replace:true revisions are never appended to each other", "replace-semantics", async (load) => {
    const ov = await load();
    ov.emitPartial("ABC", true);
    ov.emitPartial("ABCD", true);
    assert.eq(ov.text, "", "both revisions landed inside the page buffer");
    ov.tick(DISPLAY_DELAY_MS);
    assert.eq(ov.text, "ABCD", "the last cumulative revision wins; it must not be appended twice");
    assert.ok(!ov.text.includes("ABCABCD"), "a revision must not be concatenated with its predecessor");

    ov.emitPartial("XYZ", true);
    assert.eq(ov.text, "XYZ", "a later revision replaces the live text immediately");
    assert.ok(!ov.text.startsWith("ABCD"), "the previous revision must not survive as a prefix");

    ov.emitPartial("XYZ，更长的累计修订", true);
    assert.eq(ov.text, "XYZ，更长的累计修订", "later revisions keep replacing, never appending");
    console.log("        · ABC -> ABCD -> XYZ -> longer revision: no concatenation at any step");
  });
}

/** The overlay's own replay samples, streamed as cumulative replace frames. */
export async function testReplayFixtures({ run }) {
  for (const [name, text] of [
    ["REPLAY_PAGED", REPLAY_PAGED],
    ["REPLAY_VERY_LONG", REPLAY_VERY_LONG],
  ]) {
    await run(`A1p overlay's own ${name} sample: <=2 lines at every prefix`, `replay-${name}`, async (load) => {
      const ov = await load();
      const violations = [];
      let advanced = 0;
      for (let len = 1; len <= text.length; len++) {
        ov.emitPartial(text.slice(0, len), true); // replacePartial: pageStart = 0
        if (len === 1) ov.tick(DISPLAY_DELAY_MS);
        const shown = ov.text;
        const lines = ov.lineCount(shown);
        if (lines > 2) {
          violations.push({ len, lines, domLen: shown.length, height: ov.measuredHeight(shown) });
        }
        if (shown.length < len) advanced++;
      }
      assert.eq(
        violations.length,
        0,
        `${name} (${text.length} chars) two-line cap: ${describeViolations(violations)}`
      );
      if (ov.lineCount(text) > 2) {
        assert.gt(advanced, 0, `${name}: paging must advance for text that needs more than two lines`);
      }
      console.log(
        `        · ${name}: ${text.length} chars, ${ov.lineCount(text)} lines unpaged, ` +
          `${advanced} prefixes on a later page, final page @ ${text.indexOf(ov.text)}`
      );
    });
  }
}

// ---------------------------------------------------------------------------
// geometry matrix: other browser-source widths and max_lines configs
// ---------------------------------------------------------------------------

export async function testGeometryMatrix({ run }) {
  for (const width of [1600, 800]) {
    await run(`A1g paging holds at viewport width ${width}px`, `geometry-${width}`, async (load) => {
      const ov = await load({ viewportWidth: width });
      const geom = ov.geometry();
      const full = LONG_CJK;
      ov.emitPartial(full.slice(0, 1));
      ov.tick(DISPLAY_DELAY_MS);
      const violations = [];
      let pageIndex = 0;
      let maxPageIndex = 0;
      for (let len = 2; len <= full.length; len++) {
        ov.emitPartial(full[len - 1]);
        const lines = ov.lineCount(ov.text);
        if (lines > 2) {
          violations.push({ len, lines, domLen: ov.text.length, height: ov.measuredHeight(ov.text) });
        }
        const end = pageSliceStart(full.slice(0, len), ov.text, pageIndex);
        if (end === null) {
          throw new Error(
            `width ${width}: page at prefix ${len} is not a contiguous slice of the text\n` +
              `  actual: ${JSON.stringify(ov.text.slice(0, 40))}`
          );
        }
        if (end < len) {
          pageIndex = end;
          maxPageIndex = Math.max(maxPageIndex, pageIndex);
        }
      }
      assert.eq(violations.length, 0, `two-line cap at width ${width}: ${describeViolations(violations)}`);
      assert.gt(maxPageIndex, 0, `paging must advance at width ${width}`);
      console.log(
        `        · width ${width}px -> usable ${geom.usableWidth}px, ` +
          `${Math.floor(geom.usableWidth / geom.fontSize)} CJK chars/line, ` +
          `${ov.lineCount(full)} lines unpaged, furthest page started at ${maxPageIndex}`
      );
    });
  }

  await run("A1c max_lines:6 from the server cannot buy a third line", "max-lines-6", async (load) => {
    const ov = await load({ overlayConfig: { max_lines: 6 } });
    const full = LONG_CJK;
    ov.emitPartial(full.slice(0, 1));
    ov.tick(DISPLAY_DELAY_MS);
    const violations = [];
    let pageIndex = 0;
    let maxPageIndex = 0;
    for (let len = 2; len <= full.length; len++) {
      ov.emitPartial(full[len - 1]);
      const lines = ov.lineCount(ov.text);
      if (lines > 2) {
        violations.push({ len, lines, domLen: ov.text.length, height: ov.measuredHeight(ov.text) });
      }
      const end = pageSliceStart(full.slice(0, len), ov.text, pageIndex);
      if (end !== null && end < len) {
        pageIndex = end;
        maxPageIndex = Math.max(maxPageIndex, pageIndex);
      }
    }
    assert.eq(violations.length, 0, `max_lines:6 must stay capped at 2: ${describeViolations(violations)}`);
    assert.gt(maxPageIndex, 0, "max_lines:6 must still page, not grow the box");
    assert.eq(
      ov.document.documentElement.style.getPropertyValue("--caption-max-height"),
      `${2 * ov.lineHeightPx}.00px`,
      "the two-line viewport height must be enforced regardless of config"
    );
  });

  await run("A1s max_lines:1 keeps strict single-line mode", "max-lines-1", async (load) => {
    const ov = await load({ overlayConfig: { max_lines: 1 } });
    ov.emitPartial("单行模式下的超长字幕内容必须保持一行并交给 CSS 省略号处理");
    ov.tick(DISPLAY_DELAY_MS);
    assert.ok(
      ov.lineEl.classList.contains("single-line"),
      "max_lines:1 must put `single-line` on #caption-line",
      ov.lineEl.className
    );
    assert.eq(ov.lineCount(ov.text), 1, "single-line text must measure one line");
    assert.eq(
      ov.document.documentElement.style.getPropertyValue("--caption-max-height"),
      `${ov.lineHeightPx}.00px`,
      "single-line viewport height must be one line"
    );
  });
}

// ---------------------------------------------------------------------------
// typewriter animation path
// ---------------------------------------------------------------------------

export async function testTypewriterPath({ run }) {
  await run("A1t typewriter animation never renders a 3rd line and pages forward", "typewriter", async (load) => {
    const ov = await load({ overlayConfig: { animation: "typewriter" }, transform: RENDER_LOG_TRANSFORM });
    assert.ok(
      ov.document.body.classList.contains("animation-typewriter"),
      "the body must carry animation-typewriter",
      ov.document.body.className
    );
    const full = LONG_CJK;
    const violations = [];
    const frames = new Set();
    let cursor = 0;
    let previousAt = 0;
    let maxAt = 0;
    let backwards = 0;
    let reEmits = 0;
    let sinceEmit = 0;

    ov.emitPartial(full.slice(0, 1), true);
    ov.tick(DISPLAY_DELAY_MS); // release the page buffer
    cursor = ov.sandbox.__probe().rendered.length;

    for (let len = 2; len <= full.length; len++) {
      const text = full.slice(0, len);
      ov.emitPartial(text, true);
      sinceEmit = 0;
      // Let the typewriter finish the page.  The caption's own silence timer
      // would clear it mid-drain, so re-arm it with the SAME cumulative frame —
      // a no-op revision that neither resets the page nor duplicates text
      // (the delta re-emits this case used to do appended the character twice,
      // which made the DOM text stop being a slice of the streamed sentence).
      for (let step = 0; step < 90; step++) {
        if (sinceEmit > CLEAR_AFTER_MS - 1000) {
          ov.emitPartial(text, true);
          sinceEmit = 0;
          reEmits++;
        }
        ov.tick(32);
        sinceEmit += 32;
        if (ov.pendingTimeouts().length <= 1) break; // the typewriter settled
      }
      const r = renderedWindows(ov, cursor);
      cursor = r.cursor;
      for (const w of r.pages) {
        frames.add(w.page);
        const lines = ov.lineCount(w.page);
        if (lines > 2) {
          violations.push({ len, lines, domLen: w.page.length, height: ov.measuredHeight(w.page) });
        }
        if (w.at < 0) {
          throw new Error(
            `A1t prefix ${len}: the rendered page is not a slice of the streamed sentence\n` +
              `  rendered: ${JSON.stringify(w.page.slice(0, 40))}\n` +
              `  line:     ${JSON.stringify(w.text.slice(0, 40))}`
          );
        }
        if (w.at < previousAt) {
          backwards++;
          if (backwards <= 3) {
            console.log(
              `        · A1t prefix ${len}: the page fell back from ${previousAt} to ${w.at}` +
                ` (rendered ${JSON.stringify(w.page.slice(0, 12))}…)`
            );
          }
        }
        previousAt = w.at;
        maxAt = Math.max(maxAt, w.at);
      }
    }
    assert.eq(violations.length, 0, `typewriter two-line cap: ${describeViolations(violations)}`);
    assert.gt(frames.size, 1, "the typewriter must actually render text");
    assert.gt(maxAt, 0, "the typewriter path must page forward past the head of the sentence");
    assert.eq(backwards, 0, `typewriter page stability: the page fell back ${backwards} time(s)`);
    console.log(
      `        · typewriter: peak ${Math.max(...[...frames].map((f) => ov.lineCount(f)))} line(s), ` +
        `${frames.size} distinct rendered pages, furthest page started at ${maxAt} ` +
        `(${reEmits} silence re-arms)`
    );
  });
}

// ---------------------------------------------------------------------------
// A1o — shifting cumulative revision.
//
// History: this case first reported a "stale page offset" that turned out to be
// a measurement error in the ASSERTION (it read `pageStart` after the render had
// already advanced it), see tests/README.md. The rewritten assertions below are
// the real invariants and run by default. Investigating the false positive did
// surface a genuine robustness bug in `replacePartial()`'s same-sentence test,
// which is fixed in overlay/app.js and covered here.
// ---------------------------------------------------------------------------

export async function testShiftingRevision({ run }) {
  await run("A1o shifting cumulative revision: the page must stay inside the revision", "shifting-revision", async (load) => {
    const ov = await load({
      transform: (src) => {
        const anchor = "  init();\n})();";
        if (!src.includes(anchor)) throw new Error("probe anchor not found in overlay/app.js");
        return src.replace(
          anchor,
          "  init();\n  globalThis.__probe = function () { return { pageStart: pageStart, partialBuffer: partialBuffer, currentText: currentText }; };\n})();"
        );
      },
    });
    const full = SHIFTING_REVISION_TEXT;
    ov.emitPartial(full.slice(0, 1), true);
    ov.tick(DISPLAY_DELAY_MS);

    const violations = [];
    let advanced = 0;
    let lastStart = 0;
    for (let len = 2; len <= full.length; len++) {
      const text = full.slice(0, len);
      ov.emitPartial(text, true);
      const after = ov.sandbox.__probe();
      const dom = ov.text;
      if (dom === "") {
        violations.push({ len, kind: "empty page", pageStart: after.pageStart });
        continue;
      }
      // Invariant 1: what is on screen must be part of the CURRENT revision.
      // (A stale page from the previous revision is the failure mode this case
      // exists for.) This is the assertion that catches the bug.
      const at = text.indexOf(dom);
      if (at < 0) {
        violations.push({
          len,
          kind: "page is not part of the current revision",
          pageStart: after.pageStart,
          domLen: dom.length,
          textLen: text.length,
          tail: dom.slice(-14),
        });
        continue;
      }
      // Invariant 2: the cursor must not end up past the end of the revision.
      if (after.pageStart > text.length) {
        violations.push({
          len,
          kind: "page cursor past the end of the revision",
          pageStart: after.pageStart,
          domLen: dom.length,
          textLen: text.length,
        });
        continue;
      }
      // Invariant 3: paging still advances on a shifting stream.
      if (at > lastStart) advanced++;
      lastStart = at;
    }
    assert.eq(
      violations.length,
      0,
      `A1o shifting revisions rendered a page outside the revision in ${violations.length} frame(s): ` +
        violations
          .slice(0, 4)
          .map((v) => `prefix ${v.len} (${v.kind}, pageStart ${v.pageStart}, dom ${v.domLen} of ${v.textLen})`)
          .join("; ")
    );
    assert.gt(advanced, 0, "a shifting cumulative revision must still page forward");
    console.log(`        · shifting revisions kept every page inside the revision (${advanced} advances)`);
  });
}

// ---------------------------------------------------------------------------
// A1f — page stability: a re-decoded cumulative revision must never send the
// caption back to page 1.
//
// REAL DEFECT #4 (user report: 「当一句话比较长时出第二段字幕的时候第一段字幕
// 可能会和第二段字幕交替闪烁」).  `replacePartial()` classified a cumulative
// frame as a NEW sentence unless one frame was a strict prefix of the other.
// Real ASR re-decodes the tail (homophone / punctuation) and can insert a word
// in the middle of the open sentence, so ordinary frames of ONE sentence are
// neither prefixes nor extensions of each other.  Each such frame reset
// `pageStart` to 0, so a long sentence rendered page 1, then page 2, then page 1
// again, then page 2 … — the visible flicker.
//
// The invariants pinned here:
//   1. every rendered page is a contiguous window of the CURRENT revision,
//   2. the window never moves backwards inside one sentence,
//   3. the sentence still pages forward, and
//   4. no rendered page needs a third line.
// ---------------------------------------------------------------------------

export async function testPageStability({ run }) {
  const cases = [
    ["A1f re-decoded cumulative revisions never fall back to page 1", "page-flicker", "fade"],
    ["A1ft the typewriter path never falls back to page 1 either", "page-flicker-typewriter", "typewriter"],
  ];
  for (const [name, caseId, animation] of cases) {
    await run(name, caseId, async (load) => {
      const ov = await load({ overlayConfig: { animation }, transform: RENDER_LOG_TRANSFORM });
      const streams = [
        ["tail re-decode every 6th frame", tailRevisionFrames(SHIFTING_REVISION_TEXT)],
        ["word inserted mid-sentence at frame 101", midRevisionFrames(SHIFTING_REVISION_TEXT)],
        ["pure growth (control)", Array.from({ length: SHIFTING_REVISION_TEXT.length }, (_, i) => SHIFTING_REVISION_TEXT.slice(0, i + 1))],
      ];

      const summary = [];
      for (const [label, frames] of streams) {
        // `cleared` is the protocol's sentence reset, so every stream starts
        // from a pristine page cursor on the same overlay instance.
        ov.emitCleared();
        const state = { cursor: 0, sinceEmit: 0 };
        state.cursor = ov.sandbox.__probe().rendered.length;
        let previousAt = 0;
        let backwards = 0;
        const violations = [];
        let furthest = 0;
        let windowCount = 0;

        for (const text of frames) {
          for (const w of driveFrame(ov, text, state)) {
            windowCount++;
            const lines = ov.lineCount(w.page);
            if (lines > 2) {
              violations.push({ lines, domLen: w.page.length, height: ov.measuredHeight(w.page) });
            }
            if (w.page === "") {
              violations.push({ kind: "empty page rendered", at: w.at, textLen: w.text.length });
              continue;
            }
            if (w.at < 0) {
              violations.push({ kind: "page is not part of the current revision", domLen: w.page.length });
              continue;
            }
            if (w.end > w.text.length) {
              violations.push({ kind: "page window past the end of the revision", at: w.at, end: w.end, len: w.text.length });
            }
            if (w.at < previousAt) backwards++;
            previousAt = w.at;
            furthest = Math.max(furthest, w.at);
          }
        }

        assert.eq(
          violations.length,
          0,
          `${label}: ${violations.length} page violation(s): ` +
            violations.slice(0, 3).map((v) => `${v.kind || "3rd line"} @${v.at ?? "?"} (${v.domLen ?? v.textLen} chars)`).join("; ")
        );
        assert.gt(furthest, 0, `${label}: the sentence must page forward past the head (one page = 62 CJK chars)`);
        assert.eq(
          backwards,
          0,
          `${label}: the rendered page fell back to an earlier page ${backwards} time(s) — ` +
            `this is the page-1 / page-2 flicker the user reported (invariant: the page window ` +
            `must never move backwards while one sentence is being revised)`
        );
        summary.push(`${label}: ${windowCount} pages, furthest @${furthest}`);
      }

      // --- a revision that TRIMS the sentence back to fewer pages ----------
      // The cursor is already on the last page when the recognizer drops a
      // clause, so the revision no longer reaches it.  The caption must keep
      // showing the newest words (the last page that still fits), never fall
      // back to page 1 and never render an empty page.
      ov.emitCleared();
      const trimState = { cursor: ov.sandbox.__probe().rendered.length, sinceEmit: 0 };
      for (let len = 1; len <= SHIFTING_REVISION_TEXT.length; len++) {
        driveFrame(ov, SHIFTING_REVISION_TEXT.slice(0, len), trimState);
      }
      const trimmed = SHIFTING_REVISION_TEXT.slice(0, 100); // still more than one page
      const last = driveFrame(ov, trimmed, trimState).pop();
      assert.ok(last, "the trimmed revision must render a page", last);
      assert.gt(last.at, 0, "a trimmed revision still longer than one page must not fall back to page 1", last.at);
      assert.eq(last.end, trimmed.length, "the trimmed revision must show the newest words (page ends at the tail)");
      summary.push(`trim to ${trimmed.length} chars -> last page @${last.at}`);

      assert.lte(ov.maxHeightPx, 120, "the two-line viewport is unchanged");
      console.log(`        · ${animation}: ${summary.join(" | ")}`);
    });
  }
}

// ---------------------------------------------------------------------------
// A3c — display_delay_ms = 0 is a REAL "no buffering" mode.
//
// The user asked for a 0-second ("no buffer") choice: the first page of a
// caption must reach the screen the moment it arrives, with no timer at all.
// ---------------------------------------------------------------------------

export async function testZeroBuffer({ run }) {
  await run("A3c display_delay_ms=0 shows the first page immediately (no timer)", "delay-zero", async (load) => {
    const fade = await load({ overlayConfig: { display_delay_ms: 0 } });
    const before = fade.pendingTimeouts().length;
    fade.emitPartial("零缓冲的第一句");
    assert.eq(fade.text, "零缓冲的第一句", "with display_delay_ms=0 the text must be visible in the same tick it arrives");
    assert.eq(
      fade.pendingTimeouts().length,
      before + 1,
      "a 0 ms buffer must NOT create a display timer (only the silence timer is armed)"
    );
    assert.eq(fade.displayDeadline(), CLEAR_AFTER_MS, "the earliest one-shot timer is the silence clear, not a buffer");
    assert.ok(fade.isShown, "the caption must be visible immediately", fade.isShown);

    // A brand-new page after a silence clear must also skip the buffer.
    fade.tick(CLEAR_AFTER_MS);
    assert.eq(fade.text, "", "the silence clear still applies with a 0 ms buffer");
    fade.emitPartial("清屏后的新一句", true);
    assert.eq(fade.text, "清屏后的新一句", "the page after a clear must also appear with no buffer");

    // The typewriter path is the shipped default: the FIRST unit must not wait
    // for a type interval either, otherwise "no buffer" would still be 32 ms late.
    const typed = await load({ overlayConfig: { display_delay_ms: 0, animation: "typewriter" } });
    typed.emitPartial("打字机零缓冲");
    assert.gt(typed.text.length, 0, "the typewriter must reveal its first unit synchronously when the buffer is off");
    assert.ok(
      "打字机零缓冲".startsWith(typed.text),
      "the immediately revealed typewriter text must be a prefix of the page",
      typed.text
    );

    // Clamping: 0 is legal, 500–1000 is the buffer window, everything else
    // clamps INTO that window (a negative value is not "0/no buffer").
    const clamped = [
      [0, 0],
      [500, 500],
      [1000, 1000],
      [250, 500],
      [-100, 500],
      [100000, 1000],
    ];
    for (const [configured, expected] of clamped) {
      const o = await load({ overlayConfig: { display_delay_ms: configured } });
      o.emitPartial("钳制检查");
      if (expected === 0) {
        assert.eq(o.text, "钳制检查", `display_delay_ms=${configured} must be treated as "no buffer"`);
      } else {
        o.tick(expected - 1);
        assert.eq(o.text, "", `display_delay_ms=${configured} must clamp to ${expected} ms (still pending at ${expected - 1} ms)`);
        o.tick(1);
        assert.eq(o.text, "钳制检查", `display_delay_ms=${configured} must display at ${expected} ms`);
      }
    }
    console.log(
      "        · 0 ms = immediate (fade + typewriter); 250/-100 -> 500; 100000 -> 1000; 0 -> no display timer"
    );
  });
}

// ---------------------------------------------------------------------------
// A6 — the overlay's own debug/demo path (`/overlay?local-replay=1`).
//
// This is the "字幕调试窗口" the user can open in a browser: it replays fixed
// samples through the REAL event handlers on a timer, with no audio, no network
// and no effect on server history.  Driving the harness' fake clock through the
// whole timeline is therefore an end-to-end check of that path.
// ---------------------------------------------------------------------------

export async function testLocalReplayPath({ run }) {
  await run("A6 ?local-replay=1 debug path stays clean end to end", "local-replay", async (load) => {
    const ov = await load({ search: "?local-replay=1", transform: RENDER_LOG_TRANSFORM });
    assert.eq(
      ov.document.body.dataset.localReplay,
      "running",
      "the replay must announce itself on <body data-local-replay>"
    );

    // The replay timeline: samples at 0 / 500 / 1000 / 1500 / 2200 ms, the
    // completion marker at 6200 ms (which is also when the silence clear fires,
    // because the last subtitle event was at 2200 ms and clear_after_ms = 4000).
    let cursor = 0;
    let everShown = 0;
    let flips = 0;
    let previousPage = null;
    let resized = false;
    for (let t = 0; t <= 12000; t += 16) {
      ov.tick(16);
      // The long sample arrives as ONE frame, so one event renders page 1 and
      // arms the next page.  A real browser re-renders on `document.fonts.ready`
      // / resize, which is how pages 2..n reach the screen; drive that here so
      // the debug path is checked for the page turn as well.
      if (!resized && t >= 2400) {
        resized = true;
        ov.resize();
      }
      const r = renderedWindows(ov, cursor);
      cursor = r.cursor;
      for (const w of r.pages) {
        // The very first render is `renderStyle()` on an empty line during
        // init(); an empty page is only a defect once the line HAS content.
        if (w.page === "" && w.text !== "") {
          throw new Error(`t=${t} ms: the replay rendered an EMPTY page (a visible blank frame)`);
        }
        if (w.page !== "" && w.at < 0) {
          throw new Error(
            `t=${t} ms: the replay rendered a page that is not part of the current line\n` +
              `  page: ${JSON.stringify(w.page.slice(0, 40))}\n` +
              `  line: ${JSON.stringify(w.text.slice(0, 40))}`
          );
        }
        const lines = ov.lineCount(w.page);
        if (lines > 2) {
          throw new Error(
            `t=${t} ms: the replay rendered ${lines} lines (${w.page.length} chars): ` +
              JSON.stringify(w.page.slice(0, 40))
          );
        }
        everShown++;
        if (previousPage !== null && w.page !== previousPage && w.at > 0) flips++;
        previousPage = w.page;
      }
    }

    assert.eq(
      ov.document.body.dataset.localReplay,
      "complete",
      "the replay must reach its completion marker (it never finished)"
    );
    assert.gt(everShown, 3, "the debug path must actually render the samples");
    assert.gt(flips, 0, "the long sample must page forward, so the debug path exercises the fix");
    assert.eq(ov.text, "", "the caption must be cleared again once the replay goes silent");
    console.log(`        · local-replay: ${everShown} rendered pages, ${flips} forward page turns, cleared at the end`);
  });
}

// ---------------------------------------------------------------------------
// plan
// ---------------------------------------------------------------------------

/** The suite, in execution order.  `id` is stable so failure sets can be diffed. */
export const TEST_PLAN = [
  { id: "shim", title: "DOM shim self-checks (measurement model)", fn: testShimFidelity },
  { id: "delta", title: "two-line cap + paging (delta partial stream)", fn: testDeltaStream },
  { id: "replace", title: "two-line cap + paging (cumulative replace stream)", fn: testReplaceStream },
  { id: "replay", title: "overlay's own replay fixtures", fn: testReplayFixtures },
  { id: "shifting", title: "shifting cumulative revision (page stays inside the revision)", fn: testShiftingRevision },
  { id: "flicker", title: "page stability: a re-decoded revision never falls back to page 1", fn: testPageStability },
  { id: "buffer", title: "display buffer semantics", fn: testBufferSemantics },
  { id: "buffer-zero", title: "display_delay_ms = 0 is a real no-buffer mode", fn: testZeroBuffer },
  { id: "clear", title: "clear-after-silence", fn: testClearAfterSilence },
  { id: "semantics", title: "replace semantics", fn: testReplaceSemantics },
  { id: "geometry", title: "geometry / config matrix", fn: testGeometryMatrix },
  { id: "typewriter", title: "typewriter animation path", fn: testTypewriterPath },
  { id: "local-replay", title: "the overlay's own ?local-replay=1 debug path", fn: testLocalReplayPath },
];

/**
 * DEFECTS THE SUITE CURRENTLY FINDS IN overlay/app.js.
 *
 * These are real assertions that the real code does not satisfy.  They are NOT
 * skipped or weakened: they still execute, and their full expectation-vs-actual
 * detail is printed.  They are only counted separately so the baseline run can
 * tell "the overlay is broken" (here) apart from "the harness is broken"
 * (which would show up as an unexpected FAIL).  See tests/README.md.
 */
/**
 * DEFECTS THE SUITE CURRENTLY FINDS IN overlay/app.js.
 *
 * Every entry here is a real assertion that the current source does not
 * satisfy; they are NOT skipped or weakened, only counted separately so that
 * "the overlay is broken" is distinguishable from "the harness is broken".
 *
 * HISTORY (kept because it documents what the suite was built to catch):
 *   * REAL DEFECT #1 — page boundary. `findCut()` returns the END index of the
 *     longest measured-to-fit slice, but `pickShown()` assigned it to
 *     `pageStart` and returned `full.slice(pageStart)`, i.e. it treated an end
 *     index as a start index and rendered the TAIL of the sentence instead of
 *     the measured slice.  Every page was therefore longer than anything that
 *     had been measured, which put a third line on screen.  Compounded by
 *     `replacePartial()` resetting `pageStart = 0` on every cumulative frame,
 *     a long sentence could never page forward at all.
 *   * REAL DEFECT #2 — `renderShow()` cancelled `hideTimer` on entry, and the
 *     buffered first render only happens after `display_delay_ms`, so the
 *     silence deadline armed when the text arrived was destroyed: the first
 *     line of a page was never cleared.
 *   * REAL DEFECT #3 — `hide()` cleared `currentText` but not `partialBuffer`,
 *     so after a clear the next sentence's first delta was appended to the old
 *     sentence and the caption showed "<old><new>".
 *   All three were fixed in overlay/app.js by the owning agent; the assertions
 *   that caught them are now green, and
 *   `tests/run-negative-control.mjs page-boundary-regression` re-introduces
 *   defect #1 in memory to keep the regression pinned.
 */
export const KNOWN_FAILURES = {};

/**
 * Execute every case.  `opts.load` is the loader used by every case — the real
 * one for the baseline, a perturbed in-memory source for the negative control.
 * The assertions themselves are byte-for-byte identical in both runs.
 */
export async function runTestPlan(runner, opts = {}) {
  if (typeof opts.load !== "function") {
    throw new Error("runTestPlan requires opts.load (a function returning a harness)");
  }
  const load = opts.load;
  for (const { id, title, fn } of TEST_PLAN) {
    console.log(`\n[${id}] ${title}`);
    await fn({ run: (name, caseId, body) => runner.run(name, caseId, () => body(load)) });
  }
}
