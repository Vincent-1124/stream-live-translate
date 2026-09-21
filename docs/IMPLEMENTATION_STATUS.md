# 直播中文字幕改版进度

## 基线

- 上游压缩包提交：`c96907d7b5d25a7973ab6f5fcddaf2d56a941b77`（源码压缩包中记录的 Git commit）。
- 本地基线提交：`18063e7`。上游采用 MIT 许可证，Rust 1.74+、MSVC CMake OBS 薄插件和内嵌管理页/overlay。
- 实际前端源目录：`admin/`、`overlay/`。`build.rs` 在构建时同步到 `dist/`，因此不要直接修改 `dist/admin` 或 `dist/overlay`。
- 原始录播和本地分析工具保留在仓库外层工作目录，未加入源码仓库。

## 已完成的离线改动

- 新增 `bailian-fun-asr` provider，独立于旧 `fun-asr-realtime` 本地 FunASR 2pass provider，默认模型为 `fun-asr-realtime`。
- 按百炼 WebSocket 协议实现 Bearer 鉴权、`run-task`、等待 `task-started`、16k 单声道 PCM 二进制帧、`finish-task`，并解析 `result-generated`、`task-failed`、`task-finished`。
- 中间结果采用替换语义，避免累计修订重复追加为字幕；overlay 已识别替换事件。
- `/api/config` 不再返回 API Key；常规空白保存保留已有 Key。
- 管理页提供明确的“清除已保存 Key”操作；写盘后读回校验成功才更新内存配置并重启管线。
- 管理页可选择百炼 provider、填写模型与业务空间，并只显示 Key 是否已设置。
- Final 结果始终覆盖当前 partial，包括较短的修订文本；对应单元测试已覆盖。
- 对没有稳定语句 ID 的 provider，紧邻的相同 Final 重传会在 500 ms 内去重；正常的后续同文句子不会被长期合并。
- 每次运行创建独立 JSONL 最终字幕记录；管理 API 可导出当前会话的 TXT/SRT，并在音频或识别连接恢复时记录 Gap。写盘失败只告警，不停止直播。
- 管理页显示本场 JSONL 记录路径，并提供本场 TXT/SRT 下载入口。
- 管理页可设置每场 JSONL 记录目录：留空时使用配置文件旁的 `recordings/`，相对路径同样基于配置文件解析；该目录在下一次引擎启动时用于新会话。
- 百炼协议解析已拆为可测试的事件处理：音频只在匹配的 `task-started` 后发送；启动/运行失败及旧任务的迟到事件可识别或忽略。离线单元测试覆盖 partial/final、失败和任务 ID 隔离。
- 管理页显示所选音源最近一帧的 VAD 前 RMS 音量；它用于选定无线麦克风和校准过滤预设，实际试音判定仍待现场设备。（2026-09-21 复核更正：该表只在页面打开、保存配置后与手动重启后刷新，没有轮询；显示的是 `sqrt(rms)` 百分比而非 RMS 数值，不能用于实时观察增益变化。**2026-09-21 16:0x 再次更正：1 秒轮询与实时 RMS 数值读数已补齐，见下文「本轮改动（2026-09-21 16:0x 前后）」第 1 条；上面这条旧结论只适用于 16:0x 之前的代码。**）
- 管理页提供 13 秒引导试音：先采集 3 秒安静背景，再采集 10 秒正常/轻声讲话，提示过低、可能削波或讲话与背景差异不足。统计仅保留内存瞬时 RMS，不保存或上传音频。（3 秒/10 秒为前后端硬编码，试音期间界面不提示当前处于哪一段。另：削波提示使用帧 RMS 最大值与 0.98 比较，实际不会触发。**2026-09-21 16:0x 更正：时长已改为 `[audio_test] quiet_ms/speech_ms` 配置项并经 `/api/status` 下发，界面已显示两阶段倒计时与实测 RMS；削波判据本身未变——`input_peak` 仍是帧 RMS 最大值（`src/pipeline.rs:444`），用 `> 0.98` 判断（`src/server.rs:522`）在真实设备上几乎不可能触发，只是该分支现在有了单元测试覆盖。**）
- 设备采集改为使用设备实际支持的原生格式，再下混为单声道并重采样到管线采样率；避免对仅支持 48 kHz 的无线接收器强制请求 16 kHz 而启动失败。离线测试验证 48 kHz 单声道输入转换为 16 kHz 时长正确。
- CPAL 异步报告音频设备错误时，管线会将音频状态标为断开、显示可见重试提示并重启；插拔无线接收器的实际恢复行为仍待现场设备验证。
- 新字幕页默认缓冲 750 ms，可选 500–1000 ms；同页后续 partial 立即更新，缓冲期间的修订只替换待显示文本且不会反复延长计时。4 秒清屏会重置当前页状态，下一页不会绕过该缓冲或因窗口重排恢复旧文字。（**2026-09-21 16:0x 更正：清屏时长已不再是 4 秒硬编码，改为可配置的 `overlay.clear_after_ms`（默认 4000，钳制 1000–15000），见下文「本轮改动」；下面的 R06 缺口条目已同步标注。**）
- 管理页可对已保存的百炼配置进行连接测试；它会验证鉴权、任务启动和正常结束，不发送实际音频。没有 Key 时仅返回可见错误，不会请求或记录 Key。
- 已用隔离的本机调试进程完成 HTTP 冒烟验证：`/api/config` 只返回空 Key 与 `api_key_set=false`，保存 mock 配置后连接测试成功，录制信息与空 SRT 导出可读；没有 OBS 音源时试音 API 明确返回“音频输入未运行”，未将其误报为通过。

## 本轮改动（2026-09-21 16:0x 前后，逐行核对源码；**未运行、未接真实设备**）

> 核对方式：只读源码 + 文件哈希 + `node --check`，未运行 `cargo`、未调用云端、未打开浏览器或 OBS。
> 下列行号为 **2026-09-21 16:07 前后**实测。主 agent 声明在该时点之后仍会编辑 `src/pipeline.rs`、`src/main.rs`、`src/server.rs`、`overlay/app.js`，行号可能继续漂移；任何验收都必须以重新构建后的树为准。
> **本节全部是代码级结论，不能当作"已验证"**：没有真实音频设备、没有真实百炼调用、没有在 OBS 里渲染过。`target/release` 已于 16:07:32 重新构建（见文末「仍待验证」），但那次构建没有日志与 commit 绑定，也**没有针对新二进制的任何运行证据**。

### 管理页 `admin/`

