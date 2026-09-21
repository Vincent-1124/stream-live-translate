# 候选验收记录

日期：2026-09-21（2026-09-21 同源复核后更新；**16:0x 补充本轮离线证据与未验证边界**）
记录时的候选源码：`30d3a82`
记录时的候选引擎：`target/release/stream-live-translate.exe`
文件大小：3,622,400 bytes
SHA-256：`3251A0DBC5864D0FE5C00CE4D90C8F9A8A8BEBCEEC4BA97D2B182C293805302B`

> **2026-09-21 复核更正：上述哈希已作废，且当前 HEAD 没有对应的构建产物。**
>
> - 磁盘上 `target/release/stream-live-translate.exe` 与 `candidate-control/stream-live-translate.exe` 内容相同，均为 3,630,080 bytes、SHA-256 `D971B76C1ED04D7D5671C4610930BBB9EDD8C5009C0CA90D1A6A61FD704E4186`，构建时间 13:07:05。
> - 该产物早于 `5e0ad13`（13:28，`src/ingest.rs`）与 `87b9f6e`（15:41，`src/llm.rs`），不包含这两处修复。
> - 按 04 号文档“有新代码则相关测试失效”的规则，下表中的离线结论只能说明 13:07 构建时的状态，**不能用于声明当前源码已通过**，也不能用作新一轮真实转写测试的候选。
> - 重新验证前必须先 `cargo build --release` 并重新记录 commit 与哈希。

> **2026-09-21 16:0x 补充（本轮最重要的边界，务必先读）**
>
> - **二进制状态在 16:07:32 发生变化（本条已更正，务必重读）**：`target/release/stream-live-translate.exe` 已在该时刻被**重新构建**为 **3,665,408 bytes / SHA-256 `673B8F3531D63F8C1F4F3CEDE6B5356173F424206DDBD42367EE88C8752D49B8`**（`.pdb`、`.d` 同时更新）。它晚于本轮最后一次源码改动（`src/config.rs` 16:05:41），**应当**包含本轮 Rust 改动，但这是时间/体积上的推断：**没有构建日志、没有 commit 记录、没有针对该二进制的测试或运行证据**。
> - **但运行态仍是旧代码**：`candidate-control/stream-live-translate.exe` **未同步**（仍为 13:07:05 / 3,630,080 bytes / `D971B76C…`）；正在运行的两个引擎也都是旧产物——PID 23776（13:45:50 启动，`candidate-control/` 副本）与 PID 44892（2026-09-20 21:33:24 启动，OBS 插件 engine 目录内）。**本轮后端改动目前在运行态一处都没生效**，必须同步副本并重启引擎。
> - 本轮所有前端/后端改动依然只有**代码级**证据。没有真实音频设备、没有真实百炼调用、没有在 OBS 里渲染过。
> - 本文档记录的是 **2026-09-21 16:06 前后**的树状态；主 agent 声明在该时点之后仍会编辑 `src/pipeline.rs`、`src/main.rs`、`src/server.rs`、`overlay/app.js`，后续改动需重新核对。测试数在 16:03→16:06 之间已由 27 增至 30，**30 项尚无运行记录**。

## 本轮（16:0x）已执行的离线验证

