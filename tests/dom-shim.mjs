// Minimal browser shim for regression-testing overlay/app.js under plain Node.
//
// The overlay decides *what page of text to show* purely from
// `#caption-line`.scrollHeight, so a shim that returns a constant would make
// every pagination assertion meaningless.  This shim therefore implements a
// real (if simplified) layout model:
//
//   * text is laid out greedily into lines that each fit into `usableWidth`;
//   * a character's advance width is font-size dependent
//       CJK / full-width  -> 1.00 x font-size
//       spaces            -> 0.28 x font-size
//       anything else     -> 0.55 x font-size
//   * scrollHeight = lineCount * lineHeight.
//
// Geometry mirrors overlay/style.css + overlay/index.html:
//   .caption        max-width: min(96%, 1600px), padding: 10px 24px
//   .caption-line   display: block, max-width: 100%
//   .caption        font-size: var(--caption-size, 48px)
//   .caption        line-height: 1.25
// So at a 1600px-wide browser source the text box is min(1600*0.96, 1600) -
// 48px of horizontal padding = 1488px, i.e. 31 CJK chars per 48px line and
// 62 CJK chars per two-line page.
//
// Known limitation (see tests/README.md): the real browser uses the actual font
// metrics, the real 96%-vs-1600px cascade, `letter-spacing: 0.01em` and
// `word-break: break-word`.  The default model omits letter-spacing, which makes
// it optimistic by 0.48px per character at 48px font size: 1488/48 = 31 CJK
// chars per line here versus floor(1488/48.48) = 30 in the browser, i.e. up to
// one character per line and two per page.
//
// `DEFAULT_METRICS.letterSpacingPx` therefore exists so the whole suite can be
// re-run against browser-accurate advances; see `run-overlay-tests.mjs`
// (OVERLAY_TEST_LETTER_SPACING).  Conclusions must hold under BOTH geometries to
// be worth anything outside this shim.

import { FakeClock } from "./fake-clock.mjs";

export const DEFAULT_METRICS = Object.freeze({
  /// Browser-source viewport width used by the shim (OBS browser source).
  viewportWidth: 1600,
  /// `.caption { max-width: min(96%, 1600px) }`.
  maxCaptionWidth: 1600,
  /// `.caption { padding: 10px 24px }` -> 2 x 24px of horizontal padding.
  horizontalPadding: 48,
  /// `--caption-size` / `.caption { font-size: var(--caption-size, 48px) }`.
  fontSize: 48,
  /// `.caption { line-height: 1.25 }`.
  lineHeight: 1.25,
  /// Relative advance widths used by the line-breaking model.
  cjkRatio: 1,
  otherRatio: 0.55,
  spaceRatio: 0.28,
  /// Extra advance per character from `.caption { letter-spacing: 0.01em }`,
  /// as a multiple of the font size. Browsers add letter-spacing after EVERY
  /// character (including the last), so it shifts the per-line capacity:
  /// 0.01em at 48px = 0.48px, which is what turns 1488/48 = 31 chars per line
  /// into floor(1488/48.48) = 30. Kept configurable so the suite can be run
  /// against both the optimistic (0) and the browser-accurate (0.01) geometry.
  letterSpacingEm: 0,
});

/** Advance width of one character (in px) for a given font size. */
export function charWidth(ch, fontSize, metrics = DEFAULT_METRICS) {
  const spacing = fontSize * (metrics.letterSpacingEm || 0);
  if (ch === " " || ch === "\t" || ch === "\u00a0") return fontSize * metrics.spaceRatio + spacing;
  if (/[\u2e80-\u9fff\uf900-\ufaff\uff00-\uff60\u3000-\u303f\uac00-\ud7af]/.test(ch)) {
    return fontSize * metrics.cjkRatio + spacing;
  }
  return fontSize * metrics.otherRatio + spacing;
}

/** Width of `s` if it were rendered on a single unbroken line (in px). */
export function textWidth(s, fontSize, metrics = DEFAULT_METRICS) {
  let w = 0;
  for (const ch of String(s)) w += charWidth(ch, fontSize, metrics);
  return w;
}

/** Per-character advance widths, used by the greedy line breaker. */
function advances(s, fontSize, metrics) {
  const out = [];
  for (const ch of String(s)) out.push(charWidth(ch, fontSize, metrics));
  return out;
}

