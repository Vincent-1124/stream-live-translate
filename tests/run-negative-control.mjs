#!/usr/bin/env node
// NEGATIVE CONTROL: `node tests/run-negative-control.mjs [perturbation]`
//
// Proves the suite can actually fail.  It reads overlay/app.js, perturbs ONE
// invariant **in memory only** (the file on disk is never touched, and no file
// is written), and runs the identical assertions from overlay-cases.mjs.
//
// If the suite still passed with a broken invariant, the suite would be
// worthless; this script exits non-zero unless every perturbation makes at
// least one previously-passing assertion fail.
//
// Perturbations:
//   two-line-cap             if (lines > 2) lines = 2;   ->   lines = 4;
//   buffer-semantics         show(): always re-arm the display timer (deadline extends)
//   replace-semantics        replacePartial(): append instead of replacing
//   page-boundary-regression pickShown(): return the tail from pageStart (historical off-by-one)
//   page-cursor-reset        sameOpenSentence(): strict prefix compatibility again
//                            (the page-1 / page-2 flicker, user report #2)
//   page-cursor-restart      pickShown(): `pageStart >= full.length` -> `> ... = 0`
//                            (cursor restart instead of rebasing onto the last page)
//   delay-zero-clamp         clampDisplayDelayMs(): 0 is no longer "no buffer"
//   all                      run every perturbation (default)

import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { Runner, loadOverlay } from "./overlay-harness.mjs";
import { KNOWN_FAILURES, runTestPlan } from "./overlay-cases.mjs";

const TESTS_DIR = path.dirname(fileURLToPath(import.meta.url));
const APP_JS_PATH = path.join(TESTS_DIR, "..", "overlay", "app.js");

/** One-line, anchor-checked source perturbations (in memory only). */
export const PERTURBATIONS = {
  "two-line-cap": {
    description: "max line clamp in renderStyle(): `if (lines > 4) lines = 4;` -> `lines = 6;`",
    apply(src) {
      const from = "    if (lines > 4) lines = 4;";
      const to = "    lines = 6;";
      if (!src.includes(from)) throw new Error(`anchor not found: ${JSON.stringify(from)}`);
      return src.replace(from, to);
    },
  },
  "buffer-semantics": {
    description: "show(): re-arm the display timer on every buffered partial (deadline extends)",
    apply(src) {
      // Strip the `if (!displayTimer)` guard so each buffered partial restarts
      // the full display_delay_ms window — the exact regression the buffer
      // assertions exist to catch.
      const from = "      if (!displayTimer) {\n        displayTimer = setTimeout(";
      const to = "      {\n        if (displayTimer) { clearTimeout(displayTimer); displayTimer = null; }\n        displayTimer = setTimeout(";
      if (!src.includes(from)) throw new Error(`anchor not found: ${JSON.stringify(from)}`);
      return src.replace(from, to);
    },
  },
  "replace-semantics": {
    description: "replacePartial(): append the revision instead of replacing the buffer",
    apply(src) {
      const from = "    partialBuffer = next;";
      if (!src.includes(from)) throw new Error(`anchor not found: ${JSON.stringify(from)}`);
      return src.replace(from, "    partialBuffer = partialBuffer + next;");
    },
  },
  // Regression guard: bypass the measured sliding window and render the full
  // sentence, which must break the configured line cap.
  "page-boundary-regression": {
    description:
      "pickShown(): render the unbounded sentence instead of the measured window",
    apply(src) {
      const from = "    return full.slice(pageStart);";
      if (!src.includes(from)) throw new Error(`anchor not found: ${JSON.stringify(from)}`);
      return src.replace(from, "    return full;");
    },
  },
  // Regression guard for the long-sentence page flicker (user report #2).  The
  // historical rule was strict prefix compatibility, which reset the page cursor
  // to 0 on every real ASR re-decode of the open sentence.
  "page-cursor-reset": {
    description:
      "sameOpenSentence(): require strict prefix compatibility again (a re-decoded frame = new sentence)",
    apply(src) {
      const from = [
        "    if (next.startsWith(prev) || prev.startsWith(next)) return true;",
        "    if (next.length * 2 < prev.length) return false; // 重新从小片段长起来 = 新的一句",
        "    const shorter = Math.min(prev.length, next.length);",
        "    const shared = commonPrefixLength(prev, next) + commonSuffixLength(prev, next);",
        "    // 短帧只要沾一点边就算同一句；长帧要求共享至少四分之一，避免把完全无关的",
        "    // 同长度文本误判为同一句。",
        "    return shorter < 8 ? shared > 0 : shared * 4 >= shorter;",
      ].join("\n");
      if (!src.includes(from)) throw new Error(`anchor not found: ${JSON.stringify(from)}`);
      return src.replace(from, "    return next.startsWith(prev) || prev.startsWith(next);");
    },
  },
  // Regression guard for the rebase: a shortened revision must not render a
  // blank frame after the cursor reaches the end of the text.
  "page-cursor-restart": {
    description:
      "pickShown(): keep an out-of-bounds cursor after a shortened revision",
    apply(src) {
      const from = "    pageStart = pageStart >= full.length ? start : Math.max(pageStart, start);";
      if (!src.includes(from)) throw new Error(`anchor not found: ${JSON.stringify(from)}`);
      return src.replace(from, "    pageStart = Math.max(pageStart, start);");
    },
  },
  "typewriter-window-reset": {
    description: "displayShown(): flash one character on corrections and new sentences",
    apply(src) {
      const from = "      typePos = shown.length;\n      textEl.textContent = shown;\n      return;";
      if (!src.includes(from)) throw new Error(`anchor not found: ${JSON.stringify(from)}`);
      return src.replace(from, "      typePos = nextUnitEnd(shown, 0);\n      textEl.textContent = shown.slice(0, typePos);\n      return;");
    },
  },
  // Regression guard for the new "0 秒（无缓冲）" option.
  "delay-zero-clamp": {
    description: "clampDisplayDelayMs(): 0 goes back to being clamped up to 500 ms",
    apply(src) {
      const from = "    if (ms === 0) return 0;";
      if (!src.includes(from)) throw new Error(`anchor not found: ${JSON.stringify(from)}`);
      return src.replace(from, "    if (ms === 0) return 500;");
    },
  },
};