| 项目 | 结果 | 证据 |
|---|---|---|
| Rust 单元测试（本轮） | 通过（27 项那次） | 主 agent 用工作区内工具链实测 `cargo test --offline`：**27 passed / 0 failed**（exit 0）。上一轮基线 15 项，本轮新增 12 个用例 → 27 项。**16:06 复核：树上的测试已增至 30 项**（`config.rs` 5 / `server.rs` 5 / `pipeline.rs` 7 / `audio.rs` 1 / `lang.rs` 3 / `llm.rs` 4 / `recording.rs` 2 / `subtitle.rs` 3），**这 30 项尚无运行记录**，27 项那次早于新增的 3 项 |
| 新增用例清单 | — | config：`clear_after_is_clamped_to_supported_range`（`src/config.rs:398`）、`audio_test_durations_are_clamped`（`src/config.rs:407`）、扩展断言的 `older_configs_can_omit_new_bailian_fields`（`src/config.rs:369`）、`the_shipped_config_template_parses_and_matches_the_defaults`（`src/config.rs:421`，16:05 新增）。server：`quiet_window_average_uses_all_frames`（`src/server.rs:732`）、`speech_window_subtracts_the_quiet_phase`（`740`）、`a_window_without_new_frames_is_treated_as_silence`（`750`）、`counters_that_move_backwards_are_reported_as_a_reset`（`758`）、`advice_covers_too_quiet_clipping_and_insufficient_contrast`（`768`）。pipeline：`a_dead_provider_session_backs_off_longer_than_a_network_blip`（`src/pipeline.rs:518`）、`the_restart_gate_is_open_when_unset_or_elapsed`（`526`）、`the_restart_gate_reports_the_remaining_wait`（`537`）、`a_finished_provider_task_is_not_treated_as_alive`（`546`）、`a_pipeline_with_no_inner_state_has_no_live_provider`（`561`）、`an_input_that_stops_sending_frames_is_not_reported_as_active`（`566`）、`an_active_input_without_a_timestamp_is_left_alone`（`580`）。行号为 16:06 对现行文件核对 |
| 管理页与 overlay 脚本语法 | 通过 | `node --check admin/app.js`、`node --check overlay/app.js`，2026-09-21 16:0x 由本会话独立复跑，两条均 exit 0 |
| 前端 `dist/` 与源目录一致 | 通过 | 2026-09-21 16:0x 复核：`admin/app.js`、`overlay/app.js`、`admin/index.html`、`admin/style.css` 四个文件的 SHA-256 与对应源文件逐一相同（`build.rs:45-50` 在构建时同步） |
| 死副本 `src/overlay/` 已删除 | 通过（工作区清理，非验收项） | 16:06 复核：`src/overlay/` 目录已不存在（16:03–16:05 之间删除）。`build.rs:48-49` 只同步 `admin`/`overlay` 真源，删除不影响构建 |
| `tests/` Node 回归套件 | **未运行、未验证** | 截至 16:06，`tests/` 下有 `dom-shim.mjs`、`fake-clock.mjs`、`overlay-harness.mjs`、`overlay-cases.mjs`、`run-overlay-tests.mjs` 与一批 `_debug-*.mjs` 临时脚本（另一位 agent 正在写）。其引用的 `tests/README.md`、`run-negative-control.mjs` 尚不存在，因此本次**不记录任何运行命令与结果**，也不计入通过项 |

**工具链说明**：`cargo` 不在 PATH。本轮用的是工作区内的 `.build-tools`（位于工作区根目录，不在 repo 内）：设 `CARGO_HOME=<工作区根>\.build-tools\cargo`、`RUSTUP_HOME=<工作区根>\.build-tools\rustup`，并把 `%CARGO_HOME%\bin` 与 `%RUSTUP_HOME%\toolchains\stable-x86_64-pc-windows-msvc\bin` 前置到 PATH，然后在 `source/stream-live-translate-main` 下执行 `cargo test --offline`。本会话（文档 agent）受约束未运行 cargo，只做了源码计数核对与上面的 `node --check`。

## 本轮新增的不通过项判定（禁止当作通过）

| 项目 | 状态 | 说明 |
|---|---|---|
| OBS 浏览器源里的 1 秒音量轮询、RMS 读数、试音倒计时 | **未验证** | 只有代码级结论（`admin/app.js:679,684-800`）；没有在 OBS Dock 或真实浏览器里跑过 |
| 两行分页在真实渲染下的观感与换页次数 | **未验证** | 本地回放样本已加长（`overlay/app.js:617-620`），但换页是否真的发生、观感如何，未在浏览器/OBS 中确认 |
| WS 指数退避 + 40% 抖动、10 s ping / 30 s 半开检测 | **未验证** | 只有代码级结论（`overlay/app.js:31-35,521-531,554-565`）；真实断网、服务端重启、OBS 浏览器源休眠等场景均未实测 |
| 清屏时长可配置在浏览器里的实际生效 | **未验证** | 前后端链路已具备（`overlay/app.js:364-368`、`src/server.rs:225-231,630`），但未在浏览器里改过配置观察清屏时刻 |
| 试音计数器重置返回 409 的真实触发 | **未验证** | 纯函数与分支已被单测覆盖（`src/server.rs:513-521,557-562`），未在真机并发/重启场景下实测 |
| 断线自愈、drain 推迟、2 秒音频停滞检测 | **未运行验证** | `src/pipeline.rs` 16:02–16:03 的新代码；16:07:32 的 `target/release` 构建在时间上覆盖了它，但**没有构建日志、commit 绑定或任何运行证据**，运行中的引擎也仍是旧产物 |
| 真实音频转写 / 热词 / OBS 插件 / 无线麦克风 | 转写已通过真实链路；热词机制通过、专名纠正实效未通过；其余未执行 | 转写：60 秒素材真实出 8 条 final（`bailian-fun-asr` + `fun-asr-realtime`）。热词：见下行。OBS 插件与无线麦克风仍未执行 |

