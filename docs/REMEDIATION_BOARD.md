# 整改任务看板（AUDIT_ACTION_PLAN.md）

> 此看板保留整改过程记录；后续补修与最新复验见 [`REMEDIATION_STATUS_2026-09-23.md`](REMEDIATION_STATUS_2026-09-23.md)。下方旧测试数字和部分待办不再代表当前状态。

基线提交：`538e8d02b8cfdc1ad30d2747d20b635a480403ac`（`src/` 为 `source/stream-live-translate-main`）
本轮范围：**P0-01…P0-05 + P1-01…P1-08**（P0/P1 全部）；部分 P2 顺带处理（见下）。

## 工具链（cargo 不在 PATH 上，每条命令都要先设）

```powershell
$bt="C:\Users\戴尔\Desktop\语音转文字插件迭代\.build-tools"
$env:CARGO_HOME="$bt\cargo"; $env:RUSTUP_HOME="$bt\rustup"; $env:PATH="$bt\cargo\bin;$env:PATH"
cd "C:\Users\戴尔\Desktop\语音转文字插件迭代\source\stream-live-translate-main"
cargo test --offline --lib --bin stream-live-translate
```

## 文件归属（避免并发写冲突 —— 改文件前先看这里）

| 文件 | 负责人 |
| --- | --- |
| `src/auth.rs` | lead（已交付） |
| `src/server.rs` | lead（P0-01/P0-03/P0-04/P1-01 接口侧） |
| `src/main.rs` | lead |
| `src/config.rs` | lead |
| `src/llm.rs` — 顶部端点策略区 + `openai` 模块 | lead / 子任务 B（openai 模块） |
| `src/llm.rs` — `qwen` / `funasr` / `bailian` 模块 | 子任务 E（P1-03 drain） |
| `src/audio.rs`, `src/ingest.rs` | 子任务 A（P1-02 + nonce 校验） |
| `src/pipeline.rs` | 子任务 D（P1-01） |
| `src/obs.rs` | 子任务 F（P1-05） |
| `src/subtitle.rs`, `src/recording.rs` | 子任务 G（P1-06/P1-07/P2-07） |
| `plugin/stream-live-translate.c` | 子任务 C（P1-08） |
| `plugin/version.h`, `plugin/CMakeLists.txt`, `Cargo.toml`, `.github/workflows/*`, `scripts/*` | 子任务 H（P2-05/P2-06） |
| `build.rs` | 子任务 I（P2-03） |
| `admin/app.js`, `tests/admin-*` | 子任务 J（P2-04） |
| `admin/index.html`, `overlay/index.html` | lead |
| `admin/app.js`, `overlay/app.js` 的令牌接线 | lead（等子任务 J 交付后再动） |
| `tests/overlay-*`, `tests/run-*.mjs` | lead |

> 跨模块接口只能由 lead 改。任何人需要别的模块改签名，**在报告里写明，不要自己改**。

## 已交付的共享接口（其他任务按此对接）

### `src/auth.rs`（新增，P0-01）

```rust
pub enum Role { Admin, Overlay }
pub struct TokenPair { pub admin: String, pub overlay: String }
impl TokenPair { pub fn generate() -> Self; pub fn role_for(&self, presented: &str) -> Option<Role>; }
pub fn tokens_path(config_path: &Path) -> PathBuf;
pub fn load_or_create(config_path: &Path) -> std::io::Result<TokenPair>;
pub fn is_loopback_host(host: &str) -> bool;
pub enum BindPolicy { Allow, Refuse(String) }
pub fn bind_policy(host: &str, auth_configured: bool) -> BindPolicy;
pub enum Access { Public, Overlay, Admin }
pub fn required_access(method: &str, path: &str) -> Access;  // 默认 Admin
pub fn origin_allowed(origin: Option<&str>, bind_host: &str, bind_port: u16) -> bool;
```

### `src/config.rs`（P0-04）

```rust
pub fn resolve_recording_dir(config_path: &Path, recording_dir: &str) -> Result<PathBuf, String>;
impl Config {
    pub fn clamp(&mut self);
    pub fn validate(&self, config_path: &Path) -> Result<(), ValidationErrors>;
    pub fn normalise(&mut self, config_path: &Path) -> Result<(), ValidationErrors>;
}
```

