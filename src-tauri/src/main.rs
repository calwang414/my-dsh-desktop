// Without this the Windows exe is a console-subsystem app: Windows opens a
// cmd window for it at every launch (showing the shell's own log lines).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Tauri desktop shell for the DeepSeek Harness Web GUI.
//!
//! The shell is a thin supervisor, not a fork of the product: when a harness is
//! already serving this machine it attaches to it (one server, many clients —
//! the web and desktop windows then view the same sessions live), otherwise it
//! spawns the dsh web harness on an OS-assigned port, waits for the harness's
//! readiness URL line on stdout, and points a native window at that URL. The
//! GUI itself is the unmodified web frontend served by the harness — the
//! renderer uses no Tauri bridge API, so the shell owns only process and
//! window lifecycle.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use tauri::Url;
use tauri::{Manager, RunEvent, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

/// Bundled page shown while the harness boots (frontendDist asset).
const SPINNER_PAGE: &str = "index.html";
/// Bundled page shown when the harness never reports readiness.
const ERROR_PAGE: &str = "error.html";
/// How long to wait for the harness URL line before declaring failure.
const READY_TIMEOUT: Duration = Duration::from_secs(120);
/// Prefix of the harness readiness line printed by @deepseek-ai/dsh-web-app.
const URL_LINE_PREFIX: &str = "dsh web: ";
/// Served index.html marker proving a dsh harness (not some other server) owns a port.
const BOOT_MANIFEST_MARKER: &str = "__DSH_BOOT__";
/// The web profile's composed default port, probed before spawning.
const DEFAULT_HARNESS_URL: &str = "http://127.0.0.1:3080";

/// The harness child process, killed on app exit.
struct Harness(Mutex<Option<Child>>);

/// Poll interval for the child-exit monitor.
const MONITOR_INTERVAL: Duration = Duration::from_secs(1);

/// The repository checkout root when running from source: src-tauri/../..
fn dev_repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("crate is nested exactly three levels below the repository root")
        .to_path_buf()
}

/// The node executable inside the staged node dir: macOS tarballs keep it at
/// bin/node, the Windows zip at node.exe.
#[cfg(windows)]
fn bundled_node_binary(dir: &Path) -> PathBuf {
    dir.join("node/node.exe")
}

#[cfg(not(windows))]
fn bundled_node_binary(dir: &Path) -> PathBuf {
    dir.join("node/bin/node")
}

/// The directory to expose on PATH for bundled node: node.exe sits at the
/// node dir root in the Windows zip layout, bin/ in the unix tarball layout.
#[cfg(windows)]
fn bundled_node_dir(dir: &Path) -> PathBuf {
    dir.join("node")
}

#[cfg(not(windows))]
fn bundled_node_dir(dir: &Path) -> PathBuf {
    dir.join("node/bin")
}

