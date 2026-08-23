use std::sync::mpsc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_clipboard_manager::ClipboardExt;

const GROQ_URL: &str = "https://api.groq.com/openai/v1/audio/transcriptions";

// ---------------------------------------------------------------------------
// Push-to-talk: Ctrl+Win held = record, release = transcribe + paste.
// Implemented with a WH_KEYBOARD_LL hook because RegisterHotKey can't bind
// bare modifier chords. ponytail: hardcoded chord; hotkey settings UI in Phase 2.
// ---------------------------------------------------------------------------

const VK_LWIN: u32 = 0x5B;
const VK_RWIN: u32 = 0x5C;
const VK_LCONTROL: u32 = 0xA2;
const VK_RCONTROL: u32 = 0xA3;

#[derive(Debug, PartialEq)]
enum Chord {
    Pressed,
    Released,
}

/// Extra action the hook should take for this event.
#[derive(Debug, PartialEq)]
enum Action {
    None,
    /// Forward the Win key-up, but first tap a synthetic Ctrl so Windows treats
    /// the gesture as a chord and skips opening the Start menu. Never swallow
    /// the key-up itself — eating it leaves the OS convinced Win is still held
    /// (stuck-Win bugs, broken Win+Space shortcuts).
    CancelStartMenu,
}

/// Pure edge-detector for the Ctrl+Win chord so the tricky bits are unit-tested
/// without Windows.
#[derive(Default)]
struct ChordTracker {
    ctrl: bool,
    win: bool,
    /// True while both keys are held (one Pressed / one Released per gesture).
    engaged: bool,
    /// True from first Pressed until both keys are fully released — spans the
    /// window where one key is still held, so a late Win keyup gets swallowed.
    gestured: bool,
}

impl ChordTracker {
    /// Feed one key event. Returns an optional chord transition plus the action
    /// the hook should take for this event.
    fn update(&mut self, vk: u32, down: bool) -> (Option<Chord>, Action) {
        match vk {
            VK_LWIN | VK_RWIN => self.win = down,
            VK_LCONTROL | VK_RCONTROL => self.ctrl = down,
            _ => {}
        }
        let active = self.ctrl && self.win;
        let mut event = None;
        if active {
            if !self.engaged {
                self.engaged = true;
                self.gestured = true;
                event = Some(Chord::Pressed);
            }
        } else if self.engaged {
            self.engaged = false;
            event = Some(Chord::Released);
        }
        let action =
            if !down && (vk == VK_LWIN || vk == VK_RWIN) && self.gestured { Action::CancelStartMenu } else { Action::None };
        if !self.ctrl && !self.win {
            self.gestured = false;
        }
        (event, action)
    }
}

static TRACKER: Mutex<Option<ChordTracker>> = Mutex::new(None);
static CHORD_TX: std::sync::OnceLock<mpsc::Sender<Chord>> = std::sync::OnceLock::new();
static PILL_WINDOW: std::sync::OnceLock<tauri::WebviewWindow> = std::sync::OnceLock::new();
static TOAST_WINDOW: std::sync::OnceLock<tauri::WebviewWindow> = std::sync::OnceLock::new();

