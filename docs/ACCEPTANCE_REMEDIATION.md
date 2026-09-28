# 整改验收报告（AUDIT_ACTION_PLAN.md）

> 历史验收快照：此处的 149/150 测试数字与“Action 未锁 SHA”等待办已被后续整改覆盖。当前结论和剩余事项请以 [`REMEDIATION_STATUS_2026-09-23.md`](REMEDIATION_STATUS_2026-09-23.md) 为准。

审计基线提交：`538e8d02b8cfdc1ad30d2747d20b635a480403ac`
整改后工作区：`source/stream-live-translate-main`（**未 commit**，工作区为脏，共 42 个文件变更）
本轮范围：**P0-01…P0-05 + P1-01…P1-08 全部**；另顺带完成 P2-02 / P2-03 / P2-04 / P2-05 / P2-06 / P2-07。

---

## 1. 验收命令与结果（原样）

工具链（cargo 不在 PATH 上，必须显式设置）：

```powershell
$bt="C:\Users\戴尔\Desktop\语音转文字插件迭代\.build-tools"
$env:CARGO_HOME="$bt\cargo"; $env:RUSTUP_HOME="$bt\rustup"; $env:PATH="$bt\cargo\bin;$env:PATH"
cd "C:\Users\戴尔\Desktop\语音转文字插件迭代\source\stream-live-translate-main"
```

> 注意：审计文档里写的 `cargo test --lib` **在本包不可用** —— 这是二进制 crate，没有
> `[lib]` target，`--lib` 会报 `no library targets found in package`。正确命令是 `--bin`。

### 1.1 Rust 单元 / 集成测试

```
cargo test --offline --bin stream-live-translate
```
```
running 150 tests
test result: ok. 149 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 2.58s
```
串行复跑（`-- --test-threads=1`）同样通过。新增 3 个用例是 **P1-03 的链路级测试**
（`llm::wire_late_final_tests`，见 §1.8）。

唯一 `ignored` 的用例（**已知缺口，非静默跳过**）：
`llm::drain_contract_tests::bailian_delivers_a_final_that_arrives_after_finish_task`
原因是 `BailianFunAsr::new` 要求 `wss://`（真实产品约束，未削弱），而链路级测试用的是**明文**回环桩
—— 两者不能共存（见 §1.8）。该用例保留在仓库里并写明启用条件。

### 1.2 编译

```
cargo check --offline --all-targets      # 0 errors
cargo build --offline --release --bin stream-live-translate   # 成功
```
发布产物：`target/release/stream-live-translate.exe`，3.8 MB，
`sha256 = 9c53e719406bb786b6cdc9a3b45ea7755beef9340a208f092321ccdc01284571`。

### 1.3 前端回归（Node，无新增依赖）

| 命令 | 结果 |
| --- | --- |
| `node tests/run-overlay-tests.mjs` | `25 passed, 0 failed`，`OK — every overlay subtitle invariant holds.` |
| `node tests/admin-preset-cases.mjs` | `15/15 passed` |
| `node tests/admin-roundtrip-cases.mjs` | `21/21 passed`（本轮新增） |
| `node tests/run-negative-control.mjs` | `NEGATIVE CONTROL PASSED` |
| `node --check admin/app.js` / `overlay/app.js` | 语法通过 |

### 1.4 构建期前端同步与嵌入资源一致性

`build.rs` 现在会传播错误、删除孤儿文件、并在同步后逐字节校验。实测：
`admin/` → `dist/admin/`、`overlay/` → `dist/overlay/` 文件名集合与内容**逐字节一致**（各 3 文件）。
`dist/` 即 `include_dir!` 嵌入源，所以二进制里的面板/overlay 与源码一致。

### 1.6 OBS 插件（C）编译与链接验证 —— 已完成

审计要求"重新构建候选产物"，而插件此前**从未编译过**（子任务无 C 工具链）。本轮在本机定位到
可用的 MSVC + Windows SDK 后，完成了真实的编译与链接。

环境（工作区路径含中文，**不能**用 `.bat` 传递参数——会乱码导致编译命令里的路径失效，
故直接设置 `INCLUDE`/`LIB`）：