## 已执行的离线验证

| 项目 | 结果 | 证据 |
|---|---|---|
| Rust 单元测试（上一轮基线） | 通过 | `cargo test --offline`：15 passed，0 failed。2026-09-21 由独立复核会话重跑确认，exit code 0。**本轮已提升到 27 项，见上表** |
| 管理页与 overlay 脚本 | 通过 | `node --check admin/app.js`、`node --check overlay/app.js`（16:0x 复跑仍为 exit 0） |
| 百炼协议模拟 | 通过 | 覆盖 task-started 门控、partial/final、task-failed、旧/迟到 task ID |
| 配置兼容 | 通过 | 新默认百炼配置及缺少新增字段的旧配置反序列化测试；本轮扩展为同时覆盖缺失 `clear_after_ms` 时的 4 秒默认值与 `[audio_test]` 默认值（`src/config.rs:369-405`） |
| 48 kHz → 16 kHz | 通过 | 确定性单元测试确认重采样后的时长 |
| 前端 `dist/` 与源目录一致 | 通过 | 2026-09-21 复核逐一比对 6 个文件的 SHA-256，全部相同；16:0x 复核 4 个改动文件仍全部相同 |
| 本机 HTTP 冒烟 | 通过/预期失败 | 隔离调试进程验证 Key 脱敏、mock 连接测试、录制信息和空 SRT；无 OBS 音源的试音返回“音频输入未运行”；写入非真实测试 Key 后 API 仅返回已设置状态，显式清除后读回为未设置 |
| 真实百炼无音频连接测试 | 通过 | 隔离候选控制台使用用户本机保存的 Key；`/api/connection-test` 验证鉴权、任务启动和正常结束。未发送 PCM、录播或其他音频。 |
| 发布引擎构建 | 通过（已过期） | 13:07:05 的 `cargo build --release`；**不覆盖当前 HEAD**，见顶部说明 |
| 发布引擎重新构建（16:07:32） | **产物存在，但未验证** | `target/release/stream-live-translate.exe` = 3,665,408 bytes / SHA-256 `673B8F3531D63F8C1F4F3CEDE6B5356173F424206DDBD42367EE88C8752D49B8`（16:08 复核）。**没有构建日志、没有 commit 记录、没有测试或运行证据**；`candidate-control/` 副本未同步，运行中的两个引擎仍是 13:07:05 的旧产物 |

## 已执行的端到端尝试（失败）

| 用例 | 结果 | 证据 |
|---|---|---|
| T01 片段真实转写（04:54 起 11 秒） | **失败** | 状态 `current=null`、`history=[]`、`llm_connected=false`，错误“百炼任务失败：request timeout after 23 seconds.”；使用的是 13:07 旧二进制，其后的两处修复未编译。详见工作区 `docs/09-首段真实转写测试记录.md` |

该失败**不能**由模拟或 mock 结果替代或掩盖；用户已授权重跑 04:54 与 29:54 两个 11 秒片段，但重跑前必须先完成工作区 `docs/10-同源复核记录-2026-09-21.md` 第 6 节的修复清单。

