#!/usr/bin/env node
// Entry point: `node tests/run-overlay-tests.mjs`
//
// Boots the REAL overlay/app.js (read from ../overlay/app.js — never a copy of
// the algorithm) inside tests/dom-shim.mjs, drives it through a stub WebSocket
// on a virtual clock, and checks the two-line cap, the paging advance, the
// display buffer, the clear-after-silence timer and the replace semantics.
//
// Exit code: 0 when every assertion passes (or fails only in the documented
// KNOWN_FAILURES list), 1 on any unexpected failure.
//
// Geometry fidelity: the default shim omits `.caption { letter-spacing: 0.01em }`,
// which makes it optimistic by 0.48px per character (31 CJK chars per line
// instead of the browser's 30). Set OVERLAY_TEST_LETTER_SPACING=0.01 to re-run
// every assertion against browser-accurate advances. A conclusion is only worth
// taking outside this shim if it holds under BOTH geometries.

import { Runner, loadOverlay } from "./overlay-harness.mjs";
import { KNOWN_FAILURES, runTestPlan } from "./overlay-cases.mjs";

const letterSpacingEm = Number(process.env.OVERLAY_TEST_LETTER_SPACING || 0);
const trackMetrics = letterSpacingEm > 0 ? { letterSpacingEm } : undefined;

const runner = new Runner(
  "overlay subtitle regression — real overlay/app.js inside tests/dom-shim.mjs",
  KNOWN_FAILURES
);

const started = Date.now();
if (trackMetrics) {
  console.log(
    `geometry: browser-accurate mode — letter-spacing ${letterSpacingEm}em ` +
      `(+${(letterSpacingEm * 48).toFixed(2)}px per char)`
  );
}
await runTestPlan(runner, { load: (opts) => loadOverlay({ ...opts, metrics: { ...(opts?.metrics || {}), ...(trackMetrics || {}) } }) });
const unexpected = runner.report();
console.log(`\nelapsed: ${Date.now() - started} ms`);

if (unexpected > 0) {
  console.error(`\nOVERLAY REGRESSION FAILED — ${unexpected} unexpected assertion group(s) failed.`);
  process.exit(1);
}
if (runner.known.length > 0) {
  console.log(
    `\nOK for the assertions this suite owns, with ${runner.known.length} documented real-code ` +
      `defect group(s) still open (see tests/README.md).`
  );
  process.exit(0);
}
console.log("\nOK — every overlay subtitle invariant holds.");
process.exit(0);