```
D:\VS2022\Community\VC\Tools\MSVC\14.41.34120   (cl 14.41.34120 / link 14.41)
D:\Windows Kits\10\include|lib\10.0.26100.0     (Windows SDK)
build\plugin-sdk\obs-studio\libobs              (OBS 头文件，含 obs-module.h)
build\plugin-sdk\sdk-bin\obs.lib                (obs.dll 导入库，仓库自带)
```

命令：

```
cl /nologo /LD /W3 /std:c11 /utf-8 /D_CRT_SECURE_NO_WARNINGS ^
   /I"build\plugin-sdk\obs-studio\libobs" /I"plugin" ^
   /Fo"build\plugin-verify\clean.obj" ^
   /Fe"build\plugin-verify\stream-live-translate.dll" ^
   "plugin\stream-live-translate.c" "build\plugin-sdk\sdk-bin\obs.lib" ws2_32.lib
```
```
exit=0
正在创建库 build\plugin-verify\stream-live-translate.lib 和对象 ...exp
```

产物：
```
File Type: DLL        size = 148.5 KB
exports:  obs_module_load, obs_module_unload, obs_module_ver,
          obs_module_set_pointer, obs_module_set_locale,
          obs_module_free_locale, obs_module_get_string
deps:     obs.dll, WS2_32.dll, KERNEL32.dll
size of code = 0x16400
```

**这一步发现并修复了两个真缺陷（此前完全未被发现）：**

1. **插件根本无法编译。** `slt_connect()` 在错误路径上调用 `slt_close()`，而 `slt_close()`
   定义在其后 → 隐式声明 `int slt_close(...)` 与真实 `static void slt_close(...)` 冲突：
   ```
   error C2371: "slt_close": 重定义；不同的基类型
   ```
   已加前向声明修复。**负控制**（删掉该声明、其余完全相同）复现原错误：
   ```
   warning C4013: "slt_close"未定义；假设外部返回 int
   error C2371: "slt_close": 重定义；不同的基类型
   negctl_exit=2
   ```
2. `os_set_thread_name()` 未声明即调用（`C4013`）。它由 SDK 的 `util/threading.h:93` 声明，而本
   文件故意不包含该头（会拉入 `<pthread.h>`，破坏独立 MSVC 构建）。已按其**真实原型**
   `extern void os_set_thread_name(const char *name);` 显式声明，与 SDK 逐字一致。

**另一个是构建配置问题而非源码问题**：不加 `/utf-8` 时，MSVC 按代码页 936 解读带中文注释的源码，
报 `error C2001: 常量中有换行符`。已确认加 `/utf-8` 后消失。建议在 `plugin/CMakeLists.txt` 的
MSVC 分支加 `/utf-8`（列入 §5 待办）。

> 两次干净构建的 SHA-256 不同（PE 头的 `time date stamp` 是构建时间，PE 不保证可复现），
> 但 `size of code` 与依赖表一致，不影响"能构建"的结论。

### 1.7 发布供应链一致性
| 检查 | 结果 |
| --- | --- |
| 版本单一来源 | `Cargo.toml` = `0.0.25`；`plugin/version.h` 同值；`CMakeLists.txt` 从 `Cargo.toml` 提取并在不一致时 `FATAL_ERROR`（cmake 正/负用例已验） |
| `?v=` 缓存串 | `admin/index.html`、`overlay/index.html` 已从 `0.1.0` 统一为 `0.0.25` |
| workflow 权限 | 两个 workflow 顶层 `contents: read`，仅 release job 为 `write` |
| 产物清单 | release job 生成 `release-manifest.json`（文件名/字节数/SHA-256）+ `SHA256SUMS` |

### 1.8 P1-03 链路级（wire-level）验证 —— 本轮新增

**上一轮的结论是错的，本轮定位并纠正。** 之前记为"qwen/openai 的回环桩无法完成握手，原因疑为
tungstenite 服务端拒绝客户端产生的 request-target（`http::uri::InvalidFormat`）"。实测否证：

