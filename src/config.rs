use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The settings template, **baked into the exe** (no separate
/// settings.example.json shipped alongside). On first run — when no
/// settings.json exists — this is written out verbatim as the user's
/// settings.json, so they get a nicely-ordered, fully-populated file to edit.
pub const EXAMPLE_JSON: &str = include_str!("../settings.example.json");

const fn default_true() -> bool {
    true
}
const fn default_false() -> bool {
    false
}
fn default_toggle_hotkey() -> String {
    "f14".into()
}
fn default_hold_hotkey() -> String {
    "f13".into()
}
fn default_reinsert_hold_ms() -> u64 {
    1500
}
fn default_listen_tail_ms() -> u64 {
    800
}
fn default_clipboard_restore_delay_ms() -> u64 {
    300
}
fn default_max_log_mb() -> u64 {
    5
}
fn default_language() -> String {
    "en-US".into()
}
fn default_provider() -> String {
    "elevenlabs".into()
}
fn default_local_model() -> String {
    crate::local_stt::default_model_id()
}
fn default_mode() -> String {
    "toggle".into()
}
fn default_close() -> String {
    "minimize".into()
}
fn default_spinner() -> String {
    "star_wars".into()
}
fn default_width() -> u32 {
    280
}
fn default_height() -> u32 {
    140
}

fn default_replacements() -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    for (k, v) in [
        ("Super Bass", "Supabase"),
        ("super bass", "Supabase"),
        ("Clouded flyer", "Cloudflare"),
        ("clouded flyer", "Cloudflare"),
        ("Cloud flyer", "Cloudflare"),
        ("cloud flyer", "Cloudflare"),
        ("Chat GPT", "ChatGPT"),
        ("chat gpt", "ChatGPT"),
        ("Github", "GitHub"),
        ("github", "GitHub"),
    ] {
        m.insert(k.into(), v.into());
    }
    m
}

/// Per-application override, matched against the foreground window's exe at
/// transcription-commit time (see `crate::focus`). Every field besides `name`
/// and `match_` is optional; anything left `None`/absent falls back to the
/// corresponding global `Config` setting. First matching profile in the list
/// wins.
///
/// Since v0.5.4 a profile may also override the recognition **language**, the
/// **provider**, and the **custom vocabulary**, not just text processing. The
/// provider override is resolved at hotkey-press time (see
/// `Config::provider_for_exe`), so switching windows switches backend on the
/// next dictation; a provider with no configured key is ignored and the global
/// provider is used instead.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Profile {
    /// Human-readable label shown in the read-only "Active profiles" list in
    /// Settings (e.g. "Code editors").
    pub name: String,

    /// Exe basenames this profile applies to (case-insensitive, e.g.
    /// `["code.exe", "windowsterminal.exe"]`). The foreground window's exe
    /// basename is matched against this list; first profile with a match
    /// wins.
    #[serde(rename = "match")]
    pub match_: Vec<String>,

    #[serde(default)]
    pub auto_punct: Option<bool>,
    #[serde(default)]
    pub auto_space: Option<bool>,
    #[serde(default)]
    pub auto_newline: Option<bool>,

    /// How `text_replacements` below combines with the global map:
    /// "extend" (default) layers/overrides entries on top of the global map;
    /// "replace" uses only this profile's map (global entries are ignored).
    #[serde(default = "default_replacements_mode")]
    pub replacements_mode: String,

    #[serde(default)]
    pub text_replacements: BTreeMap<String, String>,

    /// Recognition language for this app (same form as the global `language`,
    /// e.g. "de-DE"). `None` falls back to the global setting.
    #[serde(default)]
    pub language: Option<String>,

    /// STT backend for this app (same ids as the global `stt_provider`).
    /// `None`, an unknown id, or a provider with no configured key all fall
    /// back to the global provider, so a typo here can never break dictation.
    #[serde(default)]
    pub stt_provider: Option<String>,

    /// Words/phrases to bias recognition toward while this app is focused.
    /// `None` uses the global list; `Some(list)` replaces it (an empty list
    /// therefore means "no biasing in this app", which is deliberate).
    #[serde(default)]
    pub custom_vocabulary: Option<Vec<String>>,

    /// LLM cleanup pass for this app (see `Config::polish_enabled`). `None`
    /// follows the global setting. This is the override worth setting: the
    /// pass can repaste nothing, but it does spend a bounded wait, and a
    /// terminal or a code editor wants raw text and instant pastes far more
    /// than it wants tidy prose.
    #[serde(default)]
    pub polish: Option<bool>,
}

fn default_replacements_mode() -> String {
    "extend".into()
}

fn default_polish_deadline_ms() -> u64 {
    300
}

fn default_polish_endpoint() -> String {
    "https://api.openai.com/v1/chat/completions".into()
}

/// The default is the best OpenAI option, because `polish_keys` falls back to
/// `openai_keys` and that is the key most people already have. **It is not the
/// best option overall.** Measured 2026-08-13, same 520-character dictation
/// and same edit-list prompt for every row, median of 3:
///
/// | model                 | median  | result                                |
/// |-----------------------|---------|---------------------------------------|
/// | gemini-3.5-flash-lite | ~0.56 s | 4 edits, all correct. **Best.**       |
/// | gemini-flash-lite-latest | ~0.63 s | 3 edits, all correct.              |
/// | gemini-3.6-flash      | ~0.99 s | 3 edits, all correct.                 |
/// | gemini-3.1-flash-lite | ~1.07 s | 3 edits, all correct.                 |
/// | gpt-4.1-nano          | ~1.7 s  | 3 edits, every one a no-op. Useless.  |
/// | gpt-4.1-mini          | ~2.0 s  | 3 edits, all correct.                 |
/// | gemini-3.7-flash      | ~2.1 s  | 3 edits, all correct. Overkill here.  |
/// | gemini-3.5-flash      | ~3.3 s  | 3 edits, all correct.                 |
/// | gpt-5-nano            | ~7.1 s  | spent the whole token budget thinking |
/// | gpt-5-mini            | ~10.4 s | same, worse.                          |
///
/// Three things that keep being true: the *lite* tiers win outright (this is
/// a small mechanical edit, not a reasoning problem, and the biggest model is
/// the slowest for no gain); never pick a model that thinks before answering,
/// which is what buried both gpt-5 rows; and `-nano`-class OpenAI models are
/// too weak to produce a single real edit.
///
/// Point `polish_endpoint` at
/// `https://generativelanguage.googleapis.com/v1beta/openai/chat/completions`
/// with `polish_model: "gemini-3.5-flash-lite"` and `polish_keys` to get the
/// top row. At ~0.6 s the deadline race is winnable outright rather than
/// depending on the speculative pass.
fn default_polish_model() -> String {
    "gpt-4.1-mini".into()
}

impl Profile {
    /// Case-insensitive match against an exe basename (already lowercased by
    /// the caller, but this normalizes defensively either way).
    pub fn matches(&self, exe_name: &str) -> bool {
        let exe_name = exe_name.to_ascii_lowercase();
        self.match_
            .iter()
            .any(|m| m.to_ascii_lowercase() == exe_name)
    }
}

/// The effective text-processing settings after folding in a matched profile
/// (or just the globals, if no profile matched / none configured). Built by
/// [`Config::effective_settings`].
pub struct EffectiveSettings {
    pub auto_punct: bool,
    pub auto_space: bool,
    pub auto_newline: bool,
    pub text_replacements: BTreeMap<String, String>,
    /// Recognition language after folding in a matched profile.
    pub language: String,
    /// Recognition bias list after folding in a matched profile.
    pub custom_vocabulary: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_mode")]
    pub mode: String, // "toggle" | "hold"
    #[serde(default = "default_language")]
    pub language: String,
    #[serde(default = "default_toggle_hotkey")]
    pub toggle_hotkey: String,
    #[serde(default = "default_hold_hotkey")]
    pub hold_hotkey: String,
    #[serde(default = "default_reinsert_hold_ms")]
    pub reinsert_hold_ms: u64,
    /// How long QuickDictate keeps listening after you stop speaking — the
    /// "dynamic tail" silence window, in ms. Once you go quiet it waits this
    /// long for more speech before finalizing: raise it if trailing words get
    /// clipped, lower it for a snappier finish. The hard cap on the tail is
    /// this value plus ~1 s of head-room (see `stt::TAIL_MAX_HEADROOM`). The
    /// default 800 ms reproduces the original fixed behavior. Read fresh per
    /// dictation, so a change applies on the next utterance without a restart.
    #[serde(default = "default_listen_tail_ms")]
    pub listen_tail_ms: u64,
    /// Grace period (ms) between sending Ctrl+V (clipboard-path pastes, i.e.
    /// long transcriptions) and restoring whatever the clipboard held before.
    /// SendInput only *queues* the keystroke — a target app that processes it
    /// after an early restore reads the OLD clipboard and pastes stale content
    /// instead of the transcription. The default 300 ms covers even janky
    /// Electron apps/browsers; raise it if a sluggish app still pastes old
    /// clipboard text, or set 0 to skip the restore entirely (the
    /// transcription then simply stays on the clipboard). Read per paste, so
    /// a change applies to the next dictation without a restart.
    #[serde(default = "default_clipboard_restore_delay_ms")]
    pub clipboard_restore_delay_ms: u64,
    #[serde(default = "default_true")]
    pub auto_space: bool,
    #[serde(default = "default_false")]
    pub auto_newline: bool,
    #[serde(default = "default_true")]
    pub auto_punct: bool,
    #[serde(default = "default_true")]
    pub hotkeys_enabled: bool,
    #[serde(default = "default_false")]
    pub enable_sound: bool,
    #[serde(default = "default_close")]
    pub close_behavior: String,
    #[serde(default = "default_width")]
    pub window_width: u32,
    #[serde(default = "default_height")]
    pub window_height: u32,
    #[serde(default)]
    pub window_x: Option<i32>,
    #[serde(default)]
    pub window_y: Option<i32>,
    #[serde(default = "default_true")]
    pub mouse_follower_enabled: bool,
    #[serde(default = "default_true")]
    pub delay_output_till_release: bool,
    #[serde(default = "default_spinner")]
    pub spinner_type: String,

