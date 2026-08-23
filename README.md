# Tauri + Vanilla

This template should help get you started developing with Tauri in vanilla HTML, CSS and Javascript.

## Recommended IDE Setup

- [VS Code](https://code.visualstudio.com/) + [Tauri](https://marketplace.visualstudio.com/items?itemName=tauri-apps.tauri-vscode) + [rust-analyzer](https://marketplace.visualstudio.com/items?itemName=rust-lang.rust-analyzer)


2. Transcript history + Hub window (~a session)
- Every dictation saved locally: text, timestamp, engine, duration → append-only JSONL file in %APPDATA%\pitch (lazy-correct at this scale; SQLite only if search ever gets slow)
- The currently-hidden main window becomes the Hub: openable from tray, transcripts grouped by day, search box, click-to-copy, delete

3. Settings (~a session)
- Engine mode (auto/local/cloud), microphone picker (cpal enumeration), Groq key field, autostart-at-login toggle
- Persisted to %APPDATA%\pitch\config.json, replacing the PITCH_ENGINE/env-var era
- Hotkey customization explicitly deferred — remapping the Ctrl+Win hook means generalizing the whole chord tracker; worth it, but not until the rest feels good

4. Branding + installer (~a session, once identity is decided)
- Real name, icon, color palette across pill/toast/hub/tray
- NSIS installer, launch-at-login, proper product metadata

Three questions before I start executing:
1. Order — happy to start at #1 (recording pill), or do you want to daily-drive sooner and skip to branding/installer first?
2. Branding — keep "pitch" as the name? And what vibe: Flow-like dark/glassy minimal, or something more distinct? Any color you're attached to?
3. Sounds — want the pings, or silent operation?