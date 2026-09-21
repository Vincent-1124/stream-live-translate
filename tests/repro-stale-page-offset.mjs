import { loadOverlay } from "./overlay-harness.mjs";

const TEXT =
  "直播字幕遇到特别长的句子时必须持续换页而不能依赖滚动，所以这一段刻意写得比两行长得多：" +
  "它要确认翻过去的内容不会在下一帧整段重新出现，也要确认全程没有任何一帧同时显示三行，" +
  "并且换页之后仍能一直读到句尾，如果句子还能更长分页逻辑也应当继续推进到最后一页。";

const ov = await loadOverlay({
  transform: (src) =>
    src.replace(
      "  init();\n})();",
      "  init();\n  globalThis.__probe = function () { return { pageStart: pageStart, partialBuffer: partialBuffer, currentText: currentText }; };\n})();"
    ),
});

ov.emitPartial(TEXT.slice(0, 1), true);
ov.tick(750);
for (let len = 2; len <= TEXT.length; len++) {
  const prevBuffer = ov.sandbox.__probe().partialBuffer;
  const text = TEXT.slice(0, len);
  const grew = text.startsWith(prevBuffer) && prevBuffer.length > 0;
  ov.emitPartial(text, true);
  const st = ov.sandbox.__probe();
  const dom = ov.text;
  const inRange = st.pageStart + dom.length <= text.length;
  if (!inRange || dom === "") {
    console.log(
      `prefix ${len}: pageStart=${st.pageStart} domLen=${dom.length} textLen=${text.length} ` +
        `-> window [${st.pageStart}, ${st.pageStart + dom.length}) is out of range; dom=${JSON.stringify(dom)}`
    );
    console.log(
      `   prevBuffer tail=${JSON.stringify(prevBuffer.slice(-6))} newText tail=${JSON.stringify(text.slice(-6))} ` +
        `extensionFrame=${grew}`
    );
  }
}