- `/api/status` 新增 1 秒轮询：`STATUS_POLL_MS = 1000`（`admin/app.js:679`）、`startStatusPolling()`（`admin/app.js:794-800`），在 `boot()` 中启动（`admin/app.js:1181`）。隐藏标签页暂停轮询（`admin/app.js:797` 的 `if (document.hidden) return;`）。因此页面打开后不再只刷新一次。
- 新增实时 RMS 数值读数 `#input-level-rms`（`admin/index.html:136`）：显示 VAD 前原始 RMS（5 位小数，< 0.0005 显示 0.00000，`admin/app.js:687`）、当前 `silence_rms` 阈值与预设名，以及"当前高于/低于阈值"（`admin/app.js:684-697`），供现场校准。
- 新增可编辑的静音阈值输入 `#silence-rms`（`admin/index.html:150-154`）；过滤预设新增 `custom`「自定义（使用下方静音阈值）」（`admin/index.html:146`）。预设→数值（`admin/app.js:1143-1149`）与数值→预设（`admin/app.js:1153-1157`，匹配逻辑 `presetForRms` 在 `admin/app.js:336-341`）双向联动；保存时 `custom` 直接采用输入值而不是回落到预设（`admin/app.js:343-355`）。
- 修复 WS 预览忽略 `replace` 标记的缺陷：`replace === true` 时整体替换 `pendingPartial`，不再追加（`admin/app.js:849-856`）。百炼的累计修订因此不会在预览里叠成重复文字。
- 「清除已保存 Key」现在校验服务端返回：非 `ok: true` 直接抛错（`admin/app.js:1008-1015`），随后重新 `loadConfig()` 并确认 `api_key_set === false` 才报成功；若仍为已设置则提示检查 `config.toml`（`admin/app.js:1016-1026`）。
- 新增「性能模式」开关按钮 `#perf-mode-btn`（`admin/index.html:190-191`），写入 `localStorage["slt.perfMode"]`（`admin/app.js:155-184`；事件绑定 `admin/app.js:1159-1168`）。overlay 侧一直会读取该键（`overlay/app.js:319-335`）并由 `overlay/style.css:122-124` 关闭 `backdrop-filter`，此前没有任何写入入口。
- 试音按钮文案/提示/倒计时改为读取服务端 `/api/status` 的 `audio_test_quiet_ms`/`audio_test_speech_ms`（`admin/app.js:742-746`，渲染 `admin/app.js:706-717`）；试音过程中按本地时钟显示「安静 Ns…」→「讲话 Ns…」两阶段倒计时（`admin/app.js:1065-1085`），结束后显示背景/讲话 RMS 实测值（`admin/app.js:1089-1094`）。

### overlay `overlay/`

- 清屏时长不再硬编码：`clearAfterMs` 默认 4000，从 `overlay.clear_after_ms` 读取并二次钳制到 1000–15000（`overlay/app.js:25-26`、`364-368`），实际生效点在 `scheduleHide()`（`overlay/app.js:452-457`）。
- WebSocket 重连改为指数退避 + 抖动：500 ms 起、每次翻倍、上限 15 s、最多 40% 抖动（`overlay/app.js:31-35`、`521-531`）。新增半开检测：每 10 s ping 一次，超过 30 s 收不到任何帧就主动关闭并重连（`overlay/app.js:34-35`、`554-565`）。
- 删除只写不读的死变量 `lastPartialAt`：现行 `overlay/app.js` 中已无该标识符（旧死副本 `src/overlay/app.js:23,409` 仍有）。
- 本地回放样本加长：新增 `REPLAY_PAGED`（约 59 字）与 `REPLAY_VERY_LONG`（约 146 字）两个样本（`overlay/app.js:617-620`），事件表在 `overlay/app.js:626-633`。加长的目的是让两行分页真的被执行——按 10 号文档 4.13 的结论，旧样本在 1600 px/48 px 下只需两行，分页逻辑一次都没跑到。
- **未验证**：分页的真实观感、退避重连在真实断网/服务端重启下的表现、OBS 浏览器源里的实际刷新节奏，全部没有实机验证。

### Rust 后端与配置

- `src/config.rs` 新增 `overlay.clear_after_ms`：默认 4000（`default_clear_after_ms`，`src/config.rs:122-124`），字段带 `#[serde(default = "default_clear_after_ms")]`（`src/config.rs:278-279`）；`clamp_clear_after_ms` 钳制到 1000–15000（常量 `src/config.rs:128-129`，函数 `src/config.rs:133-137`）。
- `src/config.rs` 新增 `[audio_test] quiet_ms/speech_ms`：默认 3000/10000（`src/config.rs:139-145`），结构体 `AudioTestConfig`（`src/config.rs:159-176`），`Config.audio_test` 带 `#[serde(default)]` 以兼容旧配置（`src/config.rs:27-29`）；`clamp_audio_test_ms` 钳制到 1000–30000（`src/config.rs:149-156`），生效值由 `AudioTestConfig::durations()` 给出（`src/config.rs:178-186`）。
- `src/server.rs` 的 `run_audio_test` 不再硬编码 3 秒/10 秒，改用配置（`src/server.rs:544`）；`/api/status` 返回 `audio_test_quiet_ms`/`audio_test_speech_ms`（字段 `src/server.rs:447-448`，赋值 `src/server.rs:472-473`）。
- `/api/config` 下发钳制后的 `clear_after_ms`（`src/server.rs:225-231`），WS 的 `config` 推送同样钳制后下发（`src/server.rs:630`）。
- 试音统计改为纯函数 `average_over_window`（`src/server.rs:513-521`），并**修复一个真实缺陷**：若统计计数器在试音期间被重置（管线重启或并发第二次试音），旧实现会把帧数饱和到 0 并返回 0 平均值，从而报出"讲话音量过低"这种误导性结论。现在一旦检测到计数回退即返回 409「试音期间音频统计被重置（管线可能刚重启），请重试」（`src/server.rs:557-562`）。
- 试音建议拆为纯函数 `audio_test_advice`，四类结论都可单测（`src/server.rs:526-536`）。
- `dist/config.toml` 新增两个配置项说明：`clear_after_ms`（`dist/config.toml:60`）与 `[audio_test] quiet_ms/speech_ms`（`dist/config.toml:66-68`）；`max_lines` 注释改为"直播实现上限为 2"（`dist/config.toml:58`）。
  - **更正备注**：旧的"max_lines 支持 1–4 行"注释是错的（实现上限是 2）。`src/config.rs:265-269` 的注释本轮也已改为"capped at 2"；原先还带着这条错误注释的死副本 `src/overlay/app.js` 已在 16:03–16:05 之间**整体删除**（16:06 复核 `src/overlay/` 不存在），该处不一致随之消失。
  - 本轮**未修**的过期注释：`src/config.rs:45` 仍把 provider 写成 `qwen-realtime / openai-realtime / mock`，不含默认的 `bailian-fun-asr`（10 号文档 4.9 已记录）。

## 本轮实现、但只有代码级证据的自愈与 drain 修复（16:02–16:03）

