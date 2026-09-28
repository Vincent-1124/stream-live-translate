# 编译指南

Stream Live Translate 用 Rust 1.74+ 写，跨平台只依赖各系统的标准库。下面按平台给出"装好就能编译"的最短步骤。

> **要编译的是 OBS 插件包？**（C 薄壳插件 + 引擎，复制进 OBS 即用的正式产品形态）
> 直接看 [docs/PLUGIN.md](PLUGIN.md) 的"手动编译"章节：Windows 跑 `scripts\package-plugin.ps1`，
> Linux/macOS 跑 `bash scripts/package-plugin.sh`；或打 `v*` tag 让 GitHub Actions（`plugin.yml`）自动产出三平台成品。
> 下面内容针对**引擎本体**（Rust 单二进制）的编译。


## Version single source of truth

**`Cargo.toml` 的 `[package] version` 字段是唯一的权威版本号**（当前 `0.0.25`）。
其余所有位置都必须从它派生，不允许各写一份：

| 位置 | 如何从权威版本派生 |
| --- | --- |
| Rust 引擎 | `env!("CARGO_PKG_VERSION")`（Cargo 自动提供） |
| `plugin/CMakeLists.txt` | `file(READ ../Cargo.toml)` + `string(REGEX MATCH ...)`，喂给 `project(... VERSION ...)`；**抽不到就 `message(FATAL_ERROR)`** |
| `plugin/version.h` | 手写的 `#define SLT_VERSION "..."`；CMake 在 configure 阶段比对，不一致直接报错中止 |
| `plugin/stream-live-translate.c` 日志 | `#include "version.h"` 后用 `SLT_VERSION` 打印（见下方 "Plugin load-time version log"） |
| `scripts/package-plugin.ps1` / `.sh` | 从 `Cargo.toml` 读取，产出 `release/...-<version>.zip`、macOS `Info.plist` 的 `CFBundleVersion` |
| `admin/index.html`、`overlay/index.html` 的 `?v=` | `scripts/sync-version.ps1` / `sync-version.sh`（见下方 "Asset cache-busting policy"） |

**为什么 CMake 选"读 Cargo.toml"而不是"读 plugin/version.h"**：`project(... VERSION ...)`
必须在任何 target 之前调用，而 `version.h` 只是给 C 源码用的；从 `Cargo.toml` 取说明
只需要维护一处抽取逻辑，且 `version.h` 一旦过期会被上面的比对拦下来，不可能悄悄发版。
两种做法都在验收口径内，这里选的是能保证 "Cargo.toml 永远最高" 的那一种。

发新版时：

```powershell
# 1. 只改 Cargo.toml 的 version
# 2. 把 plugin/version.h 的 SLT_VERSION 字面量改成同一个值（必须手改，故意不自动生成）
# 3. 检查并改写 HTML 缓存串
powershell -ExecutionPolicy Bypass -File scripts\sync-version.ps1 -Fix
```

`scripts/sync-version.ps1`（Windows）和 `scripts/sync-version.sh`（macOS/Linux）会
报告 `plugin/version.h` 是否漂移、并把 `admin/`、`overlay/` 以及 `dist/` 里的
`?v=` 改写成权威版本。两个 `package-plugin` 脚本在构建前都会调用它：
默认只**检查**（`-SkipHtml` / `--skip-html`），漂移就以非零退出并打印确切编辑内容；
加 `-FixAssetVersions`（Windows）或 `FIX_VERSION_ASSETS=1`（bash）才会真正改写。

### Asset cache-busting policy（`?v=`）

`admin/index.html` 与 `overlay/index.html` 通过 `?v=<version>` 给 `/admin-assets/*`、
`/overlay-assets/*` 做缓存击穿。**该 `<version>` 必须等于 `Cargo.toml` 的权威版本**——
它此前写成 `0.1.0`，会被当成另一个发布版本，因此这是一处 P2-05 缺陷。

