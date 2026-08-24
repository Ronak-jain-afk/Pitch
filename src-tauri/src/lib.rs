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
const VK_CAPITAL: u32 = 0x14;
const VK_F9: u32 = 0x78;

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

/// Hotkey presets: each entry is a group — one key from every group held
/// simultaneously triggers the chord. Single-group presets are plain holds.
fn preset_groups(preset: &str) -> Vec<Vec<u32>> {
    match preset {
        "caps" => vec![vec![VK_CAPITAL]],
        "f9" => vec![vec![VK_F9]],
        _ => vec![vec![VK_LCONTROL, VK_RCONTROL], vec![VK_LWIN, VK_RWIN]], // ctrl_win
    }
}

/// Pure edge-detector for the configured hotkey so the tricky bits are
/// unit-tested without Windows.
struct ChordTracker {
    groups: Vec<Vec<u32>>,
    /// Held state per key, flattened across groups.
    held: Vec<bool>,
    /// True while the hotkey is fully held (one Pressed / one Released per gesture).
    engaged: bool,
    /// True from first Pressed until every preset key is fully released — spans
    /// the window where one key is still held, so a late Win keyup gets swallowed.
    gestured: bool,
    /// Caps preset swallows its own key so it never reaches apps as a case toggle.
    swallow_own_key: bool,
}

impl ChordTracker {
    fn new(preset: &str) -> Self {
        let groups = preset_groups(preset);
        let held = vec![false; groups.iter().map(|g| g.len()).sum()];
        Self {
            swallow_own_key: preset == "caps",
            groups,
            held,
            engaged: false,
            gestured: false,
        }
    }

    fn active(&self) -> bool {
        let mut off = 0;
        for g in &self.groups {
            let any = (0..g.len()).any(|j| self.held[off + j]);
            off += g.len();
            if !any {
                return false;
            }
        }
        !self.groups.is_empty()
    }