`src/pipeline.rs` 在 16:02–16:03 被重写，针对 10 号文档 4.2/4.3/4.4 给出实现。**这些改动已被 16:07:32 的 `cargo build --release` 编进 `target/release` 产物（时间上晚于改动，但没有构建日志或 commit 绑定），除此之外没有任何运行证据。**
下列 `src/pipeline.rs` 行号已于 **16:06 对 16:03:55 版重新核对**（该文件此后未再变动）：

- **云端断线自愈（对应 4.2 / R08）**：`watch()` 现在检查 `llm_connected`；曾连接过（`saw_llm_connected`）且 provider 任务已结束 → 写入 `识别连接已断开，正在自动重连…` 并 `restart(delay)`（`src/pipeline.rs:348-366`）。`saw_llm_connected` 用于避免把握手中的正常连接误判为掉线（初始化 `src/pipeline.rs:269`）。
- **重试间隔**：`provider_restart_backoff(attempts)` 按**连续失败次数**指数退避 2 → 10 → 20 → 40 → 80 s，并以 `PROVIDER_BACKOFF_MAX = 80 s` 封顶（`src/pipeline.rs`）。此前"连接过 2 s / 从未连接 10 s"的写法里 10 s 分支实际不可达，且**没有重连上限**——坏端点会每 2 秒永久重连。失败计数存于 `AppState::provider_failures`（`src/main.rs`），每当观察到 `llm_connected` 为真即清零。
- **drain 竞态（对应 4.3）**：`restart(drain_grace)` 在 provider 任务仍存活时**推迟**下一次启动而不是 abort 掉 session；run 循环用 `wait_for_provider(DRAIN_GRACE)` **等 drain 自己结束**（而不是在截止时刻 abort）。
  - **2026-09-21 16:3x 修正**：`DRAIN_GRACE` 由 2 s 提到 **12 s**，并且"2 s 门闩不得截断 10 s drain"由 `safe_wait = max(gate, DRAIN_GRACE)` 保证。此前 2 s 的 grace 会在读者 10 s drain 窗口还剩 8 s 时就 `try_start` → `PipelineInner::drop` → `_llm_task.abort()`，**超过 2 秒才到的最终句仍会丢**，即该修复当时并未达成自己声明的目标。单测 `the_drain_window_covers_the_provider_reader_timeout` 钉住该下限。
- **无音频空会话（对应 4.4 / P0 第 5 条）**：`try_start` 不再在启动时置 `audio_active = true`，并由 **`AUDIO_FIRST_FRAME_GRACE = 30 s` 的首帧门控**决定何时建云会话。
  - **2026-09-21 16:3x 修正（按能量而非按帧）**：门控条件从"收到第一帧"改为 **`frame_opens_audio_gate(rms)`（`rms > AUDIO_GATE_RMS_FLOOR = 0.001`）**。原因是 OBS 插件默认 `gate_silence=false`（`plugin/stream-live-translate.c`）且 WASAPI 在源静音时**仍持续推送数字静音帧**，所以按帧门控在"源在播放但整段无声"这条真实路径上照旧会建空闲会话——此前"不会再产生空会话"的表述不成立。单测 `digital_silence_does_not_open_the_audio_gate` 覆盖。
  - **仍未消除的边界**：`/api/connection-test`（`src/server.rs`）**有意绕过**门控——它是用户显式点击的鉴权探测，会建立一个空任务。因此准确表述是"**除显式连接测试外**，不会为静音输入创建云会话"。
  - 30 s 内没有达到阈值的音频则中止本次启动并释放采集任务，报"等待音频输入超时…未创建云端会话"，由 run loop 退避重试。
- **新增真正的停止端点（`POST /api/stop`）**：此前 run loop 只在进程退出时结束，因此"停止"若仅丢弃 inner 状态会被紧随其后的 `try_start` 撤销，而在 30 s 首帧等待期间**任何操作都打断不了它**（`/api/restart` 与保存配置都返回 `{ok:true}`）。现由 `PipelineHandle` 的 run-gate（`pause`/`resume`/`is_enabled`）实现可恢复的停止：`pause` 让 run loop 停在门闸上，`resume`（由 `/api/restart` 调用）重新放行；等待期间 `select!` 同时监听门闸与 shutdown，故暂停可即时中断 30 s 等待。单测 `pausing_holds_the_run_loop_and_resuming_releases_it`。管理页新增「停止管线」按钮 `#stop-btn`。
- `src/main.rs` 的 `AppStatus` 新增 `last_input_at` 字段（`src/main.rs:100`），供上面的停滞判定使用。

## 授权录播回放链路（新增，尚未验证成功）

- 新增仅覆盖当前进程的 `--ingest-port`，不会写回配置或影响 OBS（`src/main.rs`）。
- `scripts/send-replay-pcm.ps1` 按实时节奏把指定录播片段以 16 kHz 单声道 PCM 送入本机 ingest 口。
- `overlay?local-replay=1` 提供纯前端事件回放，不发送音频、不调用百炼。
- 2026-09-21 15:37 的首次真实转写尝试**失败**：状态为 `current=null`、`history=[]`、`llm_connected=false`，错误为“百炼任务失败：request timeout after 23 seconds.”。记录见工作区 `docs/09-首段真实转写测试记录.md`。
- 该次测试使用的是 **13:07 构建的旧二进制**；其后的 `5e0ad13`（有限回放输入关闭）与 `87b9f6e`（finish-task 后继续收取最终事件）两处修复当时**尚未编译、尚未验证**。**（16:08 更正：`target/release` 已于 16:07:32 重新构建，这两处修复与本轮改动都应在其中；但产物没有 commit 绑定，`candidate-control/` 副本与正在运行的引擎仍是 13:07 的旧产物，因此"重跑"仍必须先同步副本并重启引擎。）**
- **（2026-09-21 16:3x）ingest 自锁缺陷已修**：`ingest.rs` 的 `Registration::drop` 原先无条件 `*SENDER.lock() = None`。而 `try_start` 会**先** `register()`（装入新管线的 sender），**再**用 `*handle.inner.lock() = Some(...)` 析构旧 `PipelineInner`——旧守卫随之析构，把刚注册的新 sender 抹掉。于是 `obs_filter` 模式下新管线再也收不到帧：首帧门控超时 → 重试再次 register → 再次被抹掉，形成**永久自锁**（而回放走的正是 ingest 通道）。
  修法：registry 引入代际计数（`GENERATION`），`register` 记录自己的代际，`drop` 仅在"自己仍是当前 owner"时才清空。新增 3 项单测，其中 `dropping_a_stale_guard_keeps_the_new_registration` 直接复现该自锁。
