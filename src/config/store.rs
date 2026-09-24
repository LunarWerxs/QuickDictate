//! Finding, reading and writing settings.json.
//!
//! The search order for the file, the load path (including the parse-failure
//! backup and the key unsealing), the atomic write, and the two migrations a
//! load applies: unreadable sealed keys and a local model that no longer ships.

use std::fs;
use std::path::{Path, PathBuf};

use super::defaults::default_local_model;
use super::{Config, EXAMPLE_JSON};

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
        if let Some(path) = Self::env_settings_path().filter(|p| p.exists()) {
            return path;
        }

        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf));

        if let Some(path) = exe_dir.as_deref().and_then(Self::exe_or_ancestor_settings) {
            return path;
        }

        // The well-known off-exe location. Only adopted when the file is really
        // there, so this can never redirect an install that has its own copy.
        if let Some(path) = crate::paths::app_data_dir()
            .map(|d| d.join("settings.json"))
            .filter(|p| p.exists())
        {
            return path;
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

    /// An existing settings.json next to the exe, or in one of the five
    /// folders above it that [`Config::ancestor_settings_allowed`] accepts.
    fn exe_or_ancestor_settings(dir: &Path) -> Option<PathBuf> {
        let direct = dir.join("settings.json");
        if direct.exists() {
            return Some(direct);
        }
        // Walk up looking for a settings.json. This exists purely so a dev
        // run from target/{profile}/ (or .../deps/) picks up the project
        // root's settings.json. It must NOT happen in a shipped build: a
        // portable exe dropped in, say, Downloads\QuickDictate\ would
        // otherwise adopt an unrelated settings.json sitting in the user's
        // profile folder and then overwrite it wholesale on the next Save.
        // Debug builds walk freely; release builds only accept an ancestor
        // that is explicitly marked as a QuickDictate working tree.
        dir.ancestors()
            .skip(1)
            .take(5)
            .find(|parent| {
                parent.join("settings.json").exists() && Self::ancestor_settings_allowed(parent)
            })
            .map(|parent| parent.join("settings.json"))
    }

    /// Load settings.json, generating one with defaults if no file is found.
    /// We don't use `tracing` here because logging may not be initialized yet
    /// (logging is now configured *from* the loaded settings). Instead we
    /// return a list of diagnostic messages the caller replays via tracing
    /// after init_logging has run.
    pub fn load_or_create() -> (Self, Vec<String>) {
        let path = Self::settings_path();
        if path.exists() {
            Self::load_existing(&path)
        } else {
            Self::create_from_template(path)
        }
    }

    /// The load half of [`Config::load_or_create`]: `path` exists.
    fn load_existing(path: &Path) -> (Self, Vec<String>) {
        let data = match fs::read_to_string(path) {
            Ok(data) => data,
            // Not valid UTF-8 (Notepad's "UTF-16 LE" or "ANSI" with an accented
            // character) or held by another process. Same treatment as a parse
            // failure: the file is the user's only copy of their keys, and the
            // defaults about to be used must not be mistaken for it.
            Err(e) => {
                let alert =
                    back_up_unloadable(path, &format!("failed to read {}: {e}", path.display()));
                return (Config::default(), vec![alert]);
            }
        };
        match parse_settings(&data) {
            Ok(mut c) => {
                let mut diags = vec![format!("INFO: Loaded settings from {}", path.display())];
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
            // Preserve the unparseable file instead of silently discarding the
            // user's keys/prefs on a hand-edit typo or truncated write: copy it
            // aside so it can be recovered or hand-fixed, and report it loudly.
            Err(e) => {
                let alert =
                    back_up_unloadable(path, &format!("failed to parse {}: {e}", path.display()));
                (Config::default(), vec![alert])
            }
        }
    }

    /// The create half of [`Config::load_or_create`]: nothing at `path` yet.
    fn create_from_template(path: PathBuf) -> (Self, Vec<String>) {
        let mut diags: Vec<String> = Vec::new();
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

    /// Every API-key array, mutably, in canonical order: the per-provider ones
    /// plus the LLM cleanup endpoint's. One place to add a key array so the
    /// seal/unseal passes can never miss one.
    fn key_arrays_mut(&mut self) -> Vec<&mut Vec<String>> {
        vec![
            &mut self.elevenlabs_keys,
            &mut self.deepgram_keys,
            &mut self.openai_keys,
            &mut self.assemblyai_keys,
            &mut self.dashscope_keys,
            &mut self.google_keys,
            &mut self.local_keys,
            &mut self.polish_keys,
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
    /// version) fall back to a full [`Config::save`] of what is ON DISK plus
    /// the id, never of `self`: when startup could not load the file, `self`
    /// is `Config::default()`, and saving it would replace the user's keys
    /// with defaults. For the same reason a file that cannot be read or
    /// parsed, or already holds an id, is left alone and reported as an error.
    pub fn save_install_id(&self, path: &Path) -> anyhow::Result<()> {
        const EMPTY_SLOT: &str = "\"install_id\": \"\"";
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            // Nothing on disk to preserve: a plain save creates the file.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return self.save(path),
            Err(e) => anyhow::bail!("could not read {}: {e}", path.display()),
        };
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
        let mut on_disk = parse_settings(&text).map_err(|e| {
            anyhow::anyhow!("{} does not parse ({e}); leaving it alone", path.display())
        })?;
        if !on_disk.install_id.trim().is_empty() {
            anyhow::bail!(
                "{} already has an install id this run did not load; leaving it alone",
                path.display()
            );
        }
        on_disk.install_id.clone_from(&self.install_id);
        on_disk.normalize_local_model();
        on_disk.save(path)
    }
}

/// Parse settings.json text. A leading UTF-8 byte-order mark is skipped:
/// Notepad writes one under "UTF-8 with BOM", and serde_json rejects it, which
/// would otherwise turn a routine hand edit into "failed to parse".
fn parse_settings(text: &str) -> serde_json::Result<Config> {
    serde_json::from_str(text.strip_prefix('\u{feff}').unwrap_or(text))
}

/// Copy a settings.json that exists but could not be loaded to
/// `settings.json.bad`, and return the ALERT naming `failure` and the backup.
/// The copy is what lets the user recover their keys after the defaults the
/// app falls back to get saved over the original.
fn back_up_unloadable(path: &Path, failure: &str) -> String {
    let bad = path.with_extension("json.bad");
    match fs::copy(path, &bad) {
        Ok(_) => format!(
            "ALERT: {failure}. Backed up the original to {} and started from defaults — restore or fix it to recover your settings.",
            bad.display()
        ),
        Err(copy_err) => {
            format!("ALERT: {failure}. Using defaults. (Could not back up the original: {copy_err})")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let original =
            "{\n  \"mode\": \"toggle\",\n\n  \"install_id\": \"\",\n  \"update_auto_check\": true\n}\n";
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

    fn temp_settings(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "qd-store-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.join("settings.json")
    }

    #[test]
    fn save_install_id_fallback_keeps_the_files_settings_not_the_callers() {
        // Startup could not load this file, so the caller holds defaults. The
        // fallback must add the id to what is on disk, keys included.
        let path = temp_settings("keep-disk");
        fs::write(
            &path,
            r#"{ "mode": "hold", "elevenlabs_keys": ["el-key"] }"#,
        )
        .unwrap();

        let defaults = Config {
            install_id: "11111111-2222-4333-8444-555555555555".into(),
            ..Config::default()
        };
        defaults.save_install_id(&path).unwrap();

        let reloaded: Config = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(reloaded.install_id, defaults.install_id);
        assert_eq!(reloaded.mode, "hold");
        assert_eq!(reloaded.elevenlabs_keys, vec!["el-key".to_string()]);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn save_install_id_leaves_an_unparseable_or_already_stamped_file_alone() {
        let path = temp_settings("leave-alone");
        let c = Config {
            install_id: "11111111-2222-4333-8444-555555555555".into(),
            ..Config::default()
        };

        let typo = "{ \"mode\": \"hold\", \"elevenlabs_keys\": [\"el-key\"] ";
        fs::write(&path, typo).unwrap();
        assert!(c.save_install_id(&path).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), typo);

        // A real id means this run's config did not come from this file.
        let stamped = r#"{ "install_id": "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee" }"#;
        fs::write(&path, stamped).unwrap();
        assert!(c.save_install_id(&path).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), stamped);

        // Not UTF-8 at all (UTF-16 LE from Notepad): unreadable, untouched.
        let utf16: Vec<u8> = "{ \"mode\": \"höld\" }"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        fs::write(&path, &utf16).unwrap();
        assert!(c.save_install_id(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), utf16);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn an_unreadable_settings_file_is_backed_up_and_alerted() {
        let path = temp_settings("unreadable");
        let utf16: Vec<u8> = "{ \"mode\": \"höld\" }"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        fs::write(&path, &utf16).unwrap();

        let (cfg, diags) = Config::load_existing(&path);
        assert!(cfg.install_id.is_empty(), "fell back to defaults");
        assert_eq!(diags.len(), 1);
        assert!(diags[0].starts_with("ALERT: failed to read"), "{diags:?}");
        assert_eq!(fs::read(path.with_extension("json.bad")).unwrap(), utf16);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_utf8_bom_does_not_break_loading() {
        let path = temp_settings("bom");
        fs::write(&path, "\u{feff}{ \"mode\": \"hold\" }").unwrap();
        let (cfg, diags) = Config::load_existing(&path);
        assert_eq!(cfg.mode, "hold");
        assert!(diags[0].starts_with("INFO:"), "{diags:?}");
        assert!(!path.with_extension("json.bad").exists());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn polish_keys_are_sealed_at_rest_like_the_provider_keys() {
        let path = temp_settings("polish-seal");
        let cfg = Config {
            protect_keys_at_rest: true,
            polish_keys: vec!["gem-secret".into()],
            ..Config::default()
        };
        cfg.save(&path).unwrap();
        let on_disk = fs::read_to_string(&path).unwrap();
        assert!(
            !on_disk.contains("gem-secret"),
            "polish key written in plaintext"
        );

        let mut loaded: Config = serde_json::from_str(&on_disk).unwrap();
        assert!(loaded.unseal_keys().is_empty());
        assert_eq!(loaded.polish_keys, cfg.polish_keys);
        let _ = fs::remove_dir_all(path.parent().unwrap());
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
}
