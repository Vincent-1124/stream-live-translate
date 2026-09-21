# 本地录播回放准备

`scripts/extract-replay-samples.ps1` 只会从用户指定的录播提取 16 kHz、单声道 PCM WAV 片段，写入 `build/replay-samples/`。它不会修改原始 MP4、启动 OBS、调用识别服务或上传任何音频。

运行前需要本机可用的 `ffmpeg`。示例：

```powershell
./scripts/extract-replay-samples.ps1 -InputPath "C:\path\to\recording.mp4"
```

脚本覆盖文档 `06-回放样本与基线证据.md` 中的两个画面回归窗口（04:54、29:54）及其余人工听辨候选窗口，并输出每个 WAV 的起点、时长和 SHA-256。输出哈希只能证明测试输入可追溯，不能证明识别准确率。

在用户明确同意将指定片段发送到百炼、并自行在本地控制台保存 Key 前，不得把这些 WAV 发送到任何云端服务。旧字幕仅可作展示节奏对照，不能当作准确率真值；准确率比较仍需要人工参考文本。