    /// Which STT backend to use:
    /// "elevenlabs" | "deepgram" | "openai" | "assemblyai" | "dashscope" |
    /// "google" | "local".
    /// (google = batch/non-streaming.)
    #[serde(default = "default_provider")]
    pub stt_provider: String,

    /// The user's own API keys **per provider**, read from settings.json.
    /// Multiple keys per provider are round-robined with per-key health
    /// tracking. Empty = that provider is unavailable; the app surfaces a
    /// clear "add your key" message when the active provider has none.
    #[serde(default)]
    pub elevenlabs_keys: Vec<String>,
    #[serde(default)]
    pub deepgram_keys: Vec<String>,
    #[serde(default)]
    pub openai_keys: Vec<String>,
    #[serde(default)]
    pub assemblyai_keys: Vec<String>,
    #[serde(default)]
    pub dashscope_keys: Vec<String>,
    #[serde(default)]
    pub google_keys: Vec<String>,

    /// Optional per-provider model override (else the provider's default).
    #[serde(default)]
    pub stt_model: Option<String>,

    /// Download-on-demand model used by the keyless local provider. The model
    /// weights themselves live in Local AppData and are never part of config,
    /// settings sync, the repository, or the QuickDictate executable.
    #[serde(default = "default_local_model")]
    pub local_model: String,

    /// DashScope region: `false` = mainland-China host (default),
    /// `true` = the `-intl` host for International accounts. A key from the
    /// wrong region 401s at the WebSocket upgrade, so match this to your key.
    #[serde(default = "default_false")]
    pub dashscope_intl: bool,

    /// Automatically check for a newer release at startup (throttled to once
    /// per day). The check goes to LunarWerx's update endpoint (see
    /// `update::RELEASES_API`), which relays GitHub's release info and also
    /// counts the hit as one anonymous install ping — details in SECURITY.md.
    /// Finding a newer release only *reports* it (tray tooltip + the About
    /// pill); installing it is a click, unless you opt into
    /// `update_auto_install`. Settings → About "Check for updates" works
    /// regardless of this flag, and there the update installs as soon as you
    /// click the pill.
    #[serde(default = "default_true")]
    pub update_auto_check: bool,

    /// Anonymous install id, sent as the `X-Install-Id` header with update
    /// checks so the update endpoint can count unique installs instead of raw
    /// hits. A crypto-random UUID generated locally on first launch (see
    /// `update::init_install_id`) — **never** derived from hostname, MAC,
    /// username, or any other machine/personal identifier, so it identifies
    /// nothing but itself. Sent only with update checks (see SECURITY.md);
    /// clear the value to get a fresh id on the next launch.
    #[serde(default)]
    pub install_id: String,

    /// Start QuickDictate automatically at Windows login (per-user Run key,
    /// no admin rights needed). Reconciled on every launch: flipping this and
    /// restarting adds/removes the registry entry.
    #[serde(default = "default_false")]
    pub run_at_startup: bool,

    /// Hide the tray/notification-area icon. The app keeps running in the
    /// background either way -- this only controls whether the icon is
    /// drawn. Applied live (no restart needed) by the UI poll loop. Because
    /// hiding the icon also hides the "Settings…" menu item, the guaranteed
    /// way back in is launching QuickDictate again: the single-instance guard
    /// detects the already-running process and asks it to reveal Settings
    /// instead of starting a second copy (see `main.rs`).
    #[serde(default = "default_false")]
    pub hide_tray_icon: bool,

    /// Probe the active provider's keys at startup (in the background) so dead
    /// or limited keys are known before the first dictation and a working key
    /// is queued up ready to go. Key health lives in memory only — a fresh
    /// launch re-checks everything, so a temporary outage never permanently
    /// brands a key dead.
    #[serde(default = "default_true")]
    pub prewarm_keys: bool,

    /// Legacy single-provider key field (pre-multi-provider builds). Still
    /// deserialized for back-compat: a non-empty `local_keys` is treated as
    /// `elevenlabs_keys` (see [`Config::active_keys`]). Prefer the per-provider
    /// arrays above in new settings.json files.
    #[serde(default)]
    pub local_keys: Vec<String>,

    /// Where QuickDictate keeps its runtime files: the `logs\` folder, the
    /// stats json, the settings-sync credential blob, and the update-check
    /// cache. Empty (the default) means "the folder settings.json is in",
    /// which for a shipped exe is the folder the exe sits in -- the historical
    /// behaviour. Set it to move that clutter somewhere else, which is the
    /// point: an exe on the Desktop otherwise turns the Desktop into a scratch
    /// directory.
    ///
    /// `%VARIABLES%` are expanded, so `%LOCALAPPDATA%\QuickDictate` is a valid
    /// value. The path must be absolute; a relative one is refused (it would
    /// resolve against whatever working directory Explorer or the Run key
    /// happened to hand us). The environment variable `QUICKDICTATE_DATA_DIR`
    /// overrides this. See [`crate::paths`] for the full resolution order.
    ///
    /// settings.json itself is NOT moved by this -- it has to be found before
    /// it can be read. [`Config::settings_path`] also looks in
    /// `%LOCALAPPDATA%\QuickDictate`, which is how the exe's own folder can be
    /// left completely empty.
    ///
    /// Machine-local (a path on this PC), so it is not synced.
    #[serde(default)]
    pub data_dir: String,

    /// When true, the app writes `logs\quickdictate.log` into the data folder
    /// above. Off by default; flip this on if you need to diagnose anything.
    /// (The env var `QUICKDICTATE_LOG` also forces it on.)
    #[serde(default = "default_false")]
    pub enable_logging: bool,

    /// Soft size cap for `quickdictate.log`, in **megabytes**. The writer
    /// rotates at startup and during a long run, keeping one previous
    /// generation as `logs\quickdictate.log.old`. `0` disables the cap
    /// (unbounded — the old behavior). Machine-local (like `enable_logging`),
    /// so it is not synced.
    #[serde(default = "default_max_log_mb")]
    pub max_log_mb: u64,

    /// When true, log lines that would otherwise summarize a recognized
    /// transcript (char count, provider, timing) instead log the **full
    /// dictated text**. Off by default: with this false, your spoken words
    /// never get written to disk, even with `enable_logging` on. Only flip
    /// this on for deep debugging of a specific transcription problem, and
    /// turn it back off afterwards -- `logs\quickdictate.log` is not encrypted
    /// or access-controlled beyond normal filesystem permissions.
    #[serde(default = "default_false")]
    pub log_transcripts: bool,

    // Note: `delay_output_till_release` (declared above) now means:
    //   true  -> *Hybrid*: hold mid-session VAD commits, flush them as one paste
    //            the instant you release the hotkey, and then live-paste any
    //            additional commits that arrive during the dynamic tail
    //            (so continued speech after release gets appended).
    //   false -> Live throughout: every committed_transcript pastes immediately.
    // True remains the default; it matches what feels like "responsive on
    // release without typing while you're mid-thought".
    #[serde(default = "default_replacements")]
    pub text_replacements: BTreeMap<String, String>,

    /// Master on/off switch for the text-replacement pass. When false the
    /// `text_replacements` map is kept (so toggling it back on restores the
    /// list) but not applied to recognized text.
    #[serde(default = "default_true")]
    pub enable_text_replacements: bool,

