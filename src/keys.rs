//! In-memory pool of the user's own API keys, with per-key health tracking,
//! cooldown backoff, and round-robin selection.
//!
//! Health lives **only in RAM** (owner decision, 2026-07-04): every launch
//! starts fresh and the startup prewarm (`stt::spawn_prewarm`) re-probes the
//! keys, so a key that was rate-limited or hit a temporary outage yesterday is
//! never permanently branded dead — and there's no `key-health.json` cluttering
//! the folder. Within a run, a failed key cools down for a duration keyed to
//! *why* it failed and becomes eligible again when the cooldown lapses.

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::RwLock;

use crate::config::Config;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum KeyHealthStatus {
    /// Not probed yet this run (treated as usable; prewarm sorts it out).
    Untested,
    Alive,
    Quota,
    Dead,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum FailKind {
    Invalid,
    Exhausted,
    Transient,
    RateLimit,
}

impl FailKind {
    /// How long a key sits out after this kind of failure. Nothing is
    /// permanent — even Invalid/Exhausted keys become eligible again after the
    /// cooldown (billing gets fixed, quotas reset, outages end); they're just
    /// tried last while anything healthier exists.
    fn cooldown(self) -> Duration {
        match self {
            FailKind::Invalid | FailKind::Exhausted => Duration::from_secs(6 * 3600),
            FailKind::RateLimit => Duration::from_secs(60),
            FailKind::Transient => Duration::from_secs(30),
        }
    }

    fn status(self) -> Option<KeyHealthStatus> {
        match self {
            FailKind::Invalid => Some(KeyHealthStatus::Dead),
            FailKind::Exhausted => Some(KeyHealthStatus::Quota),
            FailKind::Transient | FailKind::RateLimit => None,
        }
    }
}

#[derive(Clone, Debug)]
struct KeyEntry {
    value: String,
    status: KeyHealthStatus,
    cooldown_until: Option<Instant>,
    last_success: Option<Instant>,
    failures: u32,
    total_audio_ms: u64,
    successful_sessions: u64,
}

struct Inner {
    provider_id: String,
    /// The most recent failure reason seen this run, so the error pip and tray
    /// tooltip can say "out of credit" or "rate limited" instead of collapsing
    /// every non-all-dead failure into a bare "!".
    last_fail: Option<FailKind>,
    keys: Vec<KeyEntry>,
    /// The key we intend to use next — either the last one that carried a real
    /// session, or the first one the prewarm probe validated. `acquire`
    /// prefers it, so a working key is always queued up and ready to go.
    last_good: Option<String>,
}

pub struct KeyPool {
    inner: RwLock<Inner>,
}

/// A log-safe positional label for a key ("#2 of 3"). Never any part of the
/// credential itself: log files get attached to bug reports.
fn position_label(keys: &[KeyEntry], key: &str) -> String {
    match keys.iter().position(|e| e.value == key) {
        Some(i) => format!("#{} of {}", i + 1, keys.len()),
        None => "#? (not in pool)".to_string(),
    }
}

fn configured_keys(cfg: &Config) -> Vec<String> {
    configured_keys_for(cfg, &cfg.stt_provider)
}

fn configured_keys_for(cfg: &Config, provider: &str) -> Vec<String> {
    // The local provider uses the same session runner but has no credential.
    // A private sentinel keeps the generic pool/startup readiness plumbing
    // usable without storing or exposing a fake key in settings.json.
    if provider.trim().eq_ignore_ascii_case("local") {
        return vec!["local".into()];
    }
    cfg.keys_for(provider)
        .iter()
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .collect()
}

impl KeyPool {
    pub fn new(cfg: &Config) -> Arc<Self> {
        Self::for_provider(cfg, &cfg.stt_provider)
    }

    /// A pool for an EXPLICIT provider rather than the globally configured
    /// one. Used when a Per-App Profile overrides `stt_provider`: the session
    /// needs that provider's keys, not the global provider's.
    pub fn for_provider(cfg: &Config, provider: &str) -> Arc<Self> {
        let keys = configured_keys_for(cfg, provider)
            .into_iter()
            .map(|value| KeyEntry {
                value,
                status: KeyHealthStatus::Untested,
                cooldown_until: None,
                last_success: None,
                failures: 0,
                total_audio_ms: 0,
                successful_sessions: 0,
            })
            .collect();
        Arc::new(Self {
            inner: RwLock::new(Inner {
                provider_id: provider.trim().to_ascii_lowercase(),
                last_fail: None,
                keys,
                last_good: None,
            }),
        })
    }

    /// The provider this pool was built for.
    pub fn provider_id(&self) -> String {
        self.inner.read().provider_id.clone()
    }

    /// A log-safe label for one key: its 1-based position in the configured
    /// list, never any part of the key itself. Diagnosing "which of my three
    /// keys failed" does not require putting a slice of the credential into a
    /// file the user may well attach to a bug report.
    pub fn label(&self, key: &str) -> String {
        position_label(&self.inner.read().keys, key)
    }

    /// Whether this pool still represents the provider and keys in the latest
    /// config. Settings are hot-swapped, so the main loop checks this before a
    /// new session and replaces the pool when the user changed credentials.
    pub fn matches_config(&self, cfg: &Config) -> bool {
        let inner = self.inner.read();
        inner.provider_id == cfg.stt_provider.trim().to_ascii_lowercase()
            && inner
                .keys
                .iter()
                .map(|entry| entry.value.as_str())
                .eq(configured_keys(cfg).iter().map(String::as_str))
    }

    /// Every key in config order — the prewarm probe walks this list.
    pub fn all_keys(&self) -> Vec<String> {
        self.inner
            .read()
            .keys
            .iter()
            .map(|e| e.value.clone())
            .collect()
    }

    /// True if at least one key is currently usable (no active cooldown).
    pub fn has_usable_key(&self) -> bool {
        self.available_key_count() > 0
    }

    /// True if the pool has keys and **every** one is currently marked
    /// [`KeyHealthStatus::Dead`] — i.e. all of the active provider's keys were
    /// rejected as invalid/unauthorized (a 401/403 this run), as opposed to a
    /// transient, rate-limit, or quota failure. Drives the pip's dead-key glyph
    /// and the "keys were rejected" tray tooltip so the error explains itself.
    /// (Status-based, unlike `has_usable_key`, which is cooldown-based — a Dead
    /// key past its cooldown is still "usable" but is still Dead here.)
    pub fn all_dead(&self) -> bool {
        let inner = self.inner.read();
        !inner.keys.is_empty() && inner.keys.iter().all(|e| e.status == KeyHealthStatus::Dead)
    }

    fn available_key_count(&self) -> usize {
        let now = Instant::now();
        self.inner
            .read()
            .keys
            .iter()
            .filter(|e| e.cooldown_until.map(|cd| cd <= now).unwrap_or(true))
            .count()
    }

    /// Snapshot the best usable key. Preference order:
    ///   1. the queued last-known-good key, if not cooling down
    ///   2. any key with a prior success this run
    ///   3. most audio served / fewest recent failures
    ///
    /// Keys are gated by their cooldown, not their status — a Dead/Quota key
    /// becomes eligible again once its (long) cooldown lapses, so nothing is
    /// ever permanently blacklisted. Returns None if every key is cooling down
    /// or the pool is empty.
    pub fn acquire(&self) -> Option<String> {
        let now = Instant::now();
        let inner = self.inner.read();
        let mut best: Option<(&KeyEntry, u32)> = None;
        for entry in &inner.keys {
            if let Some(cd) = entry.cooldown_until {
                if cd > now {
                    continue;
                }
            }
            let mut score: u32 = 0;
            if inner.last_good.as_deref() == Some(entry.value.as_str()) {
                score |= 1 << 31;
            }
            if entry.last_success.is_some() {
                score |= 1 << 29;
            }
            // A probed-dead key that outlived its cooldown is a last resort:
            // eligible, but never preferred over an untested/alive key.
            if matches!(entry.status, KeyHealthStatus::Dead | KeyHealthStatus::Quota) {
                score = score.saturating_sub(1 << 30);
            }
            score = score.saturating_add((entry.total_audio_ms / 60_000).min(100) as u32);
            score = score.saturating_add(100u32.saturating_sub(entry.failures));
            match best {
                None => best = Some((entry, score)),
                Some((_, s)) if score > s => best = Some((entry, score)),
                _ => {}
            }
        }
        best.map(|(e, _)| e.value.clone())
    }

    pub fn mark_success(&self, key: &str, audio_ms: u64) {
        let now = Instant::now();
        let mut inner = self.inner.write();
        let label = position_label(&inner.keys, key);
        let mut totals = None;
        if let Some(e) = inner.keys.iter_mut().find(|e| e.value == key) {
            e.status = KeyHealthStatus::Alive;
            e.last_success = Some(now);
            e.failures = 0;
            e.cooldown_until = None;
            e.total_audio_ms = e.total_audio_ms.saturating_add(audio_ms);
            e.successful_sessions = e.successful_sessions.saturating_add(1);
            totals = Some((e.total_audio_ms, e.successful_sessions));
        }
        inner.last_good = Some(key.to_string());
        inner.last_fail = None;
        if let Some((total, sessions)) = totals {
            tracing::info!(
                "key {} alive: +{:.1}s audio this session, {:.1} min total across {sessions} session(s) this run",
                label,
                audio_ms as f64 / 1000.0,
                total as f64 / 60_000.0,
            );
        }
    }

    /// Prewarm verdict: the key answered a probe. Marks it Alive and, if
    /// nothing is queued yet, queues it — the first validated key is the one
    /// that's "ready to go" when the user first presses the hotkey.
    pub fn mark_alive_probe(&self, key: &str) {
        let now = Instant::now();
        let mut inner = self.inner.write();
        if let Some(e) = inner.keys.iter_mut().find(|e| e.value == key) {
            e.status = KeyHealthStatus::Alive;
            e.last_success = Some(now);
            e.failures = 0;
            e.cooldown_until = None;
        }
        if inner.last_good.is_none() {
            inner.last_good = Some(key.to_string());
            let label = position_label(&inner.keys, key);
            tracing::info!("key {label} queued as the ready key");
        }
    }

    /// The most recent failure reason recorded this run, if any. Cleared by a
    /// success so a recovered provider stops reporting a stale cause.
    pub fn last_failure(&self) -> Option<FailKind> {
        self.inner.read().last_fail
    }

    pub fn mark_failed(&self, key: &str, kind: FailKind) {
        let cd = kind.cooldown();
        let now = Instant::now();
        let mut inner = self.inner.write();
        inner.last_fail = Some(kind);
        let label = position_label(&inner.keys, key);
        if let Some(e) = inner.keys.iter_mut().find(|e| e.value == key) {
            e.failures = e.failures.saturating_add(1);
            e.cooldown_until = Some(now + cd);
            if let Some(status) = kind.status() {
                e.status = status;
            }
            tracing::warn!(
                "key {} {:?}: cooling down for {:?} (status {:?}, {} failure(s) this run)",
                label,
                kind,
                cd,
                e.status,
                e.failures
            );
        }
        if inner.last_good.as_deref() == Some(key) {
            inner.last_good = None;
        }
    }

    /// One-line health summary for the log (prewarm prints this when done).
    pub fn summary(&self) -> String {
        let inner = self.inner.read();
        inner
            .keys
            .iter()
            .enumerate()
            .map(|(i, e)| format!("#{} {:?}", i + 1, e.status))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Local-only pool: there is no remote key source, so this cannot conjure
    /// new keys. Kept (async, same signature) so the session retry loop
    /// compiles unchanged.
    pub async fn schedule_refresh_and_wait(&self, _timeout: Duration) -> bool {
        false
    }

    /// Whether the pool currently has at least one usable key. Resolves
    /// immediately (no remote source to wait on).
    pub async fn wait_until_ready(&self, _timeout: Duration) -> bool {
        self.has_usable_key()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool_with(keys: &[&str]) -> Arc<KeyPool> {
        let cfg = Config {
            elevenlabs_keys: keys.iter().map(|k| k.to_string()).collect(),
            ..Config::default()
        };
        KeyPool::new(&cfg)
    }

    #[test]
    fn starts_untested_and_usable() {
        let p = pool_with(&["key_aaaaaa", "key_bbbbbb"]);
        assert!(p.has_usable_key());
        assert!(p.acquire().is_some());
        assert_eq!(p.all_keys().len(), 2);
    }

    #[test]
    fn failed_key_rotates_to_next() {
        let p = pool_with(&["key_aaaaaa", "key_bbbbbb"]);
        let first = p.acquire().unwrap();
        p.mark_failed(&first, FailKind::Exhausted);
        let second = p.acquire().unwrap();
        assert_ne!(first, second, "cooling-down key must not be re-acquired");
    }

    #[test]
    fn probe_queues_first_alive_key() {
        let p = pool_with(&["key_aaaaaa", "key_bbbbbb", "key_cccccc"]);
        p.mark_failed("key_aaaaaa", FailKind::Exhausted);
        p.mark_alive_probe("key_bbbbbb");
        p.mark_alive_probe("key_cccccc"); // second alive must NOT steal the queue
        assert_eq!(p.acquire().as_deref(), Some("key_bbbbbb"));
    }

    #[test]
    fn success_promotes_to_queued() {
        let p = pool_with(&["key_aaaaaa", "key_bbbbbb"]);
        p.mark_success("key_bbbbbb", 5_000);
        assert_eq!(p.acquire().as_deref(), Some("key_bbbbbb"));
    }

    #[test]
    fn all_keys_cooling_means_no_usable_key() {
        let p = pool_with(&["key_aaaaaa"]);
        p.mark_failed("key_aaaaaa", FailKind::RateLimit);
        assert!(!p.has_usable_key());
        assert!(p.acquire().is_none());
    }

    #[test]
    fn empty_pool_is_unusable() {
        let p = pool_with(&[]);
        assert!(!p.has_usable_key());
        assert!(p.acquire().is_none());
    }

    #[test]
    fn local_provider_uses_an_internal_keyless_sentinel() {
        let cfg = Config {
            stt_provider: "local".into(),
            ..Config::default()
        };
        let pool = KeyPool::new(&cfg);
        assert!(pool.has_usable_key());
        assert_eq!(pool.acquire().as_deref(), Some("local"));
    }

    #[test]
    fn pool_detects_provider_and_key_config_changes() {
        let mut cfg = Config {
            stt_provider: "deepgram".into(),
            deepgram_keys: vec![" one ".into(), "two".into()],
            ..Config::default()
        };
        let pool = KeyPool::new(&cfg);
        assert!(pool.matches_config(&cfg));

        cfg.deepgram_keys.push("three".into());
        assert!(!pool.matches_config(&cfg));
        cfg.deepgram_keys.pop();
        cfg.stt_provider = "openai".into();
        cfg.openai_keys = vec!["one".into(), "two".into()];
        assert!(!pool.matches_config(&cfg));
    }
}
