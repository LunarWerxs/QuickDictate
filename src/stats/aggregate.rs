//! The counters a total is built from: one provider's tally, one period's
//! totals, and one device's history bucketed by hour.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::*;

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct ProviderStats {
    #[serde(rename = "w", alias = "words")]
    pub words: u64,
    #[serde(rename = "a", alias = "audio_ms")]
    pub audio_ms: u64,
    #[serde(rename = "d", alias = "dictations")]
    pub dictations: u64,
}

/// How two copies of a counter combine: summed across devices, or the
/// larger kept when merging two replicas of the same device's monotonic
/// counters.
type Combine = fn(u64, u64) -> u64;

impl ProviderStats {
    pub(super) fn add_assign(&mut self, other: &Self) {
        self.combine(other, u64::saturating_add);
    }

    fn combine(&mut self, other: &Self, op: Combine) {
        self.words = op(self.words, other.words);
        self.audio_ms = op(self.audio_ms, other.audio_ms);
        self.dictations = op(self.dictations, other.dictations);
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct PeriodStats {
    #[serde(rename = "w", alias = "words")]
    pub words: u64,
    #[serde(rename = "a", alias = "audio_ms")]
    pub audio_ms: u64,
    #[serde(rename = "d", alias = "dictations")]
    pub dictations: u64,
    #[serde(rename = "lw", alias = "longest_dictation_words")]
    pub longest_dictation_words: u64,
    #[serde(rename = "la", alias = "longest_dictation_audio_ms")]
    pub longest_dictation_audio_ms: u64,
    #[serde(rename = "p", alias = "providers")]
    pub providers: BTreeMap<String, ProviderStats>,
}

impl PeriodStats {
    pub(super) fn record(&mut self, provider: &str, words: u64, audio_ms: u64) {
        self.words = self.words.saturating_add(words);
        self.audio_ms = self.audio_ms.saturating_add(audio_ms);
        self.dictations = self.dictations.saturating_add(1);
        self.longest_dictation_words = self.longest_dictation_words.max(words);
        self.longest_dictation_audio_ms = self.longest_dictation_audio_ms.max(audio_ms);
        self.providers
            .entry(provider.to_string())
            .or_default()
            .add_assign(&ProviderStats {
                words,
                audio_ms,
                dictations: 1,
            });
    }

    pub(super) fn add_assign(&mut self, other: &Self) {
        self.combine(other, u64::saturating_add);
    }

    pub(super) fn merge_monotonic(&mut self, other: &Self) {
        self.combine(other, u64::max);
    }

    /// The one field walk both merges share, so a counter added later is
    /// never summed by one and forgotten by the other. The longest-dictation
    /// records are maxima under either merge.
    fn combine(&mut self, other: &Self, op: Combine) {
        self.words = op(self.words, other.words);
        self.audio_ms = op(self.audio_ms, other.audio_ms);
        self.dictations = op(self.dictations, other.dictations);
        self.longest_dictation_words = self
            .longest_dictation_words
            .max(other.longest_dictation_words);
        self.longest_dictation_audio_ms = self
            .longest_dictation_audio_ms
            .max(other.longest_dictation_audio_ms);
        for (provider, totals) in &other.providers {
            self.providers
                .entry(provider.clone())
                .or_default()
                .combine(totals, op);
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct DeviceStats {
    #[serde(rename = "s", alias = "tracking_started_unix_secs")]
    pub tracking_started_unix_secs: u64,
    #[serde(rename = "l", alias = "last_dictation_unix_secs")]
    pub last_dictation_unix_secs: u64,
    #[serde(rename = "t", alias = "totals")]
    pub totals: PeriodStats,
    /// Sparse UTC-hour buckets. Only the most recent week is retained.
    #[serde(rename = "h", alias = "hours")]
    pub hours: BTreeMap<u64, PeriodStats>,
}

impl DeviceStats {
    pub(super) fn record(&mut self, provider: &str, words: u64, audio_ms: u64, now: u64) {
        if self.tracking_started_unix_secs == 0 {
            self.tracking_started_unix_secs = now;
        }
        self.last_dictation_unix_secs = now;
        self.totals.record(provider, words, audio_ms);
        self.hours
            .entry(now / HOUR_SECS * HOUR_SECS)
            .or_default()
            .record(provider, words, audio_ms);
        let oldest = now.saturating_sub(RECENT_HISTORY_HOURS * HOUR_SECS);
        self.hours.retain(|hour, _| *hour >= oldest);
    }

    pub(super) fn merge_monotonic(&mut self, other: &Self) {
        self.tracking_started_unix_secs = earliest_nonzero(
            self.tracking_started_unix_secs,
            other.tracking_started_unix_secs,
        );
        self.last_dictation_unix_secs = self
            .last_dictation_unix_secs
            .max(other.last_dictation_unix_secs);
        self.totals.merge_monotonic(&other.totals);
        for (hour, totals) in &other.hours {
            self.hours.entry(*hour).or_default().merge_monotonic(totals);
        }
    }
}
