#!/usr/bin/env node
// Page-boundary evidence script (REAL DEFECT #1, now fixed in overlay/app.js).
//
//   node tests/repro-page-boundary.mjs
//
// It uses only the harness geometry helpers plus one hard-coded string, and
// prints, for each prefix length of that string:
//
//   1. what the HISTORICAL pickShown() produced
//        findCut() returns the END index of the longest slice that fits, but the
//        old code assigned it to pageStart and returned full.slice(pageStart),
//        i.e. it used an end index as a start index and rendered the tail of the
//        sentence instead of the measured slice — every page was longer than
//        anything that had been measured, which put a third line on screen.
//        (Compounded by replacePartial() resetting pageStart = 0 on every
//        cumulative frame, so a long sentence could not page forward at all.)
//   2. what the CURRENT pickShown() produces
//        full.slice(pageStart, cut), advancing pageStart only when the tail does
//        not fit — the measured slice, nothing more.
//   3. the LIVE overlay's real DOM for the same prefixes, as the end-to-end
//      cross-check (the DOM must equal what pickShown() returned, and must fit
//      two lines).
//
// The two competing width models are printed side by side so the geometry
// question can be settled from overlay/style.css rather than from an opinion:
//
//   A) usableWidth = min(96% x 1600, 1600) - 48px padding = 1488px  <-- real CSS
//      (.caption { box-sizing: border-box; max-width: min(96%,1600px);
//                 padding: 10px 24px } and .caption-line { max-width: 100% })
//   B) usableWidth = min(96% x 1600, 1600)              = 1552px  <-- padding
//      forgotten, which is what a model that skips the padding measures.

import { loadOverlay } from "./overlay-harness.mjs";
import { lineModel } from "./dom-shim.mjs";

// Same shape as the overlay's own replay sample (REPLAY_VERY_LONG, 146 chars).
const TEXT =
  "直播字幕遇到特别长的句子时必须持续换页而不能依赖滚动，所以这一段刻意写得比两行长得多：" +
  "它要确认翻过去的内容不会在下一帧整段重新出现，也要确认全程没有任何一帧同时显示三行，" +
  "并且换页之后仍能一直读到句尾，如果句子还能更长分页逻辑也应当继续推进到最后一页。";

const WIDTH_SOURCE = {
  label: "A) .caption max-width - 2 x 24px padding  (what the overlay really measures)",
  usableWidth: 1488,
};
const PARENT_MODEL = {
  label: "B) .caption max-width only, padding ignored (the 1552px variant)",
  usableWidth: 1552,
};

function analyse(model) {
  const font = 48;
  const maxH = 2 * font * 1.25; // 120px
  const geom = { fontSize: font, lineHeight: 1.25, metrics: { horizontalPadding: 0, maxCaptionWidth: model.usableWidth } };
  const measure = (s) => lineModel(s, geom).height;
  const linesOf = (s) => lineModel(s, geom).lines;

  // Verbatim transcription of overlay/app.js findCut(): returns the END index
  // of the longest slice that fits inside maxH.
  function findCut(text, start, maxH_) {
    let lo = start + 1;
    let hi = text.length;
    let best = start + 1;
    while (lo <= hi) {
      const mid = (lo + hi) >> 1;
      if (measure(text.slice(start, mid)) <= maxH_) {
        best = mid;
        lo = mid + 1;
      } else {
        hi = mid - 1;
      }
    }
    return best;
  }

  const sweep = (renderPage) => {
    let violations = 0;
    let firstBad = null;
    let worst = { lines: 0 };
    const samples = [];
    for (let len = 1; len <= TEXT.length; len++) {
      const full = TEXT.slice(0, len);
      const { shown, pageStart } = renderPage(full);
      const shownLines = linesOf(shown);
      if (shownLines > 2) {
        violations++;
        if (firstBad === null) firstBad = len;
      }
      if (shownLines > worst.lines) worst = { lines: shownLines, len, domLen: shown.length, pageStart };
      if (len >= 120 && len <= 127) samples.push({ len, pageStart, shownLen: shown.length, shownLines });
    }
    return { violations, firstBad, worst, samples };
  };

  // HISTORICAL code path.
  const historical = sweep((full) => {
    let pageStart = 0;
    if (measure(full.slice(pageStart)) > maxH) pageStart = findCut(full, pageStart, maxH);
    return { shown: full.slice(pageStart), pageStart };
  });

  // CURRENT code path (overlay/app.js as of this writing).
  const current = sweep((full) => {
    let pageStart = 0;
    if (measure(full.slice(pageStart)) > maxH) {
      const cut = findCut(full, pageStart, maxH);
      if (cut < full.length) {
        const shown = full.slice(pageStart, cut);
        pageStart = cut;
        return { shown: shown || full.slice(0, cut) || full.slice(0, 1), pageStart };
      }
    }
    return { shown: full.slice(pageStart), pageStart };
  });

  return { model, maxH, historical, current };
}

console.log("=".repeat(78));
console.log("page-boundary evidence — overlay/app.js pickShown()/findCut()");
console.log("=".repeat(78));
console.log(`hard-coded sample: ${TEXT.length} chars`);
console.log(`fonts: font-size 48px, line-height 1.25 -> ${48 * 1.25}px/line, 2-line viewport 120px\n`);