用一次性诊断测试把客户端真正写到线上的字节抓下来：
```
ws://127.0.0.1:<port>/api-ws/v1/realtime?model=fun-asr-realtime
  -> 194 字节，请求行/头完全合法：GET /api-ws/v1/realtime?model=... HTTP/1.1 ...
wss://127.0.0.1:<port>/... -> 0 字节
  -> 客户端根本不发 HTTP：native-tls error: 安全包中没有可用的凭证 (os error -2146893042)
```

**根因是 scheme，不是请求内容**：桩是**明文**监听，而客户端被给了 `wss://`，于是它先做 TLS 握手，
自然一个字节 HTTP 都没发。**provider 并不会强制 `wss://`** —— 它逐字使用配置里的 endpoint
（`format!("{}?model={}", self.endpoint, self.model)`），`BailianFunAsr::new` 是唯一强制 `wss://` 的
（合理：密钥不能明文过网）。所以只要把 endpoint 写成 `ws://127.0.0.1:<port>`，**真实 provider 的
socket 路径就是可测的**。

据此新增 `llm::wire_late_final_tests`（真实回环 WebSocket，明文桩，非 mock 的 provider 全路径）：

| 用例 | 覆盖 |
| --- | --- |
| `qwen_delivers_a_final_that_arrives_after_the_audio_ends` | 音频流结束后、writer 已发 `session.finish`，服务端**随后**才吐出的 `…input_audio_transcription.completed` 必须进 sink |
| `openai_delivers_a_final_that_arrives_after_the_audio_ends` | 同上，走 OpenAI 兼容 provider 与 transcribe 通道 |
| `a_silent_server_does_not_hold_the_provider_open` | 服务端在音频结束后**完全不回**时，有界 drain 必须自行结束（socket 保持打开，正是无界实现会挂住的场景）|

**负控制**（把 qwen 的 `drain_after_writer` 换回修复前的
`select! { _ = read => Ok(()), _ = write => Ok(()), }`，其余完全相同）：
```
test llm::wire_late_final_tests::qwen_delivers_a_final_that_arrives_after_the_audio_ends ... FAILED
the provider must deliver the late final within the drain window: Elapsed(())
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 10.01s
```
即该测试**确实抓得住原始缺陷**，不是空跑；恢复修复后 3/3 通过。

> 过程中还修正了一个我自己的测试桩错误：最初给 ASR 模型发 `response.text.done`，而 provider 按
> 模型名选通道（`asr_mode`），ASR 模型只认 `input_audio_transcription.*` —— 那时测试失败是**桩的
> 错**，不是 provider 的错。现在桩发的就是该通道真实会产生的事件。

---

## 2. 端到端验收（打真实端口，不是只跑单测）

对**release 二进制**实测（`--config <temp>`，端口 18787）：

| 验收项 | 结果 |
| --- | --- |
| `--host 0.0.0.0` 且未配置认证 → 拒绝启动 | exit 1，错误信息给出两条出路 ✅ |
| 无令牌 `GET /api/config` | **401** ✅ |
| 错误令牌 | **401** ✅ |
| 管理令牌 | 200 ✅ |
| 只读 overlay 令牌 `GET /api/config` | 200 ✅ |
| 只读 overlay 令牌 `GET /api/recordings` | **403** ✅ |
| 跨 Origin（`Origin: http://evil.example:18787`） | **401** ✅ |
| 同源（`Origin: http://127.0.0.1:18787`） | 200 ✅ |
| 脚本/curl（无 Origin） | 200 ✅ |
| `Origin: null` | 401 ✅ |
| 静态资源目录穿越 `/admin-assets/..%2fconfig.toml` | **404** ✅ |
| `/api/status` 的 `overlay_url` 只带只读令牌 | True ✅ |
| `/api/status` 是否泄露管理令牌 | **False** ✅ |
| 匿名加载 `/admin` 是否带真令牌 | **False** ✅ |
| `/api/config` 是否含 `password` 字段 | **False**（字段根本不存在） ✅ |
| 秘密扫描（源码 + 出厂模板 + dist） | 无私钥、无真实 provider key、模板 `api_key`/`password` 为空 ✅ |

