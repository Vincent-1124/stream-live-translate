# 代码审计整改清单

审计基线：`538e8d02b8cfdc1ad30d2747d20b635a480403ac`  
审计方式：只读源码审查、前端内存测试、构建产物与目录结构核对。未在本轮运行会写入 `target/` 的 Cargo 构建或测试。  
发布结论：完成 P0 和 P1 前，不应作为正式版发布，也不应把管理端口绑定到非回环地址。

## 执行原则

- 按 P0 → P1 → P2 顺序处理；同一优先级按编号执行。
- 每项只修根因，不顺手扩展功能。
- 每个非平凡修复必须同时增加一个能复现旧问题的最小自动测试。
- 安全边界、停止语义、音频格式和记录一致性不能用“仅文档提示”代替代码约束。
- 修复完成后重新生成候选产物，并记录 commit、文件哈希、测试命令和结果。

## P0：发布阻断——安全边界

### P0-01 管理 API 与字幕 WebSocket 鉴权

范围：`src/server.rs`、`admin/app.js`、`overlay/app.js`。

需要做：

- 启动时生成或加载随机管理令牌。
- 所有 `/api/*` 写接口、字幕/录音读取接口和 `/ws/subtitles` 统一校验令牌。
- WebSocket 和管理接口校验 `Origin`；删除 `CorsLayer::permissive()`，只允许同源管理页。
- 默认继续只绑定 `127.0.0.1`；非回环绑定必须显式启用认证，未配置认证时拒绝启动。
- Overlay 使用只读令牌，不应获得修改配置、停止管线或清除记录的权限。

验收：

- 无令牌、错误令牌和跨 Origin 请求均返回拒绝。
- 正常 Admin、Overlay 和 OBS Browser Source 仍能工作。
- `--host 0.0.0.0` 在无认证配置时启动失败并给出明确错误。

### P0-02 阻断“修改 endpoint 后复用旧 Key”的外送链

范围：`src/server.rs`、`src/llm.rs`、配置模型。

需要做：

- 云端 provider 使用端点允许列表。
- 只有明确的“自定义/本地 provider”允许任意 WebSocket 地址。
- provider 或 endpoint 发生信任域变化时，不得静默保留并发送旧 API Key；要求用户重新确认或重新输入。
- `connection-test` 使用相同规则，不能成为绕过路径。

验收：

- 只修改 endpoint 的请求不能让已有 Key 发往非允许主机。
- Qwen、OpenAI、百炼的合法官方端点仍能连接。

### P0-03 清理所有配置响应中的秘密

范围：`src/server.rs`、`admin/app.js`。

需要做：

- 不再直接序列化内部 `Config` 作为 API 响应，改用专用响应结构。
- `llm.api_key`、`obs.password` 等秘密只返回 `*_set` 布尔状态。
- 保存时空密码沿用现值；清除秘密使用独立、已鉴权的明确操作。

验收：

- `/api/config`、错误响应、日志和 WebSocket 中均搜不到已保存的模型 Key 与 OBS 密码。

### P0-04 验证所有外部可写配置

范围：`src/server.rs`、`src/config.rs`、`src/recording.rs`。

需要做：

- 固定内部音频管线为 16 kHz，或严格限制 `audio.sample_rate`；禁止产生零帧、溢出或巨额分配的值。
- `recording_dir` 默认只允许配置目录下的子目录，拒绝 `..`、UNC 和未经本机交互授权的绝对路径。
- 对端口、尺寸、时长、通道数和字符串长度设置服务端边界，不能只依赖 HTML 控件。

验收：

- `sample_rate=0/1/u32::MAX`、路径穿越、UNC、越界端口等请求均被拒绝且不会写盘。
- 模糊或属性测试不会触发 panic、OOM 或目录逃逸。

### P0-05 OBS 插件与引擎之间做身份握手

范围：`plugin/stream-live-translate.c`、`src/ingest.rs`、进程启动参数。

需要做：

- 不能再以“8788 端口可连接”作为引擎身份判断。
- 插件启动引擎时传递一次性随机 nonce，SLTA 握手必须双向验证。
- 端口被未知进程占用时关闭音频发送并显示错误；更理想的后续方案是当前用户 ACL 的命名管道/Unix socket。
- 增加握手和空闲超时，并限制同时连接数。

验收：

- 抢占 8788 的假服务无法收到 PCM。
- 错误 nonce、慢连接和重复连接不会中断合法音频或耗尽任务。

## P1：核心正确性与稳定性