/** Greedy line wrapping: how many lines does `s` need inside `usableWidth`? */
export function layoutLines(s, usableWidth, fontSize, metrics = DEFAULT_METRICS, nowrap = false) {
  return lineBreaks(s, usableWidth, fontSize, metrics, nowrap).length;
}

/**
 * Greedy line breaking.  Returns one entry per rendered line:
 * `{ start, end, width }` where `[start, end)` is the byte range on that line.
 * Tests use the ranges to explain *which* character spilled onto line three.
 */
export function lineBreaks(s, usableWidth, fontSize, metrics = DEFAULT_METRICS, nowrap = false) {
  const w = advances(s, fontSize, metrics);
  if (w.length === 0) return [{ start: 0, end: 0, width: 0 }];
  if (nowrap) {
    return [{ start: 0, end: w.length, width: w.reduce((a, b) => a + b, 0) }];
  }
  const out = [];
  let start = 0;
  let x = 0;
  for (let i = 0; i < w.length; i++) {
    const adv = w[i];
    if (x > 0 && x + adv > usableWidth) {
      out.push({ start, end: i, width: x });
      start = i;
      x = adv;
    } else {
      x += adv;
    }
  }
  out.push({ start, end: w.length, width: x });
  return out;
}

/**
 * The shim's own answer to "how many lines would this text occupy?" — used by
 * tests to derive an expected line count independently of the overlay code.
 */
export function lineModel(text, opts = {}) {
  const metrics = { ...DEFAULT_METRICS, ...(opts.metrics || {}) };
  const fontSize = Number(opts.fontSize) || metrics.fontSize;
  const lineHeight = Number(opts.lineHeight) || metrics.lineHeight;
  const viewport = Number(opts.viewportWidth) || metrics.viewportWidth;
  const captionWidth = Math.min(viewport * 0.96, metrics.maxCaptionWidth);
  const usableWidth = (opts.usableWidth || captionWidth) - metrics.horizontalPadding;
  const lines = layoutLines(text, usableWidth, fontSize, metrics, !!opts.nowrap);
  return {
    lines,
    usableWidth,
    fontSize,
    lineHeight: lineHeight * fontSize,
    height: lines * lineHeight * fontSize,
  };
}

// ---------------------------------------------------------------------------
// DOM node stubs
// ---------------------------------------------------------------------------

class ClassList {
  constructor(owner) {
    this._owner = owner;
    this._set = new Set();
  }
  add(...names) {
    for (const n of names) this._set.add(n);
    this._owner._onClassChange?.();
    return undefined;
  }
  remove(...names) {
    for (const n of names) this._set.delete(n);
    this._owner._onClassChange?.();
    return undefined;
  }
  contains(name) { return this._set.has(name); }
  toggle(name, force) {
    const on = force === undefined ? !this._set.has(name) : !!force;
    if (on) this._set.add(name); else this._set.delete(name);
    this._owner._onClassChange?.();
    return on;
  }
  get value() { return [...this._set].join(" "); }
  toString() { return this.value; }
}

class StyleDecl {
  constructor(owner) {
    this._owner = owner || null;
    this._props = new Map();
  }
  setProperty(name, value) {
    const v = String(value);
    this._props.set(name, v);
    this._owner?._onStyleChange?.(name, v);
  }
  getPropertyValue(name) { return this._props.get(name) ?? ""; }
  removeProperty(name) { this._props.delete(name); }
}

/** Element stub. `scrollHeight` is computed from the layout model above. */
export class ElementStub {
  constructor({ id, doc, metrics = DEFAULT_METRICS }) {
    this.id = id || "";
    this.ownerDocument = doc;
    this._metrics = { ...DEFAULT_METRICS, ...metrics };
    this.classList = new ClassList(this);
    this.dataset = {};
    this.style = new StyleDecl(this);
    this._textContent = "";
    this._children = [];
    this._listeners = new Map();
    // `--caption-size` from the stylesheet default; renderStyle() overwrites it.
    this._fontSize = this._metrics.fontSize;
    this._lineHeight = this._metrics.lineHeight;
    // `single-line` adds white-space: nowrap in the real stylesheet.
    this._noWrap = false;
  }

  _onClassChange() {
    this._noWrap = this.classList.contains("single-line");
  }