/// Frameless always-on-top overlay (pill, toast). ponytail: caller must keep the
/// returned handle alive — dropping the last ref destroys the native window.
fn build_overlay(
    app: &AppHandle,
    label: &str,
    title: &str,
    url: &str,
    w: f64,
    h: f64,
) -> tauri::Result<tauri::WebviewWindow> {
    tauri::WebviewWindowBuilder::new(app, label, tauri::WebviewUrl::App(url.into()))
        .title(title)
        .inner_size(w, h)
        .visible(false)
        .decorations(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(false)
        .transparent(true)
        .shadow(false)
        .build()
}

fn spawn_hotkey_hook(app: AppHandle) {
    let (tx, rx) = mpsc::channel();
    let _ = CHORD_TX.set(tx);
    // Drain thread does the slow work (mic open etc.) so the hook proc stays fast.
    tauri::async_runtime::spawn_blocking(move || {
        while let Ok(event) = rx.recv() {
            match event {
                Chord::Pressed => {
                    start_recording(&app);
                    show_pill(&app);
                }
                Chord::Released => {
                    stop_recording(&app);
                    hide_pill(&app);
                }
            }
        }
    });
    // ponytail: never unhooked — lives until process exit
    std::thread::spawn(|| unsafe { keyboard_hook_loop() });
}

#[cfg(windows)]
unsafe fn keyboard_hook_loop() {
    use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, GetMessageW, SetWindowsHookExW, HHOOK, KBDLLHOOKSTRUCT, MSG,
        WH_KEYBOARD_LL,
    };

    unsafe extern "system" fn proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code >= 0 {
            let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
            const LLKHF_INJECTED: u32 = 0x10;
            // Ignore synthetic keys — our own paste's Ctrl+V and the Start-menu
            // cancel tap below must not retrigger us.
            if kb.flags.0 & LLKHF_INJECTED == 0 {
                const WM_KEYDOWN: usize = 0x100;
                const WM_SYSKEYDOWN: usize = 0x104;
                let down = wparam.0 == WM_KEYDOWN || wparam.0 == WM_SYSKEYDOWN;
                let (event, action) = {
                    let mut guard = TRACKER.lock().unwrap();
                    guard.get_or_insert_with(ChordTracker::default).update(kb.vkCode, down)
                };
                if let Some(event) = event {
                    if let Some(tx) = CHORD_TX.get() {
                        let _ = tx.send(event);
                    }
                }
                // Always fall through — swallowing input is how Win gets stuck.
                if action == Action::CancelStartMenu {
                    inject_ctrl_tap();
                }
            }
        }
        CallNextHookEx(HHOOK::default(), code, wparam, lparam)
    }

    let _ = SetWindowsHookExW(WH_KEYBOARD_LL, Some(proc), HINSTANCE::default(), 0)
        .inspect_err(|e| eprintln!("[pitch] hook install failed: {e}"));
    let mut msg = MSG::default();
    while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {}
}

/// Inject a Ctrl down+up pair so Windows counts "another key pressed during the
/// Win hold" and skips the Start menu. Injected events are ignored by our own
/// tracker.
#[cfg(windows)]
unsafe fn inject_ctrl_tap() {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
        VK_CONTROL,
    };
    let tap = |flags: KEYBD_EVENT_FLAGS| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VK_CONTROL,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    SendInput(&[tap(KEYBD_EVENT_FLAGS(0)), tap(KEYEVENTF_KEYUP)], std::mem::size_of::<INPUT>() as i32);
}

// ---------------------------------------------------------------------------
// Recording pipeline
// ---------------------------------------------------------------------------

#[derive(Default)]
struct AppState(Mutex<Option<Session>>);

struct Session {
    stop: Arc<AtomicBool>,
    done: mpsc::Receiver<Result<Capture, String>>,
}

#[derive(Clone)]
struct Capture {
    samples: Vec<i16>,
    rate: u32,
    channels: u16,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Engine {
    Local,
    Cloud,
}

impl Engine {
    fn name(self) -> &'static str {
        match self {
            Engine::Local => "parakeet",
            Engine::Cloud => "groq",
        }
    }
}

/// ponytail: PITCH_ENGINE=auto|local|cloud env for now; settings UI in Phase 2.
/// auto = Parakeet first with Groq as fallback.
fn engines(setting: Option<&str>, model_present: bool) -> Vec<Engine> {
    match setting {
        Some("local") => vec![Engine::Local],
        Some("cloud") => vec![Engine::Cloud],
        _ if model_present => vec![Engine::Local, Engine::Cloud],
        _ => vec![Engine::Cloud],
    }
}

fn model_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8")
}

// ---------------------------------------------------------------------------
// History: append-only JSONL in the app data dir
// ---------------------------------------------------------------------------

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Entry {
    ts: u64,
    text: String,
    engine: String,
    ms: u64,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn history_path(app: &AppHandle) -> Option<std::path::PathBuf> {
    let dir = app.path().app_data_dir().ok()?;
    let _ = std::fs::create_dir_all(&dir);
    Some(dir.join("history.jsonl"))
}

fn save_entry(app: &AppHandle, text: &str, engine: &str, ms: u64) {
    let Some(path) = history_path(app) else { return };
    let entry = Entry { ts: now_ms(), text: text.into(), engine: engine.into(), ms };
    let Ok(line) = serde_json::to_string(&entry) else { return };
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        use std::io::Write;
        let _ = writeln!(f, "{line}");
    }
}

fn load_history(app: &AppHandle) -> Vec<Entry> {
    let Some(path) = history_path(app) else { return vec![] };
    let Ok(content) = std::fs::read_to_string(&path) else { return vec![] };
    let mut entries: Vec<Entry> =
        content.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    entries.sort_by(|a, b| b.ts.cmp(&a.ts));
    entries
}

