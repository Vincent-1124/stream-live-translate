// Browser Source overlay JS.
//   * 样式以服务端 /api/config 为权威（保存后经 WebSocket 实时推送）；
//     URL hash（#size=&color=&bg=…）只作为首帧兜底，会被配置覆盖，
//     因此 admin 改完样式 OBS 字幕立即生效，无需重新复制浏览器源 URL。
//   * 字幕实时渲染：模型每吐一个字就立刻显示（不等人把话说完），
//     追求尽可能低的延迟。
//   * 超过「最大行数」的部分不再省略，而是直接换到下一句字幕显示，
//     不做滚动 / 位移。
//   * 宽 / 高 / 圆角可用 bgWidth / bgHeight / radius 固定。

(function () {
  "use strict";

  const captionEl = document.getElementById("caption");
  const lineEl = document.getElementById("caption-line");
  // 字幕文字直接写在 #caption-line 上：超出最大行数的部分会换到下一句，
  // 不做位移，因此不需要「视口 + 内层」那套结构。
  // textEl 只是给渲染代码用的别名，保持内部写法统一。
  const textEl = lineEl;
  let currentText = "";
  let hideTimer = null;
  let displayTimer = null;
  let pendingText = null;
  let displayDelayMs = 750;
  /// 最后一条字幕事件后自动清屏的时长（服务端 overlay.clear_after_ms）。
  let clearAfterMs = 4000;
  let partialBuffer = "";
  let ws = null;

  // WebSocket 断线重连：指数退避 + 抖动，避免服务端重启期间刷屏重连。
  const WS_RETRY_MIN_MS = 500;
  const WS_RETRY_MAX_MS = 15000;
  /// 超过该时长没有收到任何帧（含 pong）就判定连接半开并主动重连。
  const WS_STALE_MS = 30000;
  const WS_PING_MS = 10000;
  /// 服务端在 server.rs 中回以 "pong"，用来证明链路仍然活着。
  const WS_PING_TEXT = "ping";
  let wsRetryDelay = WS_RETRY_MIN_MS;
  let wsRetryTimer = null;
  let wsPingTimer = null;
  let wsLastFrameAt = 0;
  let wsGeneration = 0;

  const WATERMARK_WORDS = [
    "字幕", "subtitle", "翻译", "translate", "实时", "real-time", "live",
    "AI", "人工智能", "智能翻译", "同声传译", "直播", "stream"
  ];

  const recentSentences = [];
  const MAX_RECENT = 5;

  function cleanText(text) {
    if (!text) return "";
    let cleaned = text.trim();
    for (const word of WATERMARK_WORDS) {
      const regex = new RegExp(`^${word}[\\s,.，、.]*|[\\s,.，、.]*${word}$|^${word}$`, "gi");
      cleaned = cleaned.replace(regex, "");
    }
    cleaned = cleaned.replace(/(.)\1{3,}/g, "$1$1$1");
    cleaned = cleaned.replace(/\s+/g, " ");
    return cleaned.trim();
  }

  function isDuplicate(text) {
    const cleaned = cleanText(text);
    if (!cleaned || cleaned.length < 3) return true;
    for (const recent of recentSentences) {
      const aWords = new Set(cleaned.toLowerCase().split(/\s+/));
      const bWords = new Set(recent.toLowerCase().split(/\s+/));
      const intersection = [...aWords].filter((x) => bWords.has(x));
      if (aWords.size > 0 && intersection.length / aWords.size > 0.7) return true;
    }
    recentSentences.push(cleaned);
    if (recentSentences.length > MAX_RECENT) recentSentences.shift();
    return false;
  }

  // ---- style -------------------------------------------------------------

  function hexToRgba(hex, alpha) {
    if (!hex || hex[0] !== "#") return null;
    let h = hex.slice(1);
    if (h.length === 3) {
      h = h[0] + h[0] + h[1] + h[1] + h[2] + h[2];
    }
    if (h.length !== 6 || /[^0-9a-fA-F]/.test(h)) return null;
    const r = parseInt(h.slice(0, 2), 16);
    const g = parseInt(h.slice(2, 4), 16);
    const b = parseInt(h.slice(4, 6), 16);
    return `rgba(${r},${g},${b},${alpha})`;
  }

  /// `overlay.display_delay_ms` 的合法取值：0 = 关闭缓冲（首个页面到达即显示），
  /// 或 500–1000 ms 的缓冲窗口。缺省 / 非法值落到 750；负数与 1–499 这类
  /// "既不是 0 也不在窗口内"的值仍然钳进 500–1000。
  function clampDisplayDelayMs(value) {
    if (value === null || value === undefined || value === "") return 750;
    const ms = Number(value);
    if (!isFinite(ms)) return 750;
    if (ms === 0) return 0;
    return Math.min(1000, Math.max(500, ms));
  }

  /// 0–100 的透明度百分比 → 0.0–1.0 的 alpha。缺省 75。
  function toAlpha(v) {
    let n = Number(v);
    if (!isFinite(n)) n = 75;
    n = Math.min(100, Math.max(0, n));
    return Math.round(n) / 100;
  }

  // 所有样式先写进这里，最后统一 render，避免「配置」和「hash」互相打架。
  const style = {
    size: null,
    color: null,
    bgColor: "#000000",
    bgOpacity: 75,
    width: 0,
    height: 0,
    radius: 8,
    maxLines: 2,
  };
  let maxLines = 2;

  function renderStyle() {
    const root = document.documentElement.style;

    if (style.size) root.setProperty("--caption-size", style.size + "px");
    if (style.color) root.setProperty("--caption-color", style.color);

    // 半透明背景：颜色 + 透明度合成为 rgba()，这一步是「半透明化」的关键。
    const alpha = toAlpha(style.bgOpacity);
    const bg = hexToRgba(style.bgColor, alpha) || `rgba(0,0,0,${alpha})`;
    root.setProperty("--caption-bg", bg);

    // 0 = 自动：宽度贴合文字，高度恒为一行。
    root.setProperty("--caption-width", Number(style.width) > 0 ? Number(style.width) + "px" : "auto");
    root.setProperty("--caption-height", Number(style.height) > 0 ? Number(style.height) + "px" : "auto");
    const r = Number(style.radius);
    root.setProperty("--caption-radius", (isFinite(r) && r >= 0 ? r : 8) + "px");

    // 行数由管理页设置，超限的长句由分页逻辑推进。
    let lines = Math.round(Number(style.maxLines));
    if (!isFinite(lines) || lines < 1) lines = 2;
    if (lines > 4) lines = 4;
    maxLines = lines;
    lineEl.classList.toggle("single-line", lines <= 1);

    // 视口最大高度按当前字号/行高算出，字号变化时自动跟随。
    const fs = parseFloat(getComputedStyle(captionEl).fontSize);
    const lhRaw = parseFloat(getComputedStyle(captionEl).lineHeight);
    const lh = isFinite(lhRaw) && lhRaw > 0 ? lhRaw : (isFinite(fs) ? fs : 48) * 1.25;
    root.setProperty("--caption-max-height", (lines * lh).toFixed(2) + "px");

    // 行数 / 字号变了，当前这句要重新切。
    refreshCaption();
  }

  // ---- 长句显示 ---------------------------------------------------------
  // 流式识别时始终显示最新内容。超过行数上限后保留能容纳的最长尾部，
  // 而不是立刻跳到只有几个字的下一页；起点尽量对齐词边界。

  let pageStart = 0; // 当前这句在完整文本中的起始字符下标

  /// 把 s 写进字幕测高度，读完立即恢复原文本 —— 绝不能让测量破坏打字机
  /// 的显示进度。
  function measureHeight(s) {
    const prev = textEl.textContent;
    textEl.textContent = s;
    const h = textEl.scrollHeight;
    textEl.textContent = prev;
    return h;
  }

  const wordSegmenter = typeof Intl !== "undefined" && typeof Intl.Segmenter === "function"
    ? new Intl.Segmenter("zh", { granularity: "word" }) : null;
  function wordAlignedStart(text, start) {
    if (start === 0 || start >= text.length) return start;
    if (wordSegmenter) {
      for (const part of wordSegmenter.segment(text)) {
        if (part.index >= start && part.isWordLike) return part.index;
      }
    } else {
      for (let i = start; i < text.length; i++) {
        if (/\s/.test(text[i - 1]) && !/\s/.test(text[i])) return i;
      }
    }
    // 单个超长词本身超过上限时只能硬截；至少不要截断代理对。
    if (/[\uDC00-\uDFFF]/.test(text[start]) && /[\uD800-\uDBFF]/.test(text[start - 1])) start++;
    return start;
  }

  /// 满足 `full.slice(start)` 仍能塞进视口的最小 start。
  function findLastPageStart(full, maxH) {
    let lo = 0;
    let hi = full.length;
    let best = 0;
    while (lo <= hi) {
      const mid = (lo + hi) >> 1;
      if (measureHeight(full.slice(mid)) <= maxH) {
        best = mid;
        hi = mid - 1;
      } else {
        lo = mid + 1;
      }
    }
    return best;
  }

  /// 渲染完整累积文本中「当前这句」。
  function renderCaption(full) {
    if (!textEl) return;
    const shown = pickShown(full);
    displayShown(shown);
  }

  /// 决定应显示哪一段：单行模式全文给 CSS 省略号；多行显示最新的完整窗口。
  function pickShown(full) {
    if (maxLines <= 1) return full;
    const lh = parseFloat(getComputedStyle(captionEl).lineHeight) || 0;
    const maxH = maxLines * lh;
    if (maxH <= 0) return full;
    if (!full) return full;
    const start = wordAlignedStart(full, findLastPageStart(full, maxH));
    // ASR 修订偶尔缩短尾巴，不能因此跳回旧的第一页；真正越界时重新定位。
    pageStart = pageStart >= full.length ? start : Math.max(pageStart, start);
    return full.slice(pageStart);
  }

  /// 文字或行数/字号变化后重画当前这句。
  function refreshCaption() {
    renderCaption(currentText);
  }

  // ---- 逐单位显示（打字机） ---------------------------------------------
  // typewriter 动画模式：字幕按语言粒度「逐一蹦出」——
  //   * 拉丁语系（英/法/西等，空格分词）→ 一个词一个词出现
  //   * 中日韩（无空格分词）→ 一个字一个字蹦出来
  // 由字幕文本内容判定：含 CJK（汉字/假名/谚文）逐字，否则逐词。这也与
  // config 的 target_lang 天然一致（日/韩/中目标 → 文本必是 CJK）。
  //
  // 与流式 partial 协同：partial 追加时只是把目标文本变长，打字游标不动，
  // 继续把新增内容逐单位打出来；整句（mock / final / 重连快照）一次到达时
  // 则自动从当前显示处往下打，观感接近实时。

  const TYPE_WORD_MS = 55;  // 拉丁语：一个词间隔（ms）
  const TYPE_CHAR_MS = 32;  // CJK：一个字间隔（ms）
  // 实时流领先打字游标超过这么多「单位」时，直接整段追上显示，
  // 防止模型一次性吐出大段 partial 时字幕越来越落后于说话进度。
  const TYPE_CATCHUP_UNITS = 8;
  let typeTimer = null;
  let typeMode = "word";
  let typeTarget = "";      // 期望显示的整段
  let typePos = 0;          // 已打到的字符下标

  function isTypewriterMode() {
    return document.body.classList.contains("animation-typewriter");
  }

  function setUnitMode(text) {
    typeMode = /[\u3040-\u30ff\uac00-\ud7af\u4e00-\u9fff]/.test(text)
      ? "char"
      : "word";
  }

  /// 返回从 from 起一个单位的结束下标（char 一个码点；word 一段连续同类）。
  function nextUnitEnd(text, from) {
    if (from >= text.length) return text.length;
    if (typeMode === "char") {
      const cp = text.codePointAt(from);
      return from + (cp > 0xffff ? 2 : 1);
    }
    let i = from;
    const ws = /\s/.test(text[i]);
    while (i < text.length && /\s/.test(text[i]) === ws) i++;
    return i;
  }

  function stopTyping() {
    if (typeTimer) { clearTimeout(typeTimer); typeTimer = null; }
  }

  /// 隐藏 / 换字幕时重置打字机。
  function resetTyping() {
    stopTyping();
    typeTarget = "";
    typePos = 0;
  }

  function typeTick() {
    typeTimer = null;
    if (typePos >= typeTarget.length) return; // 打完了
    // 落后太多（实时流一次推来一大段）→ 直接追上，保证低延迟。
    if (typeTarget.length - typePos > TYPE_CATCHUP_UNITS) {
      typePos = typeTarget.length;
    } else {
      typePos = nextUnitEnd(typeTarget, typePos);
    }
    textEl.textContent = typeTarget.slice(0, typePos);
    if (typePos < typeTarget.length) {
      typeTimer = setTimeout(typeTick, typeMode === "word" ? TYPE_WORD_MS : TYPE_CHAR_MS);
    }
  }

  /// 把应显示内容交出去：typewriter 模式走打字机，其余模式整段即时显示。
  function displayShown(shown) {
    if (!textEl) return;
    if (!isTypewriterMode()) {
      stopTyping();
      if (textEl.textContent !== shown) textEl.textContent = shown;
      return;
    }
    // ponytail: 非追加修订、新句和窗口前移一次性替换；只让真正追加的部分逐字出现。
    // 这样不会在每次更正时先清空，再闪出一个字和突然变窄的背景。
    if (!typeTarget || !shown.startsWith(typeTarget)) {
      stopTyping();
      setUnitMode(shown);
      typeTarget = shown;
      typePos = shown.length;
      textEl.textContent = shown;
      return;
    }
    typeTarget = shown;
    if (typePos > typeTarget.length) typePos = typeTarget.length;
    if (typePos < typeTarget.length) {
      if (!typeTimer) {
        typeTimer = setTimeout(typeTick, typeMode === "word" ? TYPE_WORD_MS : TYPE_CHAR_MS);
      }
    } else {
      textEl.textContent = typeTarget;
    }
  }

  // ---- 性能模式 ---------------------------------------------------------
  // 与 admin 面板共用同一个 localStorage 开关（同源）。集显直播时关掉
  // backdrop-filter —— 它需要每帧对背景重新采样合成，是最吃 GPU 的一项。

  const PERF_MODE_KEY = "slt.perfMode";

  function applyPerfMode(on) {
    document.body.classList.toggle("perf-mode", !!on);
  }

  function initPerfMode() {
    try {
      applyPerfMode(localStorage.getItem(PERF_MODE_KEY) === "1");
    } catch {
      // localStorage 不可用时保持默认的玻璃效果。
    }
    // admin 那边一改，这里立刻跟着变，不必刷新 OBS 浏览器源。
    window.addEventListener("storage", (ev) => {
      if (ev.key === PERF_MODE_KEY) applyPerfMode(ev.newValue === "1");
    });
  }

  function setBodyVariant(prefix, value) {
    if (!value) return;
    document.body.className = document.body.className.replace(
      new RegExp(prefix + "-\\w+", "g"),
      ""
    );
    document.body.classList.add(prefix + "-" + value);
  }

  function applyOverlayConfig(ov) {
    if (!ov) return;
    if (ov.font_size) style.size = ov.font_size;
    if (ov.font_color) style.color = ov.font_color;
    if (ov.background_color) style.bgColor = ov.background_color;
    // 老配置只有 0–1 的 background_opacity，这里兼容一下。
    if (ov.bg_opacity !== undefined && ov.bg_opacity !== null) {
      style.bgOpacity = ov.bg_opacity;
    } else if (ov.background_opacity !== undefined && ov.background_opacity !== null) {
      style.bgOpacity = ov.background_opacity * 100;
    }
    if (ov.bg_width !== undefined) style.width = Number(ov.bg_width) || 0;
    if (ov.bg_height !== undefined) style.height = Number(ov.bg_height) || 0;
    if (ov.border_radius !== undefined) style.radius = Number(ov.border_radius) || 0;
    if (ov.max_lines !== undefined) style.maxLines = Number(ov.max_lines) || 2;
    if (ov.display_delay_ms !== undefined) {
      displayDelayMs = clampDisplayDelayMs(ov.display_delay_ms);
    }
    if (ov.clear_after_ms !== undefined) {
      const ms = Number(ov.clear_after_ms);
      // 1–15 秒：短到不至于残留旧字幕，长到足够读完一句话。
      if (isFinite(ms) && ms > 0) clearAfterMs = Math.min(15000, Math.max(1000, ms));
    }
    renderStyle();
    setBodyVariant("position", ov.position);
    setBodyVariant("animation", ov.animation);
    setBodyVariant("layout", ov.layout);
  }

  function parseHash() {
    const hash = location.hash.replace(/^#/, "");
    if (!hash) return;
    const params = new URLSearchParams(hash);
    const get = (k) => {
      const v = params.get(k);
      return v === null || v === "" ? null : v;
    };

    if (get("size") !== null) style.size = Number(get("size")) || null;
    if (get("color") !== null) style.color = decodeURIComponent(get("color"));
    if (get("bg") !== null) style.bgColor = decodeURIComponent(get("bg"));
    if (get("bgOpacity") !== null) style.bgOpacity = Number(get("bgOpacity"));
    if (get("bgWidth") !== null) style.width = Number(get("bgWidth")) || 0;
    if (get("bgHeight") !== null) style.height = Number(get("bgHeight")) || 0;
    if (get("radius") !== null) style.radius = Number(get("radius")) || 0;
    if (get("maxLines") !== null) style.maxLines = Number(get("maxLines")) || 2;
    renderStyle();

    setBodyVariant("position", get("position"));
    setBodyVariant("animation", get("animation"));
    setBodyVariant("layout", get("layout"));
  }

  // ---- rendering ---------------------------------------------------------

  /// 折叠模型可能吐出的换行与多余空白。是否折行由 CSS 依据宽度决定，
  /// 所以这里只需保证没有「硬换行」把背景撑成三行。
  function toSingleLine(text) {
    return String(text == null ? "" : text)
      .replace(/[\r\n]+/g, " ")
      .replace(/\s+/g, " ")
      .trim();
  }

  function renderShow(text) {
    // Deliberately does NOT touch `hideTimer`. The silence timer is armed when
    // a subtitle event arrives (scheduleHide), and a buffered first render used
    // to cancel it here — so the page that reached the screen through the
    // display buffer was never cleared and stayed up until the next event.
    // Every caller re-arms the timer with scheduleHide() right after this
    // returns, so letting the arrival-time deadline stand is both correct and
    // what the buffer contract promises.
    const line = toSingleLine(text);
    currentText = line;
    captionEl.classList.remove("empty");
    captionEl.classList.add("show");
    // 病态输入兜底；超出的部分由 renderCaption 换到下一句显示。
    renderCaption(line.length > 1000 ? line.slice(0, 1000) + "…" : line);
  }

  function show(text) {
    // Buffer only the first render of a page. Later partials replace the
    // queued text without resetting its deadline, so a fast stream cannot
    // postpone the page indefinitely and visible text stays live.
    if (!currentText && displayDelayMs > 0) {
      pendingText = text;
      if (!displayTimer) {
        displayTimer = setTimeout(() => {
          displayTimer = null;
          const queued = pendingText;
          pendingText = null;
          if (queued !== null) renderShow(queued);
        }, displayDelayMs);
      }
      return;
    }
    renderShow(text);
  }

  function hide() {
    if (displayTimer) { clearTimeout(displayTimer); displayTimer = null; }
    pendingText = null;
    // A completed silence period starts a new page. Keeping stale text here
    // would bypass the next page's display buffer and let resize re-render it.
    currentText = "";
    // The accumulated sentence ends with the page. Leaving it behind glued the
    // NEXT sentence onto the old tail (`<old><new>`), because both
    // appendPartial and the `grew` check in replacePartial treat an empty
    // buffer as "this is a fresh sentence".
    partialBuffer = "";
    captionEl.classList.add("empty");
    captionEl.classList.remove("show");
    pageStart = 0;
    resetTyping();
    if (textEl) textEl.textContent = "";
  }

  function scheduleHide(extraMs) {
    if (hideTimer) clearTimeout(hideTimer);
    hideTimer = setTimeout(() => {
      hide();
      recentSentences.length = 0;
    }, clearAfterMs + (extraMs || 0));
  }

  function appendPartial(delta) {
    if (!delta) return;
    // 上一句已经收尾（finalize 会清空 buffer），这是新的一句，从头开始显示。
    if (partialBuffer.length === 0) pageStart = 0;
    partialBuffer += delta;
    // 不做任何过滤，直接渲染：cleanText 的水印词表是针对整句设计的，
    // 套在流式片段上会误伤（delta 恰好是 "live" / "AI" 就被整段丢掉），
    // 既吞内容又让人误以为字幕要等说完才出。水印只在 finalize 时清理。
    show(partialBuffer);
    scheduleHide();
  }

  /// 两段修订的公共前缀 / 公共后缀长度。
  function commonPrefixLength(a, b) {
    const n = Math.min(a.length, b.length);
    let i = 0;
    while (i < n && a.charCodeAt(i) === b.charCodeAt(i)) i++;
    return i;
  }

  function commonSuffixLength(a, b) {
    const n = Math.min(a.length, b.length);
    let i = 0;
    while (i < n && a.charCodeAt(a.length - 1 - i) === b.charCodeAt(b.length - 1 - i)) i++;
    return i;
  }

  /// `replace: true` 的帧是服务商对**尚未结束的那一句**的累计修订（百炼
  /// Fun-ASR：src/llm.rs 里 `sentence_end: true` 才发 Final，其间的 Partial 都是
  /// 同一句的累计修订），所以一句之内的每一帧都属于同一句 —— 句子的边界是
  /// `final`（`finalize()` 会清空 partialBuffer），不是某一帧的形状。
  ///
  /// 旧判定要求严格前缀兼容，于是真实的二次解码（第 N+1 帧既不是第 N 帧的前缀
  /// 也不是它的延长：尾字同音修正、句子中间补一个词）会被判成"新的一句"并把翻页
  /// 光标重置为 0。长句字幕于是退回第一页、下一帧又跳回第二页 —— 就是用户报的
  /// 「第一段字幕和第二段字幕交替闪烁」。
  ///
  /// 现在只把"明显是新的一句"当作新的一句：帧长还不到上一帧的一半（新句子总是
  /// 从小片段重新长起来），或者与上一帧几乎不共享任何文本。真正的新一句仍然从
  /// 第一页开始；修订导致光标越界时由 pickShown() 重新定位到最后一页，不会回到
  /// 第一页。
  function sameOpenSentence(prev, next) {
    if (!prev || !next) return false;
    if (next.startsWith(prev) || prev.startsWith(next)) return true;
    if (next.length * 2 < prev.length) return false; // 重新从小片段长起来 = 新的一句
    const shorter = Math.min(prev.length, next.length);
    const shared = commonPrefixLength(prev, next) + commonSuffixLength(prev, next);
    // 短帧只要沾一点边就算同一句；长帧要求共享至少四分之一，避免把完全无关的
    // 同长度文本误判为同一句。
    return shorter < 8 ? shared > 0 : shared * 4 >= shorter;
  }

  function replacePartial(text) {
    // Bailian partial results are cumulative revisions, not deltas.
    const next = text || "";
    if (!sameOpenSentence(partialBuffer, next)) pageStart = 0;
    partialBuffer = next;
    show(partialBuffer);
    scheduleHide();
  }

  function finalize(text) {
    if (text) partialBuffer = text;
    const cleaned = cleanText(partialBuffer);
    partialBuffer = "";
    if (cleaned && !isDuplicate(cleaned)) {
      show(cleaned);
      scheduleHide();
    } else {
      if (hideTimer) { clearTimeout(hideTimer); hideTimer = null; }
      hide();
    }
  }

  function clearAll() {
    partialBuffer = "";
    currentText = "";
    if (hideTimer) { clearTimeout(hideTimer); hideTimer = null; }
    hide();
    recentSentences.length = 0;
  }

  // ---- transport ---------------------------------------------------------

  /// 访问令牌（P0-01）。
  ///
  /// 服务端给管理接口和字幕 WebSocket 都加了鉴权，overlay 用的是**只读令牌**：
  /// 它能读字幕和样式，但改配置 / 停管线 / 读录像都会被拒。令牌有三个来源，
  /// 按优先级取第一个可用的：
  ///   1. `window.__SLT_TOKEN__` —— 服务端渲染 /overlay 时注入的（推荐路径，
  ///      令牌不会出现在 URL 里，因此不进浏览器历史、Referer 和访问日志）；
  ///   2. URL 的 `?token=` —— OBS 浏览器源直接填的地址，服务端也会注入同一
  ///      个值，这里作为兜底；
  ///   3. 空 —— 旧版服务端或手工打开的文件，此时请求会被拒绝，而不是拿到别
  ///      人的数据。
  function accessToken() {
    const injected = typeof window !== "undefined" && typeof window.__SLT_TOKEN__ === "string"
      ? window.__SLT_TOKEN__
      : "";
    if (injected) return injected;
    try {
      return new URLSearchParams(location.search).get("token") || "";
    } catch {
      return "";
    }
  }

  /// 所有 API 请求都带上令牌。用请求头而不是查询参数，避免令牌进入日志。
  function authHeaders(extra) {
    const token = accessToken();
    return token ? Object.assign({ "x-slt-token": token }, extra || {}) : (extra || {});
  }

  /// admin 保存配置后由服务端 /api/config 广播推来，overlay 立即重刷样式。
  async function loadConfig() {
    try {
      const r = await fetch("/api/config", { cache: "no-store", headers: authHeaders() });
      const cfg = await r.json();
      if (cfg.overlay) applyOverlayConfig(cfg.overlay);
    } catch (e) {
      console.warn("overlay: failed to load config from API, using defaults", e);
    }
  }

  function stopWsTimers() {
    if (wsRetryTimer) { clearTimeout(wsRetryTimer); wsRetryTimer = null; }
    if (wsPingTimer) { clearInterval(wsPingTimer); wsPingTimer = null; }
  }

  /// 重连排队。退避从 500 ms 起翻倍到 15 s，并加最多 40% 抖动，避免服务端
  /// 重启后多个浏览器源在同一毫秒一起撞上来。
  function scheduleReconnect() {
    if (wsRetryTimer) return;
    const jitter = Math.random() * wsRetryDelay * 0.4;
    const wait = Math.round(wsRetryDelay + jitter);
    console.warn(`overlay ws reconnecting in ${wait} ms`);
    wsRetryTimer = setTimeout(() => {
      wsRetryTimer = null;
      connectWS();
    }, wait);
    wsRetryDelay = Math.min(WS_RETRY_MAX_MS, wsRetryDelay * 2);
  }

  function connectWS() {
    stopWsTimers();
    const generation = ++wsGeneration;
    const wsScheme = location.protocol === "https:" ? "wss" : "ws";
    // 令牌走查询参数：浏览器的 WebSocket 构造函数不能自定义请求头。
    // 这也是服务端把令牌注入页面的原因之一——同一个令牌两处都用，用户不必
    // 手工拼接 URL。
    const token = accessToken();
    const url = `${wsScheme}://${location.host}/ws/subtitles` +
      (token ? `?token=${encodeURIComponent(token)}` : "");
    try {
      ws = new WebSocket(url);
    } catch (e) {
      console.warn("overlay ws construction failed", e);
      scheduleReconnect();
      return;
    }
    wsLastFrameAt = Date.now();
    ws.addEventListener("open", () => {
      console.log("overlay ws connected");
      wsRetryDelay = WS_RETRY_MIN_MS;
      wsLastFrameAt = Date.now();
      // 兜底：即使服务端是旧版（不会主动推 config），重连后也能拿到最新样式。
      loadConfig();
      // 半开检测：连上了但长时间收不到任何帧时，TCP 可能已经断了而浏览器
      // 还没触发 close。定时发 ping 并检查最后收帧时间，必要时主动重连。
      wsPingTimer = setInterval(() => {
        if (generation !== wsGeneration) return;
        if (Date.now() - wsLastFrameAt > WS_STALE_MS) {
          console.warn("overlay ws stale, forcing reconnect");
          try { ws.close(); } catch {}
          scheduleReconnect();
          return;
        }
        if (ws && ws.readyState === WebSocket.OPEN) {
          try { ws.send(WS_PING_TEXT); } catch {}
        }
      }, WS_PING_MS);
    });
    ws.addEventListener("message", (ev) => {
      wsLastFrameAt = Date.now();
      // Liveness pong from the server: it is proof of life but carries no
      // subtitle payload, so it must not be parsed as one.
      if (ev.data === "pong") return;
      let payload;
      try { payload = JSON.parse(ev.data); } catch { return; }
      if (payload.type === "config") {
        if (payload.overlay) applyOverlayConfig(payload.overlay);
      } else if (payload.type === "current" && payload.line) {
        const cleaned = cleanText(payload.line.text || "");
        if (cleaned && !isDuplicate(cleaned)) {
          show(cleaned);
          scheduleHide();
        }
      } else if (payload.type === "partial") {
        if (payload.replace === true) replacePartial(payload.text || "");
        else appendPartial(payload.text || "");
      } else if (payload.type === "final") {
        finalize(payload.text || "");
      } else if (payload.type === "cleared") {
        clearAll();
      }
    });
    ws.addEventListener("close", () => {
      if (generation !== wsGeneration) return;
      if (wsPingTimer) { clearInterval(wsPingTimer); wsPingTimer = null; }
      scheduleReconnect();
    });
    ws.addEventListener("error", () => {
      if (generation !== wsGeneration) return;
      console.warn("overlay ws error");
    });
  }

  // A browser-only inspection path for layout and timing regressions.  It
  // never sends audio, does not alter the server's subtitle history, and is
  // opt-in so normal OBS browser sources are unchanged.
  //
  // 事件顺序刻意做成一次完整回归：
  //   0.0 s  替换语义 partial 修订（累计修订不应被重复追加）
  //   0.5 s  短 Final 覆盖更长的 partial
  //   1.0 s  两行分页样本（约 100 字，两行装不下 → 必须换页且不出现第三行）
  //   1.5 s  同一句的 Final 收尾
  //   2.2 s  超长句（146 字 → 实测换页 2 次），确认翻过的内容不会整段重现
  //   约 6.2 s（最后一条事件 + 4 s 配置值）自动清屏；默认 clear_after_ms
  //          被改动时清屏时刻会随之后移。
  //
  // 浏览器源宽度会影响换页次数：上面两条样本在 1600 px 上限、48 px 字号下
  // （约 32 字/行）分别需要 2 页和 3 页，最宽时也不会出现第三行。
  const REPLAY_PAGED =
    "这是一段用于验证两行分页的本地模拟字幕，它足够长，应该在当前页面填满后切换到后续文字页面，而不产生第三行或滚动。";
  const REPLAY_VERY_LONG =
    "直播字幕遇到特别长的句子时必须持续换页而不能依赖滚动，所以这一段刻意写得比两行长得多：它要确认翻过去的内容不会在下一帧整段重新出现，也要确认全程没有任何一帧同时显示三行，并且换页之后仍能一直读到句尾。" +
    "如果句子还能更长，分页逻辑也应当继续推进到最后一页，而不是把尾巴丢掉或者让背景高度撑成三行。";

  function startLocalReplayIfRequested() {
    if (new URLSearchParams(location.search).get("local-replay") !== "1") return;
    document.body.dataset.localReplay = "running";
    const events = [
      [0, () => replacePartial("这是正在修订的一句字幕，先显示较长的识别结果。")],
      [500, () => finalize("这是修订后的字幕。")],
      [1000, () => replacePartial(REPLAY_PAGED)],
      [1500, () => finalize(REPLAY_PAGED)],
      [2200, () => replacePartial(REPLAY_VERY_LONG)],
      [6200, () => { document.body.dataset.localReplay = "complete"; }],
    ];
    for (const [delay, run] of events) setTimeout(run, delay);
  }

  async function init() {
    initPerfMode();
    // 顺序很关键：先 hash（旧版留下的 URL 参数）再 /api/config，
    // 让服务端配置成为唯一权威来源 —— 这样 admin 改动即时生效，
    // 用户不必重新复制 OBS 浏览器源 URL。
    parseHash();
    await loadConfig();
    connectWS();
    startLocalReplayIfRequested();
    // 字体加载完成会改变行高，重新切一次当前这句。
    if (document.fonts && document.fonts.ready) {
      document.fonts.ready.then(refreshCaption);
    }
    window.addEventListener("resize", refreshCaption);
  }

  init();
})();