    /// Per-application overrides (⭐ Per-App Profiles), matched against the
    /// foreground window's exe at transcription-commit time. Empty by
    /// default -- global behavior is byte-identical to a settings.json with
    /// no `profiles` key at all. See [`Profile`] for the shape and
    /// [`Config::effective_settings`] for how a match is applied.
    #[serde(default)]
    pub profiles: Vec<Profile>,

    /// Master on/off switch for Per-App Profiles. Defaults to `true` so
    /// existing behavior is unchanged (an empty `profiles` list already
    /// means "off" in practice). Flipping this to `false` disables profile
    /// matching entirely -- even with `profiles` populated -- and
    /// `effective_settings` falls back to the global settings, same as if
    /// `profiles` were empty.
    #[serde(default = "default_true")]
    pub profiles_enabled: bool,

    /// Enables the precision "scratch that" voice command (see
    /// `crate::voice_commands`). Off by default. Deliberately capped to just
    /// this one phrase -- a broader pause-gated punctuation command set is
    /// **out of scope / deferred**, not built here.
    #[serde(default = "default_false")]
    pub voice_commands: bool,

    /// Words and phrases to bias recognition toward: names, jargon, product
    /// names, anything the provider keeps mishearing. Unlike
    /// `text_replacements` (which repairs the text *after* recognition), this
    /// is sent to the provider so it can get the word right in the first
    /// place. Forwarded as each backend's own biasing parameter (Deepgram
    /// `keyterm`, AssemblyAI `keyterms_prompt`, OpenAI/Whisper `prompt`,
    /// ElevenLabs biasing, local `initial_prompt`); providers without a
    /// biasing knob simply ignore it. Empty by default.
    #[serde(default)]
    pub custom_vocabulary: Vec<String>,

    /// Install a newer release automatically, with no prompt, when the daily
    /// check finds one. **Off by default since v0.5.4**: the trust chain for an
    /// update is the release itself (URL + SHA-256 both come from the release
    /// payload), so anything able to publish a release could otherwise reach
    /// every install unattended within 24 h. With this off, `update_auto_check`
    /// still runs and QuickDictate surfaces "update available" on the tray icon
    /// and the About pill; clicking the pill is the consent and installs it.
    /// Turn this on if you would rather have it applied silently.
    #[serde(default = "default_false")]
    pub update_auto_install: bool,

    /// Encrypt the API keys in settings.json at rest with Windows DPAPI
    /// (CurrentUser scope), the same primitive that seals the settings-sync
    /// refresh token. Off by default **because it costs portability**: a sealed
    /// settings.json only decrypts for this Windows user on this machine, so
    /// copying the folder to another PC or another account loses the keys and
    /// you have to paste them again. Turn it on if the folder is somewhere
    /// other local accounts or backup/sync tooling can read it. Toggling it
    /// re-writes settings.json in the new form immediately; turning it back off
    /// restores plaintext, so you can always get your keys back on the machine
    /// that sealed them.
    #[serde(default = "default_false")]
    pub protect_keys_at_rest: bool,

    /// Run an LLM cleanup pass over the transcript before pasting it (see
    /// [`crate::polish`]). Off by default: it is the only part of the paste
    /// path that talks to a third party, and everything else here works with
    /// no network at all once the transcript has landed.
    ///
    /// It cannot make dictation slower than `polish_deadline_ms`, and while
    /// the hotkey is still down it usually costs nothing at all -- the pass
    /// runs against the held transcript in the background, so by the time you
    /// release, the answer is already waiting.
    #[serde(default = "default_false")]
    pub polish_enabled: bool,

    /// The longest the paste will wait for that pass. Whichever finishes
    /// first wins, so a slow model costs exactly this and never the round
    /// trip. Speculation during a held dictation usually beats it to zero.
    #[serde(default = "default_polish_deadline_ms")]
    pub polish_deadline_ms: u64,

    /// OpenAI-compatible chat-completions endpoint. Any provider speaking
    /// that shape works by changing this one line -- e.g. Groq
    /// (`https://api.groq.com/openai/v1/chat/completions`) or Cerebras
    /// (`https://api.cerebras.ai/v1/chat/completions`), both of which are
    /// several times faster than the default and much likelier to win the
    /// race on a long dictation.
    #[serde(default = "default_polish_endpoint")]
    pub polish_endpoint: String,

    /// Model for the cleanup pass. Pick the fastest one that can follow the
    /// instructions: the reply is a short edit list, so latency here is
    /// dominated by how quickly the model starts talking, not by how hard the
    /// text is.
    #[serde(default = "default_polish_model")]
    pub polish_model: String,

    /// Key for `polish_endpoint`. Empty falls back to `openai_keys`, which is
    /// the right thing when the endpoint is OpenAI's and wrong for anyone
    /// else -- set this explicitly when you point it somewhere new.
    #[serde(default)]
    pub polish_keys: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mode: default_mode(),
            language: default_language(),
            toggle_hotkey: default_toggle_hotkey(),
            hold_hotkey: default_hold_hotkey(),
            reinsert_hold_ms: default_reinsert_hold_ms(),
            listen_tail_ms: default_listen_tail_ms(),
            clipboard_restore_delay_ms: default_clipboard_restore_delay_ms(),
            auto_space: true,
            auto_newline: false,
            auto_punct: true,
            hotkeys_enabled: true,
            enable_sound: false,
            close_behavior: default_close(),
            window_width: default_width(),
            window_height: default_height(),
            window_x: None,
            window_y: None,
            mouse_follower_enabled: true,
            delay_output_till_release: true,
            spinner_type: default_spinner(),
            stt_provider: default_provider(),
            elevenlabs_keys: Vec::new(),
            deepgram_keys: Vec::new(),
            openai_keys: Vec::new(),
            assemblyai_keys: Vec::new(),
            dashscope_keys: Vec::new(),
            google_keys: Vec::new(),
            stt_model: None,
            local_model: default_local_model(),
            dashscope_intl: false,
            update_auto_check: true,
            install_id: String::new(),
            run_at_startup: false,
            hide_tray_icon: false,
            prewarm_keys: true,
            local_keys: Vec::new(),
            data_dir: String::new(),
            enable_logging: false,
            max_log_mb: default_max_log_mb(),
            log_transcripts: false,
            text_replacements: default_replacements(),
            enable_text_replacements: true,
            profiles: Vec::new(),
            profiles_enabled: true,
            voice_commands: false,
            custom_vocabulary: Vec::new(),
            polish_enabled: false,
            polish_deadline_ms: default_polish_deadline_ms(),
            polish_endpoint: default_polish_endpoint(),
            polish_model: default_polish_model(),
            polish_keys: Vec::new(),
            update_auto_install: false,
            protect_keys_at_rest: false,
        }
    }
}

impl Config {
    /// Whether a `settings.json` found in an ANCESTOR of the exe directory may
    /// be adopted. Debug builds: always (that is the dev-run convenience).
    /// Release builds: only when the directory is unmistakably a QuickDictate
    /// working tree, i.e. it holds a `Cargo.toml` whose package name is ours,
    /// or an explicit `.quickdictate-root` marker file. Everything else is
    /// somebody else's folder and must be left alone.
    fn ancestor_settings_allowed(dir: &Path) -> bool {
        if cfg!(debug_assertions) {
            return true;
        }
        if dir.join(".quickdictate-root").exists() {
            return true;
        }
        fs::read_to_string(dir.join("Cargo.toml"))
            .map(|t| t.contains("name = \"quickdictate\""))
            .unwrap_or(false)
    }

    /// `settings.json` inside the folder named by `QUICKDICTATE_DATA_DIR`, if
    /// that variable names a usable absolute path. Checked first so the
    /// environment override is a COMPLETE relocation lever: without this, a
    /// scripted portable install could move every runtime file and still be
    /// forced to leave settings.json behind next to the exe.
    fn env_settings_path() -> Option<PathBuf> {
        let raw = std::env::var(crate::paths::DATA_DIR_ENV).ok()?;
        crate::paths::expand(&raw).map(|dir| dir.join("settings.json"))
    }

