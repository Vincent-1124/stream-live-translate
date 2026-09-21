// Admin panel logic (rewritten).
// 设计目标：
//   * 任何 JS 错误立刻可见（不再被静默吞掉）
//   * 文件导入（拖放 / 选择）支持 SRT / VTT / TXT / JSON
//   * WebSocket 状态、连接、断线、重连都有明确提示
//   * 按钮有明显反馈（点击 → 进度 → 成功 / 失败）
//   * 启动只依赖 /api/*（不再需要浏览器端 i18n，避免 key 缺失）

(function () {
  "use strict";

  const $ = (id) => document.getElementById(id);

  // ---- 错误覆盖层：所有未捕获错误都会出现在这里 ------------------------
  function showError(msg) {
    console.error("[admin]", msg);
    const overlay = $("err-overlay");
    const pre = $("err-msg");
    if (!overlay || !pre) return;
    pre.textContent = (pre.textContent ? pre.textContent + "\n\n" : "") + msg;
    overlay.hidden = false;
  }
  window.addEventListener("error", (e) => {
    showError((e.error && e.error.stack) || e.message || String(e));
  });
  window.addEventListener("unhandledrejection", (e) => {
    const r = e.reason;
    showError("未处理的 Promise 拒绝: " + ((r && r.stack) || r || "unknown"));
  });
  $("err-dismiss").addEventListener("click", () => { $("err-overlay").hidden = true; });

  // ---- Toast ------------------------------------------------------------
  let toastTimer = null;
  function toast(msg, kind) {
    const el = $("toast");
    if (!el) return;
    el.textContent = msg;
    el.className = "toast show " + (kind || "info");
    el.hidden = false;
    if (toastTimer) clearTimeout(toastTimer);
    toastTimer = setTimeout(() => { el.hidden = true; }, 3500);
  }

  // ---- Provider hints（直接内置；不再走 i18n 避免 key 缺失） -----------
  const PROVIDER_HINTS = {
    "qwen": {
      boxHtml: `<strong>💡 通义 Qwen API</strong><br />只能用 qwen3 系列<strong>语音（多模态）Realtime</strong>实时模型。同传翻译：<code>qwen3.5-livetranslate-flash-realtime</code>；实时识别：<code>qwen3-asr-flash-realtime</code> 或 <code>qwen-audio-3.0-realtime-flash</code>。`,
      modelPlaceholder: "qwen3.5-livetranslate-flash-realtime",
      modelSuggestions: [
        { value: "qwen3.5-livetranslate-flash-realtime", label: "同传翻译（推荐，多语言→目标语言）" },
        { value: "qwen3-asr-flash-realtime", label: "实时语音识别（ASR，边说边出字幕）" },
        { value: "qwen-audio-3.0-realtime-flash", label: "Qwen-Audio 3.0 实时（语音对话）" },
        { value: "qwen-audio-realtime-plus", label: "Qwen-Audio Realtime Plus（语音对话）" }
      ],
      endpointPlaceholder: "留空使用内置默认（wss://dashscope.aliyuncs.com/api-ws/v1/realtime）",
      endpointDefault: "",
      className: "qwen-hint"
    },
    "glm": {
      boxHtml: `<strong>💡 智谱 GLM-Realtime</strong><br />OpenAI 兼容实时协议，端点默认 <code>wss://open.bigmodel.cn/api/paas/v4/realtime</code>。做直播字幕请勾选下方「实时字幕模式」。`,
      modelPlaceholder: "glm-realtime",
      modelSuggestions: [
        { value: "glm-realtime", label: "GLM-Realtime（默认）" },
        { value: "glm-realtime-flash", label: "GLM-Realtime-Flash（9B，更便宜）" },
        { value: "glm-realtime-air", label: "GLM-Realtime-Air（32B）" }
      ],
      endpointPlaceholder: "wss://open.bigmodel.cn/api/paas/v4/realtime",
      endpointDefault: "wss://open.bigmodel.cn/api/paas/v4/realtime",
      className: "online-hint"
    },
    "online": {
      boxHtml: `<strong>🌐 OpenAI / 其它在线 API</strong><br />支持 OpenAI 兼容 Realtime 接口的在线服务。直播字幕同样请勾选下方「实时字幕模式」。`,
      modelPlaceholder: "gpt-realtime",
      modelSuggestions: [
        { value: "gpt-realtime", label: "OpenAI GPT-Realtime" },
        { value: "gpt-4o-realtime-preview", label: "OpenAI GPT-4o Realtime" },
        { value: "gpt-4o-mini-realtime-preview", label: "OpenAI GPT-4o-mini Realtime" }
      ],
      endpointPlaceholder: "例如：wss://api.openai.com/v1/realtime",
      endpointDefault: "wss://api.openai.com/v1/realtime",
      className: "online-hint"
    },
    "local": {
      boxHtml: `<strong>💻 本机部署 API</strong><br />连接本地运行的模型服务（如 Ollama、huggingface/speech-to-speech）。需确保服务已启动并开启 Realtime API。`,
      modelPlaceholder: "模型名称（根据你的本地部署）",
      modelSuggestions: [
        { value: "llama3.2-realtime", label: "Llama 3.2 Realtime（Ollama）" },
        { value: "qwen2.5-realtime", label: "Qwen 2.5 Realtime（Ollama）" }
      ],
      endpointPlaceholder: "例如：ws://localhost:11434/v1/realtime",
      endpointDefault: "ws://localhost:11434/v1/realtime",
      className: "local-hint"
    },
    "funasr": {
      boxHtml: `<strong>🎙️ FunASR 本地流式识别</strong><br />对接本地 FunASR 实时识别服务（SenseVoice / Fun-ASR-Nano / Paraformer 等，Docker 一键部署）。默认 <code>ws://127.0.0.1:10095</code>。自带 VAD/断句，只返回人说话内容。`,
      modelPlaceholder: "SenseVoiceSmall（服务端已加载，可留空）",
      modelSuggestions: [
        { value: "SenseVoiceSmall", label: "SenseVoiceSmall（多语言，推荐）" },
        { value: "fun-asr-nano", label: "Fun-ASR-Nano（LLM-ASR）" },
        { value: "paraformer-zh", label: "Paraformer-zh（普通话）" }
      ],
      endpointPlaceholder: "ws://127.0.0.1:10095（FunASR Docker 默认）",
      endpointDefault: "ws://127.0.0.1:10095",
      className: "online-hint"
    },
    "bailian": {
      boxHtml: `<strong>☁️ 百炼云 Fun-ASR</strong><br />使用百炼实时语音识别，默认模型 <code>fun-asr-realtime</code>。API Key 只在本机保存，页面仅显示是否已设置。` ,
      modelPlaceholder: "fun-asr-realtime",
      modelSuggestions: [
        { value: "fun-asr-realtime", label: "fun-asr-realtime（默认）" }
      ],
      endpointPlaceholder: "留空使用百炼北京默认端点",
      endpointDefault: "",
      className: "qwen-hint"
    },
    "mock": {
      boxHtml: `<strong>🧪 模拟模式</strong><br />本地模拟输出，不联网、不消耗额度，仅用于界面测试。`,
      modelPlaceholder: "mock",
      modelSuggestions: [],
      endpointPlaceholder: "无需填写",
      endpointDefault: "",
      className: "qwen-hint"
    }
  };
  const PROVIDER_TYPE_MAP = {
    "qwen": "qwen-realtime",
    "glm": "openai-realtime",
    "online": "openai-realtime",
    "local": "openai-realtime",
    "funasr": "fun-asr-realtime",
    "bailian": "bailian-fun-asr",
    "mock": "mock"
  };

  // ---- 热词（R10）--------------------------------------------------------
  // 官方约束（与 src/hotwords.rs 保持一致）：每轮上下文 ≤ 400 字符，服务端只
  // 保留最近 5 轮；词形规范：含非 ASCII 的单词 ≤ 15 字符，纯 ASCII ≤ 7 个空格片段。
  // 生效机制是**词表匹配**，所以这里写的必须是音频里会出现的原词。
  const HOTWORD_ROUND_MAX = 400;
  const HOTWORD_ROUNDS_MAX = 5;
  const HOTWORD_NON_ASCII_MAX = 15;
  const HOTWORD_ASCII_SEGMENTS_MAX = 7;

  /// 解析输入框：一行一个，也接受逗号/分号/顿号分隔；去空去重保持顺序。
  function parseHotwords(raw) {
    const out = [];
    String(raw || "")
      .split(/[\n\r,;、]+/)
      .forEach((piece) => {
        const word = piece.trim();
        if (word && out.indexOf(word) === -1) out.push(word);
      });
    return out;
  }

  /// 与服务端同一套切分：先体检，再按 400 字符/轮切，只留最近 5 轮。
  function planHotwords(words) {
    const warnings = [];
    words.forEach((word) => {
      const chars = Array.from(word).length;
      if (/^[\x00-\x7F]*$/.test(word)) {
        const segments = word.split(/\s+/).filter(Boolean).length;
        if (segments > HOTWORD_ASCII_SEGMENTS_MAX) {
          warnings.push(`「${word}」按空格切分有 ${segments} 个片段，规范上限 ${HOTWORD_ASCII_SEGMENTS_MAX} 个`);
        }
      } else if (chars > HOTWORD_NON_ASCII_MAX) {
        warnings.push(`「${word}」共 ${chars} 字符，含非 ASCII 的单词规范上限 ${HOTWORD_NON_ASCII_MAX} 个`);
      }
    });
    const rounds = [];
    let current = "";
    words.forEach((word) => {
      const candidate = current ? current + " " + word : word;
      if (Array.from(candidate).length <= HOTWORD_ROUND_MAX) {
        current = candidate;
        return;
      }
      if (current) {
        rounds.push(current);
        current = "";
      }
      const chars = Array.from(word).length;
      if (chars > HOTWORD_ROUND_MAX) {
        // 与服务端一致：单条超长的词截断后仍会下发，不会静默丢掉。
        rounds.push(Array.from(word).slice(0, HOTWORD_ROUND_MAX).join(""));
        warnings.push(`「${word}」超过每轮 ${HOTWORD_ROUND_MAX} 字符，已截断后才下发`);
      } else {
        current = word;
      }
    });
    if (current) rounds.push(current);
    const dropped = Math.max(0, rounds.length - HOTWORD_ROUNDS_MAX);
    if (dropped > 0) {
      warnings.push(`词表需要 ${rounds.length} 轮，服务端只保留最近 ${HOTWORD_ROUNDS_MAX} 轮，最早 ${dropped} 轮不会生效（请精简热词）`);
    }
    return { words, rounds: rounds.slice(dropped), dropped, warnings };
  }

  /// 编辑框实时校验：违规词给出可见警告，并显示分轮数量。
  function renderHotwordWarnings() {
    const box = $("hotword-warnings");
    if (!box) return null;
    const plan = planHotwords(parseHotwords($("hotwords").value));
    const providerBailian = $("provider-type").value === "bailian";
    const lines = plan.warnings.slice();
    if (!providerBailian) {
      lines.push("当前大模型通道不支持热词：请把「服务商」切到百炼 Fun-ASR 实时。");
    }
    if (plan.words.length && providerBailian) {
      lines.push(`共 ${plan.words.length} 个热词，将分 ${plan.rounds.length} 轮下发（每轮 ≤ ${HOTWORD_ROUND_MAX} 字符）。`);
    }
    box.textContent = lines.join("\n");
    box.hidden = lines.length === 0;
    box.classList.toggle("good", false);
    return plan;
  }

  /// 显示热词是否已生效：来自由 /api/config 与 /api/status 的 hotword_status。
  function renderHotwordStatus(hs) {
    const el = $("hotword-status-text");
    if (!el || !hs) return;
    if (!hs.count) {
      el.textContent = "热词状态：未配置热词（不携带上下文，行为与旧版本一致）";
      el.className = "hint dim";
      return;
    }
    const when = hs.last_applied_at ? new Date(hs.last_applied_at).toLocaleTimeString() : "—";
    const parts = [];
    parts.push(hs.delivered ? `已下发 ${hs.delivered_count} 个热词` : "尚未下发（等待会话启动）");
    if (hs.delivered) parts.push(`方式 ${hs.mode || "—"} / ${hs.delivered_rounds} 轮`);
    parts.push(`上次生效 ${when}`);
    parts.push(`未变化未重发 ${hs.skipped_unchanged || 0} 次`);
    if (hs.delivered_dropped_rounds) parts.push(`⚠ 因超出 5 轮被丢弃 ${hs.delivered_dropped_rounds} 轮`);
    if (hs.last_result) parts.push(hs.last_result);
    el.textContent = "热词状态：" + parts.join("　|　");
    el.className = "hint " + (hs.delivered ? "ok" : "warn");
  }

  // ---- 状态 -------------------------------------------------------------
  let currentConfig = null;
  let pendingPartial = "";
  let lastFinalText = "";
  let previewText = "";
  let previewPageStart = 0;
  let previewLines = 2;
  let ws = null;
  let wsReconnectDelay = 1500;
  let localImportedRows = []; // 用户从文件导入的历史
  let lastServerHistory = []; // 服务端的历史
  let lastInputLevel = 0;     // 最近一次 /api/status 的 VAD 前 RMS
  let statusPollTimer = null; // /api/status 轮询句柄
  let audioTestQuietMs = 3000;   // 引导试音的安静段（服务端 [audio_test] 值）
  let audioTestSpeechMs = 10000; // 引导试音的讲话段
  // 「当前生效值」的两个来源：/api/config（刚保存/刚加载）与 /api/status
  // （轮询）。后者更新，但前者带着 `_clamped` 标记，所以两者分开存、用时优先
  // 取最新的那个。
  let lastThresholdReadback = { value: null, clamped: false, loaded: false };
  let lastThresholdLive = { value: null, active: false };

  // ---- 性能模式 ---------------------------------------------------------
  // Shared with the overlay through localStorage (same origin). The overlay
  // reads this key on load and reacts to `storage` events, so toggling here
  // changes the OBS browser source without reloading it.
  const PERF_MODE_KEY = "slt.perfMode";

  function perfModeOn() {
    try {
      return localStorage.getItem(PERF_MODE_KEY) === "1";
    } catch {
      return false;
    }
  }

  /// Reflect the stored switch in the button without writing it back.
  function renderPerfModeButton() {
    const btn = $("perf-mode-btn");
    if (!btn) return;
    const on = perfModeOn();
    btn.textContent = "性能模式：" + (on ? "开" : "关");
    btn.setAttribute("aria-pressed", on ? "true" : "false");
    btn.classList.toggle("primary", on);
  }

  function applyPerfMode(on) {
    try {
      localStorage.setItem(PERF_MODE_KEY, on ? "1" : "0");
    } catch {
      toast("浏览器禁用了本地存储，无法保存性能模式", "error");
      return;
    }
    renderPerfModeButton();
    toast(on ? "性能模式已开启（字幕背景模糊已关闭）" : "性能模式已关闭", "ok");
  }

  // ---- OBS dock detection -----------------------------------------------
  if (new URLSearchParams(location.search).get("obsDock") === "1") {
    document.body.classList.add("dock");
  }

  // ---- 杂项辅助 --------------------------------------------------------
  function hexToRgba(hex, alpha) {
    if (!hex || hex[0] !== "#") return null;
    let h = hex.slice(1);
    if (h.length === 3) h = h[0] + h[0] + h[1] + h[1] + h[2] + h[2];
    if (h.length !== 6 || /[^0-9a-fA-F]/.test(h)) return null;
    return `rgba(${parseInt(h.slice(0, 2), 16)},${parseInt(h.slice(2, 4), 16)},` +
           `${parseInt(h.slice(4, 6), 16)},${alpha})`;
  }
  const PREVIEW_SCALE = 0.45;

  /// Mirrors overlay/app.js `clampDisplayDelayMs()`: 0 = no buffer (the first
  /// page is shown the instant it arrives), everything else is clamped into
  /// 500–1000 ms. A missing/garbage value keeps the 750 ms default.
  ///
  /// `Math.max(500, …)` on the raw field used to turn the new "0 秒（无缓冲）"
  /// choice into a 500 ms buffer the moment the panel was saved.
  function clampDisplayDelay(raw) {
    if (raw === null || raw === undefined || raw === "") return 750;
    const ms = Number(raw);
    if (!isFinite(ms)) return 750;
    if (ms === 0) return 0;
    return Math.min(1000, Math.max(500, ms));
  }

  /// Point a <select> at the option that best represents `value`: the exact
  /// option, otherwise the numerically closest one, otherwise `fallback`.
  ///
  /// Without this, a legal-but-unlisted value (display_delay_ms = 0 before this
  /// option existed, or a hand-edited clear_after_ms = 5000) left the select
  /// BLANK — and `parseInt("")`/`|| default` then wrote the default back on the
  /// next save, silently changing the user's configuration.
  function setSelectValue(sel, value, fallback) {
    if (!sel) return;
    const options = [...sel.options].map((o) => o.value);
    const want = String(value);
    if (options.includes(want)) {
      sel.value = want;
      return;
    }
    const target = Number(value);
    let best = null;
    if (isFinite(target)) {
      for (const option of options) {
        const n = Number(option);
        if (!isFinite(n)) continue;
        if (best === null || Math.abs(n - target) < Math.abs(Number(best) - target)) best = option;
      }
    }
    sel.value = best !== null ? best : String(fallback);
  }

  function num(id, dflt) {
    const el = $(id);
    if (!el) return dflt;
    const n = parseInt(el.value, 10);
    return isFinite(n) ? n : dflt;
  }

  // ---- Form 填充 / 收集 -------------------------------------------------
  function detectProviderType(cfg) {
    if (cfg.llm.provider === "qwen-realtime") return "qwen";
    if (cfg.llm.provider === "mock") return "mock";
    if (cfg.llm.provider === "fun-asr-realtime") return "funasr";
    if (cfg.llm.provider === "bailian-fun-asr") return "bailian";
    if (cfg.llm.provider === "openai-realtime") {
      const ep = cfg.llm.endpoint || "";
      if (ep.includes("bigmodel.cn")) return "glm";
      if (ep.includes("localhost") || ep.includes("127.0.0.1")) return "local";
      return "online";
    }
    return "online";
  }

  function fillForm(cfg) {
    const providerType = detectProviderType(cfg);
    $("provider-type").value = providerType;
    $("model").value = cfg.llm.model || "";
    // /api/config deliberately returns an empty api_key. Never put a saved
    // secret back into the DOM; the separate boolean is the only status.
    $("api_key").value = "";
    $("api-key-status").textContent = cfg.llm.api_key_set ? "✓ 已设置（不会回显）" : "未设置";
    $("api-key-status").className = "field-status " + (cfg.llm.api_key_set ? "set" : "unset");
    $("endpoint").value = cfg.llm.endpoint || "";
    $("workspace-id").value = cfg.llm.workspace_id || "";
    $("target_lang").value = cfg.llm.target_lang || "zh";
    $("translate_chinese").checked = !!cfg.llm.translate_chinese;
    $("transcribe").checked = !!cfg.llm.transcribe;
    $("transcription_model").value = cfg.llm.transcription_model || "";
    $("gateway_text").checked = !!cfg.llm.gateway_text;
    $("low_latency").checked = !!cfg.llm.segment_ms;
    $("segment_ms").value = String(cfg.llm.segment_ms || 1200);

    // Audio
    const modeSel = $("audio-mode");
    if (![...modeSel.options].some((o) => o.value === cfg.audio.mode)) {
      const opt = document.createElement("option");
      opt.value = cfg.audio.mode;
      opt.textContent = cfg.audio.mode;
      modeSel.appendChild(opt);
    }
    modeSel.value = cfg.audio.mode;
    $("use_sck").checked = !!cfg.audio.use_screen_capture_kit;
    const rms = Number(cfg.filter && cfg.filter.silence_rms);
    const rmsValue = isFinite(rms) && rms > 0 ? rms : 0.012;
    $("silence-rms").value = String(rmsValue);
    // 云端阈值：`_clamped` 由服务端给出（手改 config.toml 写越界值时），
    // 这时显示的必须是服务端真正会下发的值，并把"被钳过"告诉用户。
    const thresholdClamped = !!(cfg.llm && cfg.llm.speech_noise_threshold_clamped);
    const threshold = clampThreshold(cfg.llm ? cfg.llm.speech_noise_threshold : 0);
    $("speech-noise-threshold").value = String(threshold);
    // 预设由**两个**数值共同决定：任一被手改过就是「自定义」，避免面板显示的
    // 预设名与实际保存的值不一致。
    $("filter-preset").value = presetForValues(threshold, rmsValue);
    syncPresetUI();
    lastThresholdReadback = {
      value: threshold,
      clamped: thresholdClamped,
      loaded: true,
    };
    renderThresholdStatus();

    // OBS
    $("obs-auto").checked = !!cfg.obs.auto_connect;
    $("obs-host").value = cfg.obs.host || "127.0.0.1";
    $("obs-port").value = cfg.obs.port || 4455;
    $("obs-password").value = cfg.obs.password || "";
    $("recording-dir").value = cfg.recording_dir || "";

    // Overlay
    $("ov-size").value = cfg.overlay.font_size || 48;
    $("ov-max-lines").value = cfg.overlay.max_lines || 2;
    // 0 = 「无缓冲」 is a legal value: `|| 750` would silently turn it back into
    // a 750 ms buffer, and an unlisted value used to leave the select blank.
    setSelectValue($("ov-display-delay"), cfg.overlay.display_delay_ms ?? 750, 750);
    setSelectValue($("ov-clear-after"), cfg.overlay.clear_after_ms ?? 4000, 4000);
    $("ov-bg-width").value = cfg.overlay.bg_width || 0;
    $("ov-bg-height").value = cfg.overlay.bg_height || 0;
    $("ov-border-radius").value = cfg.overlay.border_radius || 8;
    const op = cfg.overlay.bg_opacity !== undefined ? cfg.overlay.bg_opacity : 75;
    $("ov-bg-opacity").value = op;
    $("ov-opacity-display").textContent = op + "%";
    $("ov-color").value = cfg.overlay.font_color || "#ffffff";
    $("ov-bg").value = cfg.overlay.background_color || "#000000";
    $("ov-position").value = cfg.overlay.position || "bottom";
    $("ov-animation").value = cfg.overlay.animation || "typewriter";

    $("obs-dock-url").textContent = `${location.protocol}//${location.host}/admin?obsDock=1`;

    // 热词（R10）：来自配置的多行文本 + 服务端给出的分发/生效状态。
    const hw = $("hotwords");
    if (hw) {
      const list = Array.isArray(cfg.llm.hotwords) ? cfg.llm.hotwords : [];
      // 只在该字段未被用户改动时回填，避免轮询/保存把正在编辑的内容覆盖掉。
      if (document.activeElement !== hw) hw.value = list.join("\n");
      renderHotwordWarnings();
    }
    renderHotwordStatus(cfg.hotword_status);

    updateProviderUI();
    applyPreviewStyles();
  }

  function collectPatch() {
    const providerType = $("provider-type").value;
    const provider = PROVIDER_TYPE_MAP[providerType];
    return {
      llm: {
        provider: provider,
        model: $("model").value,
        api_key: $("api_key").value,
        endpoint: $("endpoint").value.trim() || null,
        workspace_id: $("workspace-id").value.trim(),
        target_lang: $("target_lang").value,
        translate_chinese: $("translate_chinese").checked,
        segment_ms: $("low_latency").checked ? (Number($("segment_ms").value) || 1200) : 0,
        transcribe: $("transcribe").checked,
        transcription_model: $("transcription_model").value.trim(),
        gateway_text: $("gateway_text").checked,
        hotwords: parseHotwords($("hotwords") ? $("hotwords").value : ""),
      },
      audio: {
        mode: $("audio-mode").value,
        device: $("audio-device").value,
        use_screen_capture_kit: $("use_sck").checked,
      },
      filter: filterPresetPatch(),
      obs: {
        auto_connect: $("obs-auto").checked,
        host: $("obs-host").value,
        port: parseInt($("obs-port").value, 10) || 4455,
        password: $("obs-password").value,
      },
      recording_dir: $("recording-dir").value.trim(),
      overlay: {
        font_size: parseInt($("ov-size").value, 10) || 48,
        max_lines: Math.min(2, Math.max(1, parseInt($("ov-max-lines").value, 10) || 2)),
        display_delay_ms: clampDisplayDelay($("ov-display-delay").value),
        clear_after_ms: Math.min(15000, Math.max(1000, parseInt($("ov-clear-after").value, 10) || 4000)),
        bg_width: Math.max(0, parseInt($("ov-bg-width").value, 10) || 0),
        bg_height: Math.max(0, parseInt($("ov-bg-height").value, 10) || 0),
        border_radius: Math.max(0, parseInt($("ov-border-radius").value, 10) || 0),
        bg_opacity: parseInt($("ov-bg-opacity").value, 10) || 75,
        font_color: $("ov-color").value,
        background_color: $("ov-bg").value,
        position: $("ov-position").value,
        animation: $("ov-animation").value,
      },
    };
  }

  // ---- 云端噪声判定阈值 + 本地静音阈值 -----------------------------------
  //
  // 两个不同的东西，一个预设同时管它们：
  //   * `speech_noise_threshold` —— **云端**语音/噪音判定阈值，随 run-task 下发，
  //     只对 bailian-fun-asr 生效。越高越能压掉环境噪声（连带噪声幻觉字幕），
  //     代价是可能把主讲人的话判成噪声、断句更碎。会话建立后不可改，
  //     所以保存配置会重启识别会话。
  //   * `filter.silence_rms` —— **本地**静音判定，低于它的帧直接不进管线。
  //
  // 「自定义」档（或任何手改数值的动作）会把预设切到 custom，但**不改动**
  // 另一个数值：用户可能只想微调云端阈值，不想连带改本地静音阈值。
  const PRESET_VALUES = {
    soft: { threshold: 0.0, silenceRms: 0.007 },
    balanced: { threshold: 0.3, silenceRms: 0.012 },
    straight: { threshold: 0.6, silenceRms: 0.012 },
    strong: { threshold: 0.9, silenceRms: 0.012 },
  };
  const PRESET_LABELS = {
    soft: "0.0 关",
    balanced: "0.3 中等",
    straight: "0.6 较强",
    strong: "0.9 强",
    custom: "自定义",
  };
  // 快捷档位按钮：一个按钮就是一次完整的预设选择。按钮的 label 必须与它
  // 实际会写入的阈值一致，否则用户按「0.6」却看到输入框变成 0.9。
  const PRESET_BUTTONS = [
    { preset: "soft", label: "0.0 关" },
    { preset: "balanced", label: "0.3 中等" },
    { preset: "straight", label: "0.6 较强" },
    { preset: "strong", label: "0.9 强" },
  ];

  /// 钳制云端阈值到官方 [-1.0, 1.0]。空/非法值回落到 0.0（关）。
  function clampThreshold(raw) {
    if (raw === null || raw === undefined || raw === "") return 0.0;
    const n = Number(raw);
    if (!isFinite(n)) return 0.0;
    return Math.min(1, Math.max(-1, n));
  }

  /// 从两个数值反推预设名；任一不匹配就是 custom。
  function presetForValues(threshold, rms) {
    for (const [name, value] of Object.entries(PRESET_VALUES)) {
      if (Math.abs(threshold - value.threshold) < 1e-9 &&
          Math.abs(rms - value.silenceRms) < 1e-9) return name;
    }
    return "custom";
  }

  /// 应用一个预设：写 select、写输入框、同步按钮高亮与状态文案。
  function applyPreset(preset, opts) {
    const keepOther = !!(opts && opts.keepOther);
    const values = PRESET_VALUES[preset];
    if (values) {
      $("speech-noise-threshold").value = String(values.threshold);
      if (!keepOther) $("silence-rms").value = String(values.silenceRms);
    }
    $("filter-preset").value = values ? preset : "custom";
    syncPresetUI();
  }

  /// 按钮高亮 + 「自定义（高级）」行的显隐。只做展示，不写配置。
  function syncPresetUI() {
    const current = $("filter-preset").value;
    const buttons = document.querySelectorAll("#noise-preset-buttons .btn");
    buttons.forEach((btn) => {
      btn.classList.toggle("active", btn.dataset.preset === current);
    });
    const advanced = $("silence-rms-row");
    if (advanced) advanced.style.display = current === "custom" ? "" : "none";
  }

  /// 阈值的数值输入只允许 [-1, 1]；超出范围立即钳回并说明，避免"填了 5
  /// 却悄悄按 1 生效"这种不知情。空值不钳（用户正在输入时不要抢键盘）。
  function enforceThresholdRange() {
    const el = $("speech-noise-threshold");
    if (!el || el.value === "") return null;
    const typed = Number(el.value);
    if (!isFinite(typed)) return null;
    const clamped = clampThreshold(typed);
    if (clamped !== typed) {
      el.value = String(clamped);
      toast(`噪声判定阈值范围为 −1 ~ 1，已改为 ${clamped}`, "info");
    }
    return clamped;
  }

  function filterPresetPatch() {
    const preset = $("filter-preset").value;
    // 保存前再钳一次，确保送出去的 payload 一定在官方区间内。
    const threshold = clampThreshold($("speech-noise-threshold").value);
    $("speech-noise-threshold").value = String(threshold);
    // 「自定义」读输入框：手改的值原样保存，不回落到预设。
    const rmsTyped = Number($("silence-rms").value);
    const silenceRms = preset === "custom"
      ? (isFinite(rmsTyped) && rmsTyped >= 0 ? rmsTyped : 0.012)
      : PRESET_VALUES[preset].silenceRms;
    if (preset !== "custom") $("silence-rms").value = String(silenceRms);
    return { speech_noise_threshold: threshold, silence_rms: silenceRms };
  }

  /// 快捷档位按钮只建一次；点击等同于选择对应的预设。
  function renderThresholdButtons() {
    const box = $("noise-preset-buttons");
    if (!box || box.childElementCount) return;
    PRESET_BUTTONS.forEach((spec) => {
      const btn = document.createElement("button");
      btn.type = "button";
      btn.className = "btn ghost";
      btn.dataset.preset = spec.preset;
      btn.textContent = spec.label;
      box.appendChild(btn);
    });
    syncPresetUI();
  }

  /// 「当前生效值」。三件事必须同时说清楚：
  ///   1. 服务端真正会下发给云端的数值（越界时是钳过的值，不是输入框里那个）；
  ///   2. 它已经生效、还是要保存/重启后才会生效；
  ///   3. 当前通道是否用它。
  function renderThresholdStatus() {
    const el = $("noise-threshold-status");
    if (!el) return;
    const parts = [];
    let kind = "hint dim";
    // 轮询值（服务端当前配置）优先：它就是"现在生效"的那个数。
    const live = lastThresholdLive.active ? lastThresholdLive : null;
    const readback = lastThresholdReadback.loaded ? lastThresholdReadback : null;
    if (live) {
      parts.push(`当前生效值 ${live.value}`);
      kind = "hint ok";
    } else if (readback) {
      parts.push(`当前生效值 ${readback.value}`);
      kind = "hint";
    } else {
      parts.push("当前生效值：读取中…");
    }
    const shown = clampThreshold($("speech-noise-threshold").value);
    const inputEmpty = $("speech-noise-threshold").value === "";
    const sameAsActive = inputEmpty
      ? true
      : (live
        ? Math.abs(live.value - shown) < 1e-9
        : (readback ? Math.abs(readback.value - shown) < 1e-9 : true));
    if (!sameAsActive) {
      parts.push(`输入框 ${shown} 尚未生效：保存配置后会在重启识别会话时下发`);
      kind = "hint warn";
    }
    const clamped = (readback && readback.clamped) || (live && live.clamped);
    if (clamped) {
      parts.push("配置文件里的值超出 −1 ~ 1，已按边界值下发");
      kind = "hint warn";
    }
    const providerType = $("provider-type") ? $("provider-type").value : "";
    if (providerType && providerType !== "bailian") {
      parts.push("当前通道不是百炼 Fun-ASR，该阈值不会下发（仍会保存）");
      kind = "hint dim";
    }
    el.textContent = parts.join("　|　");
    el.className = kind;
    el.hidden = false;
  }

  function updateProviderUI() {
    const providerType = $("provider-type").value;
    const hint = PROVIDER_HINTS[providerType] || PROVIDER_HINTS.mock;
    const hintBox = $("provider-hint-box");
    hintBox.className = "provider-hint-box " + (hint.className || "");
    hintBox.innerHTML = hint.boxHtml;

    $("model").placeholder = hint.modelPlaceholder;
    const dl = $("model-presets");
    dl.innerHTML = "";
    (hint.modelSuggestions || []).forEach((s) => {
      const o = document.createElement("option");
      o.value = s.value;
      o.textContent = s.label;
      dl.appendChild(o);
    });

    $("endpoint").placeholder = hint.endpointPlaceholder;
    if (providerType !== "mock" && !$("endpoint").value) {
      $("endpoint").value = hint.endpointDefault || "";
    }

    const isFunasr = providerType === "funasr";
    const isBailian = providerType === "bailian";
    const openaiLike = !isFunasr && ["glm", "online", "local"].includes(providerType);
    $("transcribe_row").style.display = openaiLike ? "" : "none";
    $("transcription_model_row").style.display =
      openaiLike && $("transcribe").checked ? "" : "none";
    $("gateway_row").style.display = providerType === "local" ? "" : "none";
    $("workspace-row").style.display = isBailian ? "" : "none";
    $("low_latency_ms_row").style.display = $("low_latency").checked ? "" : "none";
    // 云端噪声判定阈值只有百炼 fun-asr 的 run-task 支持；其它通道明确提示它会
    // 被忽略（值仍保存，切回百炼即刻生效）。
    const noiseRow = $("noise-preset-row");
    if (noiseRow) noiseRow.classList.toggle("locked", !isBailian);
    const noiseLabel = $("speech-noise-threshold");
    if (noiseLabel) {
      noiseLabel.disabled = !isBailian;
      const labelRow = noiseLabel.closest("label");
      if (labelRow) labelRow.classList.toggle("locked", !isBailian);
    }
    const noiseHint = $("noise-provider-hint");
    if (noiseHint) noiseHint.hidden = isBailian;
    // 热词走百炼上下文增强，只有 bailian-fun-asr 支持；其它通道提示不支持。
    const hwCard = $("hotword-card");
    if (hwCard) hwCard.classList.toggle("locked", !isBailian);
    renderThresholdStatus();
    renderHotwordWarnings();
  }

  // ---- 预览样式 --------------------------------------------------------
  function applyPreviewStyles() {
    const el = $("preview-caption");
    const stage = $("preview-stage");
    if (!el || !stage) return;
    const size = Math.max(8, num("ov-size", 48));
    const w = Math.max(0, num("ov-bg-width", 0));
    const h = Math.max(0, num("ov-bg-height", 0));
    const radius = Math.max(0, num("ov-border-radius", 8));
    const op = Math.min(100, Math.max(0, num("ov-bg-opacity", 75)));
    const k = PREVIEW_SCALE;
    el.style.fontSize = Math.round(size * k) + "px";
    el.style.lineHeight = "1.25";
    el.style.padding = `${Math.round(10 * k)}px ${Math.round(24 * k)}px`;
    el.style.color = $("ov-color").value;
    el.style.background =
      hexToRgba($("ov-bg").value, op / 100) || `rgba(0,0,0,${op / 100})`;
    el.style.width = w > 0 ? Math.round(w * k) + "px" : "auto";
    el.style.height = h > 0 ? Math.round(h * k) + "px" : "auto";
    el.style.borderRadius = Math.round(radius * k) + "px";
    const lineEl = $("preview-line");
    if (lineEl) {
      const lines = Math.min(4, Math.max(1, num("ov-max-lines", 2)));
      lineEl.classList.toggle("single-line", lines <= 1);
      previewLines = lines;
      renderPreview();
    }
    stage.className = "preview-stage position-" + ($("ov-position").value || "bottom");
  }

  function previewMeasure(s) {
    const lineEl = $("preview-line");
    if (!lineEl) return 0;
    lineEl.textContent = s;
    return lineEl.scrollHeight;
  }
  function previewFindCut(text, start, maxH) {
    let lo = start + 1, hi = text.length, best = start + 1;
    while (lo <= hi) {
      const mid = (lo + hi) >> 1;
      if (previewMeasure(text.slice(start, mid)) <= maxH) { best = mid; lo = mid + 1; }
      else hi = mid - 1;
    }
    return best;
  }

  /// 预览分页必须和 overlay/app.js 的 `sameOpenSentence()` 同一套语义：累计修订
  /// （replace:true）是对**同一句**的修订，句子的边界是 final/cleared，不是某一帧
  /// 的形状。判定不一致时预览会在长句上反复从第一页重画 —— 也就是用户在 OBS 上
  /// 看到的那种"两段字幕交替闪烁"。
  function previewCommonPrefix(a, b) {
    const n = Math.min(a.length, b.length);
    let i = 0;
    while (i < n && a.charCodeAt(i) === b.charCodeAt(i)) i++;
    return i;
  }
  function previewCommonSuffix(a, b) {
    const n = Math.min(a.length, b.length);
    let i = 0;
    while (i < n && a.charCodeAt(a.length - 1 - i) === b.charCodeAt(b.length - 1 - i)) i++;
    return i;
  }
  function previewSameSentence(prev, next) {
    if (!prev || !next) return false;
    if (next.startsWith(prev) || prev.startsWith(next)) return true;
    if (next.length * 2 < prev.length) return false;
    const shorter = Math.min(prev.length, next.length);
    const shared = previewCommonPrefix(prev, next) + previewCommonSuffix(prev, next);
    return shorter < 8 ? shared > 0 : shared * 4 >= shorter;
  }

  /// 渲染当前这一页。光标只在"剩下的装不下"时前进（与 overlay 的 pickShown()
  /// 一致）。
  ///
  /// 旧实现在装不下时把 `previewFindCut()` 返回的**结束下标**当成起始下标用，
  /// 然后渲染 `slice(pageStart)` —— 也就是整句的尾巴：预览框只有两行高，却塞进
  /// 了四五行的文字，而且永远看不到真正会被显示的第一页。
  function renderPreview() {
    const lineEl = $("preview-line");
    if (!lineEl) return;
    if (previewLines <= 1) { lineEl.textContent = previewText; return; }
    const caption = $("preview-caption");
    const lh = parseFloat(getComputedStyle(caption).lineHeight) || 0;
    const maxH = previewLines * lh;
    if (maxH <= 0) { lineEl.textContent = previewText; return; }
    if (previewPageStart >= previewText.length) previewPageStart = 0;
    const cut = previewFindCut(previewText, previewPageStart, maxH);
    let shown;
    if (cut >= previewText.length) {
      shown = previewText.slice(previewPageStart);
    } else {
      shown = previewText.slice(previewPageStart, cut);
      if (shown) previewPageStart = cut;
      else shown = previewText.slice(0, cut) || previewText.slice(0, 1);
    }
    if (lineEl.textContent !== shown) lineEl.textContent = shown;
  }
  function setPreviewText(text, append) {
    const next = text || "";
    // A revision of the sentence on screen keeps the page cursor; anything else
    // (a new sentence, a cleared caption) starts at page 1 again.
    if (!append || !previewSameSentence(previewText, next)) previewPageStart = 0;
    previewText = next;
    renderPreview();
  }

  // ---- 历史显示 --------------------------------------------------------
  function renderHistory() {
    const cont = $("history");
    cont.innerHTML = "";
    const items = (lastServerHistory || []).slice(-30).reverse();
    const all = [...items, ...localImportedRows.slice(-30)];
    if (all.length === 0) {
      cont.innerHTML = '<div class="empty-tip">暂无字幕历史。说一句话、或导入一个 SRT/VTT 试试。</div>';
      return;
    }
    for (const line of all) {
      const row = document.createElement("div");
      row.className = "row" + (line.imported ? " imported" : "");
      const lang = document.createElement("span");
      lang.className = "lang";
      lang.textContent = line.language || (line.imported ? "导入" : "auto");
      const text = document.createElement("span");
      text.className = "text";
      text.textContent = line.text;
      row.appendChild(lang);
      row.appendChild(text);
      cont.appendChild(row);
    }
  }

  // ---- 文件导入 --------------------------------------------------------
  function parseSrtTime(s) {
    // "00:00:01,500" -> 1500
    const m = s.match(/(\d+):(\d+):(\d+)[,.](\d+)/);
    if (!m) return 0;
    return (+m[1]) * 3600000 + (+m[2]) * 60000 + (+m[3]) * 1000 + (+m[4]);
  }
  function parseVttTime(s) {
    return parseSrtTime(s.replace(".", ","));
  }
  function parseSubtitleFile(name, raw) {
    const lower = (name || "").toLowerCase();
    const text = raw.replace(/\r\n/g, "\n");
    if (lower.endsWith(".srt")) {
      const blocks = text.split(/\n\s*\n/);
      const out = [];
      for (const blk of blocks) {
        const lines = blk.split("\n").map((s) => s.trim()).filter(Boolean);
        if (lines.length < 2) continue;
        const ti = lines.findIndex((l) => l.includes("-->"));
        if (ti < 0) continue;
        const startMs = parseSrtTime(lines[ti]);
        const body = lines.slice(ti + 1).join(" ");
        if (!body) continue;
        out.push({
          id: "srt-" + out.length,
          text: body,
          language: "导入",
          started_at_ms: startMs,
          updated_at_ms: startMs,
          finalised: true,
          imported: true,
        });
      }
      return out;
    }
    if (lower.endsWith(".vtt")) {
      const blocks = text.split(/\n\s*\n/).slice(1);
      const out = [];
      for (const blk of blocks) {
        const lines = blk.split("\n").map((s) => s.trim()).filter(Boolean);
        if (lines.length === 0) continue;
        const timeLine = lines.find((l) => l.includes("-->")) || "";
        if (!timeLine) continue;
        const startMs = parseVttTime(timeLine.split("-->")[0].trim());
        const idx = lines.indexOf(timeLine);
        const body = lines.slice(idx + 1).join(" ");
        if (!body) continue;
        out.push({
          id: "vtt-" + out.length,
          text: body,
          language: "导入",
          started_at_ms: startMs,
          updated_at_ms: startMs,
          finalised: true,
          imported: true,
        });
      }
      return out;
    }
    if (lower.endsWith(".json")) {
      try {
        const j = JSON.parse(text);
        const arr = Array.isArray(j) ? j : (j.history || []);
        return arr.map((it, i) => ({
          id: "json-" + i,
          text: String(it.text || it.content || ""),
          language: it.language || "导入",
          started_at_ms: it.started_at_ms || 0,
          updated_at_ms: it.updated_at_ms || 0,
          finalised: true,
          imported: true,
        })).filter((x) => x.text);
      } catch (e) {
        throw new Error("JSON 解析失败：" + e.message);
      }
    }
    // .txt / 其它：按行当字幕
    const lines = text.split("\n").map((s) => s.trim()).filter(Boolean);
    return lines.map((line, i) => ({
      id: "txt-" + i,
      text: line,
      language: "导入",
      started_at_ms: 0,
      updated_at_ms: 0,
      finalised: true,
      imported: true,
    }));
  }

  function setupFileImport() {
    const input = $("import-file");
    if (!input) return;
    const handle = async (file) => {
      if (!file) return;
      try {
        const raw = await file.text();
        const rows = parseSubtitleFile(file.name, raw);
        if (rows.length === 0) {
          toast("没在文件里找到任何字幕行", "error");
          return;
        }
        localImportedRows = rows;
        renderHistory();
        toast(`✅ 已导入 ${rows.length} 条字幕（${file.name}）`, "ok");
        if (rows[0]) {
          setPreviewText(rows[0].text, false);
          $("preview-caption").classList.remove("empty");
        }
      } catch (e) {
        showError("导入失败：" + (e.message || e));
        toast("导入失败：" + (e.message || e), "error");
      }
    };
    input.addEventListener("change", () => {
      handle(input.files && input.files[0]);
      input.value = "";
    });

    // 拖放支持
    const card = input.closest(".card");
    if (card) {
      const stop = (e) => { e.preventDefault(); e.stopPropagation(); };
      ["dragenter", "dragover"].forEach((ev) =>
        card.addEventListener(ev, (e) => { stop(e); card.classList.add("drag-over"); }));
      ["dragleave", "drop"].forEach((ev) =>
        card.addEventListener(ev, (e) => { stop(e); card.classList.remove("drag-over"); }));
      card.addEventListener("drop", (e) => {
        const f = e.dataTransfer && e.dataTransfer.files && e.dataTransfer.files[0];
        if (f) handle(f);
      });
    }
  }

  // ---- API 调用 --------------------------------------------------------
  async function apiGet(path) {
    const r = await fetch(path, { cache: "no-store" });
    if (!r.ok) throw new Error("GET " + path + " → HTTP " + r.status);
    return r.json();
  }
  async function apiPost(path, body) {
    const r = await fetch(path, {
      method: "POST",
      headers: body ? { "content-type": "application/json" } : {},
      body: body === undefined ? null : JSON.stringify(body),
    });
    if (!r.ok) {
      let msg = "HTTP " + r.status;
      try { const j = await r.json(); if (j.error) msg = j.error; } catch {}
      throw new Error(msg);
    }
    return r.json().catch(() => ({}));
  }

  async function loadConfig() {
    const cfg = await apiGet("/api/config");
    currentConfig = cfg;
    fillForm(cfg);
    $("overlay-url-box").hidden = false;
    $("overlay-url").value = `${location.protocol}//${location.host}/overlay`;
  }

  async function loadDevices() {
    try {
      const list = await apiGet("/api/devices");
      const sel = $("audio-device");
      sel.innerHTML = "";
      const empty = document.createElement("option");
      empty.value = "";
      empty.textContent = "（默认）";
      sel.appendChild(empty);
      for (const d of list) {
        const opt = document.createElement("option");
        opt.value = d.name;
        const tag = [
          d.supports_input && "输入",
          d.supports_output && "输出"
        ].filter(Boolean).join(" / ");
        opt.textContent = `${d.name} ${tag ? `[${tag}]` : ""}`;
        sel.appendChild(opt);
      }
      if (currentConfig && currentConfig.audio.device) {
        sel.value = currentConfig.audio.device;
      }
    } catch (e) {
      console.warn("load devices failed", e);
    }
  }

  async function loadRecordingInfo() {
    try {
      const info = await apiGet("/api/recordings");
      $("recording-path").textContent = "本场记录：" + (info.jsonl_path || "不可用");
    } catch (e) {
      $("recording-path").textContent = "本场记录路径不可用：" + (e.message || e);
    }
  }

  const STATUS_POLL_MS = 1000;

  /// Live RMS readout next to the meter.  The bar is a coarse power curve; this
  /// number is the raw pre-VAD RMS of the newest captured frame, which is what
  /// microphone calibration and the silence threshold are compared against.
  function updateRmsReadout(level) {
    const el = $("input-level-rms");
    if (!el) return;
    const rmsText = level < 0.0005 ? "0.00000" : level.toFixed(5);
    const sel = $("filter-preset");
    const preset = sel ? sel.value : "";
    const presetLabel = PRESET_LABELS[preset] || "自定义";
    const threshold = num("silence-rms", 0.012);
    const margin = threshold > 0 && level > 0
      ? (level >= threshold ? "高于阈值" : "低于阈值")
      : "";
    const noise = clampThreshold($("speech-noise-threshold") ? $("speech-noise-threshold").value : 0);
    el.textContent =
      `RMS（VAD 前）${rmsText}　静音阈值 ${threshold}${margin ? "（当前" + margin + "）" : ""}　` +
      `云端噪声阈值 ${noise}　预设 ${presetLabel}`;
  }

  /// Replace the whole readout whenever the silence threshold is edited, so the
  /// number does not look stale while audio is paused.
  function refreshRmsReadout() {
    updateRmsReadout(lastInputLevel);
  }

  /// Keep the试音 button and its hint in step with the configured durations.
  /// Safe to call on every status poll: the DOM is only touched when the text
  /// actually changes, so the 1 s poll cannot cause layout churn.
  function renderAudioTestLabels() {
    const secs = (ms) => (ms / 1000).toFixed(ms % 1000 === 0 ? 0 : 1);
    const btn = $("audio-test-btn");
    if (btn) {
      const label = `开始 ${secs(audioTestQuietMs + audioTestSpeechMs)} 秒试音`;
      if (btn.textContent !== label) btn.textContent = label;
    }
    const hint = $("audio-test-hint");
    if (hint) {
      const text =
        `点击后先保持安静 ${secs(audioTestQuietMs)} 秒，` +
        `再正常讲话并包含轻声约 ${secs(audioTestSpeechMs)} 秒；` +
        `结果只用于提示，不会保存或上传音频。`;
      if (hint.textContent !== text) hint.textContent = text;
    }
  }

  async function loadStatus() {
    try {
      const s = await apiGet("/api/status");
      const set = (id, ok, warn) => {
        const el = $(id);
        if (!el) return;
        el.classList.remove("ok", "bad", "warn");
        el.classList.add(ok ? "ok" : warn ? "warn" : "bad");
      };
      set("dot-audio", !!s.audio_active, false);
      const level = Math.max(0, Math.min(1, Number(s.input_level) || 0));
      // The bar uses a mild power curve so the everyday 0.005–0.05 range stays
      // visible; the exact RMS number below it is what calibration reads.
      const percent = Math.round(Math.sqrt(level) * 100);
      const meter = $("input-level-bar");
      if (meter) {
        meter.style.width = percent + "%";
        const track = meter.parentElement;
        if (track) track.setAttribute("aria-valuenow", String(percent));
      }
      const levelText = $("input-level-text");
      if (levelText) levelText.textContent = percent < 2 ? "未检测到声音" : `${percent}%`;
      lastInputLevel = level;
      updateRmsReadout(level);
      // Guided-test timings come from the server so the button label, the
      // instruction text and the local countdown all match the real run.
      if (isFinite(Number(s.audio_test_quiet_ms))) audioTestQuietMs = Number(s.audio_test_quiet_ms);
      if (isFinite(Number(s.audio_test_speech_ms))) audioTestSpeechMs = Number(s.audio_test_speech_ms);
      renderAudioTestLabels();
      // 云端噪声判定阈值的"当前生效值"来自 /api/status（服务端已钳制）。
      const liveThreshold = Number(s.speech_noise_threshold);
      if (isFinite(liveThreshold)) {
        lastThresholdLive = {
          value: clampThreshold(liveThreshold),
          clamped: !!s.speech_noise_threshold_clamped,
          active: true,
        };
        renderThresholdStatus();
      }
      set("dot-llm", !!s.llm_connected, s.running && !s.last_error ? true : false);
      set("dot-obs", !!s.obs_connected, false);
      // 热词是否真的下发成功，只有 /api/status 知道（provider 会话开始/更新后写）。
      renderHotwordStatus(s.hotwords);
      const run = $("run-state");
      if (run) {
        if (s.running) { run.textContent = "● 管线运行中"; run.className = "run-state ok"; }
        else           { run.textContent = "● 管线未运行"; run.className = "run-state bad"; }
      }

      const errEl = $("engine-error");
      if (errEl) {
        errEl.classList.remove("good");
        if (s.last_error) {
          errEl.textContent = "❗ " + s.last_error;
          errEl.hidden = false;
        } else if (!s.obs_connected && s.obs_error) {
          errEl.textContent = "⚠️ OBS 未连接：" + s.obs_error + "（请确认 OBS 已启动，且 工具 → WebSocket 服务器设置 已开启）";
          errEl.hidden = false;
        } else if (s.running) {
          errEl.textContent = "✅ 管线运行中";
          errEl.hidden = false;
          errEl.classList.add("good");
        } else {
          errEl.hidden = true;
        }
      }
      if (s.config_path) {
        const cp = $("config-path");
        if (cp) cp.textContent = "配置文件：" + s.config_path;
      }
      const lock = $("audio-mode-lock");
      const modeSel = $("audio-mode");
      if (s.audio_mode_forced) {
        if (modeSel) modeSel.disabled = true;
        if (lock) lock.hidden = false;
      } else {
        if (modeSel) modeSel.disabled = false;
        if (lock) lock.hidden = true;
      }
    } catch (e) {
      console.warn("load status failed", e);
    }
  }

  /// Poll /api/status so the level meter and the RMS readout track the live
  /// input instead of freezing at whatever the value was when the page opened.
  /// Paused in hidden tabs, and the first request is deferred so the immediate
  /// boot-time load is not duplicated.
  function startStatusPolling() {
    if (statusPollTimer) return;
    statusPollTimer = setInterval(() => {
      if (document.hidden) return;
      loadStatus();
    }, STATUS_POLL_MS);
  }

  async function loadHistory() {
    try {
      const j = await apiGet("/api/subtitles");
      lastServerHistory = j.history || [];
      renderHistory();
    } catch (e) {
      console.warn("load history failed", e);
    }
  }

  // ---- WebSocket --------------------------------------------------------
  function setWsState(state, label) {
    const el = $("ws-state");
    if (!el) return;
    el.className = "ws-state " + state;
    el.textContent = label;
  }
  function connectWS() {
    const wsScheme = location.protocol === "https:" ? "wss" : "ws";
    const url = `${wsScheme}://${location.host}/ws/subtitles`;
    setWsState("connecting", "WS 连接中…");
    try {
      ws = new WebSocket(url);
    } catch (e) {
      setWsState("disconnected", "WS 失败");
      showError("WebSocket 创建失败: " + e);
      setTimeout(connectWS, wsReconnectDelay);
      return;
    }
    ws.addEventListener("open", () => {
      console.log("[admin] WS open", url);
      setWsState("connected", "WS 已连接");
    });
    ws.addEventListener("message", (ev) => {
      let p;
      try { p = JSON.parse(ev.data); } catch { return; }
      console.debug("[admin] WS msg", p);
      if (p.type === "current" && p.line) {
        const text = (p.line.text || "").trim();
        if (text && text !== lastFinalText) {
          pendingPartial = text;
          setPreviewText(pendingPartial, false);
          $("preview-caption").classList.remove("empty");
        }
      } else if (p.type === "partial") {
        const text = (p.text || "").trim();
        if (text) {
          if (p.replace === true) {
            // Cumulative revision (e.g. Bailian): the payload is the whole
            // still-open sentence, so appending would duplicate it. The
            // overlay already honours this flag; the preview must match.
            pendingPartial = text;
          } else {
            pendingPartial = (pendingPartial || "") + text;
          }
          setPreviewText(pendingPartial, false);
          $("preview-caption").classList.remove("empty");
        }
      } else if (p.type === "final") {
        const text = (p.text || "").trim();
        if (text) {
          pendingPartial = text;
          lastFinalText = text;
          setPreviewText(pendingPartial, false);
          $("preview-caption").classList.remove("empty");
          loadHistory();
        }
      } else if (p.type === "cleared") {
        pendingPartial = "";
        setPreviewText("", false);
        $("preview-caption").classList.add("empty");
      } else if (p.type === "config") {
        applyPreviewStyles();
      }
    });
    ws.addEventListener("close", () => {
      console.warn("[admin] WS close, retry in", wsReconnectDelay, "ms");
      setWsState("disconnected", "WS 已断开");
      setTimeout(connectWS, wsReconnectDelay);
    });
    ws.addEventListener("error", (e) => {
      console.warn("[admin] WS error", e);
      setWsState("disconnected", "WS 出错");
    });
  }

  // ---- 事件绑定 --------------------------------------------------------
  function bindEvents() {
    $("provider-type").addEventListener("change", updateProviderUI);
    $("low_latency").addEventListener("change", updateProviderUI);
    $("transcribe").addEventListener("change", updateProviderUI);
    // 热词编辑时实时校验（含 400 字符分轮与词形规范告警）。
    const hwBox = $("hotwords");
    if (hwBox) {
      let hwTimer = null;
      hwBox.addEventListener("input", () => {
        if (hwTimer) clearTimeout(hwTimer);
        hwTimer = setTimeout(renderHotwordWarnings, 150);
      });
    }

    $("model-guide-btn").addEventListener("click", () => {
      const modal = $("guide-modal");
      const body = $("guide-body");
      body.innerHTML = `
        <p>本插件只能使用能<strong>实时接收语音、并边听边返回字幕文字</strong>的<strong>语音（多模态）Realtime</strong>模型。</p>
        <p><strong>云端可用：</strong>通义 Qwen Realtime 语音（同传 / ASR / Qwen-Audio）、智谱 GLM-Realtime、OpenAI Realtime。</p>
        <p><strong>本地 / 自部署可用：</strong></p>
        <ul>
          <li><strong>FunASR 流式识别</strong>（SenseVoice / Fun-ASR-Nano / paraformer-zh）：内置通道，默认 <code>ws://127.0.0.1:10095</code>；按 FunASR 官方 runtime 文档起 Docker 即可。</li>
          <li><strong>huggingface/speech-to-speech</strong>（OpenAI Realtime 兼容网关）：<code>speech-to-speech serve --host 0.0.0.0 --stt parakeet-tdt --enable_live_transcription</code>，端点 <code>ws://&lt;主机IP&gt;:8765/v1/realtime</code>。</li>
          <li>其它 ASR（faster-whisper / whisper.cpp / Parakeet-TDT / SenseVoice）需套一个 Realtime 网关（同上）。</li>
        </ul>
        <p><strong>不可用：</strong>纯文本 / 纯视觉模型、纯语音合成（TTS）、HTTP 上传式 ASR（非实时）。</p>
        <p><strong>低延迟建议：</strong>中文直播只要中文字幕时，优先用 <strong>ASR 模型</strong>（云端 qwen3-asr-flash-realtime 或本地 FunASR）。</p>
      `;
      modal.hidden = false;
    });
    $("guide-close").addEventListener("click", () => { $("guide-modal").hidden = true; });
    $("guide-modal").addEventListener("click", (e) => {
      if (e.target === $("guide-modal")) $("guide-modal").hidden = true;
    });
    document.addEventListener("keydown", (e) => {
      if (e.key === "Escape" && !$("guide-modal").hidden) $("guide-modal").hidden = true;
    });

    $("support-btn").addEventListener("click", () => {
      toast("该入口暂未开放，敬请期待。", "info");
    });

    $("ov-bg-opacity").addEventListener("input", (e) => {
      $("ov-opacity-display").textContent = e.target.value + "%";
    });
    ["ov-size", "ov-bg-width", "ov-bg-height", "ov-border-radius",
     "ov-bg-opacity", "ov-color", "ov-bg", "ov-position", "ov-max-lines"].forEach((id) => {
      const el = $(id);
      if (el) el.addEventListener("input", applyPreviewStyles);
    });

    $("save-btn").addEventListener("click", async () => {
      const patch = collectPatch();
      const key = (patch.llm.api_key || "").trim();
      const btn = $("save-btn");
      const status = $("save-status");

      const keyAlreadySet = !!(currentConfig && currentConfig.llm && currentConfig.llm.api_key_set);
      if (patch.llm.provider !== "mock" && !key && !keyAlreadySet) {
        toast("请先填写 API Key", "error");
        status.textContent = "✗ 缺少 API Key";
        status.className = "status err";
        return;
      }
      if (/^https?:/i.test(key) || key.includes("://")) {
        toast("API Key 填成了网址！Key 是以 sk- 开头的密钥", "error");
        return;
      }
      const providerType = $("provider-type").value;
      if (providerType === "qwen" && patch.llm.endpoint && /compatible-mode|http:|https:/i.test(patch.llm.endpoint)) {
        toast("Base URL 不正确：DashScope Realtime 需要 WebSocket 地址（wss://...）", "error");
        return;
      }
      // 热词（R10）：违规词给可见警告，但不阻断保存——规范是"建议"，
      // 硬拦会让用户没法保存其它设置。用户看到 ⚠️ 后自行决定是否改。
      const hotwordPlan = renderHotwordWarnings();
      if (hotwordPlan && hotwordPlan.warnings.length) {
        toast(`⚠️ 热词有 ${hotwordPlan.warnings.length} 条规范提醒，仍会保存并下发`, "info");
      }
      if (providerType === "online" && patch.llm.endpoint && !/^wss?:/i.test(patch.llm.endpoint)) {
        toast("Base URL 必须是 WebSocket 地址（wss:// 开头）", "error");
        return;
      }
      if (providerType === "local") {
        const ep = (patch.llm.endpoint || "").trim();
        if (!ep) { toast("请填写本机部署 API 的地址", "error"); return; }
        if (!/^ws?:/i.test(ep)) { toast("本机 API 地址必须是 ws:// 开头", "error"); return; }
      }

      btn.disabled = true;
      const origText = btn.textContent;
      btn.textContent = "保存中…";
      status.textContent = "";
      status.className = "status";
      try {
        await apiPost("/api/config", patch);
        await loadConfig();
        // /api/config intentionally redacts api_key in every GET response.
        // A literal comparison here therefore turns every successful new-Key
        // save into a false failure. Validate non-secret fields and, when a
        // Key was supplied, the separate server-provided presence flag.
        const saved = currentConfig
          && currentConfig.llm.provider === patch.llm.provider
          && currentConfig.llm.model === patch.llm.model;
        const keySaved = !key || !!(currentConfig && currentConfig.llm && currentConfig.llm.api_key_set);
        if (saved && keySaved) {
          status.textContent = key ? "✓ Key 已保存，不会回显" : "✓ 已保存";
          status.className = "status ok";
          toast(key ? "✅ Key 已保存，不会回显；配置已生效" : "✅ 配置已保存并生效", "ok");
        } else {
          status.textContent = "⚠ 已保存但校验失败";
          status.className = "status err";
          toast("⚠️ 已保存，但读回内容不一致", "error");
        }
        // POST /api/config already restarts the pipeline after write-verify.
        setTimeout(loadStatus, 800);
      } catch (e) {
        status.textContent = "✗ " + (e.message || e);
        status.className = "status err";
        toast("保存失败：" + (e.message || e), "error");
      } finally {
        btn.disabled = false;
        btn.textContent = origText;
      }
    });

    $("clear-key-btn").addEventListener("click", async () => {
      const btn = $("clear-key-btn");
      const status = $("save-status");
      btn.disabled = true;
      const original = btn.textContent;
      btn.textContent = "清除中…";
      try {
        const result = await apiPost("/api/config/clear-key");
        // The server verifies the on-disk file before it drops the in-memory
        // key, so {ok:true} is the only acceptable answer here. Treating a
        // missing/negative ok as success would leave the user believing the
        // key is gone while it is still in config.toml.
        if (!result || result.ok !== true) {
          throw new Error(result && result.error ? result.error : "服务器未确认清除结果");
        }
        await loadConfig();
        const stillSet = !!(currentConfig && currentConfig.llm && currentConfig.llm.api_key_set);
        if (stillSet) {
          status.textContent = "⚠ 清除后仍显示已设置，请检查 config.toml";
          status.className = "status err";
          toast("⚠️ 清除后 Key 仍为已设置状态，请检查 config.toml", "error");
        } else {
          status.textContent = "✓ 已清除保存的 Key";
          status.className = "status ok";
          toast("已清除保存的 API Key", "ok");
        }
      } catch (e) {
        status.textContent = "✗ 清除 Key 失败：" + (e.message || e);
        status.className = "status err";
        toast("清除 Key 失败：" + (e.message || e), "error");
      } finally {
        btn.disabled = false;
        btn.textContent = original;
      }
    });

    $("connection-test-btn").addEventListener("click", async () => {
      const btn = $("connection-test-btn");
      const status = $("save-status");
      btn.disabled = true;
      const original = btn.textContent;
      btn.textContent = "测试中…";
      status.textContent = "正在验证已保存的 Key、模型与业务空间…";
      status.className = "status";
      try {
        const result = await apiPost("/api/connection-test");
        status.textContent = "✓ " + (result.message || "连接可用");
        status.className = "status ok";
        toast("连接测试通过", "ok");
      } catch (e) {
        status.textContent = "✗ " + (e.message || e);
        status.className = "status err";
        toast("连接测试失败：" + (e.message || e), "error");
      } finally {
        btn.disabled = false;
        btn.textContent = original;
      }
    });

    $("audio-test-btn").addEventListener("click", async () => {
      const btn = $("audio-test-btn");
      const status = $("save-status");
      btn.disabled = true;
      const original = btn.textContent;
      const quietMs = audioTestQuietMs;
      const speechMs = audioTestSpeechMs;
      const seconds = (ms) => (ms / 1000).toFixed(ms % 1000 === 0 ? 0 : 1);
      // The server samples the quiet window first, so drive the instruction
      // from a local countdown instead of one frozen line of text: the user
      // needs to know which phase is running right now.
      const phaseStart = Date.now();
      const renderPhase = () => {
        const elapsed = Date.now() - phaseStart;
        if (elapsed < quietMs) {
          const left = Math.ceil((quietMs - elapsed) / 1000);
          btn.textContent = `安静 ${left}s…`;
          status.textContent = `请保持安静（还有 ${left} 秒），随后 ${seconds(speechMs)} 秒正常讲话并包含轻声…`;
        } else {
          const left = Math.ceil((quietMs + speechMs - elapsed) / 1000);
          btn.textContent = `讲话 ${left}s…`;
          status.textContent = `现在请正常讲话并包含轻声（还有 ${left} 秒）…`;
        }
      };
      renderPhase();
      const phaseTimer = setInterval(renderPhase, 250);
      status.className = "status";
      try {
        const result = await apiPost("/api/audio-test");
        const quiet = Number(result.quiet_rms);
        const speech = Number(result.speech_rms);
        const numbers = isFinite(quiet) && isFinite(speech)
          ? `（背景 RMS ${quiet.toFixed(5)} / 讲话 RMS ${speech.toFixed(5)}）`
          : "";
        status.textContent = "✓ " + result.message + numbers;
        status.className = "status ok";
      } catch (e) {
        status.textContent = "✗ " + (e.message || e);
        status.className = "status err";
      } finally {
        clearInterval(phaseTimer);
        btn.disabled = false;
        btn.textContent = original;
      }
    });

    $("restart-btn").addEventListener("click", async () => {
      const btn = $("restart-btn");
      btn.disabled = true;
      const orig = btn.textContent;
      btn.textContent = "重启中…";
      try {
        await apiPost("/api/restart");
        toast("已重启管线", "ok");
        setTimeout(loadStatus, 500);
      } catch (e) {
        toast("重启失败：" + (e.message || e), "error");
      } finally {
        btn.disabled = false;
        btn.textContent = orig;
      }
    });

    // A real stop: it must survive the run loop's own restart path, otherwise
    // "停" would be undone a moment later and the button would look broken.
    $("stop-btn").addEventListener("click", async () => {
      const btn = $("stop-btn");
      btn.disabled = true;
      const orig = btn.textContent;
      btn.textContent = "停止中…";
      try {
        await apiPost("/api/stop");
        toast("已停止管线（点击「重启管线」可重新启动）", "ok");
        setTimeout(loadStatus, 300);
      } catch (e) {
        toast("停止失败：" + (e.message || e), "error");
      } finally {
        btn.disabled = false;
        btn.textContent = orig;
      }
    });

    $("clear-btn").addEventListener("click", async () => {
      try {
        await apiPost("/api/subtitles/clear");
        pendingPartial = "";
        setPreviewText("", false);
        $("preview-caption").classList.add("empty");
        toast("字幕已清空", "ok");
      } catch (e) {
        toast("清空失败：" + (e.message || e), "error");
      }
    });

    // 「清空历史」 clears the SERVER history as well as the locally imported
    // rows. It used to drop only the local rows, so the list came straight back
    // from /api/subtitles on the next refresh and the button looked broken.
    $("clear-history-btn").addEventListener("click", async () => {
      const btn = $("clear-history-btn");
      btn.disabled = true;
      const original = btn.textContent;
      btn.textContent = "清空中…";
      try {
        const result = await apiPost("/api/subtitles/history/clear");
        if (!result || result.ok !== true) {
          throw new Error((result && result.error) || "服务器未确认清空结果");
        }
        localImportedRows = [];
        lastServerHistory = [];
        renderHistory();
        toast("已清空历史（服务端与本页导入）", "ok");
      } catch (e) {
        toast("清空历史失败：" + (e.message || e), "error");
      } finally {
        btn.disabled = false;
        btn.textContent = original;
      }
    });

    // 选择一个预设 = 同时写入云端噪声判定阈值与本地静音阈值，并同步输入框，
    // 让两个控件永远不会对"将要保存什么"各说一套。
    $("filter-preset").addEventListener("change", () => {
      applyPreset($("filter-preset").value);
      refreshRmsReadout();
    });

    // 快捷档位按钮：一个按钮就是一次预设选择，所以仍会把**两个**值一起设成
    // 该档位的值（与下拉行为一致，避免两个控件给出不同答案）。
    const presetButtons = $("noise-preset-buttons");
    if (presetButtons) {
      presetButtons.addEventListener("click", (e) => {
        const btn = e.target.closest("button[data-preset]");
        if (!btn) return;
        applyPreset(btn.dataset.preset);
        refreshRmsReadout();
      });
    }

    // 「自定义」档：静音阈值可以手改；云端阈值任何档位都可手改。手改后预设
    // 会切到「自定义」，但另一个数值保持不变。
    $("silence-rms").addEventListener("input", () => {
      const typed = Number($("silence-rms").value);
      const noise = clampThreshold($("speech-noise-threshold").value);
      if (isFinite(typed)) $("filter-preset").value = presetForValues(noise, typed);
      syncPresetUI();
      refreshRmsReadout();
      renderThresholdStatus();
    });

    // 云端阈值：越界立即钳回并提示；数值变化后预设按"两个值是否都还在档位上"
    // 重新判定，并刷新「当前生效值」。
    $("speech-noise-threshold").addEventListener("input", () => {
      enforceThresholdRange();
      const noise = clampThreshold($("speech-noise-threshold").value);
      const rms = Number($("silence-rms").value);
      if (isFinite(rms)) $("filter-preset").value = presetForValues(noise, rms);
      syncPresetUI();
      refreshRmsReadout();
      renderThresholdStatus();
    });
    $("speech-noise-threshold").addEventListener("change", enforceThresholdRange);

    const perfBtn = $("perf-mode-btn");
    if (perfBtn) {
      perfBtn.addEventListener("click", () => {
        applyPerfMode(!perfModeOn());
      });
      // Another tab (or an older overlay) may flip the same switch.
      window.addEventListener("storage", (ev) => {
        if (ev.key === PERF_MODE_KEY) applyPerfMode(ev.newValue === "1");
      });
    }
  }

  // ---- 启动 ------------------------------------------------------------
  async function boot() {
    try {
      bindEvents();
      renderPerfModeButton();
      renderThresholdButtons();
      setupFileImport();
      await loadConfig();
      await loadDevices();
      await loadRecordingInfo();
      loadStatus();
      startStatusPolling();
      loadHistory();
      connectWS();
    } catch (e) {
      showError("启动失败: " + ((e && e.stack) || e));
    }
  }

  // 立即启动，不要等 DOMContentLoaded（script 在 body 末尾，DOM 已就绪）
  boot();
})();