/// The harness invocation: web --port 0 so the OS picks a free port.
/// In a packaged app the bundled node binary and npm-installed harness
/// (resources/) drive the process; in a dev build node runs the repo source
/// via tsx. DSH_DESKTOP_NODE and DSH_DESKTOP_REPO_ROOT override both modes.
fn harness_command(resource_dir: Option<&Path>) -> (String, Vec<String>, PathBuf, Vec<PathBuf>) {
    // Packaged layout: Contents/Resources/resources/{node,harness} (tauri
    // nests configured resources under a resources/ container dir).
    let bundled_root = resource_dir.map(|dir| dir.join("resources"));
    let bundled = bundled_root
        .as_deref()
        .map(|dir| bundled_node_binary(dir).is_file() && dir.join("harness/package.json").is_file())
        .unwrap_or(false);
    if bundled {
        let dir = bundled_root.expect("bundled resources present");
        let node = std::env::var("DSH_DESKTOP_NODE").unwrap_or_else(|_| {
            bundled_node_binary(&dir).to_string_lossy().into_owned()
        });
        let root = std::env::var("DSH_DESKTOP_REPO_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|_| dir.join("harness"));
        let args = vec![
            "node_modules/@deepseek-ai/dsh/lib/bin.js".to_string(),
            "web".to_string(),
            // dsh >= rc.8 opens the default browser on startup unless
            // --no-open is passed; the desktop shell is the browser.
            "--no-open".to_string(),
            "--port".to_string(),
            "0".to_string(),
        ];
        // PATH dirs so node/npm/pnpm spawned by the harness (agent shell
        // calls, dsh plugin add) resolve to the bundled toolchain.
        let mut path_dirs = vec![bundled_node_dir(&dir)];
        let pnpm_bin = dir.join("harness/node_modules/.bin");
        if pnpm_bin.is_dir() {
            path_dirs.push(pnpm_bin);
        }
        return (node, args, root, path_dirs);
    }
    let node = std::env::var("DSH_DESKTOP_NODE").unwrap_or_else(|_| "node".to_string());
    let root = std::env::var("DSH_DESKTOP_REPO_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dev_repo_root());
    let args = vec![
        "--import".to_string(),
        "tsx/esm".to_string(),
        "apps/cli/src/bin.ts".to_string(),
        "web".to_string(),
        "--port".to_string(),
        "0".to_string(),
    ];
    (node, args, root, Vec::new())
}

/// Extract the canonical GUI URL from a harness line: dsh web: http://127.0.0.1:PORT .
fn parse_gui_url(line: &str) -> Option<Url> {
    let rest = line.strip_prefix(URL_LINE_PREFIX)?;
    let candidate = rest.split_whitespace().next()?;
    if !candidate.starts_with("http://") {
        return None;
    }
    Url::parse(candidate).ok()
}

/// The bundled error page as an app-protocol URL.
fn error_page_url() -> Url {
    Url::parse(&format!("tauri://localhost/{ERROR_PAGE}")).expect("static app URL is valid")
}

/// Whether a live harness serves the boot-manifest marker on this URL.
fn probe_harness(url: &Url) -> bool {
    let Some(host) = url.host_str() else { return false };
    let Some(port) = url.port_or_known_default() else { return false };
    let Ok(mut stream) = TcpStream::connect((host, port)) else { return false };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let request = format!("GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).is_err() { return false }
    let mut response = String::new();
    if stream.read_to_string(&mut response).is_err() { return false }
    response.contains(BOOT_MANIFEST_MARKER)
}

/// The user's home directory (HOME on unix, USERPROFILE on Windows).
#[cfg(windows)]
fn user_home() -> PathBuf {
    std::env::var("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

#[cfg(not(windows))]
fn user_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

/// The harness home (DSH_HOME or the default).
fn harness_home() -> PathBuf {
    std::env::var("DSH_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| user_home().join(".dsh"))
}

/// Ensure a profile's pnpm-workspace.yaml allows node-pty's install script.
/// pnpm >=10 denies install scripts unless allowlisted, and node-pty — a
/// native dependency of terminal-using plugins such as dsh-better-sidebar —
/// ships prebuilt binaries but still declares an install script. Idempotent:
/// existing entries are never rewritten.
fn ensure_node_pty_allow_builds(workspace: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(workspace) else { return false };
    let mut lines: Vec<String> = content.lines().map(str::to_string).collect();
    match lines.iter().position(|line| line.trim() == "allowBuilds:") {
        Some(idx) => {
            let already = lines[idx + 1..].iter().any(|line| {
                let key = line
                    .trim_start()
                    .split(':')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .trim_matches('"');
                key == "node-pty"
            });
            if already {
                return false;
            }
            lines.insert(idx + 1, "  node-pty: true".to_string());
        }
        None => {
            lines.push(String::new());
            lines.push("allowBuilds:".to_string());
            lines.push("  node-pty: true".to_string());
        }
    }
    let patched = lines.join("
") + "
";
    std::fs::write(workspace, patched).is_ok()
}

/// Whether a webview navigation stays in the window: internal app pages
/// (tauri://) and harness-origin navigations (SPA routes) stay; everything
/// else — external links, mailto — goes to the system default browser.
fn navigation_decision(url: &Url, expected_host: Option<&str>) -> bool {
    if url.scheme() == "tauri" {
        return true;
    }
    url.host_str() == expected_host
}

/// window.open / target=_blank handler: same-origin popups keep Tauri's
/// default new-window behavior; external URLs open in the system default
/// browser instead of a new in-app window.
fn new_window_policy(
    url: Url,
    _features: tauri::webview::NewWindowFeatures,
    expected_host: &Mutex<Option<String>>,
) -> tauri::webview::NewWindowResponse<tauri::Wry> {
    let host = expected_host.lock().ok().and_then(|h| h.clone());
    if navigation_decision(&url, host.as_deref()) {
        return tauri::webview::NewWindowResponse::Allow;
    }
    let _ = opener::open(url.as_str());
    tauri::webview::NewWindowResponse::Deny
}

/// Patch every existing profile's pnpm-workspace.yaml so plugin installs can
/// run node-pty's install script (see ensure_node_pty_allow_builds).
fn patch_profiles_for_node_pty() {
    let profiles = harness_home().join("profiles");
    let Ok(entries) = std::fs::read_dir(&profiles) else { return };
    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let workspace = entry.path().join("pnpm-workspace.yaml");
        if !workspace.is_file() {
            continue;
        }
        if ensure_node_pty_allow_builds(&workspace) {
            println!("[desktop] allowed node-pty build script in {}", workspace.display());
        }
    }
}

/// Well-known file recording the URL of the harness THIS app spawned, so a
/// second app instance attaches to the same server instead of spawning a
/// second writer (two harness processes writing one session log corrupt it).
fn harness_port_file() -> PathBuf {
    harness_home().join("desktop-harness.url")
}

/// Read and probe a recorded harness URL from the port file.
fn recorded_harness_url() -> Option<Url> {
    let path = harness_port_file();
    let recorded = std::fs::read_to_string(&path).ok()?;
    let url = Url::parse(recorded.trim()).ok()?;
    if url.scheme() != "http" {
        return None;
    }
    probe_harness(&url).then_some(url)
}

/// Record the URL of the harness this instance spawned (owned mode only).
fn record_harness_url(url: &Url) {
    let path = harness_port_file();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if std::fs::write(&path, format!("{url}\n")).is_err() {
        eprintln!("[desktop] failed to record harness URL at {}", path.display());
    }
}

/// Remove the recorded URL when the owning instance exits with its harness.
fn clear_recorded_harness_url() {
    let _ = std::fs::remove_file(harness_port_file());
}

/// A harness already serving this machine: the URL the shell attaches to
/// instead of spawning a second server. Lookup order: DSH_DESKTOP_HARNESS_URL
/// (loopback http only), the web profile's default port, then the URL
/// recorded by a previous app instance of this shell.
fn existing_harness_url() -> Option<Url> {
    if let Ok(candidate) = std::env::var("DSH_DESKTOP_HARNESS_URL") {
        if let Ok(url) = Url::parse(&candidate) {
            let loopback = url.host_str() == Some("127.0.0.1") || url.host_str() == Some("localhost");
            if url.scheme() == "http" && loopback {
                if probe_harness(&url) {
                    return Some(url);
                }
            } else {
                eprintln!("[desktop] ignoring DSH_DESKTOP_HARNESS_URL: only loopback http is accepted");
            }
        }
    }
    if let Ok(url) = Url::parse(DEFAULT_HARNESS_URL) {
        if probe_harness(&url) {
            return Some(url);
        }
    }
    recorded_harness_url()
}


/// How long the harness may take to dispose its tree after SIGTERM before
/// the shell escalates. The harness's own shutdown budget is 5s; a pending
/// tool call or approval can keep it alive longer, and killing it mid-flush
/// is what leaves a session log in a resumable-but-dirty state.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

/// Signal the harness to shut down cleanly (SIGTERM), escalate to SIGKILL
/// only when it is genuinely stuck.
#[cfg(unix)]
fn terminate(child: &mut Child) {
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let deadline = std::time::Instant::now() + SHUTDOWN_GRACE;
    while std::time::Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => thread::sleep(Duration::from_millis(200)),
            Err(_) => break,
        }
    }
    eprintln!("[desktop] harness did not exit within {SHUTDOWN_GRACE:?}; escalating to SIGKILL");
    let _ = child.kill();
}

#[cfg(not(unix))]
fn terminate(child: &mut Child) {
    let _ = child.kill();
}

/// The pet plugin's standalone page route (served by the harness).
const PET_PAGE_PATH: &str = "/voice-pet/pet";
/// Window geometry for the pet window (logical points) and screen margin.
const PET_WINDOW_SIZE: (f64, f64) = (360.0, 480.0);
const PET_SCREEN_MARGIN: f64 = 16.0;
/// Approximate macOS Dock height when it sits at the bottom of the screen.
const PET_DOCK_OFFSET: f64 = 72.0;

/// Perform a minimal HTTP GET and return the raw response (headers + body).
fn http_get(url: &Url) -> Option<String> {
    let host = url.host_str()?;
    let port = url.port_or_known_default()?;
    let mut stream = TcpStream::connect((host, port)).ok()?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let crlf = String::from_utf8(vec![13, 10]).expect("CRLF is valid utf-8");
    let request = format!("GET {} HTTP/1.1{crlf}Host: {host}{crlf}Connection: close{crlf}{crlf}", url.path());
    stream.write_all(request.as_bytes()).ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    Some(response)
}

/// Marker proving a response is the pet plugin's standalone page: the harness
/// SPA answers 200 with the GUI's index.html for unknown routes, so a 2xx
/// alone would open the pet window showing the harness homepage.
const PET_PAGE_MARKER: &str = "pet-standalone.js";

/// Whether the pet plugin's standalone page is genuinely served.
fn pet_page_served(url: &Url) -> bool {
    http_get(url)
        .map(|response| {
            let status_ok =
                response.starts_with("HTTP/1.1 2") || response.starts_with("HTTP/1.0 2");
            status_ok && response.contains(PET_PAGE_MARKER)
        })
        .unwrap_or(false)
}

/// Place a window at the bottom-right of the primary screen's work area.
/// Uses LOGICAL coordinates computed from the configured window size, because
/// outer_size may not be settled immediately after build (a zero size would
/// push the window off the right edge). Re-applies once after a short delay.
fn place_bottom_right(window: &WebviewWindow, app: &tauri::AppHandle) {
    let Some(monitor) = app.primary_monitor().ok().flatten() else { return };
    let scale = monitor.scale_factor();
    let msize = monitor.size();
    let mw = msize.width as f64 / scale;
    let mh = msize.height as f64 / scale;
    let x = (mw - PET_WINDOW_SIZE.0 - PET_SCREEN_MARGIN).max(0.0);
    let y = (mh - PET_WINDOW_SIZE.1 - PET_SCREEN_MARGIN - PET_DOCK_OFFSET).max(0.0);
    let position = tauri::LogicalPosition::new(x, y);
    let _ = window.set_position(position);
    println!("[desktop] pet window placed at logical ({x:.0}, {y:.0})");
    let window_for_retry = window.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(800));
        let _ = window_for_retry.set_position(position);
    });
}

/// Create the standalone desktop-pet window when the harness serves the pet
/// plugin's page. The window is frameless, transparent, always-on-top, and
/// parked at the bottom-right; the main window can be minimized independently.
fn maybe_create_pet_window(app: &tauri::AppHandle, base_url: &Url) {
    let pet_url = match std::env::var("DSH_DESKTOP_PET_URL") {
        Ok(override_url) => {
            if let Ok(parsed) = Url::parse(&override_url) {
                if parsed.scheme() == "http" {
                    parsed
                } else {
                    eprintln!("[desktop] ignoring DSH_DESKTOP_PET_URL: http only");
                    base_url.join(PET_PAGE_PATH).ok().unwrap_or(base_url.clone())
                }
            } else {
                base_url.join(PET_PAGE_PATH).ok().unwrap_or(base_url.clone())
            }
        }
        Err(_) => match base_url.join(PET_PAGE_PATH).ok() {
            Some(url) => url,
            None => return,
        },
    };
    if !pet_page_served(&pet_url) {
        println!("[desktop] pet plugin page not served at {pet_url}; skipping pet window");
        return;
    }
    let pet = match WebviewWindowBuilder::new(app, "pet", WebviewUrl::External(pet_url.clone()))
        .title("语音桌宠")
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(false)
        .inner_size(PET_WINDOW_SIZE.0, PET_WINDOW_SIZE.1)
        .build()
    {
        Ok(pet) => pet,
        Err(error) => {
            eprintln!("[desktop] failed to create the pet window: {error}");
            return;
        }
    };
    place_bottom_right(&pet, app);
    println!("[desktop] pet window created at {pet_url}");
}

/// Kill the harness child, if still owned.
fn stop_harness(app: &tauri::AppHandle) {
    if let Some(state) = app.try_state::<Harness>() {
        if let Ok(mut guard) = state.0.lock() {
            if let Some(child) = guard.as_mut() {
                terminate(child);
                let _ = child.wait();
            }
        }
    }
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            // pnpm >=10 blocks node-pty's install script unless allowlisted;
            // plugins with a terminal feature need it to run.
            patch_profiles_for_node_pty();
            // The harness host the main window may navigate within; everything
            // else opens in the system default browser.
            let expected_host = Arc::new(Mutex::new(None::<String>));
            // Reuse-first: attaching to a running harness is the supported way
            // to view the same sessions from web and desktop simultaneously.
            // The shell owns no process in this mode, so exit does not stop it.
            if let Some(url) = existing_harness_url() {
                println!("[desktop] reusing running harness at {url}");
                if let Some(host) = url.host_str() {
                    *expected_host.lock().unwrap() = Some(host.to_string());
                }
                let expected_for_new_window = expected_host.clone();
                WebviewWindowBuilder::new(app, "main", WebviewUrl::External(url.clone()))
                    .title("DeepSeek Harness")
                    .inner_size(1280.0, 820.0)
                    .min_inner_size(720.0, 480.0)
                    .on_new_window(move |url, features| new_window_policy(url, features, &expected_for_new_window))
                    .build()
                    .expect("failed to create the main window");
                maybe_create_pet_window(app.handle(), &url);
                return Ok(());
            }

            let (node, args, root, path_dirs) = harness_command(app.path().resource_dir().ok().as_deref());
            println!(
                "[desktop] spawning harness: {} {} (cwd {})",
                node,
                args.join(" "),
                root.display()
            );
            let mut command = Command::new(&node);
            command
                .args(&args)
                .current_dir(&root)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            // Put the bundled toolchain first on PATH so node/npm/pnpm
            // spawned by the harness (agent shell calls, dsh plugin add)
            // resolve to the bundle instead of a system install.
            if !path_dirs.is_empty() {
                if let Ok(existing) = std::env::var("PATH") {
                    let mut parts: Vec<String> = path_dirs
                        .iter()
                        .map(|dir| dir.to_string_lossy().into_owned())
                        .collect();
                    parts.push(existing);
                    #[cfg(windows)]
                    let joined = parts.join(";");
                    #[cfg(not(windows))]
                    let joined = parts.join(":");
                    command.env("PATH", joined);
                }
            }
            // node.exe is a console app; without CREATE_NO_WINDOW Windows
            // pops a cmd window for it at every launch.
            #[cfg(windows)]
            command.creation_flags(0x0800_0000);
            let mut child = command.spawn().expect("failed to spawn the dsh harness");
            let stdout = child.stdout.take().expect("harness stdout pipe");
            let stderr = child.stderr.take().expect("harness stderr pipe");
            app.manage(Harness(Mutex::new(Some(child))));

            // SIGINT/SIGTERM must unwind through the event loop so the exit
            // handler can stop the harness: a killed process would orphan it.
            #[cfg(unix)]
            {
                use signal_hook::consts::signal::{SIGINT, SIGTERM};
                use signal_hook::iterator::Signals;
                let mut signals = Signals::new([SIGINT, SIGTERM])
                    .expect("failed to register SIGINT/SIGTERM handlers");
                let handle_for_signal = app.handle().clone();
                thread::spawn(move || {
                    if let Some(sig) = signals.forever().next() {
                        eprintln!("[desktop] received signal {sig}; shutting down");
                        handle_for_signal.exit(0);
                    }
                });
            }

            let expected_for_new_window = expected_host.clone();
            let window = WebviewWindowBuilder::new(
                app,
                "main",
                WebviewUrl::App(SPINNER_PAGE.into()),
            )
            .title("DeepSeek Harness")
            .inner_size(1280.0, 820.0)
            .min_inner_size(720.0, 480.0)
            .on_new_window(move |url, features| new_window_policy(url, features, &expected_for_new_window))
            .build()
            .expect("failed to create the main window");

            let ready = Arc::new(AtomicBool::new(false));

            // Read the harness stdout until the readiness URL line arrives.
            let window_for_url = window.clone();
            let ready_for_watchdog = ready.clone();
            let app_for_stdout_end = app.handle().clone();
            let app_for_pet = app.handle().clone();
            let expected_host_for_url = expected_host.clone();
            thread::spawn(move || {
                let lines = BufReader::new(stdout).lines();
                for line in lines.flatten() {
                    println!("[harness] {line}");
                    if let Some(url) = parse_gui_url(&line) {
                        // Navigate only on the first URL line; keep draining so
                        // harness output still reaches the console.
                        if !ready_for_watchdog.swap(true, Ordering::SeqCst) {
                            println!("[desktop] harness ready at {url}");
                            record_harness_url(&url);
                            if let Some(host) = url.host_str() {
                                *expected_host_for_url.lock().unwrap() = Some(host.to_string());
                            }
                            let _ = window_for_url.navigate(url.clone());
                            maybe_create_pet_window(&app_for_pet, &url);
                        }
                    }
                }
                // stdout ended: the harness died. If it never reported a URL, fail loudly.
                if !ready_for_watchdog.load(Ordering::SeqCst) {
                    eprintln!("[desktop] harness exited without reporting a URL");
                    let _ = window_for_url.navigate(error_page_url());
                    app_for_stdout_end.exit(1);
                }
            });

            // Forward harness stderr for diagnosis.
            thread::spawn(move || {
                for line in BufReader::new(stderr).lines().flatten() {
                    eprintln!("[harness:stderr] {line}");
                }
            });

            // Watchdog: fail loudly if readiness never arrives.
            let window_for_timeout = window.clone();
            let ready_for_timeout = ready.clone();
            let app_for_timeout = app.handle().clone();
            thread::spawn(move || {
                thread::sleep(READY_TIMEOUT);
                if !ready_for_timeout.load(Ordering::SeqCst) {
                    eprintln!("[desktop] harness not ready within {READY_TIMEOUT:?}");
                    let _ = window_for_timeout.navigate(error_page_url());
                    if let Some(state) = app_for_timeout.try_state::<Harness>() {
                        if let Ok(mut guard) = state.0.lock() {
                            if let Some(child) = guard.as_mut() {
                                let _ = child.kill();
                            }
                        }
                    }
                    app_for_timeout.exit(1);
                }
            });

            // Monitor: a harness that dies mid-session takes the app down with it.
            let app_for_monitor = app.handle().clone();
            let ready_for_monitor = ready.clone();
            thread::spawn(move || {
                loop {
                    thread::sleep(MONITOR_INTERVAL);
                    let exited = {
                        let mut exited = false;
                        if let Some(state) = app_for_monitor.try_state::<Harness>() {
                            if let Ok(mut guard) = state.0.lock() {
                                if let Some(child) = guard.as_mut() {
                                    exited = child.try_wait().map(|s| s.is_some()).unwrap_or(false);
                                }
                            }
                        }
                        exited
                    };
                    if exited && ready_for_monitor.load(Ordering::SeqCst) {
                        eprintln!("[desktop] harness exited unexpectedly; quitting");
                        app_for_monitor.exit(1);
                        return;
                    }
                    if app_for_monitor.try_state::<Harness>().is_none() {
                        return;
                    }
                }
            });

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building the tauri application")
        .run(|app_handle, event| {
            if let RunEvent::ExitRequested { .. } | RunEvent::Exit = event {
                // Only the instance that OWNED the harness clears the recorded
                // URL; a reuse-mode instance must leave the owner's record.
                if app_handle.try_state::<Harness>().is_some() {
                    stop_harness(app_handle);
                    clear_recorded_harness_url();
                }
            }
        });
}


#[cfg(test)]
mod pnpm_workspace_tests {
    use super::ensure_node_pty_allow_builds;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_workspace(name: &str, content: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dsh-pty-test-{}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst),
            name
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pnpm-workspace.yaml");
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn adds_allow_builds_to_template() {
        let path = temp_workspace("template", "packages:\n  - .\n\nnodeLinker: hoisted\nautoInstallPeers: false\n");
        assert!(ensure_node_pty_allow_builds(&path));
        let patched = fs::read_to_string(&path).unwrap();
        assert!(patched.contains("allowBuilds:\n  node-pty: true\n"), "{patched}");
        // Idempotent: a second pass changes nothing.
        assert!(!ensure_node_pty_allow_builds(&path));
    }

    #[test]
    fn adds_entry_to_existing_allow_builds() {
        let path = temp_workspace("existing", "packages:\n  - .\n\nallowBuilds:\n  esbuild: true\n");
        assert!(ensure_node_pty_allow_builds(&path));
        let patched = fs::read_to_string(&path).unwrap();
        assert!(patched.contains("allowBuilds:\n  node-pty: true\n  esbuild: true\n"), "{patched}");
    }

    #[test]
    fn leaves_existing_node_pty_alone() {
        let path = temp_workspace("already", "packages:\n  - .\n\nallowBuilds:\n  node-pty: true\n");
        assert!(!ensure_node_pty_allow_builds(&path));
        let patched = fs::read_to_string(&path).unwrap();
        assert_eq!(patched.lines().filter(|l| l.trim().starts_with("node-pty")).count(), 1);
    }

    #[test]
    fn recognizes_quoted_entry() {
        let path = temp_workspace("quoted", "packages:\n  - .\n\nallowBuilds:\n  \"node-pty\": true\n");
        assert!(!ensure_node_pty_allow_builds(&path));
    }

    #[test]
    fn missing_file_is_a_noop() {
        let missing = std::env::temp_dir().join(format!(
            "dsh-pty-test-{}-{}-missing.yaml",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        assert!(!ensure_node_pty_allow_builds(&missing));
    }
}


#[cfg(test)]
mod navigation_tests {
    use super::navigation_decision;
    use tauri::Url;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn internal_pages_always_stay() {
        assert!(navigation_decision(&url("tauri://localhost/error.html"), None));
        assert!(navigation_decision(&url("tauri://localhost/index.html"), Some("127.0.0.1")));
    }

    #[test]
    fn harness_origin_stays() {
        assert!(navigation_decision(&url("http://127.0.0.1:3080/"), Some("127.0.0.1")));
        assert!(navigation_decision(&url("http://127.0.0.1:3080/sessions/abc"), Some("127.0.0.1")));
    }

    #[test]
    fn external_links_leave() {
        assert!(!navigation_decision(&url("https://example.com/a?b=1"), Some("127.0.0.1")));
        assert!(!navigation_decision(&url("http://localhost:3080/"), Some("127.0.0.1")));
        assert!(!navigation_decision(&url("mailto:a@b.c"), Some("127.0.0.1")));
    }

    #[test]
    fn unknown_host_before_readiness_leaves() {
        assert!(!navigation_decision(&url("https://example.com/"), None));
    }
}