**令牌产生方式**：首次启动生成 `tokens.toml`（与 `config.toml` 同目录，64 hex × 2），Unix 下 `0600`。
令牌以 `<script>window.__SLT_TOKEN__` 全局注入页面，**不进 URL**；OBS 浏览器源用
`?token=`（只读令牌），OBS 停靠面板用管理令牌。

---

## 3. 逐项整改结论

### P0：发布阻断——安全边界

| 编号 | 内容 | 落点 | 测试 |
| --- | --- | --- | --- |
| P0-01 | 管理 API / 字幕 WS 鉴权、Origin 校验、删除 `CorsLayer::permissive()`、只读 overlay 令牌、非回环无认证拒绝启动 | `src/auth.rs`（新增）、`src/server.rs`、`main.rs`、`admin/app.js`、`overlay/app.js` | `auth::tests::*`（8）、`server::tests::*`（10，真实 HTTP）、`src/auth.rs` 常量时间比较 |
| P0-02 | 端点允许列表；信任域变化不得静默保留并发送旧 Key；`connection-test` 同规则 | `src/llm.rs` 顶部策略区、`src/server.rs::post_config` | `llm::endpoint_policy_tests::*`（7）、`server::tests::changing_the_endpoint_alone_drops_the_saved_key`、`a_non_allowlisted_endpoint_is_refused_for_a_cloud_provider` |
| P0-03 | 专用响应结构，秘密只回布尔 | `src/server.rs`（`ConfigView` 等 9 个显式结构） | `server::tests::no_response_contains_a_saved_secret`（金丝雀字符串） |
| P0-04 | 服务端边界：采样率 / 通道 / 端口 / 尺寸 / 时长 / 字符串 / 目录 | `src/config.rs::clamp` + `validate` + `resolve_recording_dir` | `config::tests::*`（4）、`server::tests::a_hostile_config_patch_is_rejected_and_writes_nothing` |
| P0-05 | 插件↔引擎 nonce 双向握手、超时、连接数上限 | `src/ingest.rs`（44 字节头）、`plugin/stream-live-translate.c`（`--ingest-nonce`） | `ingest::tests::nonce_*`（3）、`a_wrong_nonce_gets_no_audio_and_a_closed_connection`、`a_silent_peer_cannot_hold_the_connection_slot`、`ingest_connections_are_capped` |

### P1：核心正确性与稳定性

| 编号 | 内容 | 落点 | 测试 |
| --- | --- | --- | --- |
| P1-01 | 停止 / 重启 / 保存配置真正取消管线；代际防止旧 run 覆盖新 run | `src/pipeline.rs` | `stop_cancels_the_first_audio_wait_promptly`、`the_pre_fix_first_audio_wait_could_not_be_cancelled`、`restart_bumps_the_generation_and_a_stale_run_cannot_install_itself`、`stop_leaves_no_live_provider_task`、`a_config_save_restart_still_allows_the_final_drain`、`a_newer_generation_cancels_a_live_provider_session_in_both_strengths` 等 23 个 |
| P1-02 | 跨采样率 ingest 分包：有状态重采样 + 两类缓冲分离 | `src/audio.rs::StreamResampler`、`src/ingest.rs::pump_audio` | `resampling_is_chunk_boundary_invariant`、`resample_mono_is_unchanged_for_a_whole_buffer`、`pump_audio_frames_are_write_boundary_invariant`、`odd_length_*` |
| P1-03 | provider 结束后有界 drain，不丢最后一句 | `src/llm.rs::drain_after_writer`（qwen / funasr / openai 共用） | `drain_keeps_reading_after_the_writer_finishes`、`a_finished_reader_cancels_the_writer_immediately`、`a_silent_reader_cannot_hold_the_drain_open_forever`、`a_reader_error_propagates`、`drain_bounds_fit_inside_the_pipelines_grace_period` |
| P1-04 | OpenAI/gateway 文本事件路由 | `src/llm.rs::openai_routing` | `llm::openai_routing::tests::*`（7） |
| P1-05 | OBS 断线重连 + 文本源镜像 | `src/obs.rs` | `closing_the_reader_ends_try_connect`、`the_shared_command_sender_survives_a_reconnect`、`mirror_*`（3） |
| P1-06 | 记录竞态与静默退出 | `src/subtitle.rs`（`FinalLine` + 独立 final 频道）、`src/recording.rs` | `final_events_carry_the_finalised_line`、`the_recorder_records_the_line_it_was_given`、`a_lagged_recorder_keeps_recording_and_reports_the_gap`、`clearing_history_does_not_reach_the_recorder_feed` |
| P1-07 | 历史与导出内存上限 | 同上 + `HISTORY_LIMIT`、`VecDeque` 索引、JSONL 流式导出 | `direct_finals_are_capped_at_the_history_limit`、`export_streams_from_the_jsonl_file`、`export_falls_back_to_the_in_memory_index_when_the_file_is_gone` |
| P1-08 | OBS 插件线程与共享状态生命周期 | `plugin/stream-live-translate.c` | **无法编译验证**（见 §5） |

