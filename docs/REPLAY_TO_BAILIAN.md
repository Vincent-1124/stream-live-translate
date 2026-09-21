# 授权录播到百炼的最小回放链路

仅在用户明确授权指定录播片段发送到百炼后使用。本链路不读取、打印或传递 Key；引擎从它自己本地配置中读取已保存的 Key。

启动隔离引擎时传入临时 `--ingest-port 8798`。该参数只覆盖当前进程内存，不写回配置文件，也不会改动 OBS：

```powershell
stream-live-translate.exe --config <isolated-config> --host 127.0.0.1 --port 8797 --audio-mode obs_filter --ingest-port 8798 --headless
```

随后用 `scripts/send-replay-pcm.ps1` 指定录播、起点和短时长。脚本经 127.0.0.1:8798 发送实时节奏的 16 kHz 单声道 PCM，协议与 OBS filter ingest 一致；它只连接本机引擎，实际外发音频仅由该引擎的已配置百炼 provider 完成。

第一轮应限于 04:54 或 29:54 的 11 秒窗口。每次记录录播哈希、起点、时长、模型、日期、字幕导出和错误；旧字幕只用于展示节奏对照，准确率需要人工听辨文本。