这两个 HTML 文件由其他 agent 负责，本轮**没有直接改动**。需要落地的是
`scripts/sync-version.ps1 -Fix`（等价地：把每一处 `?v=0.1.0` 改成 `?v=0.0.25`）。
用 `grep -n '?v=' admin/index.html overlay/index.html` 可以列出当前待改行，一共 4 处：

* `admin/index.html`：`/admin-assets/style.css?v=0.1.0` 与 `/admin-assets/app.js?v=0.1.0`
* `overlay/index.html`：`/overlay-assets/style.css?v=0.1.0` 与 `/overlay-assets/app.js?v=0.1.0`
* `dist/admin/index.html`、`dist/overlay/index.html` 是 `build.rs` 从上面两个目录同步的副本，
  改完源文件后重新 `cargo build` 即可；也可以让脚本一并改写。

> 行号不要写死在文档里：`admin/`、`overlay/` 正被其他 agent 编辑，行号会漂移。
> 认准 `?v=<版本>` 这个串本身即可。

注意 `src/embedded.rs` 用 `include_dir!` 把 `dist/` 编进二进制，所以改完 `?v=`
**必须重新 `cargo build`**，否则内嵌资源仍是旧串。

> 如果本机残留旧配置目录（例如 `build/plugin-build-win/` 里还写着
> `CMAKE_PROJECT_VERSION:STATIC=0.7.0`），CMake 会因为 `CMakeLists.txt` 变更自动重跑
> `project()`，不需要手动清理；但要确认它确实重新 configure 过，别拿旧缓存里的
> 版本号当结论。

### Plugin load-time version log

插件已引用 `plugin/version.h` 的 `SLT_VERSION` 写入加载日志；该头文件与
`Cargo.toml` 的版本由构建脚本核对。

### Repo URL（TODO，发版前必须处理）

`Cargo.toml` 的 `repository` 仍是脚手架占位值
`https://github.com/yourname/stream-live-translate`。
在**本仓库内无法确定真实地址**：`git remote -v` 为空，两个 workflow 也没有写任何
仓库归属，所以按"不确定就不要编"的原则**没有替换**，只在 `Cargo.toml` 里加了
`TODO(release-blocker)` 注释。

> **TODO(release-blocker)**：发版前把 `Cargo.toml` 的 `repository`（以及需要的话
> `homepage`）改成真实仓库地址。

## Release supply chain checklist

面向 `.github/workflows/release.yml` 和 `.github/workflows/plugin.yml`。

* [x] 顶层 `permissions: contents: read`；只有真正发布 Release 的 job
      （两个 workflow 里的 `release`）额外声明 `contents: write`。
* [x] 发布前生成产物清单：文件名、字节数、SHA-256 全部写进
      `artifacts/tools/release-manifest.json`，并额外输出可直接 `sha256sum -c` 的
      `artifacts/tools/SHA256SUMS`；两者与产物一起上传。
* [x] 密钥/模板守卫：`scripts/ci-secret-guard.sh`（源码树 + 已构建产物），
      以及 `scripts/verify-checksums.sh`（校验和复核）在发布前运行。
* [x] 两个 workflow 的 30 条第三方 Action 引用已锁定为 40 位 commit SHA；
      行尾保留上游版本供审阅。tag 还会经过 `Cargo.toml` 版本门禁。
* [ ] **未完成：OBS SDK 下载未做校验和验证。** `scripts/package-plugin.ps1`
      在找不到本机 OBS 时会 `Invoke-WebRequest` 官方 `OBS-Studio-<ver>-Windows.zip`。
      官方 release 页面**没有**公布该 zip 的 SHA-256（本轮无网络，也无法去查），
      因此没有凭空写一个期望值；下载路径保持原样。需要时改为官方公布的校验值或
      `Get-AuthenticodeSignature` 验证。

## 通用前置

| 工具 | 版本 | 用途 |
| --- | --- | --- |
| Rust toolchain | 1.74+ | 编译器 |
| C 编译器 | 随系统 | cpal FFI |
| pkg-config | 任意 | Linux ALSA/Pulse |
| OpenSSL (开发版) | 任意 | `native-tls` 备用通道（可选） |

如果还没装 Rust：

