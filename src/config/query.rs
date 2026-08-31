//! Answering questions about a loaded `Config`.
//!
//! Read-only accessors: which keys are live, which provider to use, and what a
//! Per-App Profile folds the globals down to for one window.

use std::collections::BTreeMap;

use super::{Config, EffectiveSettings, Profile};

impl Config {
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
