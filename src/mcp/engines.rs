//! Which speech engines a `transcribe_file` call can use, and which one it gets
//! when the caller names none. Pure, so the choice is tested without a model.

/// Local models in preference order: the first installed one is the default
/// local choice.
pub const LOCAL_MODELS: [&str; 3] = ["parakeet-v3-q5", "cohere-q5", "whisper-turbo-q5"];

/// Cloud providers with a REST batch path. The others are realtime-only.
pub const CLOUD_PROVIDERS: [&str; 3] = ["openai", "deepgram", "elevenlabs"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Engine {
    Local(String),
    Cloud(String),
}

impl Engine {
    pub fn id(&self) -> &str {
        match self {
            Engine::Local(id) | Engine::Cloud(id) => id,
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Usable {
    pub local: Vec<String>,
    pub cloud: Vec<String>,
}

pub fn choose(
    requested: Option<&str>,
    usable: &Usable,
    configured: Option<&str>,
) -> Result<Engine, String> {
    match requested.map(str::trim).filter(|r| !r.is_empty()) {
        Some(name) => choose_named(name, usable),
        None => choose_default(usable, configured),
    }
}

fn choose_named(name: &str, usable: &Usable) -> Result<Engine, String> {
    if name.eq_ignore_ascii_case("local") {
        return first_local(usable).ok_or_else(no_local_message);
    }
    if usable.local.iter().any(|m| m == name) {
        return Ok(Engine::Local(name.to_string()));
    }
    if usable.cloud.iter().any(|c| c == name) {
        return Ok(Engine::Cloud(name.to_string()));
    }
    if LOCAL_MODELS.contains(&name) {
        return Err(format!(
            "local model '{name}' is not installed; install it in QuickDictate Settings"
        ));
    }
    if CLOUD_PROVIDERS.contains(&name) {
        return Err(format!(
            "'{name}' has no API key in QuickDictate settings; add one in Settings"
        ));
    }
    Err(format!(
        "unknown engine '{name}'; call list_engines for the names that work here"
    ))
}

fn choose_default(usable: &Usable, configured: Option<&str>) -> Result<Engine, String> {
    if usable.local.iter().any(|m| m == LOCAL_MODELS[0]) {
        return Ok(Engine::Local(LOCAL_MODELS[0].to_string()));
    }
    if let Some(provider) = configured {
        if usable.cloud.iter().any(|c| c == provider) {
            return Ok(Engine::Cloud(provider.to_string()));
        }
    }
    if let Some(cloud) = usable.cloud.first() {
        return Ok(Engine::Cloud(cloud.clone()));
    }
    first_local(usable).ok_or_else(no_local_message)
}

fn first_local(usable: &Usable) -> Option<Engine> {
    LOCAL_MODELS
        .iter()
        .find(|m| usable.local.iter().any(|u| u == *m))
        .map(|m| Engine::Local((*m).to_string()))
}

fn no_local_message() -> String {
    "no speech engine is usable: install a local model in QuickDictate Settings, or add an API key"
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usable(local: &[&str], cloud: &[&str]) -> Usable {
        Usable {
            local: local.iter().map(|s| s.to_string()).collect(),
            cloud: cloud.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn default_prefers_installed_parakeet_over_cloud() {
        let u = usable(&["parakeet-v3-q5"], &["openai"]);
        assert_eq!(
            choose(None, &u, Some("openai")),
            Ok(Engine::Local("parakeet-v3-q5".into()))
        );
    }

    #[test]
    fn default_falls_back_to_configured_provider_without_parakeet() {
        let u = usable(&["cohere-q5"], &["openai", "deepgram"]);
        assert_eq!(
            choose(None, &u, Some("deepgram")),
            Ok(Engine::Cloud("deepgram".into()))
        );
    }

    #[test]
    fn default_uses_any_keyed_provider_when_configured_one_has_no_batch_path() {
        let u = usable(&[], &["elevenlabs"]);
        assert_eq!(
            choose(None, &u, Some("assemblyai")),
            Ok(Engine::Cloud("elevenlabs".into()))
        );
    }

    #[test]
    fn default_with_nothing_usable_says_what_to_do() {
        let err = choose(None, &usable(&[], &[]), None)
            .err()
            .unwrap_or_default();
        assert!(err.contains("install a local model"), "{err}");
    }

    #[test]
    fn named_uninstalled_model_is_an_error_not_a_silent_fallback() {
        let u = usable(&["cohere-q5"], &["openai"]);
        let err = choose(Some("parakeet-v3-q5"), &u, None)
            .err()
            .unwrap_or_default();
        assert!(err.contains("not installed"), "{err}");
    }

    #[test]
    fn named_provider_without_key_is_an_error() {
        let err = choose(Some("deepgram"), &usable(&[], &[]), None)
            .err()
            .unwrap_or_default();
        assert!(err.contains("no API key"), "{err}");
    }

    #[test]
    fn named_local_takes_the_best_installed_model() {
        let u = usable(&["whisper-turbo-q5", "cohere-q5"], &[]);
        assert_eq!(
            choose(Some("local"), &u, None),
            Ok(Engine::Local("cohere-q5".into()))
        );
    }

    #[test]
    fn unknown_name_points_at_list_engines() {
        let err = choose(Some("nope"), &usable(&[], &[]), None)
            .err()
            .unwrap_or_default();
        assert!(err.contains("list_engines"), "{err}");
    }
}