```bash
# Windows (PowerShell)
winget install Rustlang.Rustup
# 或
irm https://sh.rustup.rs | iex

# macOS
brew install rustup-init && rustup-init -y
# 或
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Linux
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

## 一次性编译（开发用）

```bash
cargo build --release
# 产物 target/release/stream-live-translate
# Windows 下叫 stream-live-translate.exe
```

把 `target/release/stream-live-translate(.exe)` 与 `dist/overlay`、`dist/admin` 两个目录放在同一级，就是完整的"零安装"插件包。

## 跨平台正式版

`scripts/build-all.sh`（macOS / Linux）和 `scripts/build-all.ps1`（Windows）做下面这些事：

1. 调用 `rustup target add <target>`（首次需要联网）；
2. `cargo build --release --target <target>`；
3. 把 `dist/` 目录里的 overlay / admin / 启动器 / 文档复制到 `release/<platform>/`；
4. 把整个目录打成 `.tar.gz` / `.zip` —— **这就是要分发的正式版**。

```bash
# 在 macOS arm64 上
./scripts/build-all.sh
# -> release/macos-arm64/stream-live-translate
# -> release/macos-arm64.tar.gz
```

```powershell
# 在 Windows 上
powershell -ExecutionPolicy Bypass -File scripts\build-all.ps1
# -> release\windows-x64\bin\stream-live-translate.exe
# -> release\windows-x64.zip
```

跨平台产物用 GitHub Actions 自动出：`.github/workflows/release.yml` 在 push `v*` 标签时同时构建 4 个目标并把 `.zip` / `.tar.gz` 附到 release。

## 平台特定坑

### Windows

* 用 MSVC 工具链。装一次 *Build Tools for Visual Studio*（含 "C++ 桌面开发" 即可）。
* WASAPI 系统音频环回在 OBS 之外也能正常工作。
* 如果 OBS 安装在 `C:\Program Files\obs-studio\`，需要把整个 release 目录拷过去 —— 不会有写权限问题，因为二进制不带任何安装逻辑。

### macOS（Apple Silicon）

* 必需 Xcode Command Line Tools：`xcode-select --install`。
* 第一次抓系统音频时会弹"屏幕录制 / 麦克风"权限请求；同意后系统会缓存授权。
* 如果管理员面板上"音频"一直是红灯，进 `系统设置 -> 隐私与安全性 -> 屏幕录制 / 麦克风` 把本程序加白名单。

### Linux

发行版差异较大。下表只列"最少要装的包"：

| 发行版 | apt | dnf | pacman |
| --- | --- | --- | --- |
| Ubuntu / Debian | `libasound2-dev libpulse-dev` | — | — |
| Fedora / RHEL | — | `alsa-lib-devel pulseaudio-libs-devel` | — |
| Arch / Manjaro | — | — | `alsa-lib libpulse` |

PulseAudio / PipeWire 自带 monitor source，系统音频环回就是抓默认 sink 的 monitor。

## 验证编译

```bash
cargo test
```

跑：

```bash
RUST_LOG=info ./target/release/stream-live-translate
# 然后浏览器打开
# http://127.0.0.1:8787/admin
# http://127.0.0.1:8787/overlay
```

能用 mock provider 测：

```toml
[llm]
provider = "mock"
api_key = "any"
model = "mock"
```

## 精简二进制

已经默认开了 `lto = "thin"` + `strip = "symbols"`。再小可以加 UPX：

```bash
upx --best target/release/stream-live-translate
```

## 安装到 OBS 文件夹

最后一步：把 release 产物整个文件夹复制到 OBS 安装位置：

| 平台 | 路径 |
| --- | --- |
| Windows | `C:\Program Files\obs-studio\plugins\stream-live-translate\` |
| macOS | `/Applications/OBS.app/Contents/Resources/stream-live-translate/` |
| Linux | `~/.local/share/obs-studio/plugins/stream-live-translate/` |

然后：

* Windows：双击 `stream-live-translate/launcher.bat`
* macOS：双击 `stream-live-translate/launcher.sh`
* Linux：终端里 `./stream-live-translate/launcher.sh`