### P1-01 让停止、重启和配置保存真正取消当前管线

范围：`src/pipeline.rs`、`src/server.rs`。

需要做：

- 增加独立的 cancel/restart generation 或 watch 通知。
- 首次音频等待、provider 会话、drain 和 `watch()` 全部监听取消信号。
- `/api/stop` 必须在有界时间内停止采音和云端连接；`/api/restart` 必须启动新 generation。
- 配置保存只触发一次可靠重启，不依赖字段白名单碰巧被 watcher 检测。

验收：

- 活跃 provider 下执行 stop 后，不再发送音频、不再计费且状态真实。
- restart 后旧任务退出，新任务使用新配置启动。
- 覆盖首次音频等待、连接中、识别中和 final drain 四种阶段。

### P1-02 修复跨采样率 ingest 分包

范围：`src/ingest.rs`、`src/audio.rs`。

需要做：

- 分离“输入速率待重采样缓冲”和“输出速率待成帧缓冲”。
- 使用保留相位和上一采样点的流式重采样状态，禁止把输出尾部重新当作输入速率数据。
- 内部统一输出 16 kHz 单声道 PCM。

验收：

- 同一 PCM 以随机 TCP 分包方式送入，输出必须与一次性输入等价。
- 覆盖 44.1 kHz、48 kHz、奇数字节分包和断线尾帧。

### P1-03 修复 provider 结束时的最后一句 drain

范围：`src/llm.rs`。

需要做：

- Qwen、OpenAI、FunASR 参照已有 Bailian drain 思路：writer 正常结束后继续有界等待 reader 的 final/finished。
- reader 异常结束时才取消 writer；超时必须可见且不能无限等待。

验收：

- EOF、ingest 断开和有限回放均不会丢最后一句。
- 每个 provider 都有“finish 后延迟返回 final”的契约测试。

### P1-04 修复 OpenAI/gateway 文本事件路由

范围：`src/llm.rs`、相关配置文档。

需要做：

- transcribe 模式消费输入转写事件。
- gateway/回复文本模式消费 `response.text.delta/done`，需要时发送 `response.create`。
- 删除未使用的模式变量或让它真实控制行为。

验收：

- `transcribe=true`、`transcribe=false`、`gateway_text=true` 各有测试，均能产生预期字幕且不会把模型自言自语混入转写。

### P1-05 修复 OBS 断线重连和文本源镜像

范围：`src/obs.rs`、`src/main.rs`、字幕输出通路。

需要做：

- reader 或 writer 任一退出时，取消另一半并让 `try_connect()` 返回。
- 每次连接/断线更新共享命令 sender，不能只在 `spawn()` 后同步读取一次。
- `mirror_to_text_source` 要么接入 Final/Replace 输出，要么删除该配置和文档承诺。

验收：

- OBS 关闭再打开后自动恢复。
- 镜像开启时字幕能更新指定文本源；关闭时不发送命令。

### P1-06 修复字幕记录竞态与静默退出

范围：`src/subtitle.rs`、`src/recording.rs`。

需要做：

- Final 时直接把不可变的完整 `SubtitleLine` 发送给记录队列，不再收到事件后反查 `history().last()`。
- 明确处理 broadcast `Lagged` 和 `Closed`；发生丢失时记录可见错误，不能永久静默退出。
- 清空历史和删除磁盘记录必须在 UI 上区分；自动落盘增加明确开关，默认策略需产品确认。

验收：

- 连续 Final、广播积压和清空历史时，JSONL/TXT/SRT 与字幕事件一致。
- 记录任务异常后可见、可恢复。

### P1-07 限制历史与导出内存

范围：`src/subtitle.rs`、`src/recording.rs`、导出接口。

需要做：

- 直接 Final 和有 Partial 的 Final 走同一个 history push/cap 函数。
- RecordingStore 内存只保留有界近期索引；TXT/SRT 从 JSONL 流式导出，不全量 clone。

验收：

- 连续直接 Final 后字幕历史始终不超过 200 条。
- 长时间压力测试中内存保持有界，导出不会产生一份完整副本的额外峰值。

### P1-08 修复 OBS 插件线程和共享状态生命周期

范围：`plugin/stream-live-translate.c`。

需要做：

- Windows 卸载时检查线程等待结果；线程未退出前不得销毁事件、锁和环形缓冲。
- socket 发送增加可中断超时，确保 stop 能唤醒阻塞线程。
- `port`、`reconnect_requested`、`in_rate`、`resample_pos` 使用同一锁或真正的原子/单线程所有权，消除 C 数据竞争。
- 重采样器保存跨 chunk 的相位与上一采样点。

