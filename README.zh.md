# `@deepseek-ai/dsh-desktop`

[English](README.md) | 中文

DeepSeek Harness Web GUI 的 Tauri 桌面壳。壳采用"复用优先"策略：本机已有 harness 在提供服务时，直接附着到该服务器，于是网页端和桌面端成为同一个 harness 的两个客户端，可以同时查看同一个会话——历史与实时消息都同步。仅当没有 harness 在运行时，它才自己启动 `dsh web`（OS 分配端口），等待 stdout 就绪行，然后把原生窗口指向该地址。GUI 本身是 harness 提供的、未经修改的 Web 前端——渲染层不使用任何 Tauri 桥接 API，壳只负责进程与窗口生命周期。

## 为什么复用优先

一个会话日志文件只能有一个写入者。两个 harness 进程（网页端服务器 + 单独启动的桌面端服务器）写同一个 `~/.dsh` 会话，会产生重叠的序列号，日志校验器会拒绝提交区域——即 "corrupt session log: seq gap in committed region" 错误。附着到正在运行的 harness 则始终只有一个写入者；而 harness 本来就会把每个会话事件和投影广播给所有已连接的客户端，因此两个窗口都能保持实时。

## 工作原理

1. 启动时壳探测 `http://127.0.0.1:3080`（web profile 的合成端口；可用 `DSH_DESKTOP_HARNESS_URL` 覆盖，仅接受 loopback http）是否返回 harness 启动清单标记。命中：窗口直接附着；壳不拥有任何进程，也绝不会停止它。
2. 未命中：壳以子进程方式启动 harness——在仓库根目录执行 `node --import tsx/esm apps/cli/src/bin.ts web --port 0`（源码检出）——`--port 0` 让 OS 分配空闲端口。
3. 读取线程持续读取 harness 的 stdout，直到出现 `dsh web: http://127.0.0.1:PORT`——与监督器和无密钥 CLI 冒烟测试依赖的就绪行相同——然后把窗口导航到该地址。
4. 启动期间显示加载页（`ui/index.html`）；若 harness 在 120 秒内未报告就绪或在就绪前退出，则切换为错误页（`ui/error.html`）。
5. 自启模式下退出时（关闭窗口，或收到 SIGINT/SIGTERM），应用向 harness 发送 SIGTERM（5 秒后升级为 SIGKILL）并等待其退出，保证会话持久化；若 harness 在会话中途崩溃，监控线程会带应用一起退出。

打包构建的环境变量接口：`DSH_DESKTOP_NODE`（node 可执行文件，默认 `node`）和 `DSH_DESKTOP_REPO_ROOT`（harness 工作目录，默认由 crate 位置推导的检出根目录）。

## 开发

要求：Rust 工具链、[tauri-cli 2](https://tauri.app)、Node >= 22，以及已执行 `pnpm install` 且 `pnpm run build` 产物就绪的仓库状态（harness 服务的是构建好的 `apps/web/dist` 前端，并通过各包的构建产物 `lib/` 解析工作区依赖）。开发模式从 `deepseek-harness` 检出启动 harness；独立克隆需要把 `DSH_DESKTOP_REPO_ROOT` 指向这样的检出。

```sh
pnpm run build          # repo-wide: tsc + tsdown + vite (apps/web/dist)
cd apps/desktop/src-tauri
cargo tauri dev         # opens the desktop window
```

## 打包

打包产物是自包含的：应用资源里捆绑了官方 Node 二进制和 npm 安装的 `@deepseek-ai/dsh` harness，因此打包应用可以在任何 macOS 机器（Apple Silicon 与 Intel）上独立运行——不需要源码检出、不需要安装 Node、不需要终端。CI 同时产出两种架构的 dmg：arm64 原生构建，x64 交叉编译（`NODE_ARCH=darwin-x64` 搭配 `tauri build --target x86_64-apple-darwin`）。Rust 壳在运行时通过资源目录找到它们，只有开发构建才回退到源码检出。

```sh
scripts/stage-resources.sh   # stage node + harness into src-tauri/resources (once)
cd src-tauri
cargo tauri build                          # .app + .dmg
```

`NODE_VERSION` 和 `DSH_VERSION` 环境变量固定要暂存的版本。升级 harness 只是重新暂存资源，不是改代码：改 `DSH_VERSION`、重跑脚本、重新构建。当前包未签名（没有 Developer ID），首次打开需要在 Finder 里右键 → 打开。

### 麦克风/摄像头权限(macOS)

页面内语音(voice-pet 按住说话、dsh-jarvis 唤醒词)经 `getUserMedia` 请求麦克风。
macOS TCC 只有在应用 Info.plist 声明用途描述时才会弹授权窗并被允许；缺少声明时请求
直接 `NotAllowedError` 且**不弹窗**(桌面窗口麦克风不可用的最常见原因)。

- 声明在 `src-tauri/Info.plist`(`NSMicrophoneUsageDescription` / `NSCameraUsageDescription`),
  由 `tauri.conf.json` 的 `bundle.macOS.infoPlist` 合并进构建产物;
- 应用升级后若之前拒绝过授权:系统设置 → 隐私与安全性 → 麦克风 → 打开
  「DeepSeek Harness」并重启应用;或 `tccutil reset Microphone ai.deepseek.harness.desktop` 后重试;
- `cargo tauri dev` 开发态使用宿主签名,系统弹窗归属为开发壳,授权行为相同。

### Windows

Tauri 不支持交叉编译，Windows 安装包必须在 Windows 环境构建——要么用 **Desktop build** GitHub Actions 工作流（手动触发，在 windows-latest runner 上暂存 win-x64 Node 并产出 NSIS .exe），要么用本地 Windows 机器/虚拟机：

```powershell
powershell -File scripts/stage-resources.ps1   # stage node.exe + harness (once)
cd src-tauri
cargo tauri build --bundles nsis                          # .exe (NSIS)
```

未签名的安装包首次运行会被 SmartScreen 拦截：更多信息 → 仍要运行。

## 安装

安装包未签名（没有 Developer ID），macOS 与 Windows 首次启动都需要额外确认一步。

**macOS**（dmg）：打开 dmg，把 `DeepSeek Harness.app` 拖入"应用程序"。首次启动会被 Gatekeeper 拦截——右键点击应用选择"打开"，或执行一次清除隔离属性：

```sh
xattr -cr "/Applications/DeepSeek Harness.app"
```

**Windows**（NSIS 安装器）：运行 `DeepSeek Harness_0.1.x_x64-setup.exe`；SmartScreen 提示"已保护你的电脑"时，点"更多信息 → 仍要运行"。

打包应用保持"复用优先"行为：本机 127.0.0.1:3080 已有 harness 时直接附着（绝不停止它）；否则启动捆绑的 harness，退出时回收。