- **（2026-09-21 16:3x）回放脚本配速与编码**：`send-replay-pcm.ps1` 原先"每 20 ms 块再固定 sleep 20 ms"，实际速率是 `20 ms + I/O`，11 秒素材实测耗时 17.36 秒（0.63×，约 31.6 ms/块）——**任何基于该次回放的时序结论都不成立**。现改为**绝对时间轴配速并默认开启**：第 N 块必须在 `start + N×20 ms` 前上线，sleep 按已耗 I/O 缩短，实时成为下限；新增 `realtime_factor` / `realtime_ok` / `lateness_*` 字段，`factor < 0.9` 时打警示并声明"禁止用于时序/验收"。本地模拟（12 ms I/O/块）：旧法 2.36×，新法 **1.00×**；真实端到端（本地监听 + 录播 04:54 共 6 秒）实测 **`realtime_factor = 1.006`**。
  另：该脚本与 `extract-replay-samples.ps1`、`candidate-control/Start-Candidate-Control.ps1` 原先都是**无 BOM 的 UTF-8**，而 `powershell.exe`(5.1) 在本机 code page 936 下按 GBK 解码，中文会变乱码并**直接导致语法错误**（`Unexpected token '}'`）。三者已改为 **ASCII-only 源码**并经 PS 5.1 解析器验证；`extract-replay-samples.ps1` 另加 `-Ffmpeg` 全路径参数（本机 ffmpeg 不在 PATH）与 `-Force`。
- **注意**：`candidate-control/` 的启动脚本现会在写 `candidate-control.pid` **之前**校验该 PID 仍存活且已拥有 8797 端口，否则删除 pid 文件并非 0 退出——此前端口绑定失败已退出的实例也会被写进 pid 文件，制造"幽灵 PID"。

## 已确认的缺口（2026-09-21 同源复核，详见工作区 `docs/10-同源复核记录-2026-09-21.md`）> 本节是 **15:54 那次复核的历史记录**，条目原样保留。其中清屏时长、输入音量表、预览 `replace`、云端断线自愈、drain 竞态、无音频空会话六条已在本轮被处理，均在条目末尾用「2026-09-21 16:0x 更正」标注；这些更正**没有任何运行证据**（16:07:32 的 `target/release` 构建在时间上覆盖了它们，但无构建记录、无 commit 绑定、无运行验证），不得当作"已验证"。仍然成立的是：热词完全未实现、三档预设不影响 `speech_noise_threshold`、门控缺少句首/句尾预留与 Music 帧静音补偿。**死副本 `src/overlay/` 已在 16:03–16:05 之间被删除**（见下方更正），本条不再成立。**（2026-09-21 22:0x 再更正：热词已实现并验收（见文末 R10 节）；三档预设现已写 `speech_noise_threshold`（见文末 22:0x 节）——"仍然成立"的三条里两条已闭合，只剩门控预留未做。）**

- **R10 热词完全未实现**：源码与前端均无任何词表字段或界面，`run-task` 参数不含词表。此前把它列入“未执行”是不准确的。**（2026-09-21 18:0x 更正：已实现并已用真实百炼调用验收，见文末「R10 热词（上下文增强）实现与真实对照实验」；机制全部通过，但目标专名的纠正效果**实测未出现**——该节记录了两组原始字幕文本，不得据此宣称"热词已能纠正专名"。）**
- **云端识别断线不会自动重连**：`watch()` 只检查配置、采集设备与 `audio_active`，从不检查 `llm_connected`；百炼 provider 只连接一次。WebSocket 断开后管线停摆且不自愈。**（2026-09-21 16:02 更正：`watch()` 已新增 provider 任务结束检测与自动重连；16:3x 补充：重连改为按连续失败次数指数退避 2→80 s 封顶，不再是无上限的固定 2 s。仍属代码级，未跑真实链路。）**
- **音频断开触发的重启会打断最终结果 drain**：`watch()` 的 2 秒 ticker 在 `close_input()` 后立刻 `restart()`，而 `restart()` 会 abort LLM 任务，使 `87b9f6e` 的 10 秒 drain 实际只剩约 2 秒。**（2026-09-21 16:02 更正：`restart(drain_grace)` 已在 provider 仍存活时改为推迟拆机；16:3x 进一步更正：grace 提到 12 s 并改为"等 drain 自己结束"而非到点 abort，因为 2 s 的版本仍会丢掉 2 秒后才到的最终句。）**
- **重启会立刻新开无音频的云任务**：`try_start` 不问音频是否到达就置 `audio_active=true`，provider 随即 `run-task`；这解释了 23 秒超时的来源。**（2026-09-21 16:03 更正：`try_start` 已不再置 `audio_active=true`，改由首帧真实音频置真，并加了 2 秒停滞检测。2026-09-21 16:3x 进一步更正：现由 `AUDIO_FIRST_FRAME_GRACE = 30 s` + **能量阈值**门控决定何时建云会话，静音输入不再建会话；例外是用户显式点击的 `/api/connection-test`，它会建立一个空任务探测鉴权。见下文「自愈与 drain 修复」。）**
- **三档过滤预设不影响送往云端的音频**：预设只改本地 `silence_rms`，而 `Speech` 与 `Silence` 帧都会发送、只丢弃 `Music` 帧；`02` 号文档要求的 `speech_noise_threshold` 未被任何预设或界面改动，也无回放校准记录。**（2026-09-21 22:0x 更正：预设现已同时写 `llm.speech_noise_threshold` 并在管理页提供数值输入 + 四个快捷档位，见文末「云端噪声判定阈值 `speech_noise_threshold` 做成用户可调项」。**注意**：本条前半段对 `pipeline.rs` 的描述（Speech/Silence 都发、只丢 Music）**仍然成立**，它正是"只改 `silence_rms` 没用、必须改云端阈值"的原因；已闭合的是"没有任何界面入口"这一半。"降噪实效"仍无实测数据。）**
- **R06 清屏时长不可配置**：`overlay/app.js` 中为硬编码 4000 ms，配置与管理页均无对应字段。**（2026-09-21 16:0x 更正：已实现 `overlay.clear_after_ms` 并在管理页给出 2–8 秒选项，见「本轮改动」；仍属代码级，未在浏览器验证。）**
- **门控缺少约定的句首/句尾预留与静音替换**：无 200/300 ms 预留，`Music` 帧直接丢弃、不补同长度静音。
- **管理页输入音量表只在页面打开、保存后与重启后刷新**：没有轮询，WebSocket 也不推送 `input_level`；且只显示 `sqrt(rms)` 百分比。**（2026-09-21 16:0x 更正：已加 1 秒轮询与 RMS 数值读数，见「本轮改动」；仍属代码级，未在浏览器/OBS dock 验证。）**
- **管理页预览忽略 `replace` 标记**：会把百炼的累计修订叠加成重复文字（overlay 侧处理正确）。**（2026-09-21 16:0x 更正：预览已按 `replace` 分流，见「本轮改动」；仍属代码级。）**
- **削波判据不会触发**：`input_peak` 记录的是帧 RMS 最大值，却用 `> 0.98` 判断削波。**（2026-09-21 16:0x 复核：判据本身未改，仍不会在真实音频上触发；新增的只是覆盖该分支的单元测试 `advice_covers_too_quiet_clipping_and_insufficient_contrast`。）**
- **证据目录缺失**：`artifacts/acceptance/`、`build/replay-samples/`、`release/` 均不存在；OBS 包从未生成。