  get className() { return this.classList.value; }
  set className(v) {
    this.classList._set = new Set(String(v).split(/\s+/).filter(Boolean));
    this._onClassChange();
  }

  get textContent() { return this._textContent; }
  set textContent(v) {
    this._textContent = v === null || v === undefined ? "" : String(v);
    this.ownerDocument?._onTextWrite?.(this, this._textContent);
  }

  get fontSize() { return this._fontSize; }
  get lineHeight() { return this._lineHeight; }

  /** Callback wired by createDocument(): keeps --caption-size in sync. */
  _onStyleChange(name, value) {
    if (name === "--caption-size") {
      const n = parseFloat(value);
      if (isFinite(n) && n > 0) this._fontSize = n;
    }
  }

  get scrollHeight() {
    const model = lineModel(this._textContent, {
      metrics: this._metrics,
      fontSize: this._fontSize,
      lineHeight: this._lineHeight,
      nowrap: this._noWrap,
    });
    if (this.ownerDocument?.__trace) {
      this.ownerDocument.__trace.push({
        len: this._textContent.length,
        lines: model.lines,
        height: model.height,
        fontSize: this._fontSize,
        lineHeight: this._lineHeight,
        usableWidth: model.usableWidth,
        head: this._textContent.slice(0, 8),
      });
    }
    return model.height;
  }

  get clientWidth() {
    return lineModel("", { metrics: this._metrics }).usableWidth;
  }

  addEventListener(type, fn) {
    if (!this._listeners.has(type)) this._listeners.set(type, []);
    this._listeners.get(type).push(fn);
  }
  removeEventListener(type, fn) {
    const arr = this._listeners.get(type) || [];
    const i = arr.indexOf(fn);
    if (i >= 0) arr.splice(i, 1);
  }
  dispatchEvent(ev) {
    for (const fn of [...(this._listeners.get(ev.type) || [])]) fn(ev);
    return true;
  }
}

class ElementWithChildren extends ElementStub {
  get children() { return [...this._children]; }
  appendChild(c) { this._children.push(c); c.parentNode = this; return c; }
}

// ---------------------------------------------------------------------------
// Document / window factory
// ---------------------------------------------------------------------------

/**
 * Parse the three ids the overlay actually queries out of overlay/index.html
 * (`#caption` wraps `#caption-line` and `#caption-cursor`).  Kept deliberately
 * tiny: a real HTML parser is not the thing under test.
 */
export function parseOverlayHtml(html) {
  const bodyClass = /<body[^>]*\bclass="([^"]*)"/i.exec(html)?.[1] || "";
  const ids = [...html.matchAll(/<[^>]*\bid="([^"]+)"/g)].map((m) => m[1]);
  return { bodyClass, ids };
}

/**
 * @param {object} opts
 * @param {string} opts.html            overlay/index.html source
 * @param {number} [opts.viewportWidth] browser-source width
 * @param {object} [opts.metrics]       layout metric overrides
 * @param {FakeClock} [opts.clock]
 */
export function createDocument({ html, viewportWidth, metrics, clock } = {}) {
  const merged = { ...DEFAULT_METRICS, ...(metrics || {}) };
  if (viewportWidth) merged.viewportWidth = viewportWidth;
  const script = parseOverlayHtml(html || "");
  const theClock = clock || new FakeClock();

  const doc = {
    metrics: merged,
    __surface: "document",
    _elements: new Map(),
    _onTextWrite: null,
    fonts: { ready: Promise.resolve() },
    hidden: false,
    readyState: "complete",
  };

  const makeElement = (id, extra = {}) =>
    Object.assign(new ElementWithChildren({ id, doc, metrics: merged }), extra);

  // documentElement is built first so renderStyle()'s --caption-size write can
  // be forwarded to #caption's computed font size (see below).
  const htmlEl = new ElementWithChildren({ id: "html", doc, metrics: merged });
  const body = makeElement("__body__");
  body.className = script.bodyClass || "";

  // The overlay only ever asks for #caption, #caption-line and
  // #caption-cursor; everything else must resolve to null like a real page.
  const elements = new Map([
    ["caption", makeElement("caption")],
    ["caption-line", makeElement("caption-line")],
    ["caption-cursor", makeElement("caption-cursor")],
    ["stage", makeElement("stage")],
  ]);
  for (const id of script.ids) if (!elements.has(id)) elements.set(id, makeElement(id));
  elements.get("caption").appendChild(elements.get("caption-line"));
  elements.get("caption").appendChild(elements.get("caption-cursor"));

  // Keep --caption-size authoritative: renderStyle() writes it on
  // documentElement, and getComputedStyle(#caption) must see it.
  htmlEl._onStyleChange = (name, value) => {
    if (name === "--caption-size") {
      const n = parseFloat(value);
      if (isFinite(n) && n > 0) {
        merged.fontSize = n;
        elements.get("caption")._fontSize = n;
      }
    }
  };

  const documentElement = htmlEl;

  doc.documentElement = documentElement;
  doc.body = body;
  doc._elements = elements;
  doc.getElementById = (id) => elements.get(id) || null;
  doc.createElement = (tag) => makeElement("", { tagName: String(tag).toUpperCase() });
  doc.querySelector = (sel) => (sel && sel.startsWith("#") ? doc.getElementById(sel.slice(1)) : null);
  doc.querySelectorAll = () => [];
  doc.addEventListener = () => {};
  doc.removeEventListener = () => {};
  doc.__clock = theClock;
  doc.__metrics = merged;
  return doc;
}

