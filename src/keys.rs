//! In-memory pool of the user's own API keys, with per-key health tracking,
//! cooldown backoff, and best-first selection.
//!
//! Health lives **only in RAM** (owner decision, 2026-07-04): every launch
//! starts fresh and the startup prewarm (`stt::spawn_prewarm`) re-probes the
//! keys, so a key that was rate-limited or hit a temporary outage yesterday is
//! never permanently branded dead — and there's no `key-health.json` cluttering
//! the folder. Within a run, a failed key cools down for a duration keyed to
//! *why* it failed and becomes eligible again when the cooldown lapses.
//!
//! Two rules shape selection, both learned from a real log (2026-09-10, one
//! good key and one dead one): a single transient failure must never lock the
//! user out of their only working key, and one press must never try the same
//! key twice. So a transient failure benches a key for seconds, not half a
//! minute; [`KeyPool::acquire`] falls back to a merely-cooling key rather than
//! returning nothing while a healthy key exists; and the session runner passes
//! the keys it has already tried this press ([`KeyPool::acquire_excluding`])
//! so rotation always moves forward instead of circling.

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
    ///
    /// Transient is deliberately tiny. It covers a stalled handshake, a
    /// dropped socket, a 5xx: things that say nothing about the credential.
    /// The old 30 s here benched a key with 160 good sessions behind it over
    /// one timeout, and with the other key dead every press for the next
    /// half-minute failed. Two seconds is enough to keep a press from
    /// hammering the same key in a tight loop, and no more.
    fn cooldown(self) -> Duration {
        match self {
            FailKind::Invalid | FailKind::Exhausted => Duration::from_secs(6 * 3600),
            FailKind::RateLimit => Duration::from_secs(60),
            FailKind::Transient => Duration::from_secs(2),
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

impl KeyEntry {
    /// Still inside a cooldown at `now`.
    fn benched(&self, now: Instant) -> bool {
        self.cooldown_until.is_some_and(|cd| cd > now)
    }

    /// The provider said no to this credential itself (invalid, or out of
    /// credit), as opposed to a transient or rate-limit failure.
    fn rejected(&self) -> bool {
        matches!(self.status, KeyHealthStatus::Dead | KeyHealthStatus::Quota)
    }

    /// How much [`KeyPool::acquire_excluding`] wants this key at `now`;
    /// higher is tried first, `None` is not eligible at all. `queued` is
    /// whether this is the pool's last-known-good key. The tiers, highest
    /// bit first: free of any cooldown, not rejected by the provider, queued,
    /// succeeded this run, then most audio served and fewest failures.
    fn rank(&self, now: Instant, queued: bool) -> Option<u64> {
        let benched = self.benched(now);
        // The provider said no and nothing has changed since: out until the
        // (long) cooldown lapses.
        if benched && self.rejected() {
            return None;
        }
        let mut score: u64 = 0;
        if !benched {
            score |= 1 << 40;
        }
        if !self.rejected() {
            score |= 1 << 35;
        }
        if queued {
            score |= 1 << 31;
        }
        if self.last_success.is_some() {
            score |= 1 << 29;
        }
        score += (self.total_audio_ms / 60_000).min(100);
        score += 100u64.saturating_sub(u64::from(self.failures));
        Some(score)
    }
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

    /// Whether this pool still represents the globally selected provider and
    /// its keys in the latest config. Settings are hot-swapped, so the main
    /// loop checks this before a new session and replaces the pool when the
    /// user changed credentials.
    pub fn matches_config(&self, cfg: &Config) -> bool {
        self.matches_provider(cfg, &cfg.stt_provider)
    }

    /// Whether this pool still represents `provider` and that provider's keys
    /// in `cfg`. The per-provider form of [`KeyPool::matches_config`], for the
    /// pools a Per-App Profile's provider override runs on.
    pub fn matches_provider(&self, cfg: &Config, provider: &str) -> bool {
        let inner = self.inner.read();
        inner.provider_id == provider.trim().to_ascii_lowercase()
            && inner
                .keys
                .iter()
                .map(|entry| entry.value.as_str())
                .eq(configured_keys_for(cfg, provider)
                    .iter()
                    .map(String::as_str))
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

    /// True if a press right now would have a key to try: at least one key is
    /// configured and not sitting out a rejection. (A key that is merely
    /// cooling after a transient or rate-limit failure still counts — see
    /// [`KeyPool::acquire`].)
    pub fn has_usable_key(&self) -> bool {
        self.acquire().is_some()
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

    /// Snapshot the best key to try next. See [`KeyPool::acquire_excluding`].
    pub fn acquire(&self) -> Option<String> {
        self.acquire_excluding(&[])
    }

    /// Snapshot the best key to try next, never one of `exclude` (the keys
    /// this press has already tried, so rotation moves forward). Preference,
    /// highest first:
    ///
    ///   1. keys with no active cooldown, over keys still cooling from a
    ///      transient or rate-limit failure — which are still offered, because
    ///      the only alternative to trying a briefly-benched good key is
    ///      failing the press outright
    ///   2. keys the provider has not rejected, over a Dead/Quota key whose
    ///      long cooldown has lapsed (eligible again, but a last resort)
    ///   3. the queued last-known-good key, then any key with a success this
    ///      run, then most audio served / fewest failures
    ///
    /// Returns None only when there is genuinely nothing to try: the pool is
    /// empty, every key is in `exclude`, or every remaining key was rejected
    /// by the provider and is still inside its cooldown.
    pub fn acquire_excluding(&self, exclude: &[String]) -> Option<String> {
        let now = Instant::now();
        let inner = self.inner.read();
        inner
            .keys
            .iter()
            .filter(|entry| !exclude.iter().any(|k| k == &entry.value))
            .filter_map(|entry| {
                let queued = inner.last_good.as_deref() == Some(entry.value.as_str());
                entry.rank(now, queued).map(|rank| (entry, rank))
            })
            // `min_by_key` keeps the FIRST of equal ranks, so ties fall back
            // to config order; `max_by_key` would keep the last.
            .min_by_key(|(_, rank)| std::cmp::Reverse(*rank))
            .map(|(entry, _)| entry.value.clone())
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

    /// One-line health summary for the log (prewarm prints this when done;
    /// a press that found nothing to try prints it too).
    pub fn summary(&self) -> String {
        let inner = self.inner.read();
        if inner.keys.is_empty() {
            return format!("no {} keys configured", inner.provider_id);
        }
        inner
            .keys
            .iter()
            .enumerate()
            .map(|(i, e)| format!("#{} {:?}", i + 1, e.status))
            .collect::<Vec<_>>()
            .join(", ")
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
        assert_ne!(
            first, second,
            "a rejected, cooling key must not be re-acquired"
        );
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
    fn a_transient_failure_never_locks_out_the_only_key() {
        // The 2026-09-10 log: one good key, one dead key. A single connect
        // timeout on the good key benched it, and every press for the next
        // half-minute failed with "no API key available".
        let p = pool_with(&["key_dead00", "key_good00"]);
        p.mark_failed("key_dead00", FailKind::Exhausted);
        p.mark_success("key_good00", 60_000);
        p.mark_failed("key_good00", FailKind::Transient);
        assert!(p.has_usable_key());
        assert_eq!(
            p.acquire().as_deref(),
            Some("key_good00"),
            "a briefly-cooling good key is still offered; the dead one is not"
        );
    }

    #[test]
    fn a_rate_limited_only_key_is_still_offered() {
        let p = pool_with(&["key_aaaaaa"]);
        p.mark_failed("key_aaaaaa", FailKind::RateLimit);
        assert!(p.has_usable_key());
        assert_eq!(p.acquire().as_deref(), Some("key_aaaaaa"));
    }

    #[test]
    fn a_free_key_beats_a_cooling_one() {
        let p = pool_with(&["key_aaaaaa", "key_bbbbbb"]);
        // Even the queued last-good key drops behind a free one while it cools.
        p.mark_success("key_aaaaaa", 600_000);
        p.mark_failed("key_aaaaaa", FailKind::RateLimit);
        assert_eq!(p.acquire().as_deref(), Some("key_bbbbbb"));
    }

    #[test]
    fn rejected_keys_stay_benched_even_when_nothing_else_is_free() {
        let p = pool_with(&["key_aaaaaa"]);
        p.mark_failed("key_aaaaaa", FailKind::Invalid);
        assert!(!p.has_usable_key());
        assert!(p.acquire().is_none());
    }

    #[test]
    fn a_press_never_retries_a_key_it_already_tried() {
        let p = pool_with(&["key_aaaaaa", "key_bbbbbb"]);
        let first = p.acquire_excluding(&[]).unwrap();
        let second = p.acquire_excluding(std::slice::from_ref(&first)).unwrap();
        assert_ne!(first, second);
        assert!(p.acquire_excluding(&[first, second]).is_none());
    }

    #[test]
    fn empty_pool_is_unusable() {
        let p = pool_with(&[]);
        assert!(!p.has_usable_key());
        assert!(p.acquire().is_none());
        assert_eq!(p.summary(), "no elevenlabs keys configured");
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

    #[test]
    fn a_profile_pool_tracks_its_own_providers_keys() {
        let mut cfg = Config {
            stt_provider: "elevenlabs".into(),
            elevenlabs_keys: vec!["el_one".into()],
            deepgram_keys: vec!["dg_one".into()],
            ..Config::default()
        };
        let pool = KeyPool::for_provider(&cfg, "Deepgram");
        assert_eq!(pool.provider_id(), "deepgram");
        assert!(pool.matches_provider(&cfg, "deepgram"));
        // The global provider's keys changing is not this pool's business...
        cfg.elevenlabs_keys.push("el_two".into());
        assert!(pool.matches_provider(&cfg, "deepgram"));
        // ...its own provider's keys changing is.
        cfg.deepgram_keys.push("dg_two".into());
        assert!(!pool.matches_provider(&cfg, "deepgram"));
    }
}