## 仍待验证

- 候选版本、已执行离线验证和未执行项见 `docs/ACCEPTANCE_LOG.md`；其中未执行项不视为通过。
- **发布二进制状态（16:08 复核，已变化，务必重读）**：
  - `target/release/stream-live-translate.exe` 已在 **2026-09-21 16:07:32** 被重新构建：**3,665,408 bytes，SHA-256 `673B8F3531D63F8C1F4F3CEDE6B5356173F424206DDBD42367EE88C8752D49B8`**（同目录 `.pdb`、`.d` 同时更新，符合一次新的 `cargo build --release`）。它**晚于本轮最后一次源码改动**（`src/config.rs` 16:05:41），因此**应当**包含本轮 Rust 改动——但这只是时间与体积上的推断：**没有构建日志、没有 commit 记录、没有针对这个二进制的测试或运行证据**。
  - `candidate-control/stream-live-translate.exe` **未同步**，仍是 13:07:05 / 3,630,080 bytes / `D971B76C1ED04D7D5671C4610930BBB9EDD8C5009C0CA90D1A6A61FD704E4186`。
  - **正在运行的两个引擎都还是旧产物**：PID 23776（13:45:50 启动，路径即 `candidate-control/` 副本）与 PID 44892（2026-09-20 21:33:24 启动，路径为 OBS 插件 engine 目录 `...\obs-plugins\stream-live-translate\engine\`）。**所以本轮后端改动目前在运行态一处都没有生效**；要生效必须同步 `candidate-control/`（或 OBS 侧 engine 副本）并重启对应引擎。
  - 结论：`cargo build --release` 这一步**已完成（仅 `target/release`）**，但"构建产物 == 本轮源码"和"运行态已用上新代码"两件事都**未经验证**。
- **浏览器运行时行为整体未验证**：1 秒轮询是否真在 OBS dock 里稳定刷新、隐藏标签页暂停是否按预期、两行分页的真实观感、退避重连在真实断网下的表现、性能模式开关在 OBS 浏览器源里的实时生效——全部只有代码级结论，没有真实浏览器/OBS 证据。
- **本轮补上的可复现离线证据（2026-09-21 16:0x）**：
  - Rust 单元测试由主 agent 用工作区内工具链实测 `cargo test --offline` = **27 passed / 0 failed（exit 0）**（16:0x）。**16:06 复核：测试数已继续增长到 30 项**——`config.rs` 在 16:05:41 又加了 `the_shipped_config_template_parses_and_matches_the_defaults`（`src/config.rs:421`），`pipeline.rs` 又加了 `an_input_that_stops_sending_frames_is_not_reported_as_active`（`src/pipeline.rs:566`）与 `an_active_input_without_a_timestamp_is_left_alone`（`src/pipeline.rs:580`）。**这 30 项没有对应的运行记录**，27 项的那次运行早于它们，不能用来声明当前树已通过。
  - 计数构成（16:06 按源码 `#[test]` 属性核对，共 30 项）：`config.rs` 5、`server.rs` 5、`pipeline.rs` 7，其余 `audio.rs` 1、`lang.rs` 3、`llm.rs` 4、`recording.rs` 2、`subtitle.rs` 3。上一轮基线为 15 项，本轮新增 12 个用例至 27，其后又增至 30（其中 config 的 `older_configs_can_omit_new_bailian_fields` 是在既有用例上扩展断言）。本会话（文档 agent）**未运行 cargo**，只做了源码计数核对。
  - `node --check admin/app.js` 与 `node --check overlay/app.js` 均通过（exit 0，2026-09-21 16:0x 本会话独立复跑）。
  - `dist/` 与前端源目录一致：`admin/app.js`、`overlay/app.js`、`admin/index.html`、`admin/style.css` 四个文件的 SHA-256 与源文件逐一相同（2026-09-21 16:0x 实测）。
  - 工具链位置：`cargo` 不在 PATH，需用工作区内的 `.build-tools\cargo\bin\cargo.exe`，并设置 `CARGO_HOME`/`RUSTUP_HOME` 指向 `.build-tools\cargo` 与 `.build-tools\rustup`。
- **`tests/` 下出现了 Node 回归测试半成品（不是本轮的通过证据）**：截至 16:06，`tests/` 下有 `dom-shim.mjs`、`fake-clock.mjs`、`overlay-harness.mjs`、`overlay-cases.mjs`、`run-overlay-tests.mjs`，以及一批 `_debug-*.mjs` 临时脚本（另一位 agent 正在写）。它们引用的 `tests/README.md`、`run-negative-control.mjs` **尚不存在**，因此这里不记录任何运行命令与结果。本会话未运行、也未验证这套测试。
- 已在隔离候选控制台完成真实百炼的无音频连接测试：验证了鉴权、任务启动和正常结束；未发送 PCM 或录播。真实音频转写、热词服务、OBS、无线麦克风和录播人工标注仍未执行。
- 项目本地 `.cargo/config.toml` 使用 TLS 校验的 rsproxy 镜像。新配置默认百炼模型，旧配置缺少新增百炼字段时保持可读，均有离线测试。
- OBS 打包脚本已改为使用项目内临时目录，规避含中文用户名路径的 MSVC 链接临时文件错误，并改用 .NET 解压官方 OBS 二进制包以兼容 VS 开发 PowerShell；但本机仍缺 OBS SDK 头文件：`build/plugin-sdk/` 下只有 `obs-full/`（官方二进制包）与 `obs-full.zip`，`build/plugin-sdk/obs-studio` **不存在**、`obs.lib` **未生成**、`release/` 目录**不存在**，候选 OBS 包尚未生成（2026-09-21 16:0x 复核，结论未变）。
- 现有代码并未遵循 rustfmt：`cargo fmt --check` 报告 94 处差异（exit 1，2026-09-21 复核），其中含新增代码，不只是上游既有格式差异。该数值**早于本轮的 pipeline/admin/overlay 改动**，需要重新跑一次才有效。
- 额度策略：每个工作单元前后及约每 10 分钟读取额度；五小时窗口剩余不高于 10% 时只保存进度，**周窗口剩余不高于 20% 时停止自动开发并保留缓冲**（以工作区 `docs/08-录播云测试授权与额度更新.md` 为准，此前写的 35% 已作废）。已授权 heartbeat 在窗口重置后继续，不替代人工验收。
- **死副本已删除（16:06 复核）**：`src/overlay/app.js` 与 `src/overlay/style.css`（8 号复核记录的过期副本，含错误的「行数上限（1-4）」注释与死变量 `lastPartialAt`）已在 16:03–16:05 之间从工作区删除；`build.rs:48-49` 只同步 `admin`/`overlay` 两个真源目录，删除不影响构建。上一条历史记录保留在此以便追溯。
- **文档时点声明（16:08 复核）**：本文件记录的是 **2026-09-21 16:08 前后**的树与产物状态。主 agent 声明仍会继续修改 `src/pipeline.rs`、`src/main.rs`、`src/server.rs`、`overlay/app.js`（WS 保活等），任何后续改动都需要重新核对，不能沿用本文档的行号与结论。