function makePerturbedLoader(perturbation) {
  return (opts = {}) =>
    loadOverlay({
      ...opts,
      transform: (src) => {
        // Compose with any transform the case itself asked for (e.g. the
        // internal-state probe), then apply the perturbation.
        const base = opts.transform ? opts.transform(src) : src;
        const patched = perturbation.apply(base);
        if (patched === base) throw new Error("perturbation did not change the source");
        return patched;
      },
    });
}

const requested = process.argv[2] || "all";
const names = requested === "all" ? Object.keys(PERTURBATIONS) : [requested];
for (const n of names) {
  if (!PERTURBATIONS[n]) {
    console.error(`unknown perturbation ${JSON.stringify(n)}; known: ${Object.keys(PERTURBATIONS).join(", ")}, all`);
    process.exit(2);
  }
}

console.log("=".repeat(72));
console.log("NEGATIVE CONTROL — overlay/app.js perturbed IN MEMORY ONLY");
console.log(`source file : ${APP_JS_PATH}`);
console.log(`on-disk size: ${fs.statSync(APP_JS_PATH).size} bytes (never modified)`);
console.log(`perturbation: ${names.join(", ")}`);
console.log("=".repeat(72));

const baselineKnown = new Set(Object.keys(KNOWN_FAILURES));
let allGood = true;

for (const name of names) {
  const perturbation = PERTURBATIONS[name];
  const runner = new Runner(
    `negative control [${name}] — ${perturbation.description}`,
    KNOWN_FAILURES
  );
  console.log(`\n### perturbation: ${name}`);
  console.log(`    ${perturbation.description}`);
  await runTestPlan(runner, { load: makePerturbedLoader(perturbation) });
  const unexpected = runner.report();

  const failingIds = runner.failingIds();
  const newlyFailing = failingIds.filter((id) => !baselineKnown.has(id));

  console.log(`\n    unexpected failing cases : ${newlyFailing.length ? newlyFailing.join(", ") : "(none)"}`);
  console.log(`    known-failing cases      : ${runner.known.map((k) => k.caseId).join(", ") || "(none)"}`);
  console.log(`    passing cases            : ${runner.passed.length}`);

  if (newlyFailing.length === 0) {
    console.error(
      `\n    NEGATIVE CONTROL FAILED for [${name}]: the perturbed overlay still passed every ` +
        `assertion, so the suite does not actually test this invariant.`
    );
    allGood = false;
  } else {
    console.log(`    OK — the perturbation was detected by ${newlyFailing.length} case(s).`);
  }
  if (unexpected !== newlyFailing.length) {
    console.error("    (internal) unexpected count mismatch — check Runner bookkeeping");
    process.exit(3);
  }
}

console.log(`\n${"=".repeat(72)}`);
if (!allGood) {
  console.error("NEGATIVE CONTROL FAILED — the suite is not sensitive enough.");
  process.exit(1);
}
console.log("NEGATIVE CONTROL PASSED — every perturbation was caught by the suite.");
console.log("=".repeat(72));
process.exit(0);
