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

/// The harness invocation: web --port 0 so the OS picks a free port.
/// In a packaged app the bundled node binary and npm-installed harness
/// (resources/) drive the process; in a dev build node runs the repo source
/// via tsx. DSH_DESKTOP_NODE and DSH_DESKTOP_REPO_ROOT override both modes.
fn harness_command(resource_dir: Option<&Path>) -> (String, Vec<String>, PathBuf) {
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
            "--port".to_string(),
            "0".to_string(),
        ];
        return (node, args, root);
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
    (node, args, root)
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

/// Whether a URL answers with any 2xx (probe for plugin pages; unlike the
/// boot-manifest probe, the pet page carries no __DSH_BOOT__ marker).
fn probe_url_ok(url: &Url) -> bool {
    let Some(host) = url.host_str() else { return false };
    let Some(port) = url.port_or_known_default() else { return false };
    let Ok(mut stream) = TcpStream::connect((host, port)) else { return false };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let crlf = String::from_utf8(vec![13, 10]).expect("CRLF is valid utf-8");
    let request = format!("GET {} HTTP/1.1{crlf}Host: {host}{crlf}Connection: close{crlf}{crlf}", url.path());
    if stream.write_all(request.as_bytes()).is_err() { return false }
    let mut response = String::new();
    if stream.read_to_string(&mut response).is_err() { return false }
    response.starts_with("HTTP/1.1 2") || response.starts_with("HTTP/1.0 2")
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
    if !probe_url_ok(&pet_url) {
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
            // Reuse-first: attaching to a running harness is the supported way
            // to view the same sessions from web and desktop simultaneously.
            // The shell owns no process in this mode, so exit does not stop it.
            if let Some(url) = existing_harness_url() {
                println!("[desktop] reusing running harness at {url}");
                WebviewWindowBuilder::new(app, "main", WebviewUrl::External(url.clone()))
                    .title("DeepSeek Harness")
                    .inner_size(1280.0, 820.0)
                    .min_inner_size(720.0, 480.0)
                    .build()
                    .expect("failed to create the main window");
                maybe_create_pet_window(app.handle(), &url);
                return Ok(());
            }

            let (node, args, root) = harness_command(app.path().resource_dir().ok().as_deref());
            println!(
                "[desktop] spawning harness: {} {} (cwd {})",
                node,
                args.join(" "),
                root.display()
            );
            let mut child = Command::new(&node)
                .args(&args)
                .current_dir(&root)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("failed to spawn the dsh harness");
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

            let window = WebviewWindowBuilder::new(
                app,
                "main",
                WebviewUrl::App(SPINNER_PAGE.into()),
            )
            .title("DeepSeek Harness")
            .inner_size(1280.0, 820.0)
            .min_inner_size(720.0, 480.0)
            .build()
            .expect("failed to create the main window");

            let ready = Arc::new(AtomicBool::new(false));

            // Read the harness stdout until the readiness URL line arrives.
            let window_for_url = window.clone();
            let ready_for_watchdog = ready.clone();
            let app_for_stdout_end = app.handle().clone();
            let app_for_pet = app.handle().clone();
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