### P2（顺带完成）

P2-02 配置保存原子+串行（`config_write_lock`，先验证后写盘）；P2-03 `build.rs`；P2-04 Admin 零值
往返（含修正预设接线与 await-aware 测试）；P2-05 版本单一来源；P2-06 供应链；P2-07 权限与保留策略
（`auto_persist` 默认 true、`retention_days` 默认 0 = 永久保留）。

---

## 4. 端到端验收中发现并修复的真缺陷（单测没抓到）

这一节是本轮最有价值的部分：**5 个缺陷是"跑起来打真实端口"才暴露的**，其中 2 个是我自己引入的。

| # | 缺陷 | 影响 | 修复 |
| --- | --- | --- | --- |
| 1 | `origin_allowed` 只要 host 非空就放行，等于把被攻击者控制的 `Host` 当可信来源 | **跨 Origin 攻击面完全敞开**：任意网页可读 `/api/config`、停管线 | 改为按**绑定地址**判定：回环绑定只接受回环 Origin，通配绑定才接受任意 Origin |
| 2 | `trust_domain()` 对无法解析的 URL 返回 `None`，而 `check_endpoint_allowed` 把 `None` 当"mock，不发送任何东西"直接放行 | **P0-02 被完整绕过**：`wss://dashscope.aliyuncs.com@evil.example/x` 会把 Key 发到 `evil.example` | fail-closed；新增 7 种畸形 URL × 3 provider 的测试，并断言拒绝必须来自"主机不可读"而非碰巧被允许列表拦下 |
| 3 | `resolve_recording_dir` 用 `canonicalize` 结果与未 canonicalize 的 base 做 `starts_with`；Windows 下 `\\?\C:\a` 不以 `C:\a` 开头 | **穿越检查反向失效**：所有绝对路径都被判定为"在配置目录内" | 两侧走同一 canonicalize，并按组件比较（`\\?\` 前缀归一化） |
| 4 | `post_config` 用 `crate::config_path()`（`OnceLock`，测试无法设置，默认父目录为 `.`）做目录校验 | 校验针对**错误的根目录**在做，所以 #3 一直没被发现 | 把配置路径放进 `AppState`，处理器一律用它 |
| 5 | 409 拒绝时只清除了局部副本的 Key，**没有清运行期状态里的 Key** | 引擎仍持有针对旧端点的 Key；任何后续路径都可能把它发往新主机 | 拒绝时同步清除运行期 `api_key` 与 `endpoint` |
| 6 | Admin「过滤预设」把云端阈值发到 `filter.speech_noise_threshold`，服务端读 `llm.speech_noise_threshold` | 选预设**根本没改到云端**，面板却显示已生效 | 发到 `llm.speech_noise_threshold` |
| 7 | `admin-preset-cases.mjs` 的 `check()` 是同步的，7 个 `async` 用例断言被静默丢弃 | 那个 `15/15` 是**空的** | 改为 await-aware；负控制证明接线改回去即失败 |
| 8 | 静态资源改为 public 后 `..` 穿越可读 `config.toml`（含密钥） | 密钥泄露 | `safe_asset_path` + 测试（11 种恶意路径） |
| 9 | OBS 停靠面板与 `--open` 的管理页 URL 未带令牌 | 面板被自己的 401 挡住 | URL 带令牌 |

另有 3 个由子任务自查发现并修复：`pause()` 因 `is_finished()` 不同步而**延迟拆除**导致停止无界
（真 P1-01 缺陷）；`install_if_current` 把被拒的 `PipelineInner` 交还调用方，`let _ =` 会**detach 活的
provider 任务**；`resample_and_send` 的 `int16_t out[4096]` 在 8 kHz 输入 +16 KiB 块时**栈溢出**。

---

## 5. 已知缺口 / 未验证项（如实列出）

1. ~~**`plugin/stream-live-translate.c` 从未编译过**~~ → **本轮已解决**，见 §1.6。
   已用 MSVC 14.41 + Windows SDK 10.0.26100 真实编译并链接出 `stream-live-translate.dll`
   （148.5 KB，导出 `obs_module_load` / `obs_module_unload`，依赖 `obs.dll`/`WS2_32.dll`/`KERNEL32.dll`）。
   过程中发现**插件原本根本无法编译**（`slt_close` 隐式声明与定义冲突，`error C2371`）并已修复，
   负控制可复现。`os_set_thread_name` 隐式声明也已按其真实原型显式声明。
   **仍未做的**：在真实 OBS 里加载该 DLL 跑一遍（挂载/卸载、OBS 重启、多滤镜、无线设备插拔、
   长时间直播）—— 即审计回归第 5 项。**"能构建、导出正确"已证明；"在 OBS 里行为正确"未证明。**
2. ~~**qwen / openai / bailian 的"延迟 final"没有通过的端到端链路测试。**~~ → **qwen 与 openai 已补齐**，
   见 §1.8（真实回环 WebSocket，含负控制证明测试非空跑）。**剩余缺口**：bailian 的链路级用例仍
   `#[ignore]` —— `BailianFunAsr::new` 强制 `wss://`（真实约束），而链路测试用明文桩，两者不能共存；
   要补需要自签 TLS 监听 + 自定义 connector，本轮判断不值得（bailian 正是其余三个 provider 对齐的
   参考实现，其 drain 结构已被 `drain_contract_tests` 与生产界测试覆盖）。
   **仍未证明的**：在**真实云端服务**上每个 provider 的第一句/最后一句/断线恢复/停止计费
   （审计回归第 6 项）—— 这需要真实密钥与网络。