    pub fn settings_path() -> PathBuf {
        // Search order:
        //   0. %QUICKDICTATE_DATA_DIR%\settings.json
        //   1. settings.json next to the .exe (packaged install)
        //   2. walk up from the exe dir (covers `target/release/exe` -> project root)
        //   3. %LOCALAPPDATA%\QuickDictate\settings.json (the well-known
        //      location, so the exe's own folder can be left empty)
        //   4. current working directory
        //
        // The exe-adjacent file deliberately outranks the AppData one: a
        // settings.json sitting next to the exe IS the portable install's
        // config, and an upgrade must never silently move such a user onto a
        // different file.
        if let Some(path) = Self::env_settings_path() {
            if path.exists() {
                return path;
            }
        }

        let exe = std::env::current_exe().ok();
        let exe_dir = exe
            .as_ref()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()));

        if let Some(dir) = exe_dir.as_ref() {
            let direct = dir.join("settings.json");
            if direct.exists() {
                return direct;
            }
            // Walk up looking for a settings.json. This exists purely so a dev
            // run from target/{profile}/ (or .../deps/) picks up the project
            // root's settings.json. It must NOT happen in a shipped build: a
            // portable exe dropped in, say, Downloads\QuickDictate\ would
            // otherwise adopt an unrelated settings.json sitting in the user's
            // profile folder and then overwrite it wholesale on the next Save.
            // Debug builds walk freely; release builds only accept an ancestor
            // that is explicitly marked as a QuickDictate working tree.
            let mut cur = dir.clone();
            for _ in 0..5 {
                if let Some(parent) = cur.parent() {
                    let candidate = parent.join("settings.json");
                    if candidate.exists() && Self::ancestor_settings_allowed(parent) {
                        return candidate;
                    }
                    cur = parent.to_path_buf();
                } else {
                    break;
                }
            }
        }

        // The well-known off-exe location. Only adopted when the file is really
        // there, so this can never redirect an install that has its own copy.
        if let Some(path) = crate::paths::app_data_dir().map(|d| d.join("settings.json")) {
            if path.exists() {
                return path;
            }
        }

        let cwd = PathBuf::from("settings.json");
        if cwd.exists() {
            return cwd;
        }

        // Nothing exists yet, so this is where a NEW settings.json goes. An
        // explicit environment override wins; otherwise, next to the exe as
        // always.
        if let Some(path) = Self::env_settings_path() {
            return path;
        }
        exe_dir.map(|p| p.join("settings.json")).unwrap_or(cwd)
    }

    /// Load settings.json, generating one with defaults if no file is found.
    /// We don't use `tracing` here because logging may not be initialized yet
    /// (logging is now configured *from* the loaded settings). Instead we
    /// return a list of diagnostic messages the caller replays via tracing
    /// after init_logging has run.
    pub fn load_or_create() -> (Self, Vec<String>) {
        let mut diags: Vec<String> = Vec::new();
        let path = Self::settings_path();

        if path.exists() {
            return match fs::read_to_string(&path) {
                Ok(data) => match serde_json::from_str::<Config>(&data) {
                    Ok(mut c) => {
                        diags.push(format!("INFO: Loaded settings from {}", path.display()));
                        diags.extend(c.unseal_keys());
                        let configured_model = c.local_model.clone();
                        if c.normalize_local_model() {
                            diags.push(format!(
                                "WARN: local model '{configured_model}' is no longer available; \
                                 using '{}' instead",
                                c.local_model
                            ));
                        }
                        (c, diags)
                    }
                    Err(e) => {
                        // Preserve the unparseable file instead of silently
                        // discarding the user's keys/prefs on a hand-edit typo or
                        // truncated write: copy it aside so it can be recovered or
                        // hand-fixed, and report it loudly.
                        let bad = path.with_extension("json.bad");
                        match fs::copy(&path, &bad) {
                            Ok(_) => diags.push(format!(
                                "ALERT: failed to parse {}: {e}. Backed up the original to {} and started from defaults — restore or fix it to recover your settings.",
                                path.display(),
                                bad.display()
                            )),
                            Err(copy_err) => diags.push(format!(
                                "ALERT: failed to parse {}: {e}. Using defaults. (Could not back up the original: {copy_err})",
                                path.display()
                            )),
                        }
                        (Config::default(), diags)
                    }
                },
                Err(e) => {
                    diags.push(format!(
                        "ERROR: failed to read {}: {e}. Using defaults.",
                        path.display()
                    ));
                    (Config::default(), diags)
                }
            };
        }

        // File missing: write the embedded template (settings.example.json,
        // baked into the exe) to the canonical location, so the first launch
        // leaves a real, nicely-formatted file to edit. `settings_path` already
        // decided where that is (next to the exe, unless the environment
        // override names somewhere else), so don't re-derive it here -- the two
        // answers drifting apart would write the template to a file the next
        // launch does not read.
        let target = path;
        if let Some(parent) = target.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                diags.push(format!(
                    "WARN: could not create {} for settings.json: {e}",
                    parent.display()
                ));
            }
        }

        // Parse the template so the running config matches what we just wrote;
        // fall back to Config::default() if the bundled template can't parse
        // (shouldn't happen — it's the same struct).
        let cfg = serde_json::from_str::<Config>(EXAMPLE_JSON).unwrap_or_default();
        match fs::write(&target, EXAMPLE_JSON) {
            Ok(()) => diags.push(format!(
                "INFO: generated settings.json from the built-in template at {}",
                target.display()
            )),
            Err(e) => diags.push(format!(
                "WARN: could not write settings to {}: {e}",
                target.display()
            )),
        }
        (cfg, diags)
    }

    /// Every per-provider key array, mutably, in canonical order. One place to
    /// add a provider so the seal/unseal passes can never miss one.
    fn key_arrays_mut(&mut self) -> Vec<&mut Vec<String>> {
        vec![
            &mut self.elevenlabs_keys,
            &mut self.deepgram_keys,
            &mut self.openai_keys,
            &mut self.assemblyai_keys,
            &mut self.dashscope_keys,
            &mut self.google_keys,
            &mut self.local_keys,
        ]
    }

    /// Turn any sealed key back into plaintext for in-memory use. A value that
    /// will not decrypt (settings.json copied from another Windows account or
    /// machine) is dropped rather than passed to a provider as garbage, and
    /// reported so the user is told to paste the key again here.
    fn unseal_keys(&mut self) -> Vec<String> {
        let mut diags = Vec::new();
        let mut unreadable = 0usize;
        for array in self.key_arrays_mut() {
            array.retain_mut(|value| {
                if !crate::secretstore::is_sealed(value) {
                    return true;
                }
                match crate::secretstore::unseal_secret(value) {
                    Some(plain) => {
                        *value = plain;
                        true
                    }
                    None => {
                        unreadable += 1;
                        false
                    }
                }
            });
        }
        if unreadable > 0 {
            diags.push(format!(
                "ALERT: {unreadable} protected API key(s) in settings.json could not be decrypted \
                 on this Windows account. Protected keys are bound to the user and machine that \
                 saved them, so a copied folder needs the keys pasted in again."
            ));
        }
        diags
    }

    /// Inverse of [`Config::unseal_keys`], applied to the copy about to be
    /// written. A key that fails to seal is left as plaintext rather than
    /// written back as an unusable value.
    fn seal_keys(&mut self) {
        let mut failures = 0usize;
        for array in self.key_arrays_mut() {
            for value in array.iter_mut() {
                if value.trim().is_empty() || crate::secretstore::is_sealed(value) {
                    continue;
                }
                match crate::secretstore::seal_secret(value) {
                    Some(sealed) => *value = sealed,
                    None => failures += 1,
                }
            }
        }
        if failures > 0 {
            tracing::error!(
                "protect_keys_at_rest is on but DPAPI refused to seal {failures} key(s); \
                 those keys stay in plaintext in settings.json"
            );
        }
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        // `protect_keys_at_rest` seals the key arrays on the way to disk only.
        // The live Config keeps plaintext so KeyPool and the Settings window
        // are unaffected by the toggle.
        let pretty = if self.protect_keys_at_rest {
            let mut sealed = self.clone();
            sealed.seal_keys();
            serde_json::to_string_pretty(&sealed)?
        } else {
            serde_json::to_string_pretty(self)?
        };
        // Write-then-rename so a crash, power loss, or AV lock mid-write can
        // never leave a truncated settings.json (which would silently wipe the
        // user's API keys and preferences on the next load). Same atomic idiom
        // as sync.rs::save_creds; rename() is atomic on the same volume, which
        // the sibling temp path guarantees.
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, pretty.as_bytes())?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Replace a removed or otherwise unknown local-model id with the current
    /// default. This keeps settings created by an older QuickDictate build from
    /// leaving the Local provider in an unusable state.
    pub(crate) fn normalize_local_model(&mut self) -> bool {
        if crate::local_stt::model(&self.local_model).is_some() {
            return false;
        }
        self.local_model = default_local_model();
        true
    }

    /// Persist a freshly generated [`Config::install_id`] with the lightest
    /// possible touch: when the on-disk file still has the template's empty
    /// `"install_id": ""` slot, fill it in place — leaving the user's key
    /// ordering, grouping, and hand edits byte-for-byte intact (this write
    /// happens in the background at startup; it must not reformat a file the
    /// user curates). Files without the slot (settings.json from an older
    /// version) fall back to a normal [`Config::save`] — the same full
    /// rewrite the Settings window already does on every save.
    pub fn save_install_id(&self, path: &Path) -> anyhow::Result<()> {
        const EMPTY_SLOT: &str = "\"install_id\": \"\"";
        if let Ok(text) = fs::read_to_string(path) {
            if text.contains(EMPTY_SLOT) {
                let filled = text.replace(
                    EMPTY_SLOT,
                    &format!("\"install_id\": \"{}\"", self.install_id),
                );
                // Same write-then-rename idiom as save().
                let tmp = path.with_extension("json.tmp");
                fs::write(&tmp, filled.as_bytes())?;
                fs::rename(&tmp, path)?;
                return Ok(());
            }
        }
        self.save(path)
    }

    pub fn is_hold_mode(&self) -> bool {
        self.mode.eq_ignore_ascii_case("hold")
    }

    /// Keys for the currently selected provider. Empty means "no key
    /// configured" and the app surfaces an add-your-key message.
    ///
    /// Back-compat: for the ElevenLabs provider (the default), if
    /// `elevenlabs_keys` is empty we fall back to a legacy non-empty
    /// `local_keys` so older settings.json files keep working unchanged.
    pub fn active_keys(&self) -> &[String] {
        self.keys_for(&self.stt_provider)
    }

    /// Non-empty API keys for a given provider id (folding legacy `local_keys`
    /// into elevenlabs). Same back-compat rule as [`Config::active_keys`].
    pub fn keys_for(&self, provider: &str) -> &[String] {
        match provider.trim().to_ascii_lowercase().as_str() {
            "local" => &[],
            "deepgram" => &self.deepgram_keys,
            "openai" => &self.openai_keys,
            "assemblyai" => &self.assemblyai_keys,
            "dashscope" => &self.dashscope_keys,
            "google" => &self.google_keys,
            _ => {
                if !self.elevenlabs_keys.is_empty() {
                    &self.elevenlabs_keys
                } else {
                    &self.local_keys
                }
            }
        }
    }

    /// All provider ids that have at least one key configured, in canonical
    /// order. Used to auto-pick a provider and to decide whether to show the
    /// "no API keys" onboarding notice.
    pub fn providers_with_keys(&self) -> Vec<&'static str> {
        [
            "elevenlabs",
            "deepgram",
            "openai",
            "assemblyai",
            "dashscope",
            "google",
        ]
        .into_iter()
        .filter(|id| !self.keys_for(id).is_empty())
        .collect()
    }

    /// The provider to actually use at launch: the configured one if it has
    /// keys, otherwise the first provider that *does* (so a user who only
    /// pasted, say, Google keys opens straight into Google). `None` when no
    /// provider has any keys. Does not mutate self.
    pub fn resolve_provider(&self) -> Option<String> {
        if self.stt_provider.trim().eq_ignore_ascii_case("local") {
            return Some("local".into());
        }
        if !self.active_keys().is_empty() {
            return Some(self.stt_provider.clone());
        }
        self.providers_with_keys().first().map(|s| s.to_string())
    }

    /// The first profile whose `match` list contains `exe_name`
    /// (case-insensitive), or `None` if `exe_name` is `None`, profiles are
    /// disabled via `profiles_enabled`, or nothing matches. First match wins
    /// -- profiles earlier in the list take precedence over later ones.
    pub fn active_profile(&self, exe_name: Option<&str>) -> Option<&Profile> {
        if !self.profiles_enabled {
            return None;
        }
        let exe_name = exe_name?;
        self.profiles.iter().find(|p| p.matches(exe_name))
    }

    /// Fold a matched profile's overrides onto the global settings. With no
    /// match (no profiles configured, `profiles_enabled` is `false`, or
    /// nothing matches), this is byte-identical to the global `auto_punct` /
    /// `auto_space` / `auto_newline` / `text_replacements` (honoring
    /// `enable_text_replacements`) -- i.e. today's behavior, unchanged.
    pub fn effective_settings(&self, exe_name: Option<&str>) -> EffectiveSettings {
        let empty = BTreeMap::new();
        let global_replacements = if self.enable_text_replacements {
            &self.text_replacements
        } else {
            &empty
        };

        let Some(profile) = self.active_profile(exe_name) else {
            return EffectiveSettings {
                auto_punct: self.auto_punct,
                auto_space: self.auto_space,
                auto_newline: self.auto_newline,
                text_replacements: global_replacements.clone(),
                language: self.language.clone(),
                custom_vocabulary: self.custom_vocabulary.clone(),
            };
        };

        let text_replacements = if profile.replacements_mode.eq_ignore_ascii_case("replace") {
            profile.text_replacements.clone()
        } else {
            // "extend" (default): start from the global map, layer the
            // profile's entries on top (profile wins on key collision).
            let mut merged = global_replacements.clone();
            merged.extend(
                profile
                    .text_replacements
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone())),
            );
            merged
        };

        EffectiveSettings {
            auto_punct: profile.auto_punct.unwrap_or(self.auto_punct),
            auto_space: profile.auto_space.unwrap_or(self.auto_space),
            auto_newline: profile.auto_newline.unwrap_or(self.auto_newline),
            text_replacements,
            language: profile
                .language
                .clone()
                .filter(|l| !l.trim().is_empty())
                .unwrap_or_else(|| self.language.clone()),
            custom_vocabulary: profile
                .custom_vocabulary
                .clone()
                .unwrap_or_else(|| self.custom_vocabulary.clone()),
        }
    }

    /// Is the LLM cleanup pass on for a paste landing in `exe_name`? A
    /// profile overrides the global flag in both directions, matching how
    /// every other per-app setting folds in.
    pub fn polish_for_exe(&self, exe_name: Option<&str>) -> bool {
        self.active_profile(exe_name)
            .and_then(|p| p.polish)
            .unwrap_or(self.polish_enabled)
    }

    /// Could the cleanup pass run for *some* app? Used by the session runner,
    /// which speculates while the hotkey is still down and does not yet know
    /// where the text will land.
    pub fn polish_possible(&self) -> bool {
        if self.polish_key_pool().is_empty() {
            return false;
        }
        if self.polish_enabled {
            return true;
        }
        self.profiles_enabled && self.profiles.iter().any(|p| p.polish == Some(true))
    }

    /// Keys for `polish_endpoint`, round-robined per request: the dedicated
    /// list if set, else the OpenAI pool (right for the default endpoint, and
    /// the reason `polish_keys` exists for everyone else). Falls back rather
    /// than merging, since keys for two different providers cannot both
    /// authenticate against one endpoint.
    pub fn polish_key_pool(&self) -> Vec<String> {
        let source = if self.polish_keys.iter().any(|k| !k.trim().is_empty()) {
            &self.polish_keys
        } else {
            &self.openai_keys
        };
        source
            .iter()
            .map(|k| k.trim())
            .filter(|k| !k.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// The STT provider to use for a dictation started while `exe_name` is in
    /// the foreground. A profile may override the global provider, but only to
    /// one that actually has a key (or to the keyless local provider) -- an
    /// unknown id or a keyless cloud provider silently falls back to the global
    /// choice, so a bad profile can never leave the user unable to dictate.
    pub fn provider_for_exe(&self, exe_name: Option<&str>) -> Option<String> {
        let global = self.resolve_provider();
        let Some(profile) = self.active_profile(exe_name) else {
            return global;
        };
        let Some(want) = profile.stt_provider.as_deref().map(str::trim) else {
            return global;
        };
        let want = want.to_ascii_lowercase();
        if want.is_empty() {
            return global;
        }
        if want == "local" {
            return Some(want);
        }
        let known = [
            "elevenlabs",
            "deepgram",
            "openai",
            "assemblyai",
            "dashscope",
            "google",
        ];
        if known.contains(&want.as_str()) && !self.keys_for(&want).is_empty() {
            return Some(want);
        }
        tracing::warn!(
            "profile '{}' asks for provider '{want}', which is unknown or has no key; using the global provider",
            profile.name
        );
        global
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_elevenlabs_with_no_keys() {
        let c = Config::default();
        assert_eq!(c.stt_provider, "elevenlabs");
        assert!(c.active_keys().is_empty());
    }

    #[test]
    fn legacy_local_keys_fold_into_elevenlabs() {
        // An old settings.json with only `local_keys` and no provider fields.
        let json = r#"{ "local_keys": ["sk_old_a", "sk_old_b"] }"#;
        let c: Config = serde_json::from_str(json).unwrap();
        assert_eq!(c.stt_provider, "elevenlabs");
        assert_eq!(
            c.active_keys(),
            &["sk_old_a".to_string(), "sk_old_b".to_string()]
        );
    }

    #[test]
    fn elevenlabs_keys_take_precedence_over_local_keys() {
        let c = Config {
            local_keys: vec!["sk_legacy".into()],
            elevenlabs_keys: vec!["sk_new".into()],
            ..Config::default()
        };
        assert_eq!(c.active_keys(), &["sk_new".to_string()]);
    }

    #[test]
    fn active_keys_follow_selected_provider() {
        let json = r#"{
            "stt_provider": "deepgram",
            "elevenlabs_keys": ["el1"],
            "deepgram_keys": ["dg1", "dg2"]
        }"#;
        let c: Config = serde_json::from_str(json).unwrap();
        assert_eq!(c.active_keys(), &["dg1".to_string(), "dg2".to_string()]);
    }

    #[test]
    fn unknown_provider_falls_back_to_elevenlabs_keys() {
        let json = r#"{ "stt_provider": "myst", "elevenlabs_keys": ["el1"] }"#;
        let c: Config = serde_json::from_str(json).unwrap();
        assert_eq!(c.active_keys(), &["el1".to_string()]);
    }

    #[test]
    fn resolve_switches_to_the_only_provider_with_keys() {
        // Configured provider (default elevenlabs) has none; only Google does.
        let c = Config {
            google_keys: vec!["g1".into()],
            ..Config::default()
        };
        assert_eq!(c.resolve_provider().as_deref(), Some("google"));
        assert_eq!(c.providers_with_keys(), vec!["google"]);
    }

    #[test]
    fn resolve_keeps_configured_provider_when_it_has_keys() {
        let c = Config {
            stt_provider: "deepgram".into(),
            deepgram_keys: vec!["dg1".into()],
            google_keys: vec!["g1".into()],
            ..Config::default()
        };
        assert_eq!(c.resolve_provider().as_deref(), Some("deepgram"));
    }

    #[test]
    fn resolve_is_none_when_no_keys_anywhere() {
        let c = Config::default();
        assert!(c.resolve_provider().is_none());
        assert!(c.providers_with_keys().is_empty());
    }

    #[test]
    fn selected_local_provider_needs_no_api_key() {
        let c = Config {
            stt_provider: "LOCAL".into(),
            ..Config::default()
        };
        assert!(c.active_keys().is_empty());
        assert_eq!(c.resolve_provider().as_deref(), Some("local"));
        assert_eq!(c.local_model, "cohere-q5");
    }

    #[test]
    fn unavailable_local_model_falls_back_to_current_default() {
        let mut c = Config {
            local_model: "retired-model".into(),
            ..Config::default()
        };
        assert!(c.normalize_local_model());
        assert_eq!(c.local_model, "cohere-q5");
        assert!(!c.normalize_local_model());
    }

    #[test]
    fn bundled_example_json_parses() {
        // The template baked into the exe must always deserialize.
        let c: Config = serde_json::from_str(EXAMPLE_JSON).unwrap();
        assert!(!c.stt_provider.is_empty());
    }

    // ---- Anonymous install id ----------------------------------------------

    #[test]
    fn install_id_defaults_empty_and_round_trips() {
        assert!(Config::default().install_id.is_empty());
        // Older settings.json files without the key parse to "not generated".
        let c: Config = serde_json::from_str("{}").unwrap();
        assert!(c.install_id.is_empty());
        let c: Config = serde_json::from_str(r#"{ "install_id": "abc-123" }"#).unwrap();
        assert_eq!(c.install_id, "abc-123");
    }

    #[test]
    fn bundled_example_json_has_an_empty_install_id_slot() {
        // The first-run flow depends on filling this slot in place
        // (save_install_id) so the freshly written template keeps its
        // curated formatting.
        assert!(EXAMPLE_JSON.contains("\"install_id\": \"\""));
        let c: Config = serde_json::from_str(EXAMPLE_JSON).unwrap();
        assert!(c.install_id.is_empty());
    }

    #[test]
    fn save_install_id_fills_the_template_slot_in_place() {
        let path = std::env::temp_dir().join(format!(
            "qd-test-install-id-slot-{}.json",
            std::process::id()
        ));
        let original = "{\n  \"mode\": \"toggle\",\n\n  \"install_id\": \"\",\n  \"update_auto_check\": true\n}\n";
        fs::write(&path, original).unwrap();

        let c = Config {
            install_id: "11111111-2222-4333-8444-555555555555".into(),
            ..Config::default()
        };
        c.save_install_id(&path).unwrap();

        // The slot got filled and the rest of the file — ordering, grouping,
        // even the blank line — is byte-for-byte untouched.
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            original.replace(
                "\"install_id\": \"\"",
                "\"install_id\": \"11111111-2222-4333-8444-555555555555\""
            )
        );
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn save_install_id_falls_back_to_full_save_without_a_slot() {
        let path = std::env::temp_dir().join(format!(
            "qd-test-install-id-fallback-{}.json",
            std::process::id()
        ));
        // An older settings.json with no install_id key at all.
        fs::write(&path, "{ \"mode\": \"hold\" }").unwrap();

        let c = Config {
            mode: "hold".into(),
            install_id: "11111111-2222-4333-8444-555555555555".into(),
            ..Config::default()
        };
        c.save_install_id(&path).unwrap();

        let reloaded: Config = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(reloaded.install_id, "11111111-2222-4333-8444-555555555555");
        assert_eq!(reloaded.mode, "hold");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn keys_for_covers_every_provider_and_normalizes_the_id() {
        let c = Config {
            elevenlabs_keys: vec!["el".into()],
            deepgram_keys: vec!["dg".into()],
            openai_keys: vec!["oa".into()],
            assemblyai_keys: vec!["aa".into()],
            dashscope_keys: vec!["ds".into()],
            google_keys: vec!["gg".into()],
            ..Config::default()
        };
        assert_eq!(c.keys_for("elevenlabs"), &["el".to_string()]);
        assert_eq!(c.keys_for("deepgram"), &["dg".to_string()]);
        assert_eq!(c.keys_for("openai"), &["oa".to_string()]);
        assert_eq!(c.keys_for("assemblyai"), &["aa".to_string()]);
        assert_eq!(c.keys_for("dashscope"), &["ds".to_string()]);
        assert_eq!(c.keys_for("google"), &["gg".to_string()]);
        // The id is trimmed + lowercased before matching.
        assert_eq!(c.keys_for("  DashScope  "), &["ds".to_string()]);
        assert_eq!(c.keys_for("OPENAI"), &["oa".to_string()]);
        // Unknown provider falls back to the elevenlabs slot.
        assert_eq!(c.keys_for("mystery"), &["el".to_string()]);
        // Canonical order, all six present.
        assert_eq!(
            c.providers_with_keys(),
            vec![
                "elevenlabs",
                "deepgram",
                "openai",
                "assemblyai",
                "dashscope",
                "google"
            ]
        );
    }

    // ---- Per-App Profiles --------------------------------------------------

    fn profile(name: &str, match_: &[&str]) -> Profile {
        Profile {
            name: name.into(),
            match_: match_.iter().map(|s| s.to_string()).collect(),
            auto_punct: None,
            auto_space: None,
            auto_newline: None,
            replacements_mode: default_replacements_mode(),
            text_replacements: BTreeMap::new(),
            language: None,
            stt_provider: None,
            custom_vocabulary: None,
            polish: None,
        }
    }

    #[test]
    fn polish_is_off_without_a_key_no_matter_what_is_enabled() {
        let mut cfg = Config {
            polish_enabled: true,
            ..Default::default()
        };
        // Enabled but unauthenticated is not "possible" -- the session runner
        // must not fire speculative requests that can only 401.
        assert!(!cfg.polish_possible());
        cfg.openai_keys = vec!["  ".into()];
        assert!(!cfg.polish_possible(), "a blank key is not a key");
        cfg.openai_keys = vec!["sk-test".into(), "sk-two".into()];
        assert!(cfg.polish_possible());
        assert_eq!(cfg.polish_key_pool(), vec!["sk-test", "sk-two"]);
        // A dedicated list REPLACES the OpenAI pool rather than extending it:
        // once `polish_endpoint` points somewhere else, an OpenAI key mixed
        // into the rotation would just 401 every other request.
        cfg.polish_keys = vec!["gem-one".into(), "gem-two".into()];
        assert_eq!(cfg.polish_key_pool(), vec!["gem-one", "gem-two"]);
    }

    #[test]
    fn a_profile_overrides_polish_in_both_directions() {
        let mut cfg = Config {
            openai_keys: vec!["sk-test".into()],
            polish_enabled: true,
            ..Default::default()
        };
        let mut off = profile("Terminal", &["windowsterminal.exe"]);
        off.polish = Some(false);
        cfg.profiles = vec![off];

        assert!(
            cfg.polish_for_exe(Some("slack.exe")),
            "global still applies"
        );
        assert!(
            !cfg.polish_for_exe(Some("windowsterminal.exe")),
            "a terminal wants raw text and an instant paste"
        );

        // Off globally, on for one app: still possible, so speculation runs.
        cfg.polish_enabled = false;
        let mut on = profile("Slack", &["slack.exe"]);
        on.polish = Some(true);
        cfg.profiles = vec![on];
        assert!(cfg.polish_possible());
        assert!(cfg.polish_for_exe(Some("slack.exe")));
        assert!(!cfg.polish_for_exe(Some("code.exe")));

        // `profiles_enabled: false` takes the per-app opt-in with it.
        cfg.profiles_enabled = false;
        assert!(!cfg.polish_possible());
        assert!(!cfg.polish_for_exe(Some("slack.exe")));
    }

    #[test]
    fn profile_language_overrides_the_global_language() {
        let mut cfg = Config::default();
        let mut p = profile("German app", &["de.exe"]);
        p.language = Some("de-DE".into());
        cfg.profiles = vec![p];
        assert_eq!(cfg.effective_settings(Some("de.exe")).language, "de-DE");
        assert_eq!(
            cfg.effective_settings(Some("other.exe")).language,
            cfg.language
        );
    }

    #[test]
    fn blank_profile_language_falls_back_to_global() {
        let mut cfg = Config::default();
        let mut p = profile("Blank", &["x.exe"]);
        p.language = Some("   ".into());
        cfg.profiles = vec![p];
        assert_eq!(cfg.effective_settings(Some("x.exe")).language, cfg.language);
    }

    #[test]
    fn profile_vocabulary_replaces_the_global_list_including_when_empty() {
        let mut cfg = Config {
            custom_vocabulary: vec!["Supabase".into()],
            ..Config::default()
        };
        let mut p = profile("Quiet", &["q.exe"]);
        p.custom_vocabulary = Some(Vec::new());
        cfg.profiles = vec![p];
        assert!(cfg
            .effective_settings(Some("q.exe"))
            .custom_vocabulary
            .is_empty());
        assert_eq!(
            cfg.effective_settings(Some("z.exe")).custom_vocabulary,
            vec!["Supabase".to_string()]
        );
    }

    #[test]
    fn profile_provider_override_needs_a_configured_key() {
        let mut cfg = Config {
            elevenlabs_keys: vec!["k1".into()],
            ..Config::default()
        };
        let mut p = profile("Wants deepgram", &["a.exe"]);
        p.stt_provider = Some("deepgram".into());
        cfg.profiles = vec![p];
        // Deepgram has no key: fall back to the global provider.
        assert_eq!(
            cfg.provider_for_exe(Some("a.exe")).as_deref(),
            Some("elevenlabs")
        );
        cfg.deepgram_keys = vec!["k2".into()];
        assert_eq!(
            cfg.provider_for_exe(Some("a.exe")).as_deref(),
            Some("deepgram")
        );
    }

    #[test]
    fn profile_provider_override_accepts_keyless_local_and_rejects_junk() {
        let mut cfg = Config {
            elevenlabs_keys: vec!["k1".into()],
            ..Config::default()
        };
        let mut local = profile("Offline", &["secret.exe"]);
        local.stt_provider = Some("LOCAL".into());
        let mut junk = profile("Typo", &["typo.exe"]);
        junk.stt_provider = Some("deepgrma".into());
        cfg.profiles = vec![local, junk];
        assert_eq!(
            cfg.provider_for_exe(Some("secret.exe")).as_deref(),
            Some("local")
        );
        assert_eq!(
            cfg.provider_for_exe(Some("typo.exe")).as_deref(),
            Some("elevenlabs")
        );
    }

    #[test]
    fn sealed_keys_round_trip_through_a_saved_file() {
        let dir = std::env::temp_dir().join(format!(
            "qd-seal-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");

        let cfg = Config {
            protect_keys_at_rest: true,
            elevenlabs_keys: vec!["el-secret-1".into(), "el-secret-2".into()],
            deepgram_keys: vec!["dg-secret".into()],
            ..Config::default()
        };
        cfg.save(&path).unwrap();

        // On disk the keys must not be readable.
        let on_disk = fs::read_to_string(&path).unwrap();
        assert!(
            !on_disk.contains("el-secret-1") && !on_disk.contains("dg-secret"),
            "sealed save still wrote a plaintext key"
        );
        assert!(on_disk.contains(crate::secretstore::SEALED_PREFIX));

        // Loading brings them back in the clear for KeyPool.
        let mut loaded: Config = serde_json::from_str(&on_disk).unwrap();
        let diags = loaded.unseal_keys();
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
        assert_eq!(loaded.elevenlabs_keys, cfg.elevenlabs_keys);
        assert_eq!(loaded.deepgram_keys, cfg.deepgram_keys);

        // Turning the setting back off restores a plaintext file, so nobody is
        // locked out of their own keys on the machine that sealed them.
        loaded.protect_keys_at_rest = false;
        loaded.save(&path).unwrap();
        assert!(fs::read_to_string(&path).unwrap().contains("el-secret-1"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_undecryptable_key_is_dropped_and_reported_not_used_as_garbage() {
        let mut cfg = Config {
            elevenlabs_keys: vec!["plain-key".into(), "dpapi:bm90LXJlYWw=".into()],
            ..Config::default()
        };
        let diags = cfg.unseal_keys();
        assert_eq!(cfg.elevenlabs_keys, vec!["plain-key".to_string()]);
        assert_eq!(diags.len(), 1);
        assert!(diags[0].starts_with("ALERT:"));
    }

    #[test]
    fn a_release_build_ignores_a_settings_json_in_an_unrelated_ancestor() {
        let dir = std::env::temp_dir().join(format!(
            "qd-ancestor-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::create_dir_all(&dir).unwrap();

        // A bare directory is somebody else's folder: never adopt its config
        // in a shipped build. Debug builds keep the dev-run convenience.
        assert_eq!(
            Config::ancestor_settings_allowed(&dir),
            cfg!(debug_assertions)
        );

        // An explicit marker opts in, in every build.
        fs::write(dir.join(".quickdictate-root"), b"").unwrap();
        assert!(Config::ancestor_settings_allowed(&dir));
        fs::remove_file(dir.join(".quickdictate-root")).unwrap();

        // So does our own Cargo.toml, which is what a dev run walks up to.
        fs::write(
            dir.join("Cargo.toml"),
            b"[package]\nname = \"quickdictate\"\n",
        )
        .unwrap();
        assert!(Config::ancestor_settings_allowed(&dir));

        // Someone else's Cargo.toml does not.
        fs::write(dir.join("Cargo.toml"), b"[package]\nname = \"other-app\"\n").unwrap();
        assert_eq!(
            Config::ancestor_settings_allowed(&dir),
            cfg!(debug_assertions)
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn new_settings_round_trip_and_default_off() {
        let c = Config::default();
        assert!(!c.update_auto_install, "silent install must be opt-in");
        assert!(!c.protect_keys_at_rest, "key sealing must be opt-in");
        assert!(c.custom_vocabulary.is_empty());
        let json = serde_json::to_string(&c).unwrap();
        let back: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(back.update_auto_install, c.update_auto_install);
        assert_eq!(back.protect_keys_at_rest, c.protect_keys_at_rest);
        assert_eq!(back.custom_vocabulary, c.custom_vocabulary);
    }

    #[test]
    fn no_profiles_is_byte_identical_to_global_settings() {
        let c = Config {
            auto_punct: false,
            auto_space: true,
            auto_newline: true,
            ..Config::default()
        };
        let eff = c.effective_settings(Some("notepad.exe"));
        assert_eq!(eff.auto_punct, c.auto_punct);
        assert_eq!(eff.auto_space, c.auto_space);
        assert_eq!(eff.auto_newline, c.auto_newline);
        assert_eq!(eff.text_replacements, c.text_replacements);

        // Also true when we can't resolve a foreground exe at all.
        let eff_none = c.effective_settings(None);
        assert_eq!(eff_none.auto_punct, c.auto_punct);
        assert_eq!(eff_none.text_replacements, c.text_replacements);
    }

    #[test]
    fn profile_matching_is_case_insensitive_and_first_match_wins() {
        let mut code_profile = profile("Code editors", &["code.exe", "windowsterminal.exe"]);
        code_profile.auto_newline = Some(true);
        let mut generic_profile = profile("Catch-all", &["code.exe"]);
        generic_profile.auto_newline = Some(false);

        let c = Config {
            profiles: vec![code_profile, generic_profile],
            ..Config::default()
        };

        // Case-insensitive match against the exe basename.
        let matched = c.active_profile(Some("Code.EXE")).unwrap();
        assert_eq!(matched.name, "Code editors");

        // First matching profile wins even though a later one also matches.
        let eff = c.effective_settings(Some("code.exe"));
        assert!(eff.auto_newline);

        // No match -> None / global fallback.
        assert!(c.active_profile(Some("chrome.exe")).is_none());
    }

    #[test]
    fn profile_overrides_only_apply_the_fields_that_are_set() {
        let mut p = profile("Terminal", &["windowsterminal.exe"]);
        p.auto_space = Some(false); // override
                                    // auto_punct / auto_newline left None -> fall back to global.
        let c = Config {
            auto_punct: true,
            auto_space: true,
            auto_newline: false,
            profiles: vec![p],
            ..Config::default()
        };
        let eff = c.effective_settings(Some("windowsterminal.exe"));
        assert!(!eff.auto_space); // overridden
        assert!(eff.auto_punct); // fell back to global
        assert!(!eff.auto_newline); // fell back to global
    }

    #[test]
    fn replacements_mode_extend_layers_over_global_and_wins_on_collision() {
        let mut p = profile("Code editors", &["code.exe"]);
        p.replacements_mode = "extend".into();
        p.text_replacements.insert("dot py".into(), ".py".into());
        // Collides with a global entry -- profile should win.
        p.text_replacements
            .insert("github".into(), "GITHUB-OVERRIDE".into());

        let mut global = BTreeMap::new();
        global.insert("github".into(), "GitHub".into());
        global.insert("chat gpt".into(), "ChatGPT".into());

        let c = Config {
            text_replacements: global,
            profiles: vec![p],
            ..Config::default()
        };
        let eff = c.effective_settings(Some("code.exe"));
        assert_eq!(eff.text_replacements.get("dot py").unwrap(), ".py");
        assert_eq!(eff.text_replacements.get("chat gpt").unwrap(), "ChatGPT");
        assert_eq!(
            eff.text_replacements.get("github").unwrap(),
            "GITHUB-OVERRIDE"
        );
    }

    #[test]
    fn replacements_mode_replace_ignores_the_global_map_entirely() {
        let mut p = profile("Minimal", &["cmd.exe"]);
        p.replacements_mode = "replace".into();
        p.text_replacements.insert("foo".into(), "bar".into());

        let mut global = BTreeMap::new();
        global.insert("github".into(), "GitHub".into());

        let c = Config {
            text_replacements: global,
            profiles: vec![p],
            ..Config::default()
        };
        let eff = c.effective_settings(Some("cmd.exe"));
        assert_eq!(eff.text_replacements.len(), 1);
        assert_eq!(eff.text_replacements.get("foo").unwrap(), "bar");
        assert!(!eff.text_replacements.contains_key("github"));
    }

    #[test]
    fn disabled_global_replacements_still_extend_from_empty_base() {
        // enable_text_replacements = false means the *global* map is not
        // applied, but a profile in "extend" mode still layers its own
        // entries on top of that (now-empty) base.
        let mut p = profile("Code editors", &["code.exe"]);
        p.text_replacements.insert("foo".into(), "bar".into());

        let mut global = BTreeMap::new();
        global.insert("github".into(), "GitHub".into());

        let c = Config {
            text_replacements: global,
            enable_text_replacements: false,
            profiles: vec![p],
            ..Config::default()
        };
        let eff = c.effective_settings(Some("code.exe"));
        assert_eq!(eff.text_replacements.len(), 1);
        assert_eq!(eff.text_replacements.get("foo").unwrap(), "bar");
        assert!(!eff.text_replacements.contains_key("github"));
    }

    #[test]
    fn profiles_field_defaults_to_empty_and_round_trips_through_json() {
        let c = Config::default();
        assert!(c.profiles.is_empty());

        let json = serde_json::json!({
            "profiles": [{
                "name": "Code editors",
                "match": ["code.exe", "windowsterminal.exe"],
                "auto_newline": true,
                "replacements_mode": "extend",
                "text_replacements": { "dot py": ".py" }
            }]
        });
        let c: Config = serde_json::from_value(json).unwrap();
        assert_eq!(c.profiles.len(), 1);
        assert_eq!(c.profiles[0].name, "Code editors");
        assert_eq!(
            c.profiles[0].match_,
            vec!["code.exe", "windowsterminal.exe"]
        );
        assert_eq!(c.profiles[0].auto_newline, Some(true));
        assert_eq!(c.profiles[0].auto_punct, None);
    }

    #[test]
    fn profiles_enabled_defaults_to_true_and_round_trips_through_json() {
        let c = Config::default();
        assert!(c.profiles_enabled);

        // Also true for a settings.json that doesn't mention the key at all.
        let c: Config = serde_json::from_str("{}").unwrap();
        assert!(c.profiles_enabled);

        let json = r#"{ "profiles_enabled": false }"#;
        let c: Config = serde_json::from_str(json).unwrap();
        assert!(!c.profiles_enabled);
    }

    #[test]
    fn profiles_disabled_is_byte_identical_to_global_settings() {
        // A matching profile is configured, but the master switch is off --
        // effective_settings must fall back to the globals, same as if
        // `profiles` were empty.
        let mut p = profile("Code editors", &["code.exe"]);
        p.auto_punct = Some(false);
        p.auto_space = Some(false);
        p.auto_newline = Some(true);
        p.text_replacements.insert("foo".into(), "bar".into());

        let c = Config {
            auto_punct: true,
            auto_space: true,
            auto_newline: false,
            profiles: vec![p],
            profiles_enabled: false,
            ..Config::default()
        };

        assert!(c.active_profile(Some("code.exe")).is_none());

        let eff = c.effective_settings(Some("code.exe"));
        assert_eq!(eff.auto_punct, c.auto_punct);
        assert_eq!(eff.auto_space, c.auto_space);
        assert_eq!(eff.auto_newline, c.auto_newline);
        assert_eq!(eff.text_replacements, c.text_replacements);

        // Also true when we can't resolve a foreground exe at all.
        let eff_none = c.effective_settings(None);
        assert_eq!(eff_none.auto_punct, c.auto_punct);
        assert_eq!(eff_none.text_replacements, c.text_replacements);
    }

    // ---- Voice Commands (precision subset: "scratch that") ----------------

    #[test]
    fn voice_commands_defaults_to_false() {
        let c = Config::default();
        assert!(!c.voice_commands);

        // Also true for a settings.json that doesn't mention the key at all.
        let c: Config = serde_json::from_str("{}").unwrap();
        assert!(!c.voice_commands);
    }

    #[test]
    fn voice_commands_round_trips_through_json() {
        let json = r#"{ "voice_commands": true }"#;
        let c: Config = serde_json::from_str(json).unwrap();
        assert!(c.voice_commands);
    }

    // ---- Timing levers (re-paste hold + listen tail) ----------------------

    #[test]
    fn listen_tail_ms_defaults_to_800() {
        let c = Config::default();
        assert_eq!(c.listen_tail_ms, 800);

        // Also true for a settings.json that doesn't mention the key at all —
        // so existing files keep the original fixed-tail behavior.
        let c: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(c.listen_tail_ms, 800);
    }

    #[test]
    fn timing_levers_round_trip_through_json() {
        let json = r#"{ "reinsert_hold_ms": 2000, "listen_tail_ms": 1200 }"#;
        let c: Config = serde_json::from_str(json).unwrap();
        assert_eq!(c.reinsert_hold_ms, 2000);
        assert_eq!(c.listen_tail_ms, 1200);
    }

    #[test]
    fn clipboard_restore_delay_defaults_to_300() {
        assert_eq!(Config::default().clipboard_restore_delay_ms, 300);

        // Existing settings.json files without the key get the new default.
        let c: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(c.clipboard_restore_delay_ms, 300);
    }

    #[test]
    fn clipboard_restore_delay_round_trips_through_json() {
        let json = r#"{ "clipboard_restore_delay_ms": 0 }"#;
        let c: Config = serde_json::from_str(json).unwrap();
        assert_eq!(c.clipboard_restore_delay_ms, 0);
    }

    #[test]
    fn max_log_mb_defaults_to_5() {
        assert_eq!(Config::default().max_log_mb, 5);
        let c: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(c.max_log_mb, 5);
    }

    #[test]
    fn max_log_mb_round_trips_through_json() {
        let json = r#"{ "max_log_mb": 0 }"#;
        let c: Config = serde_json::from_str(json).unwrap();
        assert_eq!(c.max_log_mb, 0);
    }
}