**2026-09-21 16:0x 补充**：该清单中第 2 条（drain 竞态）与第 3 条（避免空任务）在当前源码里已有对应实现（`restart(drain_grace)` 推迟拆机：`src/pipeline.rs:157`；`try_start` 不再提前置 `audio_active`：`src/pipeline.rs:398-408`），且 **16:07:32 已经重新构建出 `target/release` 产物**。但重跑条件仍不完整：`candidate-control/` 里的副本**没有同步**（仍是 13:07:05），运行中的 PID 23776 也还在跑旧二进制；同时**没有任何构建记录**能把 16:07:32 的产物绑定到某个 commit，第 4 条（`send-replay-pcm.ps1` 的 31.6 ms/块节奏）本轮**未改**。因此重跑前仍需：记录构建对应的 commit 与 SHA-256 → 同步 `candidate-control/` → 重启隔离引擎 → 修正发送节奏，然后才重跑。

## 未执行或被环境阻塞的验收

| 用例 | 状态 | 原因 |
|---|---|---|
| 真实百炼音频转写、错误 Key、地域/模型诊断 | 未通过（T01 失败；其余未执行） | 首次片段转写失败；错误 Key 与地域/模型诊断尚未执行 |
| 热词创建、更新及实际生效 | **机制通过 / 专名纠正实效未通过** | 不是“未执行”：2026-09-21 18:0x 已实现并用**真实百炼调用**验收。`run-task` 携带 `input.context`、运行中 `continue-task` 更新、未变化不重发、400 字符分轮、管理页告警与状态显示**全部实测通过**；但同一 60 秒素材上 `铨洲智造` **没有**被纠正（实验组与对照组字幕逐字相同，另 4 个上下文变体同样无变化）。原始字幕两组见 `docs/IMPLEMENTATION_STATUS.md` 的「R10 热词（上下文增强）实现与真实对照实验」与工作区 `.hotword-evidence-2026-09-21.md` |
| OBS 滤镜、Dock、浏览器源安装 | 阻塞 | 本机缺少 OBS SDK 源码头文件（`build/plugin-sdk/obs-studio` 未克隆、`obs.lib` 未生成），C 插件包尚未生成，`release/` 为空 |
| 无线麦克风试音、设备拔插恢复 | 未执行 | 没有接入目标设备 |
| 录播人工标注、准确率与长时间运行 | 未执行 | 缺少标注样本和真实云端调用 |
| 云端断线自动恢复（T11） | **本轮已实现（代码级，未编译验证运行）** | 2026-09-21 16:0x 更正：`watch()` 已新增 provider 会话结束检测并重启（`src/pipeline.rs:348-366`），配 `provider_restart_backoff`（连接过 2 s / 从未连接 10 s，`src/pipeline.rs:45-56`）与 `saw_llm_connected` 握手期门控（`src/pipeline.rs:265-269`）。**仍需一次运行时确认**，且该代码不在 13:07 的二进制里 |
| 输入音量表用于现场校准 | **本轮已实现（代码级，未在浏览器/OBS 验证）** | 2026-09-21 16:0x 更正：已加 1 秒轮询（`admin/app.js:679,794-800`，隐藏标签页暂停 `:797`）与 `#input-level-rms` 原始 RMS 数值读数 + 阈值/预设对照（`admin/index.html:136`、`admin/app.js:684-697`）。原“不成立”结论适用于 16:0x 之前的代码 |
| 清屏时长可配置（R06） | **本轮已实现（代码级，未在浏览器验证）** | 2026-09-21 16:0x 更正：新增 `overlay.clear_after_ms`（默认 4000，钳制 1000–15000：`src/config.rs:122-137,278-279`），管理页提供 2–8 秒选项（`admin/index.html:208-218`），overlay 读取并二次钳制（`overlay/app.js:25-26,364-368`），`/api/config` 与 WS 推送均下发钳制值（`src/server.rs:225-231,630`）。原“未实现”结论适用于 16:0x 之前的代码 |
| “重启即开无音频云任务” | **部分修复（代码级，未编译验证运行）** | 2026-09-21 16:0x：`try_start` 已不再置 `audio_active=true`（`src/pipeline.rs:398-408`），改由首帧真实音频置真（`src/pipeline.rs:437-445`），并新增 2 秒停滞检测 `AUDIO_STALL_AFTER`（`src/pipeline.rs:63-88,328-338`）。但 provider 仍在会话建立时立即 `run-task`（`src/llm.rs:1124-1135`，本轮未改），空会话尚未完全消除 |
| 三档过滤预设的实际降噪效果 | **未成立（结论未变）** | 预设只改本地 `silence_rms`（`admin/app.js:343-355`），不影响送往云端的音频；`speech_noise_threshold` 仍无任何预设或界面入口。本轮只增加了可手工编辑的 `silence_rms` 与「自定义」档，未改变这一结论 |
| 门控的 200 ms 句首预留 / 300 ms 句尾尾音 / Music 帧静音补偿 | **未实现（结论未变）** | `src/pipeline.rs` 与 `src/vad.rs` 中仍无 200/300 ms 预留；`Music` 帧仍直接丢弃、不补同长度静音（`src/pipeline.rs:451-453`） |
| `artifacts/acceptance/<候选版本>/` 证据目录 | 未建立 | 目录不存在；`build/replay-samples/` 同样不存在（2026-09-21 16:0x 复核，结论未变） |