for (const r of [analyse(WIDTH_SOURCE), analyse(PARENT_MODEL)]) {
  const perLine = Math.floor(r.model.usableWidth / 48);
  console.log(`${r.model.label}`);
  console.log(`   usableWidth=${r.model.usableWidth}  chars/line=${perLine}  two-line page=${perLine * 2} chars`);
  console.log("   HISTORICAL pickShown()  ->  return full.slice(pageStart)");
  console.log(
    `      prefixes needing a 3rd line : ${r.historical.violations} (first at prefix ${r.historical.firstBad})`
  );
  console.log(
    `      worst case                  : ${r.historical.worst.lines} lines @ prefix ${r.historical.worst.len} ` +
      `(dom ${r.historical.worst.domLen} chars, pageStart ${r.historical.worst.pageStart})`
  );
  console.log("   CURRENT pickShown()     ->  return full.slice(pageStart, cut)");
  console.log(
    `      prefixes needing a 3rd line : ${r.current.violations} (first at prefix ${r.current.firstBad})`
  );
  console.log(
    `      worst case                  : ${r.current.worst.lines} lines @ prefix ${r.current.worst.len} ` +
      `(dom ${r.current.worst.domLen} chars, pageStart ${r.current.worst.pageStart})`
  );
  console.log("   around the first cut (current):");
  for (const s of r.current.samples) {
    console.log(
      `      prefix ${String(s.len).padStart(3)}  pageStart=${String(s.pageStart).padStart(3)}  ` +
        `shown=${String(s.shownLen).padStart(3)} chars  -> ${s.shownLines} line(s)`
    );
  }
  console.log("");
}

// ---------------------------------------------------------------------------
// End-to-end cross-check against the LIVE overlay: same prefixes, real DOM.
// ---------------------------------------------------------------------------
const ov = await loadOverlay({
  transform: (src) => {
    const anchor = "  init();\n})();";
    if (!src.includes(anchor)) throw new Error("probe anchor not found in overlay/app.js");
    return src.replace(
      anchor,
      "  init();\n  globalThis.__probe = function () { return { pageStart: pageStart, maxLines: maxLines, currentText: currentText }; };\n})();"
    );
  },
});

console.log("-".repeat(78));
console.log("live overlay cross-check (cumulative `replace:true` frames, real DOM)");
console.log("-".repeat(78));
const geom = ov.geometry();
console.log(
  `harness geometry: usableWidth=${geom.usableWidth} (padding ${geom.horizontalPadding}px subtracted), ` +
    `lineHeight=${ov.lineHeightPx}px, --caption-max-height=${ov.document.documentElement.style.getPropertyValue("--caption-max-height")}, ` +
    `getComputedStyle(#caption).lineHeight=${ov.sandbox.getComputedStyle(ov.captionEl).lineHeight}`
);

ov.emitPartial(TEXT.slice(0, 1), true);
ov.tick(750);
let violations = 0;
let mismatches = 0;
let truncated = 0;
const starts = [];
for (let len = 2; len <= TEXT.length; len++) {
  ov.emitPartial(TEXT.slice(0, len), true);
  const dom = ov.text;
  const st = ov.sandbox.__probe();
  const report = ov.overflowReport(dom);
  if (report.lines > 2) violations++;
  // renderShow() hands renderCaption at most 1000 chars ("病态输入兜底"), so
  // the page window is compared against that same clamp.
  const line = TEXT.slice(0, len);
  const capped = line.length > 1000 ? line.slice(0, 1000) + "…" : line;
  const expected = capped.slice(st.pageStart, st.pageStart + dom.length);
  if (expected !== dom) {
    mismatches++;
    if (mismatches <= 3) {
      console.log(
        `   mismatch @ prefix ${len}: pageStart=${st.pageStart} cappedLen=${capped.length} domLen=${dom.length}`
      );
    }
  }
  if (capped !== line) truncated++;
  starts.push(TEXT.indexOf(dom));
}
console.log(`live overlay: ${violations} prefix(es) rendered a 3rd line`);
console.log(`live overlay: DOM text differed from the page window in ${mismatches} prefix(es)`);
console.log(`live overlay: ${truncated} prefix(es) were subject to the 1000-char guard`);
console.log(`live overlay: page start advanced from ${Math.min(...starts)} to ${Math.max(...starts)}`);

if (violations > 0) {
  console.log(
    `\nRESULT: FAIL — the two-line cap is broken again (${violations} overflow(s)). ` +
      "This is REAL DEFECT #1; see the HISTORICAL model above for the mechanism."
  );
  process.exit(1);
}
if (mismatches > 0) {
  console.log(
    `\nRESULT: page boundary FIXED — 0 overflows in the live overlay (the historical model still ` +
      `shows ${analyse(WIDTH_SOURCE).historical.violations} overflow(s) at the same prefixes).\n` +
      `        ${mismatches} frame(s) still render outside the current revision: that is OPEN ISSUE #4 ` +
      `(stale pageStart on a shifting cumulative revision), reproduced separately by\n` +
      `        node tests/repro-stale-page-offset.mjs`
  );
  process.exit(0);
}
console.log("\nRESULT: OK — the overlay renders the measured slice, never a third line, always in range.");
process.exit(0);
