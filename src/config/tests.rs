//! Tests for settings.json's schema, defaults, and load/save behaviour.

use std::collections::BTreeMap;

use super::defaults::default_replacements_mode;
use super::{Config, Profile};

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
