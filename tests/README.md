# overlay regression tests

Repeatable Node regression tests for the live-subtitle overlay
(`overlay/app.js` + `overlay/index.html` + `overlay/style.css`).

They lock down the **configurable 1–4 line cap** (default: two lines), the **word-aligned live window**, the **display
buffer semantics**, the **clear-after-silence timer** and the **`replace:true`
revision semantics** so a future edit cannot silently break them.

No dependencies, no network, no `cargo`, no git: Node's built-in modules only
(developed on Node v24).

---

## How to run

```bash
cd source/stream-live-translate-main

# 1) the suite — real overlay/app.js inside the DOM shim
node tests/run-overlay-tests.mjs            # exit 0 = every assertion holds

# 2) negative control — perturb overlay/app.js IN MEMORY and prove the suite fails
node tests/run-negative-control.mjs                     # all perturbations
node tests/run-negative-control.mjs two-line-cap        # just one

# 3) page-boundary evidence (historical vs current pickShown, plus live DOM cross-check)
node tests/repro-page-boundary.mjs

# 4) the retracted false-positive measurement, kept only as a record
#    (its output must NOT be read as evidence of an overlay defect — see
#     "Issue #4 ... retracted" below)
node tests/repro-stale-page-offset.mjs
```

Exit codes: `0` = all assertions hold (or only documented known failures
remain), `1` = an unexpected failure, `2`/`3` = usage/internal error in the
negative control.

The suite prints the file it is testing; `overlay/app.js` is **read from disk
and executed** inside the shim — the paging algorithm is never copied into the
tests. A copy could not detect a regression in the real code.

---

## Files

| file | purpose |
| --- | --- |
| `dom-shim.mjs` | Minimal browser shim with a **real layout model** (greedy line breaking, `scrollHeight = lines x lineHeight`), `document`, element stubs, `getComputedStyle`, `localStorage`, a `WebSocket` stub with `emit(payload)`, `fetch`. |
| `fake-clock.mjs` | Deterministic virtual timeline for `setTimeout`/`setInterval`; nothing waits on real time. |
| `overlay-harness.mjs` | Loads the real `overlay/app.js` with `node:vm`, wires the shim, exposes measurement helpers (`lineCount`, `measuredHeight`, `lineBreaks`, `overflowReport`, `displayDeadline`, …) and the tiny test `Runner`. |
| `overlay-cases.mjs` | All assertions (the suite) and the overlay's own replay fixtures. |
| `run-overlay-tests.mjs` | Entry point for the suite. |
| `run-negative-control.mjs` | In-memory perturbations; proves the suite can fail. |
| `repro-page-boundary.mjs` | Standalone evidence for the page-boundary defect (historical vs current `pickShown()`, plus a live-DOM cross-check). |
| `repro-stale-page-offset.mjs` | Record of the **retracted** issue-#4 measurement; not evidence of a defect. |

---

## Assertion coverage

### S — shim self-checks (prove the measurement is not a rubber stamp)

| id | assertion |
| --- | --- |
| `shim-geometry` | usable width `= min(96% x 1600, 1600) - 2 x 24px padding = 1488px`; line height `= 48 x 1.25 = 60px`; `--caption-max-height` is published as `120.00px`. |
| `shim-lines` | exact line counting: 31 CJK chars = 1 line, 32 = 2, 62 = 2, 63 = 3; 100 Latin chars still fit two lines, 113 need three (the model weighs advances, it does not count characters). |
| `shim-latin` | a 208-char unbreakable Latin token is swept prefix by prefix: never more than two lines (mid-word wrapping). |

### A1 / A2 — two-line cap and paging advance