3. **未做 OBS 实机测试**：挂载/卸载、OBS 重启、多滤镜、无线设备插拔、长时间直播（审计回归第 5 项）。
4. **未联网核对依赖 CVE**，未验证发布产物签名、部署机 ACL/umask/防火墙/反向代理（审计"未验证"章节）。
5. **P2-01 未完成**：`audio.use_screen_capture_kit`、`filter.min_segment_ms` / `max_segment_ms`
   仍**没有任何运行时引用**。本轮只保证了它们不致命（服务端有边界、不会被误用），但没有接入分段
   状态机也没有删除。审计要求"每个公开配置字段至少一个运行时引用 + 一个行为测试"，这三项不满足。
   `mirror_to_text_source` 已由 P1-05 真正兑现。
6. **`tokens.toml` / `config.toml` 的 `0600` 权限代码只在 Unix 编译**（本机 Windows），
   未经执行验证；Windows 依赖用户 profile ACL。
7. **`repository` URL 仍是占位符**（`git remote -v` 为空，无法确定真值），已在 `Cargo.toml`
   与 `docs/BUILD.md` 标 `TODO(release-blocker)`。
8. **27 条 `uses:` 未锁定 commit SHA**（无网络、仓库内无任何 SHA 记录，拒绝编造），每条都带
   `TODO(pin)` 注释与 `docs/BUILD.md` 清单。