## R10 热词（上下文增强）实现与真实对照实验（2026-09-21 18:0x）

> 本节全部结论来自**真实百炼调用**（`wss://dashscope.aliyuncs.com/api-ws/v1/inference`，
> 模型 `fun-asr-realtime`，60 秒 16 kHz 单声道 s16le 素材）。详细证据见工作区
> `.hotword-evidence-2026-09-21.md` 与 `.hotword-ab-*.json` / `.hotword-e2e.json` /
> `.hotword-live.json` / `.hotword-admin.json`。

### 实现（文件级）

- `src/hotwords.rs`（新增）：词表体检（含非 ASCII > 15 字符、纯 ASCII 空格片段 > 7）、
  按 **400 字符/轮** 切分并只保留最近 **5 轮**、`input.context` 组装、指纹（未变化不重发）、
  `HotwordFeed`（watch 通道 + `adopt()` 共享）、`HotwordStatus`（下发状态）。12 个单元测试。
- `src/config.rs`：`llm.hotwords: Vec<String>`，`#[serde(default)]` 保持旧配置可读；
  新增 `hotwords_round_trip_through_toml`，并扩展 `older_configs_can_omit_new_bailian_fields`
  断言旧配置缺该键时解析为空表。
- `src/llm.rs`：`LlmProvider::run` 增加 `context_rounds` 参数；百炼 provider 在
  `run-task`/`continue-task` 的 `payload.input` 里带上 `context`，并在运行中订阅
  `HotwordFeed` 用 **continue-task** 下发；同时给 `parameters` 补 `language_hints: ["zh"]`。
  **未使用** `vocabulary`（`fun-asr-realtime` 不支持，会 `task-failed: CLIENT_ERROR`）。
- `src/pipeline.rs`：会话启动时把词表快照交给 provider；`watch()` 检测 `llm.hotwords`
  变化后只推进 feed，**不重启会话**（重启会掐断正在识别的那句）。
- `src/server.rs`：`/api/config` 与 `/api/status` 增加 `hotword_status`（词数、轮数、
  告警、`delivered`、`mode`、`last_applied_at`、`skipped_unchanged`、`last_result`）；
  保存配置后立即推进 feed。
- `admin/index.html` / `admin/app.js` / `admin/style.css`：新增「2. 热词」卡片（多行编辑、
  实时规范告警、分轮提示）与状态显示；`collectPatch()` 提交 `llm.hotwords`。
- `dist/config.toml`：新增 `hotwords = []` 注释示例（`build.rs` 自动同步 `admin/` → `dist/admin/`）。

### 验收结果

| 项 | 结果 |
|---|---|
| run-task 携带 `input.context` | **通过**（`/api/status`：`mode=run-task`、`round_texts=["铨洲智造 区域赛"]`） |
| 运行中更新 = **continue-task**（不是"下次会话生效"） | **通过**（回放中保存新词表 → `mode=continue-task`，`llm_connected` 全程 `true`，会话未重启） |
| 不变更不重发 | **通过**（重复保存同表 → `skipped_unchanged` 递增，`last_result="热词未变化，未重发"`） |
| 400 字符分轮 / 5 轮上限 | **通过**（60 个 5 字热词 → 2 轮，每轮 ≤ 400 字符） |
| 管理页规范告警 | **通过**（20 字中文、8 片段英文各 1 条可见告警） |
| 真实转写链路未破坏 | **通过**（同配置 60 秒 → 8 条 final 中文） |
| **目标专名纠正** | **未通过（实测未出现）** —— 见下 |

### 目标专名的对照实验结果（关键）

同一 60 秒素材、同一模型，只改 `input.context`：

- 对照组（不加热词）：赞助商句 final = 「……就是**全球制造**，提供给我们这个零件……」
- 实验组（`["铨洲智造 区域赛"]`）：赞助商句 final = 「……就是**全球制造**，提供给我们这个零件……」（**逐字相同**）
- 另测 4 个变体（`泉州制造`、`泉州智造`、5 个竞争拼写、官方 user+assistant 成对形状）：
  赞助商句 final **全部逐字相同**，`铨洲智造` 一次都没出现。
- 换 `fun-asr-realtime-2025-11-07`（文档同样声明支持上下文）：行为相同。

服务端确实在校验该字段：把 `input.context` 发成字符串（非数组）会立即
`task-failed: CLIENT_ERROR`。因此**请求形状正确、字段被接受，但模型未使用它**。
另注：跨会话存在随机性（同一段音频有时输出「区赛」、有时「区域赛」），
所以任何"单次跑好了"都不能当作生效证据；上表"逐字相同"是同配置重复运行稳定复现的结果。

### 期间修掉的两个真实缺陷

1. **`tokio::select!` 的 `biased;` 把热词分支饿死**：音频每 ~20 ms 一帧，永远优先音频
   ⇒ `changed()` 60 秒一次都没被轮询，continue-task 静默失效。移除 `biased;` 后恢复。
2. **provider 覆盖了共享 feed**：`set_hotwords` 原先写 `self.hotwords.set(feed.plan())`，
   provider 订阅的是另一个实例（日志 `subscribers=false`），管理页写入永远到不了会话。
   改为 `HotwordFeed::adopt()` 共享同一实例。

### 产物

- 新引擎：`candidate-control/stream-live-translate.exe`，3,740,672 B，
  SHA-256 `9B93B8568E61BAF692EE061A069F5274324FABD3E11F1E3830212D44A13E9A71`；
  旧引擎备份为同目录 `stream-live-translate.D53D1055.bak`
  （`D53D10558F3C7E978535E9CC9FA937AB65A194E01828DC03BC190C62ED211AAC`）。
- `cargo test --bin stream-live-translate` = **48 passed / 0 failed**。
- 为释放文件占用，验证期间停止了 `candidate-control` 里残留的引擎进程与一个
  2026-09-20 起由 OBS 插件启动的旧引擎；两者均可用原方式重新拉起。

## 2026-09-22 本轮前端 / 配置改动（0 秒无缓冲、长句翻页闪烁、管理页控件审计）