    /// Feed one key event. Returns an optional hotkey transition plus the action
    /// the hook should take for this event.
    fn update(&mut self, vk: u32, down: bool) -> (Option<Chord>, Action) {
        let mut idx = None;
        let mut off = 0;
        for g in &self.groups {
            if let Some(j) = g.iter().position(|&k| k == vk) {
                idx = Some(off + j);
                break;
            }
            off += g.len();
        }
        if let Some(i) = idx {
            self.held[i] = down;
        }
        let mut event = None;
        if self.active() {
            if !self.engaged {
                self.engaged = true;
                self.gestured = true;
                event = Some(Chord::Pressed);
            }
        } else if self.engaged {
            self.engaged = false;
            event = Some(Chord::Released);
        }
        let our_win_up = !down && matches!(vk, VK_LWIN | VK_RWIN) && idx.is_some();
        let action =
            if our_win_up && self.gestured { Action::CancelStartMenu } else { Action::None };
        if self.held.iter().all(|&h| !h) {
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
    // Tracker is (re)built from config here and on every settings save, so
    // hotkey changes apply without a restart.
    *TRACKER.lock().unwrap() =
        Some(ChordTracker::new(&load_config(&app).hotkey));
    // Drain thread does the slow work (mic open etc.) so the hook proc stays fast.
    tauri::async_runtime::spawn_blocking(move || {
        while let Ok(event) = rx.recv() {
            match event {
                Chord::Pressed => {
                    let toggle = load_config(&app).mode == "toggle";
                    let recording =
                        { app.state::<AppState>().inner().0.lock().unwrap().is_some() };
                    if toggle && recording {
                        stop_recording(&app);
                        hide_pill(&app);
                    } else {
                        start_recording(&app);
                        show_pill(&app);
                    }
                }
                Chord::Released => {
                    if load_config(&app).mode != "toggle" {
                        stop_recording(&app);
                        hide_pill(&app);
                    }
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
                let (event, action, swallow) = {
                    let mut guard = TRACKER.lock().unwrap();
                    let t = guard.get_or_insert_with(|| ChordTracker::new("ctrl_win"));
                    let (event, action) = t.update(kb.vkCode, down);
                    (event, action, t.swallow_own_key)
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
                // Only the Caps preset eats its own key (so it never case-toggles);
                // it has no other OS meaning, unlike Win/F9.
                if swallow && kb.vkCode == VK_CAPITAL {
                    return LRESULT(1);
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

/// Second slot: when the last paste happened — arms "scratch that" undo for a
/// short window so a late undo never eats unrelated manual edits.
#[derive(Default)]
struct AppState(Mutex<Option<Session>>, Mutex<Option<std::time::Instant>>);

const UNDO_WINDOW_SECS: u64 = 120;

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

/// Runtime resolution: bundled resources, then user-downloaded, then the dev
/// checkout. ponytail: the old compile-time-only path broke in shipped builds.
fn model_dir_for(app: &AppHandle) -> Option<std::path::PathBuf> {
    if let Ok(res) = app.path().resource_dir() {
        let p = res.join("models/parakeet-v2");
        if p.join("encoder.int8.onnx").exists() {
            return Some(p);
        }
    }
    if let Ok(data) = app.path().app_data_dir() {
        let p = data.join("models/parakeet-v2");
        if p.join("encoder.int8.onnx").exists() {
            return Some(p);
        }
    }
    let dev = model_dir();
    dev.join("encoder.int8.onnx").exists().then_some(dev)
}

// ---------------------------------------------------------------------------
// One-click local engine download (Hugging Face, individual files — no archive
// step). ponytail: swap HF_BASE for our GitHub release mirror before shipping.
// ---------------------------------------------------------------------------

const HF_BASE: &str =
    "https://huggingface.co/csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8/resolve/main/";
const MODEL_FILES: [&str; 4] =
    ["encoder.int8.onnx", "decoder.int8.onnx", "joiner.int8.onnx", "tokens.txt"];

static DOWNLOADING: AtomicBool = AtomicBool::new(false);

#[tauri::command]
fn model_status(app: AppHandle) -> bool {
    model_dir_for(&app).is_some()
}

#[tauri::command]
async fn download_model(app: AppHandle) -> Result<(), String> {
    use tauri::Emitter;
    if DOWNLOADING.swap(true, Ordering::SeqCst) {
        return Err("download already in progress".into());
    }
    let result = async {
        let dir = app
            .path()
            .app_data_dir()
            .map_err(|e| e.to_string())?
            .join("models/parakeet-v2");
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        for file in MODEL_FILES {
            download_file(&app, &dir, file).await?;
        }
        let _ = app.emit("model-dl", serde_json::json!({ "done": true }));
        Ok(())
    }
    .await;
    DOWNLOADING.store(false, Ordering::SeqCst);
    result
}

async fn download_file(app: &AppHandle, dir: &std::path::Path, name: &str) -> Result<(), String> {
    use std::io::Write;
    use tauri::Emitter;
    let dest = dir.join(name);
    if dest.exists() {
        return Ok(()); // resume-by-skip: finished files aren't re-fetched
    }
    let resp = reqwest::get(format!("{HF_BASE}{name}"))
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("{name}: {}", resp.status()));
    }
    let total = resp.content_length().unwrap_or(0);
    let mut resp = resp;
    let mut file = std::fs::File::create(&dest).map_err(|e| e.to_string())?;
    let mut done = 0u64;
    let mut last_pct = 255u8;
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
        file.write_all(&chunk).map_err(|e| e.to_string())?;
        done += chunk.len() as u64;
        if total > 0 {
            let pct = ((done * 100) / total) as u8;
            if pct != last_pct {
                last_pct = pct;
                let _ = app.emit("model-dl", serde_json::json!({ "file": name, "percent": pct }));
            }
        }
    }
    Ok(())
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

// ---------------------------------------------------------------------------
// Settings: config.json in the app data dir
// ---------------------------------------------------------------------------

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Config {
    engine: String,
    mic: Option<String>,
    groq_key: Option<String>,
    autostart: bool,
    theme: String,
    remove_fillers: bool,
    hotkey: String,
    mode: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            engine: "auto".into(),
            mic: None,
            groq_key: None,
            autostart: false,
            theme: "light".into(),
            remove_fillers: false,
            hotkey: "ctrl_win".into(),
            mode: "hold".into(),
        }
    }
}

fn config_path(app: &AppHandle) -> Option<std::path::PathBuf> {
    let dir = app.path().app_data_dir().ok()?;
    let _ = std::fs::create_dir_all(&dir);
    Some(dir.join("config.json"))
}

fn load_config(app: &AppHandle) -> Config {
    let Some(path) = config_path(app) else { return Config::default() };
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

#[tauri::command]
fn get_config(app: AppHandle) -> Config {
    load_config(&app)
}

#[tauri::command]
fn save_config(app: AppHandle, config: Config) -> Result<(), String> {
    use tauri_plugin_autostart::ManagerExt as _;
    let path = config_path(&app).ok_or("no app data dir")?;
    let json = serde_json::to_string_pretty(&config).map_err(|e| e.to_string())?;
    std::fs::write(path, json).map_err(|e| e.to_string())?;
    // Re-arm the keyboard hook with the saved preset — hotkey edits apply live.
    *TRACKER.lock().unwrap() = Some(ChordTracker::new(&config.hotkey));
    // registry side effect — only flip when state differs; disable() on an
    // already-absent value errors with os error 2
    let autolaunch = app.autolaunch();
    let enabled = autolaunch.is_enabled().unwrap_or(false);
    if config.autostart && !enabled {
        autolaunch.enable().map_err(|e| e.to_string())?;
    } else if !config.autostart && enabled {
        autolaunch.disable().map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
fn list_mics(app: AppHandle) -> Vec<String> {
    let _ = app;
    cpal::default_host()
        .input_devices()
        .map(|devs| devs.filter_map(|d| d.name().ok()).collect())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Dictionary & snippets: rules.json — applied to every transcript
// ---------------------------------------------------------------------------

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Rule {
    id: u64,
    kind: String, // "word" | "snippet"
    from: String,
    to: String,
    enabled: bool,
}

fn rules_path(app: &AppHandle) -> Option<std::path::PathBuf> {
    let dir = app.path().app_data_dir().ok()?;
    let _ = std::fs::create_dir_all(&dir);
    Some(dir.join("rules.json"))
}

fn load_rules(app: &AppHandle) -> Vec<Rule> {
    let Some(path) = rules_path(app) else { return vec![] };
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn store_rules(app: &AppHandle, rules: &[Rule]) -> Result<(), String> {
    let path = rules_path(app).ok_or("no app data dir")?;
    let json = serde_json::to_string_pretty(rules).map_err(|e| e.to_string())?;
    std::fs::write(path, json).map_err(|e| e.to_string())
}

#[tauri::command]
fn list_rules(app: AppHandle) -> Vec<Rule> {
    load_rules(&app)
}

/// Upsert: assigns an id when rule.id is 0.
#[tauri::command]
fn save_rule(app: AppHandle, rule: Rule) -> Result<u64, String> {
    let mut rules = load_rules(&app);
    let id = if rule.id == 0 {
        let id = rules.iter().map(|r| r.id).max().unwrap_or(0) + 1;
        rules.push(Rule { id, ..rule });
        id
    } else {
        let id = rule.id;
        match rules.iter_mut().find(|r| r.id == id) {
            Some(slot) => *slot = rule,
            None => return Err(format!("rule {id} not found")),
        }
        id
    };
    store_rules(&app, &rules)?;
    Ok(id)
}

#[tauri::command]
fn delete_rule(app: AppHandle, id: u64) -> bool {
    let mut rules = load_rules(&app);
    let before = rules.len();
    rules.retain(|r| r.id != id);
    rules.len() != before && store_rules(&app, &rules).is_ok()
}

/// One pass, longest-match-first, case-insensitive, word-bounded; replacement
/// inserted verbatim. Covers both word corrections and snippet expansion.
fn apply_rules(text: &str, rules: &[Rule]) -> String {
    let mut alts: Vec<&Rule> =
        rules.iter().filter(|r| r.enabled && !r.from.trim().is_empty()).collect();
    if alts.is_empty() {
        return text.to_string();
    }
    // longest first so "my linkedin" wins over "linkedin"
    alts.sort_by(|a, b| b.from.chars().count().cmp(&a.from.chars().count()));
    let mut map = std::collections::HashMap::new();
    let mut pattern = String::new();
    for r in &alts {
        map.entry(r.from.to_lowercase()).or_insert(r.to.as_str());
        if !pattern.is_empty() {
            pattern.push('|');
        }
        pattern.push_str(&regex::escape(r.from.trim()));
    }
    let re = match regex::RegexBuilder::new(&format!(r"\b(?:{pattern})\b"))
        .case_insensitive(true)
        .build()
    {
        Ok(re) => re,
        Err(_) => return text.to_string(),
    };
    re.replace_all(text, |caps: &regex::Captures| {
        map.get(&caps[0].to_lowercase()).copied().unwrap_or(&caps[0]).to_string()
    })
    .into_owned()
}

/// Strip filler words ("um", "uh", "erm", "hmm"…). Word-bounded, case-insensitive,
/// then tidies the seams: space before punctuation, doubled spaces, edge trim.
fn remove_fillers(text: &str) -> String {
    static FILLER: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static PUNCT: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static SPACES: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let filler = FILLER.get_or_init(|| {
        regex::RegexBuilder::new(r"\b(?:um+|uh+|erm+|hm+)\b ?")
            .case_insensitive(true)
            .build()
            .unwrap()
    });
    let punct = PUNCT.get_or_init(|| regex::Regex::new(r"\s+([,.!?;:])").unwrap());
    let spaces = SPACES.get_or_init(|| regex::Regex::new(r"[ \t]{2,}").unwrap());
    let out = filler.replace_all(text, "");
    let out = punct.replace_all(&out, "$1");
    spaces.replace_all(&out, " ").trim().to_string()
}

/// True when the entire transcript is just an undo request.
fn is_undo_command(text: &str) -> bool {
    let t = text.trim().to_lowercase();
    let t = t.trim_end_matches(['.', ',', '!', '?']).trim();
    matches!(t, "scratch that" | "undo that")
}

/// Spoken layout cues: "new paragraph" → blank line, "new line"/"newline" → break.
/// Surrounding single spaces and any punctuation stuck to the cue by the ASR
/// ("new paragraph.") are consumed — the cue ends the line, it isn't a sentence.
fn apply_line_commands(text: &str) -> String {
    static PARA: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static LINE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let para = PARA.get_or_init(|| {
        regex::RegexBuilder::new(r" ?\bnew paragraphs?\b[,.!?;:]* ?")
            .case_insensitive(true)
            .build()
            .unwrap()
    });
    let line = LINE.get_or_init(|| {
        regex::RegexBuilder::new(r" ?\b(?:new lines?|newlines?)\b[,.!?;:]* ?")
            .case_insensitive(true)
            .build()
            .unwrap()
    });
    let out = para.replace_all(text, "\n\n");
    line.replace_all(&out, "\n").into_owned()
}

/// Cloud-only soft bias: enabled word rules become a whisper `prompt` glossary.
/// ponytail: ~4 chars/token heuristic against the 224-token cap.
fn glossary(rules: &[Rule]) -> Option<String> {
    let mut out = String::from("Glossary of correct spellings: ");
    let mut any = false;
    for r in rules.iter().filter(|r| r.enabled && r.kind == "word") {
        let item = format!("{} = {}; ", r.from, r.to);
        if out.chars().count() + item.chars().count() > 880 {
            break;
        }
        out.push_str(&item);
        any = true;
    }
    any.then_some(out)
}

pub fn run() {
    tauri::Builder::default()
        .manage(AppState::default())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
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
                .tooltip("PITCH — hold Ctrl+Win to dictate")
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
            debug_ping,
            get_config,
            save_config,
            list_mics,
            list_rules,
            save_rule,
            delete_rule,
            model_status,
            download_model
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
    let mic = load_config(app).mic;
    // ponytail: dedicated blocking thread owns the !Send cpal Stream; 20ms poll is
    // noise next to human hotkey latency. Event-driven wake-up only if CPU ever matters.
    tauri::async_runtime::spawn_blocking(move || record(flag, tx, mic));
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

        let config = load_config(&app);
        let rules = load_rules(&app);
        let list = engines(Some(config.engine.as_str()), model_dir_for(&app).is_some());
        let mut last_err = None;
        for engine in list {
            println!("[pitch] engine: {engine:?}");
            let result = match engine {
                Engine::Local => match model_dir_for(&app) {
                    Some(dir) => {
                        let capture = capture.clone();
                        tauri::async_runtime::spawn_blocking(move || {
                            transcribe_local(&capture, dir)
                        })
                        .await
                        .unwrap_or_else(|e| Err(e.to_string()))
                    }
                    None => Err("local engine not installed (Settings → Local engine)".into()),
                },
                Engine::Cloud => {
                    transcribe_cloud(
                        encode_wav(&capture.samples, capture.rate, capture.channels),
                        config.groq_key.clone(),
                        glossary(&rules),
                    )
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
                Ok(raw) => {
                    let cleaned =
                        if config.remove_fillers { remove_fillers(&raw) } else { raw };
                    // Voice command: undo the previous dictation instead of
                    // transcribing this one. Not saved to history.
                    if is_undo_command(&cleaned) {
                        if undo_last_paste(&app) {
                            toast(&app, "↩ Undid last dictation", 1800);
                        } else {
                            toast(&app, "Nothing recent to undo", 2500);
                        }
                        return;
                    }
                    let cleaned = apply_line_commands(&cleaned);
                    let text = apply_rules(&cleaned, &rules);
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

fn record(
    stop: Arc<AtomicBool>,
    tx: mpsc::Sender<Result<Capture, String>>,
    mic: Option<String>,
) {
    let res = (|| -> Result<Capture, String> {
        let host = cpal::default_host();
        let device = match &mic {
            Some(name) => host
                .input_devices()
                .map_err(|e| e.to_string())?
                .find(|d| d.name().map(|n| &n == name).unwrap_or(false))
                .ok_or_else(|| format!("microphone '{name}' not found"))?,
            None => host.default_input_device().ok_or("no microphone found")?,
        };
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
fn transcribe_local(capture: &Capture, dir: std::path::PathBuf) -> Result<String, String> {
    let mut guard = RECOGNIZER.lock().unwrap();
    let recognizer = match &*guard {
        Some(r) => r,
        None => {
            println!("[pitch] loading parakeet…");
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

async fn transcribe_cloud(
    wav: Vec<u8>,
    config_key: Option<String>,
    prompt: Option<String>,
) -> Result<String, String> {
    // ponytail: env var kept as fallback for the pre-settings era
    let key = config_key
        .filter(|k| !k.trim().is_empty())
        .or_else(|| std::env::var("GROQ_API_KEY").ok())
        .ok_or("no Groq API key set (Settings → Groq API key)")?;
    let part = reqwest::multipart::Part::bytes(wav)
        .file_name("audio.wav")
        .mime_str("audio/wav")
        .map_err(|e| e.to_string())?;
    let mut form = reqwest::multipart::Form::new()
        .text("model", "whisper-large-v3-turbo")
        .text("response_format", "text")
        .part("file", part);
    if let Some(p) = prompt {
        form = form.text("prompt", p);
    }
    let resp = reqwest::Client::new()
        .post(GROQ_URL)
        .bearer_auth(key)
        .multipart(form)
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

/// Paste via clipboard without destroying its previous contents: capture what
/// was there, paste, then restore once the target app has consumed the paste
/// (skipped if the user copied something new in the meantime). Arms the undo.
fn paste(app: &AppHandle, text: &str) -> Result<(), Err> {
    let prev = app.clipboard().read_text().ok();
    // ponytail: non-text clipboards (images/files) can't be restored — read_text
    // fails, prev is None, our text stays on the clipboard.
    app.clipboard().write_text(text.to_string()).map_err(|e| e.to_string())?;
    *app.state::<AppState>().inner().1.lock().unwrap() = Some(std::time::Instant::now());
    // ponytail: 80ms so the target window regains focus after hotkey release
    std::thread::sleep(std::time::Duration::from_millis(80));
    use enigo::{Direction, Key, Keyboard};
    let mut enigo = enigo::Enigo::new(&enigo::Settings::default()).map_err(|e| e.to_string())?;
    enigo.key(Key::Control, Direction::Press).map_err(|e| e.to_string())?;
    enigo.key(Key::Unicode('v'), Direction::Click).map_err(|e| e.to_string())?;
    enigo.key(Key::Control, Direction::Release).map_err(|e| e.to_string())?;
    let app = app.clone();
    let mine = text.to_string();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(500));
        if app.clipboard().read_text().ok().as_deref() == Some(mine.as_str()) {
            if let Some(prev) = prev {
                let _ = app.clipboard().write_text(prev);
            }
        }
    });
    Ok(())
}

/// Revert the most recent dictation via the target app's own Ctrl+Z. Only works
/// within UNDO_WINDOW_SECS of a paste; returns false (and does nothing) if no
/// recent paste or the keystroke couldn't be sent.
fn undo_last_paste(app: &AppHandle) -> bool {
    let recent = {
        let mut slot = app.state::<AppState>().inner().1.lock().unwrap();
        match *slot {
            Some(t) if t.elapsed().as_secs() < UNDO_WINDOW_SECS => {
                *slot = None;
                true
            }
            _ => false,
        }
    };
    if !recent {
        return false;
    }
    use enigo::{Direction, Key, Keyboard};
    let Ok(mut enigo) = enigo::Enigo::new(&enigo::Settings::default()) else { return false };
    enigo.key(Key::Control, Direction::Press).is_ok()
        && enigo.key(Key::Unicode('z'), Direction::Click).is_ok()
        && enigo.key(Key::Control, Direction::Release).is_ok()
}

#[cfg(test)]
mod tests {
    use super::{Action, Chord, ChordTracker, Engine, Capture, downmix, engines};

    const LWIN: u32 = super::VK_LWIN;
    const RCTRL: u32 = super::VK_RCONTROL;

    #[test]
    fn plain_win_tap_untouched() {
        let mut t = ChordTracker::new("ctrl_win");
        assert_eq!(t.update(LWIN, true), (None, Action::None), "win alone: no trigger");
        assert_eq!(
            t.update(LWIN, false),
            (None, Action::None),
            "start menu must still work, key-up never touched"
        );
        assert!(!t.swallow_own_key);
    }

    #[test]
    fn full_cycle_and_cancel_start_menu() {
        let mut t = ChordTracker::new("ctrl_win");
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
        let mut t = ChordTracker::new("ctrl_win");
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
    fn mixed_sides_trigger_chord() {
        let mut t = ChordTracker::new("ctrl_win");
        assert_eq!(t.update(super::VK_LCONTROL, true), (None, Action::None));
        assert_eq!(t.update(super::VK_RWIN, true), (Some(Chord::Pressed), Action::None));
    }

    #[test]
    fn caps_preset_triggers_alone_and_swallows() {
        let mut t = ChordTracker::new("caps");
        assert!(t.swallow_own_key);
        assert_eq!(
            t.update(super::VK_CAPITAL, true),
            (Some(Chord::Pressed), Action::None)
        );
        assert_eq!(
            t.update(super::VK_CAPITAL, false),
            (Some(Chord::Released), Action::None)
        );
        // a Win key-up is untouched when the preset has no Win in it
        t.update(super::VK_CAPITAL, true);
        t.update(super::VK_CAPITAL, false);
        assert_eq!(t.update(LWIN, true), (None, Action::None));
        assert_eq!(t.update(LWIN, false), (None, Action::None));
    }

    #[test]
    fn f9_preset_triggers_alone() {
        let mut t = ChordTracker::new("f9");
        assert!(!t.swallow_own_key);
        assert_eq!(t.update(super::VK_F9, true), (Some(Chord::Pressed), Action::None));
        assert_eq!(t.update(super::VK_F9, false), (Some(Chord::Released), Action::None));
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
        let text = super::transcribe_local(&cap, dir).unwrap();
        println!("parakeet says: {text}");
        assert!(!text.trim().is_empty(), "expected actual words from sample audio");
    }

    fn rule(id: u64, kind: &str, from: &str, to: &str, enabled: bool) -> super::Rule {
        super::Rule { id, kind: kind.into(), from: from.into(), to: to.into(), enabled }
    }

    #[test]
    fn longest_match_wins() {
        let rules = [
            rule(1, "snippet", "linkedin", "https://li.example", true),
            rule(2, "snippet", "my linkedin", "https://me.example", true),
        ];
        assert_eq!(
            super::apply_rules("send it to my linkedin please", &rules),
            "send it to https://me.example please"
        );
        assert_eq!(
            super::apply_rules("check linkedin", &rules),
            "check https://li.example"
        );
    }

    #[test]
    fn case_insensitive_replacement_verbatim() {
        let rules = [rule(1, "word", "get hub", "GitHub", true)];
        assert_eq!(super::apply_rules("push it to Get Hub", &rules), "push it to GitHub");
    }

    #[test]
    fn punctuation_adjacency_still_matches() {
        let rules = [rule(1, "snippet", "my linkedin", "https://me.example", true)];
        assert_eq!(
            super::apply_rules("Sent. Check my linkedin.", &rules),
            "Sent. Check https://me.example."
        );
    }

    #[test]
    fn disabled_and_empty_rules_skipped() {
        let rules = [
            rule(1, "word", "foo", "bar", false),
            rule(2, "word", "   ", "x", true),
        ];
        assert_eq!(super::apply_rules("foo baz", &rules), "foo baz");
    }

    #[test]
    fn no_rules_passthrough() {
        assert_eq!(super::apply_rules("untouched", &[]), "untouched");
    }

    #[test]
    fn every_occurrence_replaced() {
        let rules = [rule(1, "word", "kubernetes", "K8s", true)];
        assert_eq!(
            super::apply_rules("kubernetes on Kubernetes", &rules),
            "K8s on K8s"
        );
    }

    #[test]
    fn glossary_only_words_and_disabled_skipped() {
        let rules = [
            rule(1, "word", "get hub", "GitHub", true),
            rule(2, "snippet", "my linkedin", "https://x", true),
            rule(3, "word", "off", "OFF", false),
        ];
        let g = super::glossary(&rules).unwrap();
        assert!(g.contains("get hub = GitHub"));
        assert!(!g.contains("linkedin"));
        assert!(!g.contains("OFF"));
        assert!(super::glossary(&[]).is_none());
    }

    #[test]
    fn fillers_removed_with_seam_cleanup() {
        assert_eq!(super::remove_fillers("um so this is uh great"), "so this is great");
        assert_eq!(super::remove_fillers("Um hello"), "hello");
        assert_eq!(super::remove_fillers("wait um, what"), "wait, what");
        assert_eq!(super::remove_fillers("hmm okay then"), "okay then");
    }

    #[test]
    fn fillers_never_eat_real_words() {
        assert_eq!(super::remove_fillers("an album column of hummus"), "an album column of hummus");
        assert_eq!(super::remove_fillers("no fillers here"), "no fillers here");
    }

    #[test]
    fn undo_command_needs_exact_phrase() {
        assert!(super::is_undo_command("scratch that"));
        assert!(super::is_undo_command("  Undo that. "));
        assert!(!super::is_undo_command("scratch that and try again"));
        assert!(!super::is_undo_command("undo the settings"));
        assert!(!super::is_undo_command(""));
    }

    #[test]
    fn line_commands_expand_and_consume_spaces() {
        assert_eq!(super::apply_line_commands("point one new paragraph point two"), "point one\n\npoint two");
        assert_eq!(super::apply_line_commands("a new line b"), "a\nb");
        assert_eq!(super::apply_line_commands("x newline y"), "x\ny");
        assert_eq!(
            super::apply_line_commands("New Paragraph at start"),
            "\n\nat start"
        );
        assert_eq!(super::apply_line_commands("draw a new lineart"), "draw a new lineart");
    }

    #[test]
    fn line_commands_consume_asr_punctuation() {
        assert_eq!(
            super::apply_line_commands("how are you doing? new line, I'm doing fine"),
            "how are you doing?\nI'm doing fine"
        );
        assert_eq!(
            super::apply_line_commands("greeting. new paragraph. thank you."),
            "greeting.\n\nthank you."
        );
    }
}