/** getComputedStyle for the shim: only fontSize / lineHeight are meaningful. */
export function createGetComputedStyle(doc) {
  return (el) => {
    const caption = doc.getElementById("caption");
    const fontSize = caption?._fontSize ?? doc.metrics.fontSize;
    const lineHeight = fontSize * (caption?._lineHeight ?? doc.metrics.lineHeight);
    return {
      fontSize: `${fontSize}px`,
      lineHeight: `${lineHeight}px`,
      getPropertyValue(name) {
        if (name === "font-size") return `${fontSize}px`;
        if (name === "line-height") return `${lineHeight}px`;
        return el?.style?.getPropertyValue?.(name) ?? "";
      },
    };
  };
}

/** In-memory localStorage with the Storage API surface the overlay uses. */
export function createLocalStorage() {
  const map = new Map();
  return {
    getItem: (k) => (map.has(String(k)) ? map.get(String(k)) : null),
    setItem: (k, v) => { map.set(String(k), String(v)); },
    removeItem: (k) => { map.delete(String(k)); },
    clear: () => map.clear(),
    key: (i) => [...map.keys()][i] ?? null,
    get length() { return map.size; },
    _map: map,
    /** Test helper: raise a same-tab-like `storage` event via the window. */
  };
}

/**
 * WebSocket stub. Records listeners, exposes `emit(payload)` so tests can
 * drive the overlay's message handler, and never opens a real socket.
 */
export function createWebSocketStub({ instances, onConstruct } = {}) {
  const list = instances || [];
  class WebSocketStub {
    constructor(url) {
      this.url = String(url);
      this.readyState = WebSocketStub.CONNECTING;
      this.sent = [];
      this.closed = false;
      this._listeners = new Map();
      list.push(this);
      onConstruct?.(this);
    }
    addEventListener(type, fn) {
      if (!this._listeners.has(type)) this._listeners.set(type, []);
      this._listeners.get(type).push(fn);
    }
    removeEventListener(type, fn) {
      const arr = this._listeners.get(type) || [];
      const i = arr.indexOf(fn);
      if (i >= 0) arr.splice(i, 1);
    }
    send(data) { this.sent.push(data); }
    close() {
      this.closed = true;
      this.readyState = WebSocketStub.CLOSED;
      this.dispatch("close", {});
    }
    dispatch(type, ev) {
      for (const fn of [...(this._listeners.get(type) || [])]) fn(ev);
    }
    /** Fire the browser `open` event (server accepted the upgrade). */
    open() {
      this.readyState = WebSocketStub.OPEN;
      this.dispatch("open", {});
    }
    /** Deliver one server frame exactly like a real browser would. */
    emit(payload) {
      const data = typeof payload === "string" ? payload : JSON.stringify(payload);
      this.dispatch("message", { data });
    }
  }
  WebSocketStub.CONNECTING = 0;
  WebSocketStub.OPEN = 1;
  WebSocketStub.CLOSING = 2;
  WebSocketStub.CLOSED = 3;
  return WebSocketStub;
}

export const __internal = { ClassList, StyleDecl };