这些未执行项不应视为通过，也不能由模拟或 mock 结果替代。**本轮新增的“代码级已实现”同样不等于通过**：它们没有构建产物、没有运行时证据，不能写进任何验收结论。本轮唯一新增的通过项是离线可复现的 **Rust 单元测试（27 项那次运行；树上现为 30 项待跑）**与 **`node --check` 语法检查**。

## 2026-09-21 18:0x 追加：R10 热词（上下文增强）真实验收

| 项目 | 结果 | 证据 |
|---|---|---|
| `run-task` 携带 `input.context` | **通过（真实调用）** | `/api/status.hotwords`：`mode=run-task`、`delivered=true`、`round_texts=["铨洲智造 区域赛"]`；`.hotword-e2e.json` |
| 运行中更新走 `continue-task`（**不是**“下次会话生效”） | **通过（真实调用）** | 回放中保存新词表 → `mode=continue-task`、`delivered_count=3`，`llm_connected` 全程 `true`，会话未重启；`.hotword-live.json` |
| 不变更不重发 | **通过** | 重复保存同表 → `skipped_unchanged` 1→2，`last_result="热词未变化，未重发（3 个热词 / 1 轮）"` |
| 400 字符/轮 与 5 轮上限切分 | **通过** | 60 个 5 字热词 → `rounds=2`，每轮 ≤ 400 字符，`dropped=0` |
| 词形规范可见告警 | **通过** | 20 字中文 → “规范上限 15 个”；8 片段英文 → “规范上限 7 个”（不阻断保存） |
| 管理页契约 | **通过** | `/admin` 含 `hotword-card`/`hotwords`/`hotword-status-text`；`/api/config` 与 `/api/status` 均返回 `hotword_status` |
| 真实转写链路未被破坏 | **通过（真实调用）** | 热词为空与有词表两种配置下，同一 60 秒素材各出 **8 条 final** 中文 |
| **目标专名纠正（`铨洲智造`）** | **未通过（实测未出现）** | 同素材同模型的 5 组 `input.context` 变体（含 5 个竞争拼写、官方 user+assistant 成对形状、`fun-asr-realtime-2025-11-07`）产出字幕**逐字相同**，`铨洲智造` 一次都没出现。对照组为「全球制造」/「泉州制造」 |
| `input.context` 是否被服务端解析 | **通过（对照）** | 把 `input.context` 发成字符串（非数组）→ 立即 `task-failed: CLIENT_ERROR`；发成官方数组形状 → `task-started` 正常。说明字段被校验但未被模型使用 |

- 产物：`candidate-control/stream-live-translate.exe` = 3,740,672 B，
  SHA-256 `9B93B8568E61BAF692EE061A069F5274324FABD3E11F1E3830212D44A13E9A71`；
  旧引擎备份 `candidate-control/stream-live-translate.D53D1055.bak` =
  `D53D10558F3C7E978535E9CC9FA937AB65A194E01828DC03BC190C62ED211AAC`。
- `cargo test --bin stream-live-translate`：**48 passed / 0 failed**。
- **未执行**：预编译热词（`vocabulary_id`）、`qwen-audio-3.0-asr-flash-streaming` 通道、
  真实 OBS 内的管理页交互、真实麦克风。
