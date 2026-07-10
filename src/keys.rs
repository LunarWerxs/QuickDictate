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
#[allow(dead_code)] // LowBalance is reserved for a future live-balance probe.
pub enum FailKind {
    Invalid,
    Exhausted,
    LowBalance,
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
            FailKind::LowBalance => Duration::from_secs(15 * 60),
            FailKind::RateLimit => Duration::from_secs(60),
            FailKind::Transient => Duration::from_secs(30),
        }
    }

    fn status(self) -> Option<KeyHealthStatus> {
        match self {
            FailKind::Invalid => Some(KeyHealthStatus::Dead),
            FailKind::Exhausted | FailKind::LowBalance => Some(KeyHealthStatus::Quota),
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
    keys: Vec<KeyEntry>,
    /// The key we intend to use next — either the last one that carried a real
    /// session, or the first one the prewarm probe validated. `acquire`
    /// prefers it, so a working key is always queued up and ready to go.
    last_good: Option<String>,
}

pub struct KeyPool {
    inner: RwLock<Inner>,
}

fn key_suffix(key: &str) -> String {
    key.chars()
        .rev()
        .take(6)
        .collect::<String>()
        .chars()
        .rev()
        .collect()
}

impl KeyPool {
    pub fn new(cfg: &Config) -> Arc<Self> {
        let keys = cfg
            .active_keys()
            .iter()
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
            .map(|v| KeyEntry {
                value: v.to_string(),
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
                keys,
                last_good: None,
            }),
        })
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
        if let Some((total, sessions)) = totals {
            tracing::info!(
                "key ...{} alive: +{:.1}s audio this session, {:.1} min total across {sessions} session(s) this run",
                key_suffix(key),
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
            tracing::info!("key ...{} queued as the ready key", key_suffix(key));
        }
    }

    pub fn mark_failed(&self, key: &str, kind: FailKind) {
        let cd = kind.cooldown();
        let now = Instant::now();
        let mut inner = self.inner.write();
        if let Some(e) = inner.keys.iter_mut().find(|e| e.value == key) {
            e.failures = e.failures.saturating_add(1);
            e.cooldown_until = Some(now + cd);
            if let Some(status) = kind.status() {
                e.status = status;
            }
            tracing::warn!(
                "key ...{} {:?}: cooling down for {:?} (status {:?}, {} failure(s) this run)",
                key_suffix(key),
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
            .map(|e| format!("...{} {:?}", key_suffix(&e.value), e.status))
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
}