#[tauri::command]
fn list_history(app: AppHandle) -> Vec<Entry> {
    load_history(&app)
}

#[tauri::command]
fn delete_history(app: AppHandle, ts: u64) -> bool {
    let Some(path) = history_path(&app) else { return false };
    let Ok(content) = std::fs::read_to_string(&path) else { return false };
    let mut out = String::new();
    for line in content.lines() {
        match serde_json::from_str::<Entry>(line) {
            Ok(e) if e.ts == ts => continue,
            _ => out.push_str(line),
        }
        out.push('\n');
    }
    std::fs::write(&path, out).is_ok()
}

#[tauri::command]
fn copy_text(app: AppHandle, text: String) -> bool {
    app.clipboard().write_text(text).is_ok()
}

// ponytail: headless diagnostic channel for webview pages
#[tauri::command]
fn debug_ping(msg: String) {
    println!("[hub] {msg}");
}

pub fn run() {
    tauri::Builder::default()
        .manage(AppState::default())
        .plugin(tauri_plugin_clipboard_manager::init())
        .setup(|app| {
            spawn_hotkey_hook(app.handle().clone());

            // ponytail: built here instead of tauri.conf.json — conf-defined second
            // window silently never materialized on this machine.
            let pill = build_overlay(app.handle(), "pill", "pitch-pill", "pill.html", 260.0, 76.0)?;
            let _ = PILL_WINDOW.set(pill);
            let toast =
                build_overlay(app.handle(), "toast", "pitch-toast", "toast.html", 340.0, 88.0)?;
            let _ = TOAST_WINDOW.set(toast);
            for (label, _) in app.webview_windows() {
                println!("[pitch] window created: {label}");
            }

            let open_hub = tauri::menu::MenuItem::with_id(
                app,
                "open_hub",
                "Open pitch",
                true,
                None::<&str>,
            )?;
            let test = tauri::menu::MenuItem::with_id(
                app,
                "test_pill",
                "Test pill",
                true,
                None::<&str>,
            )?;
            let quit =
                tauri::menu::MenuItem::with_id(app, "quit", "Quit pitch", true, None::<&str>)?;
            let menu = tauri::menu::Menu::with_items(app, &[&open_hub, &test, &quit])?;
            tauri::tray::TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("pitch — hold Ctrl+Win to dictate")
                .menu(&menu)
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "quit" => app.exit(0),
                    "open_hub" => {
                        if let Some(hub) = app.get_webview_window("main") {
                            let _ = hub.show();
                            let _ = hub.set_focus();
                        }
                    }
                    "test_pill" => {
                        show_pill(app);
                        let app = app.clone();
                        std::thread::spawn(move || {
                            std::thread::sleep(std::time::Duration::from_millis(3000));
                            hide_pill(&app);
                        });
                    }
                    _ => {}
                })
                .build(app)?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_history,
            delete_history,
            copy_text,
            debug_ping
        ])
        .on_window_event(|window, event| {
            // Hub close hides to tray instead of quitting.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// Recording indicator: bottom-center pill with animated bars + pings.
fn show_pill(app: &AppHandle) {
    let Some(win) = app.get_webview_window("pill") else {
        eprintln!("[pitch] pill window NOT FOUND");
        return;
    };
    if let Ok(Some(monitor)) = app.primary_monitor() {
        let size = monitor.size();
        eprintln!("[pitch] monitor {}x{}, positioning pill", size.width, size.height);
        let _ = win.set_position(tauri::PhysicalPosition::new(
            (size.width as i32 - 260) / 2,
            size.height as i32 - 160,
        ));
    } else {
        eprintln!("[pitch] no primary monitor info, default position");
    }
    if let Err(e) = win.show() {
        eprintln!("[pitch] pill show failed: {e}");
    }
    match win.eval("setActive(true)") {
        Ok(()) => println!("[pitch] pill shown"),
        Err(e) => eprintln!("[pill] eval failed: {e}"),
    }
}

fn hide_pill(app: &AppHandle) {
    let Some(win) = app.get_webview_window("pill") else { return };
    let _ = win.eval("setActive(false)");
    // ponytail: delayed so the stop ping finishes before the webview sleeps
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(400));
        let _ = win.hide();
    });
}

/// Show a brief always-on-top status message and auto-hide.
fn toast(app: &AppHandle, msg: &str, ms: u64) {
    let Some(win) = app.get_webview_window("toast") else { return };
    let json = serde_json::to_string(msg).unwrap_or_default();
    let _ = win.eval(&format!("setStatus({json})"));
    let _ = win.show();
    let win = win.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(ms));
        let _ = win.hide();
    });
}