验收：

- 假服务接受连接但不读取时，卸载 OBS 不挂死、不崩溃。
- ThreadSanitizer/等价检查无共享状态竞态。

## P2：配置、构建与发布一致性

### P2-01 删除或兑现无效配置

范围：`src/config.rs`、`src/pipeline.rs`、`src/audio.rs`、Admin、文档。

需要做：

- `min_segment_ms`、`max_segment_ms`：接入分段状态机，或删除字段和 UI。
- `use_screen_capture_kit`：实现真实 SCK 捕获，或在支持前移除并明确提示不支持。
- `mirror_to_text_source` 按 P1-05 处理。

验收：每个公开配置字段至少有一个运行时引用和一个行为测试。

### P2-02 配置保存改为原子、串行

范围：`src/config.rs`、`src/server.rs`。

需要做：同目录临时文件写入并 flush 后原子替换；保存、读回校验和更新内存使用同一串行锁或版本检查，避免并发请求 lost update。

### P2-03 构建时前端同步失败必须中止

范围：`build.rs`。

需要做：传播 `read_dir/create_dir_all/copy` 错误；删除 `dist` 中源目录已经不存在的孤儿文件；同步后校验源与目标清单和内容。

### P2-04 修复 Admin 的合法零值往返

范围：`admin/app.js`、Admin 测试。

需要做：

- `bg_opacity=0` 不得被 `|| 75` 改回 75。
- `border_radius=0` 不得在加载时变回 8。
- `silence_rms=0` 若定义为合法禁用值，必须原样往返；否则服务端和 UI 都应明确禁止。

验收：加载、保存、重新读取三个字段的 0 值保持不变。

### P2-05 统一版本与发布元数据

范围：`Cargo.toml`、`plugin/CMakeLists.txt`、插件日志、HTML 缓存版本、README、打包脚本。

需要做：确定一个版本源，其余位置构建时读取或生成；修正占位仓库地址。当前同时出现 `0.0.25`、`0.0.6.1`、`0.1.0`、`0.7.0`。

### P2-06 收紧发布供应链

范围：`.github/workflows/*.yml`。

需要做：默认 `contents: read`，仅 release job 使用 `contents: write`；第三方 Action 锁定完整 commit SHA；增加产物清单、secret 模板检查和校验和验证。

### P2-07 配置与录音文件权限和保留策略

范围：`src/config.rs`、`src/recording.rs`、Admin。

需要做：配置和记录文件显式限制为当前用户可读写；提供录音启用开关、保留期限和删除本场磁盘记录操作；默认隐私策略需产品确认。

## 回归与最终发布门槛

全部 P0/P1 完成后执行：

1. Rust 单元与集成测试；重点覆盖鉴权、取消、随机 TCP 分包、EOF drain、OBS 重连、记录竞态和资源上限。
2. Overlay 回归与负控制；保持当前 `25/25` 和全部负控制通过。
3. Admin 控件契约测试；所有公开控件完成配置往返与后端行为验证。
4. 本地恶意请求测试：跨 Origin、错误令牌、endpoint 外送、采样率 DoS、目录穿越、端口抢占。
5. OBS 实机测试：挂载/卸载、OBS 重启、多个滤镜、无线设备插拔、长时间直播。
6. 对每个受支持 provider 做短音频和有限回放，确认第一句、最后一句、断线恢复和停止计费。
7. 重新构建正式包，记录 commit、平台、文件大小、SHA-256、测试日志和产物内容清单。
8. 扫描源码、嵌入资源和发布包，确认无 API Key、OBS 密码、私钥或真实字幕记录。

## 本轮已通过的只读检查

- Admin/Overlay JavaScript 语法检查通过。
- Overlay 回归 `25/25` 通过。
- 7 类内存负控制均被测试捕获。
- Admin 过滤预设测试 `15/15` 通过。
- `admin/overlay` 与 `dist` 当前内容一致。
- 未发现可证实的字幕 DOM XSS；字幕与导入文本使用 `textContent`。
- 当前模板、发布包和二进制扫描未发现真实 API Key 或私钥。

## 本轮未验证

- 未联网核对依赖的最新 CVE。
- 未重新构建 Rust/OBS 产物，也未证明现有二进制与审计提交可复现对应。
- 未验证部署机器的 ACL、umask、防火墙、浏览器 PNA 和反向代理配置。
- 未验证发布产物签名。