> 全部结论来自可离线复现的命令（原始输出见 `docs/ACCEPTANCE_LOG.md`）。
> 未触碰 `src/pipeline.rs`、`src/llm.rs`、`src/ingest.rs`、`src/hotwords.rs`、`src/obs.rs`、`src/main.rs`；
> 任务 1 在 `src/config.rs`、`src/server.rs` 内只动了 `display_delay_ms` 相关部分。

### 1. 新增「0 秒（无缓冲）」选项（`overlay.display_delay_ms = 0`）

- `admin/index.html`：`#ov-display-delay` 新增 `<option value="0">0 秒（无缓冲）</option>` + 字段说明。
- `admin/app.js`：新增 `clampDisplayDelay()`（0 = 无缓冲；其余钳到 500–1000；缺省 750）与
  `setSelectValue()`（未列出的合法值不再让下拉框变空白、并在下次保存时被改写成默认值）。
  `fillForm()` 用 `?? 750` 取代 `|| 750`（**旧写法把 0 变回 750**），
  `collectPatch()` 不再用 `Math.max(500, …)`（**旧写法把 0 变回 500**）。
- `src/config.rs`：新增 `clamp_display_delay_ms()`（`None`→750、`Some(0)`→0、其余 500–1000）
  与 `DISPLAY_DELAY_MIN_MS/MAX_MS`；新增单测 `display_delay_allows_zero_and_clamps_the_rest`
  （含 0 的 TOML 往返），`OverlayConfig::display_delay_ms` 的文档注释同步。
- `src/server.rs`：`GET /api/config` 与 WS `config` 推送都走同一函数，两条路径下发值一致。
- `overlay/app.js`：`clampDisplayDelayMs()` 允许 0；0 时**不创建任何显示定时器**（真·无缓冲）；
  typewriter 路径的首个单位改为同步写出，0 缓冲下第一个字不再额外等 32 ms。
- `dist/admin`、`dist/overlay` 已同步（`build.rs` 构建时也会自动同步；二进制 embed `dist/`）。

### 2. 修「长句翻页时第一段/第二段交替闪烁」（REAL DEFECT #4）

- 根因：`replacePartial()` 只在「一帧是另一帧的前缀」时才算同一句。百炼 Fun-ASR 的
  `replace:true` 帧是对**同一句**的累计修订（句子边界是 `sentence_end:true` → `Final`），
  真实的尾字同音修正 / 句中补词既不前缀也不延长 ⇒ 每帧都被判成「新的一句」并把翻页光标重置为 0
  ⇒ 长句在第 1 页与第 2 页之间来回跳。
- 修复：`sameOpenSentence()`（按两帧共享文本比例判定同一句）+ `pickShown()` 用
  `findLastPageStart()` **重新定位**光标而非推回第 0 页（并修掉 `pageStart === full.length` 的空白帧）。
- 复现脚本 `node tests/repro-page-flicker.mjs`：S1 纯增长 0 次、S2 尾字重解码 10 次、
  S3 句中补词 1 次、S4 打字机 10 次 → **修复前 21 次回退（exit 1）**，**修复后 0 次（exit 0）**。
- 回归用例：`page-flicker`、`page-flicker-typewriter`（新），负控制新增 `page-cursor-reset` /
  `page-cursor-restart` 扰动，两个用例都会重新失败。
- 另加 `local-replay` 用例：把假时钟推过 `?local-replay=1` 内置调试路径的完整 12 s 时间线，
  断言无空白帧、无第三行、每页都属于当前句、完成标记到达、长样本确实向前翻页、结束后清屏。

### 3. 管理页控件审计（逐项表见 `docs/ACCEPTANCE_LOG.md`）

56 个控件全部有后端支撑；3 项此前有缺陷，已修：

1. 「新字幕页显示缓冲」= 0 被前端改回 500 / 750（已修，见上）。
2. 实时预览分页把 `previewFindCut()` 的**结束下标当起始下标**用，渲染整句尾巴（超出两行预览框、
   永远看不到真正的第一页）；已改为与 overlay 同一套 `pickShown()` 语义（含同一句判定）。
3. 「清空历史」只清本页导入行，服务端历史没有对应接口，点完列表照旧 ⇒ 新增
   `POST /api/subtitles/history/clear`（`SubtitleHub::clear_history()` + 单测），按钮改为调用它
   并校验 `ok`；`/api/recordings/export` 的本场录制不受影响。

不确定是否算 bug、留给你判断：`#support-btn`（只弹「暂未开放」，无后端）、配置里出现下拉列表
之外的服务商名时保存会改写为 `openai-realtime`、`/api/locale` 已无前端调用者、
`GET /api/config` 明文返回 `obs.password`（默认只监听 127.0.0.1）。

### 4. 本轮测试数字

`node tests/run-overlay-tests.mjs` = **25 passed / 0 failed**（基线 21）；
`node tests/run-negative-control.mjs` = **PASSED**（7 个扰动全部被捕获）；
`cargo test --offline`（隔离工具链）= **50 passed / 0 failed**。
原始输出存档在 `evidence/`（工作区根目录）。

## 2026-09-21 22:0x 追加：云端噪声判定阈值 `speech_noise_threshold` 做成用户可调项

> 目标：把官方 `fun-asr-realtime` 的 `run-task.parameters.speech_noise_threshold`
> 暴露到管理页，并让「过滤预设」真正影响识别结果。**本轮没有跑任何校准或整场回放**，
> 0.3 / 0.6 两档没有实测数据（用户明确要求不要为填这两档做实验）。

### 为什么原来的预设几乎无效

`src/pipeline.rs` 把 `Speech` 与 `Silence` 帧**都**发给云端、只丢 `Music`，所以预设只改
本地 `filter.silence_rms` 时，送往云端的音频基本不变 ⇒ 识别结果几乎不变（原「已确认的缺口」
第 3 条的结论成立）。真正影响识别的是随 `run-task` 下发的 `speech_noise_threshold`，
而它此前既无界面入口也无预设。**该缺口本轮已闭合（代码级 + 本地 HTTP 证据，见下）。**

### 最终设计：一个预设同时设置两个值（不拆成两个控件）

「过滤预设」保持**单一入口**，但每个档位现在**同时**写 `llm.speech_noise_threshold`（云端）
与 `filter.silence_rms`（本地），两个数值输入都留在界面上并实时回显：

| 预设（下拉 + 快捷按钮） | `speech_noise_threshold` | `filter.silence_rms` |
|---|---|---|
| 0.0 关（保留轻声）`soft` | 0.0 | 0.007 |
| 0.3 中等（平衡）`balanced` | 0.3 | 0.012 |
| 0.6 较强 `straight` | 0.6 | 0.012 |
| 0.9 强（强过滤）`strong` | 0.9 | 0.012 |
| 自定义 `custom` | 读输入框 | 读输入框（此档才显示） |