fn start_recording(app: &AppHandle) {
    let mut state = app.state::<AppState>().inner().0.lock().unwrap();
    if state.is_some() {
        return;
    }
    let (tx, done) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    // ponytail: dedicated blocking thread owns the !Send cpal Stream; 20ms poll is
    // noise next to human hotkey latency. Event-driven wake-up only if CPU ever matters.
    tauri::async_runtime::spawn_blocking(move || record(flag, tx));
    *state = Some(Session { stop, done });
    println!("[pitch] recording…");
}

fn stop_recording(app: &AppHandle) {
    let session = app.state::<AppState>().inner().0.lock().unwrap().take();
    let Some(session) = session else { return };
    session.stop.store(true, Ordering::Relaxed);
    println!("[pitch] transcribing…");
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let capture = match session.done.recv() {
            Ok(Ok(capture)) => capture,
            Ok(Err(e)) => {
                toast(&app, &e, 3500);
                return eprintln!("[pitch] mic error: {e}");
            }
            Err(_) => return eprintln!("[pitch] recorder died"),
        };

        let setting = std::env::var("PITCH_ENGINE").ok();
        let list = engines(setting.as_deref(), model_dir().join("encoder.int8.onnx").exists());
        let mut last_err = None;
        for engine in list {
            println!("[pitch] engine: {engine:?}");
            let result = match engine {
                Engine::Local => {
                    let capture = capture.clone();
                    tauri::async_runtime::spawn_blocking(move || transcribe_local(&capture))
                        .await
                        .unwrap_or_else(|e| Err(e.to_string()))
                }
                Engine::Cloud => {
                    transcribe_cloud(encode_wav(&capture.samples, capture.rate, capture.channels))
                        .await
                }
            };
            match result {
                Ok(text) if text.trim().is_empty() => {
                    // Silent audio is terminal — no engine can do better.
                    toast(&app, "Nothing heard", 2500);
                    return eprintln!("[pitch] empty transcript");
                }
                // First engine to produce text wins — the chain exists for
                // transcription failures only, so stop here either way.
                Ok(text) => {
                    let audio_ms = capture.samples.len() as u64 * 1000
                        / (capture.rate as u64 * capture.channels.max(1) as u64);
                    save_entry(&app, &text, engine.name(), audio_ms);
                    let _ = app.emit("dictation", ());
                    match paste(&app, &text) {
                        Ok(()) => {
                            let preview: String = text.chars().take(48).collect();
                            toast(&app, &format!("✓ {preview}"), 1600);
                            println!("[pitch] pasted: {text}");
                        }
                        Err(e) => {
                            toast(&app, &format!("Paste failed: {e}"), 3500);
                            eprintln!("[pitch] paste failed: {e}")
                        }
                    }
                    return;
                }
                Err(e) => {
                    toast(&app, &format!("{engine:?} failed: {e}"), 3500);
                    eprintln!("[pitch] {engine:?} failed: {e}");
                    last_err = Some(e);
                }
            }
        }
        if last_err.is_some() {
            toast(&app, "All transcription engines failed", 3500);
        }
    });
}

fn record(stop: Arc<AtomicBool>, tx: mpsc::Sender<Result<Capture, String>>) {
    let res = (|| -> Result<Capture, String> {
        let device = cpal::default_host()
            .default_input_device()
            .ok_or("no microphone found")?;
        let cfg = device.default_input_config().map_err(|e| e.to_string())?;
        let (rate, ch, fmt) = (cfg.sample_rate().0, cfg.channels(), cfg.sample_format());
        println!("[pitch] mic open: {rate} Hz, {ch}ch, {fmt:?}");

        let buf = Arc::new(Mutex::new(Vec::<i16>::new()));
        let sink = buf.clone();
        let err_fn = |e| eprintln!("[pitch] audio stream error: {e}");
        let stream = match fmt {
            SampleFormat::F32 => device
                .build_input_stream(
                    &cfg.into(),
                    move |d: &[f32], _| {
                        sink.lock().unwrap()
                            .extend(d.iter().map(|s| (s.clamp(-1.0, 1.0) * 32767.0) as i16))
                    },
                    err_fn,
                    None,
                )
                .map_err(|e| e.to_string())?,
            SampleFormat::I16 => device
                .build_input_stream(
                    &cfg.into(),
                    move |d: &[i16], _| sink.lock().unwrap().extend(d.iter().copied()),
                    err_fn,
                    None,
                )
                .map_err(|e| e.to_string())?,
            SampleFormat::U16 => device
                .build_input_stream(
                    &cfg.into(),
                    move |d: &[u16], _| {
                        sink.lock().unwrap()
                            .extend(d.iter().map(|s| (*s as i32 - 32768) as i16))
                    },
                    err_fn,
                    None,
                )
                .map_err(|e| e.to_string())?,
            other => return Err(format!("unsupported sample format {other:?}")),
        };
        stream.play().map_err(|e| e.to_string())?;

        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        drop(stream);
        let samples = buf.lock().unwrap();
        println!("[pitch] captured {} KB", samples.len() * 2 / 1024);
        Ok(Capture { samples: samples.clone(), rate, channels: ch })
    })();
    let _ = tx.send(res);
}

