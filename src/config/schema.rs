//! The shape of settings.json.
//!
//! `Config` is the whole file, `Profile` is one Per-App Profile entry, and
//! `EffectiveSettings` is what a profile folds down to for one paste.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::defaults::{
    default_clipboard_restore_delay_ms, default_close, default_false, default_height,
    default_hold_hotkey, default_language, default_listen_tail_ms, default_local_model,
    default_max_log_mb, default_mode, default_polish_deadline_ms, default_polish_endpoint,
    default_polish_model, default_provider, default_reinsert_hold_ms, default_replacements,
    default_replacements_mode, default_spinner, default_toggle_hotkey, default_true, default_width,
};

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
    /// Which microphone to capture from.
    ///
    /// Empty (the default) follows whatever Windows has set as the default
    /// recording device. Otherwise it is matched case-insensitively against
    /// any part of a device name, so `"yeti"` is enough to pin the Yeti.
    ///
    /// A named device that is not currently present falls back to the default
    /// rather than failing: an absent microphone must never be the reason
    /// dictation stops working. Applies without a restart, and the running
    /// capture switches over within a couple of seconds.
    ///
    /// Worth knowing for remote-desktop use: an app can only record a
    /// microphone that exists on THIS machine as an audio input device. A
    /// remote client's microphone is therefore reachable only if the remote
    /// tool publishes one (Microsoft's RDP does; RustDesk and Chrome Remote
    /// Desktop do not forward the client mic at all). When one exists, name it
    /// here like any other device — there is no transport-specific handling,
    /// because there is nothing useful for it to do.
    #[serde(default)]
    pub input_device: String,
    /// When a hotkey is bound to a mouse button, whether that button ALSO
    /// still reaches the app under the cursor.
    ///
    /// Default `false` (the button is consumed): a thumb button bound to
    /// dictation that also navigated your browser back would be a bug, not a
    /// feature. Set `true` if you would rather share the button than claim it
    /// — the hotkey still fires, the click just passes through as well.
    /// Only affects mouse-bound hotkeys; keyboard hotkeys are unaffected
    /// either way. Applies after a restart.
    #[serde(default)]
    pub mouse_hotkey_passthrough: bool,
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
            input_device: String::new(),
            mouse_hotkey_passthrough: false,
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