理由：用户面对的是「降噪强度」这一个取舍，两个数值是实现细节；拆成两个预设会把同一个
取舍变成两套互相打架的选择。副作用是原来的 `strong`（`silence_rms = 0.020`）不再改本地
阈值（0.020 会切掉轻声），「强过滤」的语义改为由**云端阈值**承担 —— 这是有意的：本地阈值
越狠丢的帧越多、且不会减少噪声幻觉，真正能压掉幻觉的是云端阈值。

### 文件级改动

- `src/config.rs`：新增 `SPEECH_NOISE_THRESHOLD_MIN/MAX = -1.0/1.0`、
  `clamp_speech_noise_threshold()`、`clamp_speech_noise_threshold_flagged()`；字段文档补齐
  取值方向与代价。新增 2 项单测（钳制/NaN/±inf + 六个合法值 TOML 往返）。
- `src/llm.rs`：把 `run-task` 的 `payload.parameters` 抽成 `run_task_parameters(cfg)`、
  首帧抽成 `run_task_payload(cfg, task_id, input, language_hints)`，**真实会话 `run()` 与
  `test_connection()` 共用同一份构造**（此前是两处各写一遍 `serde_json::json!`），并在下发前
  再钳一次。新增 3 项单测，其中包括「配置值原样出现在 `run-task` 参数里」。
- `src/server.rs`：`POST /api/config` 写盘前钳制并在越界时打日志；`GET /api/config` 回传
  钳制后的 `llm.speech_noise_threshold` 与 `llm.speech_noise_threshold_clamped`；
  `/api/status` 新增 `speech_noise_threshold` / `speech_noise_threshold_clamped`。
  新增 2 项单测（`merge_json` 补丁往返、钳制一致性）。
- `src/main.rs`：`AppState` 新增运行期标记 `speech_noise_threshold_clamped`。
  **必须单独记**：钳过之后磁盘里存的就是边界值，"用户填的是 5.0"这件事再也看不出来，
  管理页要如实提示只能靠这个标记（下次保存合法值时清零）。
- `admin/index.html`、`admin/app.js`、`admin/style.css`：新增数值输入
  `#speech-noise-threshold`（`type=number min=-1 max=1 step=0.1`）、四个快捷档位按钮
  `#noise-preset-buttons`、「当前生效值」行 `#noise-threshold-status`、非百炼通道提示
  `#noise-provider-hint`；预设下拉扩到 4 档 + 自定义；`silence_rms` 行改为「自定义」档才显示。
- `dist/admin/*`：与源目录同步（SHA-256 逐一相同，见 `docs/ACCEPTANCE_LOG.md`）。
- `tests/admin-preset-cases.mjs`（新增）：用 `tests/dom-shim.mjs` + `node:vm` 起一个无头管理页，
  断言 15 项前端契约（输入属性、回填、四档双值联动、保存 patch 带阈值、越界钳制、
  手改回落「自定义」、生效值提示、输入框清空不谎报、非百炼提示）。

### 界面上说清的两件事

1. **代价**：数值输入下方写明「越低越容易把环境噪声当语音转写，越高越容易把主讲人的话判成
   噪声、断句也会更碎」。
2. **生效方式**：写明该参数在云端 `run-task` 时下发、**保存配置后会重启识别会话**；
   「当前生效值」行在输入框与服务端生效值不一致时显示「输入框 X 尚未生效：保存配置后会在
   重启识别会话时下发」。**界面上没有标注任何"实测/未实测"字样**（按用户决定删除）。

### 本轮证据（全部本机离线，未使用任何真实 API Key）

- `cargo test --offline` = **57 passed / 0 failed**（本轮新增 7 项；基线 50）。
- `node tests/admin-preset-cases.mjs` = **15/15 passed**。
- 自测引擎（8897 端口、mock provider、独立 config 副本，未触碰 8787/8788/8797/8798）：
  真实 `GET /api/config` 读出 `speech_noise_threshold`；`POST` 0.9 → 读回
  `0.8999999761581421`（f32）；`POST` 5.0 → 读回 `1` 且 `clamped = true`；
  `POST` −9.0 → 读回 `−1`；再存 0.9 后 `clamped` 清零；`/api/status` 读出 0.9；
  `/admin` 返回的 HTML 里 6 个新控件标记全部 FOUND。原始输出：
  `.build-tools/selftest-8897/evidence.txt`。
- 旧配置兼容：删掉 `speech_noise_threshold` 整行的 `config.toml` 在 8899 上正常启动，
  `/api/config` 与 `/api/status` 都读出默认 0.0。
- `node --check`：`admin/app.js`、`overlay/app.js`、`tests/*.mjs` 全部 exit 0。
- `node tests/run-overlay-tests.mjs` = **25 passed / 0 failed**；
  `node tests/run-negative-control.mjs` = **PASSED**（overlay 未改动，结果与基线一致）。

### 明确未验证

- **0.3 / 0.6 两档没有任何实测数据**（本轮按用户要求不做校准回放）。
- 阈值对**真实百炼**识别结果的影响（垃圾字幕条数、断句碎度）本轮未测：`run-task` 参数
  只由单元测试断言，未用真实端点抓包。
- 面板在**真实浏览器 / OBS dock** 里的观感（快捷按钮高亮、`silence_rms` 行的显隐、
  「当前生效值」与 1 秒轮询的配合）未在浏览器验证 —— 只有无头 DOM 断言。
- 非百炼通道的提示文案只是界面提示，未逐通道实测。
- 界面不标注任何"实测/未实测"字样（用户明确要求），因此用户看不到哪些值有实测数据。

### 文档事故（必须知悉）

写入本节时，`docs/ACCEPTANCE_LOG.md` **丢失了一节未提交内容**：「2026-09-22 追加：前端 / 配置
三项任务（0 秒无缓冲、长句翻页闪烁、管理页控件审计）」，含 59 项管理页控件审计表与
`node --check` / `dist` 哈希 / 密钥扫描记录。原因是误用 `git checkout -- docs/ACCEPTANCE_LOG.md`
去处理一份带未提交改动的文档，工作区内无副本，无法恢复。

- **未丢**：前端改动本身（`admin/*`、`overlay/*`、`tests/*`、`src/subtitle.rs` 等）都还在磁盘上。
- **已丢**：那份审计表与其中的哈希/扫描记录，需要重新生成。
- **教训**：对带未提交改动的文件**不要**用 `git checkout --`；本次同一类操作还曾把 `src/llm.rs`
  的 UTF-8 编码改成 BOM+GBK 乱码（`Set-Content -Encoding UTF8` 在 PS 5.1 下按 ANSI 读入），
  该文件已 `git checkout` 回 HEAD 后用编辑工具重新应用（`src/llm.rs` 当时无未提交改动，故无损失）。