| id | assertion |
| --- | --- |
| `delta-stream` | **every** prefix of a 155-char CJK sentence, streamed as true deltas: DOM `scrollHeight <= 120px` (<= 2 lines); after overflow, the latest window still occupies two lines and ends with the newest character. |
| `two-row-word-window` | a long spoken sentence including “分数占比20%” retains two rows after overflow and starts at an `Intl.Segmenter` word boundary. |
| `two-row-typewriter` | the default typewriter keeps two *visible* rows immediately after each live window shift and after an ASR tail revision, without waiting for its next timer tick. |
| `atomic-typewriter-revision` | corrections and new sentences replace the visible caption atomically (no one-character flash); only a true append waits for the next typewriter tick. |
| `replace-stream` | same sweep with cumulative `replace:true` revisions: <= 2 lines, pages are contiguous slices, `pageStart` never moves backwards, paging advances (max page start > 0) and the **union of all rendered ranges covers 155/155 characters** (nothing skipped). |
| `replay-REPLAY_PAGED` / `replay-REPLAY_VERY_LONG` | the overlay's own replay samples (`overlay/app.js`), prefix by prefix, <= 2 lines. |
| `geometry-1600` / `geometry-800` | the same sweep at browser-source widths 1600 px and 800 px (800 px = 15 CJK chars/line, more cuts). |
| `max-lines-3` / `max-lines-4` | configured three- and four-line pages really render at those heights. |
| `max-lines-6` | a server config asking for `max_lines: 6` is capped at four lines and still pages. |
| `punctuation-page` | a comma on the first line does not force an early page turn when the configured second line is available. |
| `max-lines-1` | `max_lines: 1` keeps strict single-line mode (`single-line` class, one-line viewport). |
| `typewriter` | with `animation: typewriter`, every page handed to `displayShown()` is still <= 2 lines, the page is a contiguous slice of the streamed sentence, the page advances (furthest page start > 0) and it never moves backwards. The page is read from an in-memory render log, because `textContent` is only a typed *prefix* of the page while the animation runs. |

### A1f — page stability (the long-sentence flicker)

| id | assertion |
| --- | --- |
| `page-flicker` | for three one-sentence streams (tail re-decoded every 6th frame, a word inserted mid-sentence, and pure growth as a control): every rendered page is a contiguous window of the **current** revision, never needs a 3rd line, the sentence pages forward, and the window **never moves backwards**. A final sub-check trims a paged sentence back to 100 chars (still more than one page) and requires the caption to show the newest words (last page, no empty frame, no fall back to page 1). |
| `page-flicker-typewriter` | the same four streams with `animation: typewriter` (the shipped default). |

### A6 — the overlay's own debug/replay path

| id | assertion |
| --- | --- |
| `local-replay` | loading the overlay with `?local-replay=1` and driving the fake clock over the whole 12 s timeline renders no empty page, never a 3rd line, every rendered page is part of the current line, the completion marker (`<body data-local-replay="complete">`) is reached, a page turn happens for the long sample (driven through `resize()`, which is how a real browser re-renders after the fonts load), and the caption is cleared again once the replay goes silent. |

### A3 — display buffer semantics

| id | assertion |
| --- | --- |
| `buffer` | the first partial of a page is delayed by `display_delay_ms`; a partial arriving **inside** the buffer replaces the queued text and does **not** re-arm the timer (the deadline keeps counting down: 750 -> 450 -> 50 ms remaining); at 749 ms nothing is shown, at 750 ms the **latest** text appears; after the page is live, later partials update immediately. |
| `delay-config` | `display_delay_ms` is configurable and clamps to `[500, 1000]` (500 shows at exactly 500 ms; an out-of-range value lands at 1000 ms). |
| `delay-zero` | `display_delay_ms = 0` is a real "no buffer" mode: the text is visible in the same tick the event arrives, **no** display timer is created (the only new one-shot timer is the silence clear), the page after a silence clear is also unbuffered, and on the typewriter path the first unit is written synchronously (no 32 ms wait). Clamping: `0 -> 0`, `250 -> 500`, `-100 -> 500`, `100000 -> 1000`. |

### A4 — clear-after-silence