**`recording.rs` 必须调用 `resolve_recording_dir`**，不要再自己拼路径：`recordings_dir()`
原来的实现允许 `recording_dir` 是任意绝对路径，会写到配置目录之外。

### `src/llm.rs` 端点策略（P0-02）

```rust
pub fn url_host(endpoint: &str) -> Option<String>;
pub fn endpoint_allowed(provider: &str, host: &str) -> bool;
pub fn trust_domain(cfg: &LlmConfig) -> Option<String>;
pub fn default_endpoint_host(provider: &str, workspace_id: &str) -> Option<&'static str>;
pub fn check_endpoint_allowed(cfg: &LlmConfig) -> Result<()>;  // build() 与 test_connection() 都已调用
```

### `AppState` 新增字段（`src/main.rs`）

```rust
pub tokens: crate::auth::TokenPair,
pub bind_addr: (String, u16),
pub auth_configured: bool,
pub llm_key_domain: parking_lot::Mutex<Option<String>>,
pub ingest_nonce: Option<Arc<str>>,          // --ingest-nonce / SLT_INGEST_NONCE
pub config_write_lock: parking_lot::Mutex<()>, // 配置保存串行化（P2-02）
```

### 需要别人实现的接口（**已在 `server.rs` 里按此调用，未实现会编译失败**）

```rust
// src/recording.rs —— 子任务 G 负责
impl RecordingStore {
    /// 删除本场磁盘记录；返回删除前的信息。内存索引同时清空。
    /// 与「清空历史」语义不同：这是删除证据，UI 必须分开。
    pub fn delete_disk(&self) -> std::io::Result<RecordingInfo>;
}
```

```rust
// src/ingest.rs —— 子任务 A 负责
// 12 字节头（"SLTA" + u32 rate + u32 format）之后，必须再收 32 字节 nonce，
// 与本进程启动参数一致才继续；不一致立即断开且一个 PCM 字节都不接收。
// 读头有超时（≤5 s），并发连接数有上限（≤2）。nonce 来自 state.ingest_nonce。
```

## 任务清单

**全部子任务已收尾。最终验收见 [`ACCEPTANCE_REMEDIATION.md`](ACCEPTANCE_REMEDIATION.md)。**

| 编号 | 内容 | 状态 |
| --- | --- | --- |
| P0-01 | 鉴权 / Origin / 只读令牌 / 非回环拒绝启动 | ✅ 完成（服务端 + 前端接线 + 真实端口验收） |
| P0-02 | 端点允许列表 + 信任域变更不复用旧 Key | ✅ 完成 |
| P0-03 | 专用响应结构，剔除秘密 | ✅ 完成 |
| P0-04 | 外部可写配置服务端校验 | ✅ 完成 |
| P0-05 | 插件↔引擎 nonce 握手 | ✅ 完成（Rust 有测试；**C 侧已 MSVC 真编译+链接出 DLL**） |
| P1-01 | stop/restart/保存配置真正取消当前管线 | ✅ 完成（23 个 pipeline 测试） |
| P1-02 | 跨采样率 ingest 分包 | ✅ 完成 |
| P1-03 | provider 结束时的最后一句 drain | ✅ 完成（共享 drain 契约 + **qwen/openai 链路级测试 + 负控制**） |
| P1-04 | OpenAI/gateway 文本事件路由 | ✅ 完成 |
| P1-05 | OBS 断线重连 + 文本源镜像 | ✅ 完成 |
| P1-06 | 字幕记录竞态与静默退出 | ✅ 完成 |
| P1-07 | 限制历史与导出内存 | ✅ 完成 |
| P1-08 | OBS 插件线程与共享状态生命周期 | ✅ 已编译链接验证（该项还发现"插件原本编译不过"）；⚠️ 未在真实 OBS 加载 |
| P2-01 | 删除或兑现无效配置（`min_segment_ms` / `max_segment_ms` / `use_screen_capture_kit`） | ❌ **未做**，见报告 §5.5 |
| P2-02 | 配置保存原子、串行 | ✅ 完成 |
| P2-03 | 构建时前端同步失败必须中止 | ✅ 完成 |
| P2-04 | Admin 合法零值往返 | ✅ 完成 |
| P2-05 | 统一版本与发布元数据 | ✅ 完成（`repository` URL 仍为占位符，待定） |
| P2-06 | 收紧发布供应链 | ⚠️ 30 条 Action 已锁 SHA；OBS SDK 自动下载仍缺校验材料 |
| P2-07 | 配置/录音权限与保留策略 | ✅ 完成（Unix 权限分支未在 Windows 上执行验证） |

