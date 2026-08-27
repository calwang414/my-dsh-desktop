# `@deepseek-ai/dsh-desktop`

English | [中文](README.zh.md)

A Tauri desktop shell for the DeepSeek Harness Web GUI. The shell is reuse-first: when a harness is already serving the machine it attaches to that server, so the web and desktop windows become two clients of one harness and view the same sessions — history and live messages — simultaneously. Only when no harness is running does it spawn `dsh web` itself on an OS-assigned port, wait for the readiness line on stdout, and point a native window at that URL. The GUI itself is the unmodified web frontend — the renderer uses no Tauri bridge API, so the shell owns only process and window lifecycle.

## Why reuse-first

One session log file has exactly one writer. Two harness processes (the web GUI's server and a separately spawned desktop server) writing the same `~/.dsh` session produce overlapping sequence numbers, and the log validator rejects the committed region — "corrupt session log: seq gap in committed region". Attaching to the running harness instead keeps one writer, and the harness already broadcasts every session event and projection to all connected clients, so both windows stay live.

## How it works

1. On startup the shell probes `http://127.0.0.1:3080` (the web profile's composed port; override with `DSH_DESKTOP_HARNESS_URL`, loopback http only) for the harness boot-manifest marker. Found: the window attaches directly; the shell owns no process and never stops it.
2. Not found: the shell spawns the harness as a child — `node --import tsx/esm apps/cli/src/bin.ts web --port 0` from the repository root (source checkout) — `--port 0` asks the OS for a free port.
3. A reader thread drains the harness stdout until `dsh web: http://127.0.0.1:PORT` — the same readiness line supervisors and the keyless CLI smoke already rely on — then navigates the window to it.
4. A spinner page (`ui/index.html`) shows while booting; `ui/error.html` replaces it when the harness never reports readiness within 120s or dies before doing so.
5. In spawned mode, exit (window close, or SIGINT/SIGTERM) signals the harness (SIGTERM, escalating to SIGKILL after 5s) and waits, so sessions stay durable; a monitor thread takes the app down if the harness dies mid-session.

Environment seams for packaged builds: `DSH_DESKTOP_NODE` (node executable, default `node`) and `DSH_DESKTOP_REPO_ROOT` (harness working directory, default the checkout root derived from the crate location).

## Development

Requirements: Rust toolchain, [tauri-cli 2](https://tauri.app), Node >= 22, and a repo state with `pnpm install` done plus `pnpm run build` artifacts (the harness serves the built `apps/web/dist` frontend and resolves workspace packages through their built `lib/`). Dev mode spawns the harness from a `deepseek-harness` checkout; a standalone clone points `DSH_DESKTOP_REPO_ROOT` at such a checkout.

```sh
pnpm run build          # repo-wide: tsc + tsdown + vite (apps/web/dist)
cd apps/desktop/src-tauri
cargo tauri dev         # opens the desktop window
```

## Packaging

The bundle is self-contained: the app ships an official Node binary and an npm-installed `@deepseek-ai/dsh` harness in its resources, so the packaged app runs standalone on any macOS machine (Apple Silicon or Intel) — no repo checkout, no Node install, no terminal. CI builds both dmg architectures: arm64 natively, x64 by cross-compiling (`NODE_ARCH=darwin-x64` with `tauri build --target x86_64-apple-darwin`). The Rust shell finds these via the resource dir at runtime and only falls back to a source checkout in dev builds.

```sh
scripts/stage-resources.sh   # stage node + harness into src-tauri/resources (once)
cd src-tauri
cargo tauri build                          # .app + .dmg
```

`NODE_VERSION` and `DSH_VERSION` env vars pin the staged versions. Upgrading the harness is a resource re-staging, not a code change: bump `DSH_VERSION`, re-run the staging script, rebuild. The bundle is currently unsigned (no Developer ID), so first launch requires right-click → Open in Finder.

### 麦克风/摄像头权限(macOS)

页面内语音(voice-pet 按住说话、dsh-jarvis 唤醒词)经 `getUserMedia` 请求麦克风。
macOS TCC 只有在应用 Info.plist 声明用途描述时才会弹授权窗并被允许;缺少声明时请求
直接 `NotAllowedError` 且**不弹窗**(桌面窗口麦克风不可用的最常见原因)。

- 声明在 `src-tauri/Info.plist`(`NSMicrophoneUsageDescription` / `NSCameraUsageDescription`),
  由 `tauri.conf.json` 的 `bundle.macOS.infoPlist` 合并进构建产物;
- 应用升级后若之前拒绝过授权:系统设置 → 隐私与安全性 → 麦克风 → 打开
  「DeepSeek Harness」并重启应用;或 `tccutil reset Microphone ai.deepseek.harness.desktop` 后重试;
- `cargo tauri dev` 开发态使用宿主签名,系统弹窗归属为开发壳,授权行为相同。

### Windows

Tauri does not cross-compile, so the Windows installer must be built on a Windows machine — either the **Desktop build** GitHub Actions workflow (manual trigger; it stages the win-x64 Node binary and uploads the NSIS .exe from a windows-latest runner) or a local Windows machine/VM:

```powershell
powershell -File scripts/stage-resources.ps1   # stage node.exe + harness (once)
cd src-tauri
cargo tauri build --bundles nsis                          # .exe (NSIS)
```

The unsigned installer triggers SmartScreen on first run: More info → Run anyway.

## Installing

The installers are unsigned (no Developer ID), so macOS and Windows add an extra confirmation step on first launch.

**macOS** (dmg): open the dmg and drag `DeepSeek Harness.app` into Applications. Gatekeeper blocks the first launch; right-click the app and choose Open, or clear the quarantine attribute once:

```sh
xattr -cr "/Applications/DeepSeek Harness.app"
```

**Windows** (NSIS installer): run `DeepSeek Harness_0.1.x_x64-setup.exe`; when SmartScreen shows "Windows protected your PC", choose More info -> Run anyway.

The reuse-first behavior is unchanged in the packaged app: when a harness already serves 127.0.0.1:3080 the app attaches to it (and never stops it); otherwise it spawns the bundled harness and tears it down on exit.