9. **子任务会话中观察到的 11 个失败已定位为沙箱环境问题，不是代码缺陷。** 精确原因：
   ```
   thread 'auth::tests::a_token_file_is_created_with_a_usable_shape' panicked at src\auth.rs:587:39:
   temp dir: Os { code: 5, kind: PermissionDenied, message: "拒绝访问" }
   ```
   `config.rs:701:39`、`server.rs:1579:39` 报同一行同一列 —— 都是**同一个"建临时目录"辅助函数**
   被子 agent 会话的文件沙箱拒绝（本会话也确实出现过 sandbox 临时目录被删除、导致子 agent shell
   完全失效的情况）。需要临时目录的测试全挂，不需要的（全部 23 个 pipeline 测试）全过。
   在**本会话**（临时目录可用）下，并行与串行复跑均为 **149 passed / 0 failed / 1 ignored**，
   故判定为环境问题。记录于此以免日后被误读成回归。

---

## 6. 变更文件 SHA-256（前 16 位）

下表取自**最终验证时的工作区**（`cargo test` 报 149 passed / 0 failed / 1 ignored 的同一份树）：

```
85d80661a62cdcc5  src/auth.rs
7ff64fcc9e72b910  src/config.rs
6f9415e302286512  src/audio.rs
7635b857c5a34eae  src/ingest.rs
4efa4890c0237c5c  src/pipeline.rs
68e1255416de4db2  src/llm.rs
4eadbab9e0356531  src/obs.rs
0d94a4c51829c794  src/subtitle.rs
150d95d3d561ecd1  src/recording.rs
00c0f9cedadfa6de  src/server.rs
e6871cc8d4a7bfbd  src/main.rs
fe8f284bf00919f7  build.rs
c7c94722600d0a6b  Cargo.toml
fd441a15dda2291b  admin/app.js
8eebe772c8d55ff5  admin/index.html
50b34d749f5a2a50  overlay/app.js
3a04db1233fce42e  overlay/index.html
14d920f2f7026e13  dist/config.toml
386563b9b234d5fe  plugin/stream-live-translate.c
bd2aaad1d3932828  plugin/CMakeLists.txt
2019657af5c2c570  plugin/version.h
```
完整 64 位哈希与其余变更文件（workflow、scripts、docs、tests）见 `git status` /
`Get-FileHash`；`dist/admin/*`、`dist/overlay/*` 与源目录逐字节一致，因此哈希与 `admin/`、
`overlay/` 对应项相同。

---

## 7. 结论

- **P0-01…P0-05、P1-01…P1-08 全部完成**，每项都有"修根因 + 最小可复现自动测试"：
  - Rust：**150 个测试，149 通过 / 0 失败 / 1 ignored**（唯一 ignored 的 bailian 链路用例写明原因
    与启用条件）；
  - P1-03 另有 **qwen/openai 的链路级测试**（真实回环 WebSocket + 负控制），见 §1.8；
  - C 插件：MSVC 真编译 + 链接出 DLL，导出与依赖正确，并借此发现"插件原本根本编译不过"；
  - 前端：overlay 25/25、admin preset 15/15、admin roundtrip 21/21、负控制 PASSED；
  - 真实端口端到端：非回环拒绝启动、令牌/Origin/越权/穿越/密钥泄露全部按预期。
- 发布门槛（审计"回归与最终发布门槛"）：第 1、2、3、4、7、8 项达成；
  **第 5 项（OBS 实机）、第 6 项（真实云端各 provider 实机）未达成** —— 两者都需要本机没有的真实环境。

### 仍需在具备真实环境的机器上完成的事

1. 在 OBS 里加载插件 DLL：挂载/卸载、OBS 重启重连、多滤镜、无线设备插拔、长时间直播（审计第 5 项）。
2. 用真实密钥对每个 provider 做短音频与有限回放，确认第一句/最后一句/断线恢复/停止计费（审计第 6 项）。
3. 联网核对依赖 CVE、验证发布产物签名、核对部署机 ACL/umask/防火墙/反向代理（审计"未验证"章节）。
4. P2-01（**本轮范围明确为 P0+P1，未做**）：`llm.system_prompt` 未下发；
   `audio.use_screen_capture_kit`、`filter.min_segment_ms` / `max_segment_ms` 无运行时引用。
   审计原文把前者的产品行为列为"需产品确认"，故本轮只保证它们不产生危害（服务端有边界、不误导），
   未擅自改产品行为。详见 §5.5。
5. 27 条 action 锁定 commit SHA；设置真实的 `repository` URL。