fn encode_wav(samples: &[i16], rate: u32, channels: u16) -> Vec<u8> {
    let len = samples.len() * 2;
    let mut out = Vec::with_capacity(44 + len);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + len as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * channels as u32 * 2).to_le_bytes());
    out.extend_from_slice(&(channels * 2).to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(len as u32).to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

type Err = Box<dyn std::error::Error + Send + Sync>;

static RECOGNIZER: Mutex<Option<sherpa_onnx::OfflineRecognizer>> = Mutex::new(None);

/// Parakeet-tdt-0.6b-v2 via sherpa-onnx, CPU. ponytail: model load (~1s) happens
/// once on first dictation; move to startup if first-use latency ever annoys.
fn transcribe_local(capture: &Capture) -> Result<String, String> {
    let mut guard = RECOGNIZER.lock().unwrap();
    let recognizer = match &*guard {
        Some(r) => r,
        None => {
            println!("[pitch] loading parakeet…");
            let dir = model_dir();
            let config = sherpa_onnx::OfflineRecognizerConfig {
                model_config: sherpa_onnx::OfflineModelConfig {
                    transducer: sherpa_onnx::OfflineTransducerModelConfig {
                        encoder: Some(dir.join("encoder.int8.onnx").to_string_lossy().into()),
                        decoder: Some(dir.join("decoder.int8.onnx").to_string_lossy().into()),
                        joiner: Some(dir.join("joiner.int8.onnx").to_string_lossy().into()),
                    },
                    tokens: Some(dir.join("tokens.txt").to_string_lossy().into()),
                    num_threads: 2,
                    provider: Some("cpu".into()),
                    model_type: Some("nemo_transducer".into()),
                    ..Default::default()
                },
                ..Default::default()
            };
            let r = sherpa_onnx::OfflineRecognizer::create(&config)
                .ok_or("failed to create parakeet recognizer")?;
            *guard = Some(r);
            guard.as_ref().unwrap()
        }
    };
    // sherpa wants mono; it resamples internally.
    let mono = downmix(&capture.samples, capture.channels);
    let stream = recognizer.create_stream();
    stream.accept_waveform(capture.rate as i32, &mono);
    recognizer.decode(&stream);
    stream.get_result().map(|r| r.text).ok_or("parakeet returned no result".into())
}

fn downmix(samples: &[i16], channels: u16) -> Vec<f32> {
    let ch = channels.max(1) as usize;
    samples
        .chunks(ch)
        .map(|frame| frame.iter().fold(0i32, |a, s| a + *s as i32) as f32 / ch as f32 / 32768.0)
        .collect()
}

async fn transcribe_cloud(wav: Vec<u8>) -> Result<String, String> {
    let key = std::env::var("GROQ_API_KEY").map_err(|_| "GROQ_API_KEY not set".to_string())?;
    let part = reqwest::multipart::Part::bytes(wav)
        .file_name("audio.wav")
        .mime_str("audio/wav")
        .map_err(|e| e.to_string())?;
    let resp = reqwest::Client::new()
        .post(GROQ_URL)
        .bearer_auth(key)
        .multipart(
            reqwest::multipart::Form::new()
                .text("model", "whisper-large-v3-turbo")
                .text("response_format", "text")
                .part("file", part),
        )
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let body = resp.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("groq {status}: {body}"));
    }
    Ok(body)
}