## 最终测试数字

```
cargo test --offline --bin stream-live-translate
running 157 tests
test result: ok. 156 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out

node tests/run-overlay-tests.mjs        -> 25 passed, 0 failed
node tests/admin-preset-cases.mjs       -> 15/15 passed
node tests/admin-roundtrip-cases.mjs    -> 21/21 passed
node tests/run-negative-control.mjs     -> NEGATIVE CONTROL PASSED

OBS 插件: cl /LD + obs.lib -> stream-live-translate.dll (148.5 KB)
          exports obs_module_load / obs_module_unload / ...
          deps obs.dll, WS2_32.dll, KERNEL32.dll
```


## 验收纪律

1. 每个非平凡修复必须附一个**在旧代码上会失败**的最小自动测试。
2. 报告必须给出：改了什么、测试名、**原样粘贴的命令与输出**。
3. 不能验证的必须明说，不许写"应该可以"。
4. **不要 git commit**，留脏工作区。
5. 只改自己名下的文件。

## 负责人（lead）已执行的端到端验证

| 验证项 | 命令 / 方式 | 结果 |
| --- | --- | --- |
| 非回环绑定无认证必须拒绝启动 | `stream-live-translate.exe --host 0.0.0.0` | exit 1，错误信息给出两条出路 ✅ |
| 无令牌 / 错误令牌 / 只读令牌越权 | 真实 HTTP 打真实端口 | 401 / 401 / 403 ✅ |
| 跨 Origin（含 DNS rebinding 名、错端口、`null`） | 真实 HTTP `Origin:` 头 | 全部 401，日志有 `rejected cross-origin request` ✅ |
| 同源与脚本（无 Origin）仍可用 | `Origin: http://127.0.0.1:<port>` / 无头 | 200 ✅ |
| 令牌注入不泄露 | 匿名 / 管理 / overlay 三种加载 `/admin`、`/overlay` | 匿名不带任何真令牌；overlay 页只带只读令牌 ✅ |
| 静态资源目录穿越 | `/admin-assets/..%2f..%2fconfig.toml` | 404 ✅ |
| 响应体不含秘密 | `/api/config`、`/api/status` | 无 `password` 字段；`api_key` 为空串 + `api_key_set` 布尔 ✅ |
| 前端回归 | overlay / admin preset / admin roundtrip / 负控制 | 25/25、15/15、21/21、PASSED ✅ |
| 插件版本单一来源 | `cmake` configure 正/负用例（子任务 H） | `0.0.25`；不一致时 configure 失败 ✅ |
| 构建期前端同步 | 孤儿文件删除、复制失败中止（子任务 I） | 两条路径均按预期 ✅ |

**本轮由端到端验证发现并修复的真缺陷**（单测没抓到）：
1. 跨 Origin 校验实际放行任意主机名 —— `origin_allowed` 早期实现只要 host 非空就通过，
   把 `Host` 当成了可信来源。已改为按**绑定地址**判定：回环绑定只接受回环 Origin，
   通配绑定才接受任意 Origin（此时令牌是唯一边界，且 `bind_policy` 已强制认证）。
2. Admin「过滤预设」把云端阈值发到了 `filter.speech_noise_threshold`，服务端读的是
   `llm.speech_noise_threshold` —— 选预设根本没改到云端，面板却显示已生效。已修。
3. `admin-preset-cases.mjs` 的 `check()` 是同步的，7 个 `async` 用例断言被静默丢弃
   （那个 `15/15` 是空的）。已改为 await-aware；负控制证明接线改回去就会失败。
4. 静态资源改为 public 后 `..` 穿越可读 `config.toml`（含密钥）。已加 `safe_asset_path`。
5. OBS 停靠面板与 `--open` 的管理页 URL 未带令牌，会被自己的 401 挡住。已修。