| id | assertion |
| --- | --- |
| `clear-first-line` | a line that reached the screen through the display buffer is cleared `clear_after_ms` after the last event, carries `empty`, drops `show`, and does not come back. |
| `clear-live` | a live line is cleared at `clear_after_ms - 1` / `clear_after_ms`; the next page re-enters the buffer with a full `display_delay_ms`; a `resize` during that buffer (start and mid-buffer) does **not** resurrect text; a second silence period clears again. |
| `clear-rebuffer` | after a silence clear the next page is queued (not painted instantly). |
| `clear-delta` | after a clear, a fresh delta stream renders **only its own text** (the internal `partialBuffer` must be empty at that point — asserted through the overlay's own state). |
| `clear-config` | `clear_after_ms` is configurable (2000 ms) and clamps up to >= 1000 ms. |

### A5 — replace semantics

| id | assertion |
| --- | --- |
| `replace-semantics` | `ABC` then `ABCD` with `replace: true` renders `ABCD`, never `ABCABCD`; a later revision replaces the live text immediately and no earlier revision survives as a prefix. |

---

## Negative control (the suite is proven to fail)

`node tests/run-negative-control.mjs` reads `overlay/app.js`, perturbs one
invariant **in memory only** (the file on disk is never written, and no file is
written at all), and re-runs the identical assertions. It exits non-zero unless
every perturbation is caught by at least one case that passes in the baseline.

| perturbation | what it breaks | caught by |
| --- | --- | --- |
| `two-line-cap` | `if (lines > 4) lines = 4;` -> `lines = 6;` | the configured line-cap case |
| `buffer-semantics` | `show()` re-arms the display timer on every buffered partial | `buffer`, `local-replay` |
| `replace-semantics` | `replacePartial()` appends the revision instead of replacing the buffer | `replace-stream`, `shifting-revision`, `page-flicker`, `page-flicker-typewriter`, `clear-live`, `clear-config`, `replace-semantics`, `typewriter` |
| `page-boundary-regression` | `pickShown()` renders from `pageStart` to the end again (the historical off-by-one) | 12 cases, incl. `page-flicker`, `local-replay` |
| `page-cursor-reset` | `sameOpenSentence()` requires strict prefix compatibility again (a re-decoded frame = "new sentence") — the page-1 / page-2 flicker | `page-flicker`, `page-flicker-typewriter` |
| `page-cursor-restart` | `pickShown()` restarts the cursor at 0 instead of rebasing onto the last page that fits | `page-flicker`, `page-flicker-typewriter` |
| `delay-zero-clamp` | `clampDisplayDelayMs()` clamps `0` up to 500 ms again (no more "no buffer") | `delay-zero` |

---

## Defects this suite found (and their status)

The suite was written against `overlay/app.js` and immediately failed. Three
real defects were confirmed and have since been fixed by the owning agent; the
assertions that caught them are now green and stay in place as regression
guards.

1. **Page boundary (fixed).** Two compounding faults:
   `findCut()` returns the **end** index of the longest slice that fits, but
   `pickShown()` assigned that value to `pageStart` and returned
   `full.slice(pageStart)` — an end index used as a start index, so the page
   rendered the **tail** of the sentence instead of the slice that had actually
   been measured (longer than anything measured, hence the third line). On top
   of that, `replacePartial()` reset `pageStart = 0` on **every** cumulative
   frame, so a long sentence could never page forward at all.
   Fix: render exactly `full.slice(pageStart, cut)`, and only reset `pageStart`
   when the revision does not extend the previous frame.
   Evidence: `tests/repro-page-boundary.mjs` (historical model: 1 overflow at
   prefix 125; live overlay after the fix: 0 overflows, pages advance 0 -> 62).
2. **The first line of a page was never cleared (fixed).** `renderShow()` opened
   with `clearTimeout(hideTimer)`, but the buffered first render only runs after
   `display_delay_ms`, so the silence deadline armed when the text *arrived* was
   destroyed — the caption stayed on screen indefinitely. Caught by
   `clear-first-line` and `clear-delta`.
3. **A cleared page glued the next sentence onto the old one (fixed).** `hide()`
   cleared `currentText` but not `partialBuffer`, so the next sentence's first
   delta was appended to the previous sentence (`"<old><new>"`). Caught by
   `clear-delta`.

### Issue #4 — "stale page offset on a shifting revision": **retracted, it was a measurement error in this suite**

The first version of this section claimed the overlay rendered text belonging to
the previous revision. That claim is **withdrawn**; the suite was reading the
cursor at the wrong moment. The history is kept here because the mistake is
instructive.

What the old assertion did:

```js
ov.emitPartial(text, true);
const after = ov.sandbox.__probe();          // pageStart AFTER renderShow()
if (after.pageStart + dom.length > text.length) → "window outside the revision"
```

`pickShown()` renders `full.slice(oldPageStart, cut)` and **then** assigns
`pageStart = cut`. Reading `pageStart` after the render therefore pairs the
*advanced* cursor with the *pre-advance* DOM: for prefix 63 of a 63-char text it
computed `62 + 62 = 124 > 63` and reported a window of `[62, 124)`. The real
window was `[0, 62)`, and the rendered text was a contiguous substring of the
current revision (`text.indexOf(dom) === 0`) the whole time. The old claim that
`findCut()` could "return an index below `pageStart`" was also wrong: it always
returns at least `start + 1`.

Two further observations on the mechanism, both verified:

* `getComputedStyle(#caption).lineHeight` returns `"60px"` in the shim, so
  `maxH = 2 x 60 = 120px`, matching the browser.
* The 1552 px/32-char model used in an early hand-port could not reproduce the
  real overflow because it omitted the 48 px of `.caption` padding; the correct
  usable width is 1488 px.

The case now asserts the three invariants that are actually true, and it runs by
default (it is no longer gated):

1. the rendered page is a contiguous substring of the **current** revision
   (`text.indexOf(dom) >= 0`) — this is the assertion that would catch a genuine
   stale page;
2. `pageStart <= text.length` (the cursor never ends past the end);
3. a shifting cumulative revision still pages forward.

```bash
node tests/run-overlay-tests.mjs      # 25 passed / 0 failed
```

`OVERLAY_TEST_OPEN_ISSUES=1` is no longer needed and is ignored.

`repro-stale-page-offset.mjs` was written against the retracted measurement; it
is kept only as the record of that measurement and its output must **not** be
read as evidence of an overlay defect.

#### Separate finding: `replacePartial()`'s same-sentence test was one-directional

This is a **different, genuine** robustness problem, found while investigating
the false positive above. It is fixed in `overlay/app.js`; keep the two findings
apart — one was a bad assertion, this one was real code.

The old check was:

```js
const grew = next.startsWith(partialBuffer) && partialBuffer.length > 0;
if (!grew) pageStart = 0;
```

A revision that *reshapes the tail* (`"…整段重"` → `"…整段重新"`) is neither an
extension nor a truncation of the previous frame, so `grew` was `false` and the
sentence restarted — correct by luck, but the opposite case is not: a revision
that is a **prefix** of the previous frame (`"…abc"` → `"…ab"`, a deletion) also
failed `startsWith`, while any revision that *is* an extension but moved the tail
kept a `pageStart` that no longer described the new text. The predicate is now
symmetric — either frame being a prefix of the other means "same sentence", so
the page survives a pure extension/trim and resets on a genuine rewrite:

```js
const sameSentence =
  partialBuffer.length > 0 &&
  (next.startsWith(partialBuffer) || partialBuffer.startsWith(next));
```

`A1o` covers this (case id `shifting-revision`).

A real, adjacent defect *was* found while investigating this and is fixed:
`replacePartial()` treated a revision as "the same sentence" using
`next.startsWith(partialBuffer)` alone. A revision that trims or reshapes the
tail (`"…整段重"` → `"…整段重新"`) is neither a prefix nor an extension, and
keeping the old `pageStart` could leave the cursor outside the new text. The
check is now symmetric (either revision being a prefix of the other keeps the
page; anything else restarts the sentence), and the case above covers it.

---

## REAL DEFECT #4 — long-sentence page flicker (reported by the user, fixed)

> 「当一句话比较长时出第二段字幕的时候第一段字幕可能会和第二段字幕交替闪烁」

**Symptom.** A long sentence that needs a second page shows page 2, then page 1
again, then page 2 … — the two segments alternate on screen.

**Mechanism.** `replace: true` frames are cumulative revisions of the provider's
*open* sentence (Bailian Fun-ASR: `src/llm.rs` pushes `SubtitleEvent::Replace`
for every partial and only `sentence_end: true` produces a `Final`). Real ASR
re-decodes the tail (homophone/punctuation) and can insert a word mid-sentence,
so frame N+1 is frequently neither an equal, a prefix, nor an extension of
frame N. The same-sentence predicate above — symmetric *prefix* compatibility —
classified every such frame as a **new sentence** and ran `pageStart = 0`.

The flicker is one frame wide, which is why reading the cursor *after* the
render hides it: `pickShown()` immediately re-cuts page 1 and advances `pageStart`
to 62 again before the frame returns, but the **text handed to the DOM for that
frame was page 1**. `repro-page-flicker.mjs` therefore records what was actually
rendered instead of the end-of-frame cursor.

**Fix (`overlay/app.js`).**
* `sameOpenSentence(prev, next)` keeps the page cursor for the whole open
  sentence: a frame that is a prefix/extension in either direction, or that
  still shares enough text (not less than half the length of the shorter frame,
  and not less than a quarter of it once the frames are long) is the same
  sentence. Only a frame that clearly restarts — much shorter than the previous
  one, or sharing next to nothing — resets to page 1.
* `pickShown()` **rebases** the cursor with `findLastPageStart()` when a revision
  leaves it at/past the end of the text (the cursor used to be pushed back to 0,
  and `pageStart === full.length` rendered an empty caption for a frame).
* `displayShown()` writes the first typewriter unit synchronously, so
  `display_delay_ms = 0` is genuinely immediate on the typewriter path too.

**Evidence.**

```bash
node tests/repro-page-flicker.mjs                    # S1 growth 0 / S2 tail re-decode 10 /
                                                     # S3 mid-sentence insert 1 / S4 typewriter 10
                                                     # pre-fix: 21 backward page moves, exit 1
node tests/run-negative-control.mjs page-cursor-reset   # the new cases fail again
node tests/repro-page-flicker.mjs                    # post-fix: 0 backward page moves, exit 0
```

---

## `display_delay_ms = 0` (「0 秒（无缓冲）」)

The panel offers 0 as "no buffer". It is a legal value on every layer:
`overlay/app.js:clampDisplayDelayMs()` (0, or 500–1000; missing/garbage -> 750),
`src/config.rs:clamp_display_delay_ms()` (identical rule, used by both
`GET /api/config` and the WebSocket `config` push), and the admin panel
(`clampDisplayDelay()` + `setSelectValue()`, because `value || 750` and
`Math.max(500, …)` both used to turn 0 back into a buffer). With 0 no display
timer is created at all. `A3c` (case id `delay-zero`) pins it.

---

## Known limitations

* **The model is optimistic by up to one character per line.** `.caption` sets
  `letter-spacing: 0.01em`, which real browsers add after every character:
  `0.48px` at 48px font size. The default shim geometry omits it, so it fits
  **31** CJK chars per line where a browser fits `floor(1488 / 48.48) = 30` —
  up to one character per line and **two per two-line page (~3.2%)**.
  Every assertion is therefore also runnable against browser-accurate advances:

  ```bash
  node tests/run-overlay-tests.mjs                              # 31 chars/line model
  OVERLAY_TEST_LETTER_SPACING=0.01 node tests/run-overlay-tests.mjs   # 30 chars/line model
  ```

  Both runs are **21 passed / 0 failed**, i.e. the two-line cap, paging advance
  and coverage results are insensitive to that 3.2% geometry error. That is a
  robustness result for the *algorithm*; it is **not** proof that a real browser
  never overflows. Treat real-browser layout as **not yet verified** — see below.

* **Real-browser rendering is NOT verified.** Everything here runs against
  `dom-shim.mjs`. The two-line cap in an actual OBS browser source has never been
  observed in this project: no build of the overlay has been rendered in OBS or a
  browser, so "≤2 lines" is a model-level result only. Do not upgrade it to
  "verified" on the strength of a green suite.

* **Shim geometry is a model, not a browser.** The usable text width is taken
  as `min(96% x viewport, 1600px) - 48px` (the `.caption` box is
  `box-sizing: border-box` with `padding: 10px 24px`, and `.caption-line` is
  `max-width: 100%`, so 24 px of padding sits on each side). The default
  viewport is **1600 px**, i.e. 1488 px usable, 31 CJK chars per line and 62 per
  two-line page at 48 px. A model that forgets the padding gets 1552 px / 32
  chars per line and *will not reproduce* every real overflow — see
  `tests/repro-page-boundary.mjs`, which prints both models side by side. If
  your OBS browser source is not 1600 px wide, pass the real width:
  `loadOverlay({ viewportWidth: 1280 })` (the `geometry-800` case does this).
* **Character advance widths are approximations**: CJK/full-width = `1.00 x`
  font-size, space = `0.28 x`, everything else = `0.55 x`. Real fonts differ per
  glyph and per family, and `overlay/style.css` also sets
  `letter-spacing: 0.01em` (+1% per character) which the shim ignores. Line
  breaking is a greedy per-character wrap with no wrapping at word boundaries
  (`word-break: break-word` in the CSS is modelled as "break anywhere").
  Consequence: a cut position can differ from a real browser by about one
  character; the assertions are written to be insensitive to that (they check
  the cap, contiguity, advancement and coverage, not exact page lengths).
* **`--caption-max-height` has no effect in the real stylesheet.** `app.js`
  publishes it, but no rule in `overlay/style.css` consumes it: the cap is
  enforced entirely by `app.js`'s own measurement plus `overflow: hidden`. The
  tests therefore assert the *overflow* (the third line inside the element), not
  a CSS clamp. Prose in the code comments that says the variable caps the box is
  not accurate.
* **The typewriter path is time-stepped, not frame-accurate.** The shim advances
  the virtual clock in 32 ms steps (the overlay's `TYPE_CHAR_MS`), so it samples
  the animation rather than reproducing a real display's frame timing.
* **`document.fonts.ready`** resolves immediately, so the "re-paginate after the
  webfont loads" path is only exercised as a single `refreshCaption()` call.
* **WebSocket transport is stubbed.** Reconnection, exponential backoff, ping
  and half-open detection are not covered by these assertions.
* **The silence-clear test is driven by a virtual clock**, so it verifies the
  logic (`clear_after_ms` after the last event) and not real wall-clock drift.

---

## Quick reference: shim geometry

| quantity | value | source |
| --- | --- | --- |
| viewport (browser source) | 1600 px | `loadOverlay({ viewportWidth })`, default |
| `.caption` max width | `min(96%, 1600px)` = 1536 px at 1600 px | `overlay/style.css:45` |
| horizontal padding | 24 px each side (border-box) | `overlay/style.css:46` |
| usable text width | 1488 px | derived |
| font size | 48 px | `overlay/style.css:58`, `index.html` |
| line height | 1.25 -> 60 px | `overlay/style.css:52` |
| two-line viewport | 120 px | `2 x 60` |
| CJK chars per line / page | 31 / 62 | derived |
| `display_delay_ms` | 750 (0 = no buffer, otherwise clamped 500–1000) | `overlay/app.js`, `src/config.rs`, `src/server.rs` |
| `clear_after_ms` | 4000 (clamped 1000–15000) | `overlay/app.js`, `src/config.rs` |