fn paste(app: &AppHandle, text: &str) -> Result<(), Err> {
    app.clipboard().write_text(text.to_string()).map_err(|e| e.to_string())?;
    // ponytail: 80ms so the target window regains focus after hotkey release
    std::thread::sleep(std::time::Duration::from_millis(80));
    use enigo::{Direction, Key, Keyboard};
    let mut enigo = enigo::Enigo::new(&enigo::Settings::default()).map_err(|e| e.to_string())?;
    enigo.key(Key::Control, Direction::Press).map_err(|e| e.to_string())?;
    enigo.key(Key::Unicode('v'), Direction::Click).map_err(|e| e.to_string())?;
    enigo.key(Key::Control, Direction::Release).map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Action, Chord, ChordTracker, Engine, Capture, downmix, engines};

    const LWIN: u32 = super::VK_LWIN;
    const RCTRL: u32 = super::VK_RCONTROL;

    #[test]
    fn plain_win_tap_untouched() {
        let mut t = ChordTracker::default();
        assert_eq!(t.update(LWIN, true), (None, Action::None), "win alone: no trigger");
        assert_eq!(
            t.update(LWIN, false),
            (None, Action::None),
            "start menu must still work, key-up never touched"
        );
    }

    #[test]
    fn full_cycle_and_cancel_start_menu() {
        let mut t = ChordTracker::default();
        assert_eq!(t.update(RCTRL, true), (None, Action::None));
        assert_eq!(t.update(LWIN, true), (Some(Chord::Pressed), Action::None));
        assert_eq!(t.update(LWIN, true), (None, Action::None), "autorepeat ignored");
        assert_eq!(
            t.update(LWIN, false),
            (Some(Chord::Released), Action::CancelStartMenu),
            "inject tap, but forward the key-up"
        );
        assert_eq!(t.update(RCTRL, false), (None, Action::None));
        // re-arms after full release
        assert_eq!(t.update(RCTRL, true), (None, Action::None));
        assert_eq!(t.update(LWIN, true), (Some(Chord::Pressed), Action::None));
    }

    #[test]
    fn ctrl_released_first_still_cancels_start_menu() {
        let mut t = ChordTracker::default();
        t.update(LWIN, true);
        t.update(RCTRL, true);
        assert_eq!(t.update(RCTRL, false), (Some(Chord::Released), Action::None));
        assert_eq!(
            t.update(LWIN, false),
            (None, Action::CancelStartMenu),
            "late win-up gets the cancel tap"
        );
        assert_eq!(t.update(LWIN, true), (None, Action::None), "no phantom press");
    }

    #[test]
    fn engine_selection() {
        use Engine::{Cloud, Local};
        assert_eq!(engines(None, true), vec![Local, Cloud], "auto prefers local");
        assert_eq!(engines(None, false), vec![Cloud], "auto without model = cloud only");
        assert_eq!(engines(Some("local"), false), vec![Local]);
        assert_eq!(engines(Some("cloud"), true), vec![Cloud], "explicit beats auto");
    }

    #[test]
    fn stereo_downmix_averages_frames() {
        let out = downmix(&[0, 100, 200, 400], 2);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0] * 32768.0, 50.0);
        assert_eq!(out[1] * 32768.0, 300.0);
        let mono = downmix(&[-32768], 1);
        assert!((mono[0] + 1.0).abs() < 1e-6);
    }

    #[test]
    fn capture_roundtrip_wav_header() {
        let cap = Capture { samples: vec![0, 100, -100], rate: 16000, channels: 1 };
        let wav = super::encode_wav(&cap.samples, cap.rate, cap.channels);
        assert_eq!(wav.len(), 50);
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(wav[4..8].try_into().unwrap()), 42);
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 16000);
        assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 6);
        assert_eq!(&wav[48..50], &(-100i16).to_le_bytes());
    }

    // Integration: loads the real model. Skips cleanly if you deleted models/.
    #[test]
    fn parakeet_transcribes_sample_wav() {
        let dir = super::model_dir();
        if !dir.join("encoder.int8.onnx").exists() {
            return;
        }
        let wav_path = dir.join("test_wavs/0.wav");
        let wave = sherpa_onnx::Wave::read(wav_path.to_str().unwrap()).unwrap();
        let samples: Vec<i16> =
            wave.samples().iter().map(|s| (*s * 32767.0) as i16).collect();
        let cap = Capture { samples, rate: wave.sample_rate() as u32, channels: 1 };
        let text = super::transcribe_local(&cap).unwrap();
        println!("parakeet says: {text}");
        assert!(!text.trim().is_empty(), "expected actual words from sample audio");
    }
}
