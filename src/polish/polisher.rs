//! The pass itself: the speculation slot, the deadline race, and the shared
//! state that keeps a second request from racing the first.

use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;
use tokio::runtime::Handle as TokioHandle;

use super::*;

pub struct Polisher {
    pub(super) rt: TokioHandle,
    pub(super) client: reqwest::Client,
    pub(super) state: Arc<Mutex<State>>,
    /// Round-robin cursor over `PolishSettings::keys`. Plain wrapping
    /// increment: a skipped or repeated key costs nothing here, because a
    /// failed pass just pastes the unpolished text.
    pub(super) next_key: Arc<std::sync::atomic::AtomicUsize>,
}

impl Polisher {
    pub fn new(rt: TokioHandle) -> Self {
        Self {
            rt,
            client: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .unwrap_or_default(),
            state: Arc::new(Mutex::new(State::default())),
            next_key: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// The next key to send with, or `None` if none are configured.
    pub(super) fn take_key(
        settings: &PolishSettings,
        cursor: &std::sync::atomic::AtomicUsize,
    ) -> Option<String> {
        if settings.keys.is_empty() {
            return None;
        }
        let n = cursor.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        settings.keys.get(n % settings.keys.len()).cloned()
    }

    /// Drop any cached answer. Called when a new dictation starts so a
    /// result computed for the previous press can never be handed to this one.
    pub fn reset(&self) {
        let mut st = self.state.lock();
        st.ready = None;
        st.queued = None;
        // `inflight` is deliberately left alone: the task owns it and clears
        // it on completion. Its result will simply not match any future text.
    }

    /// Run a pass over `text` in the background, if one isn't already running
    /// for it. Fire-and-forget: this is the free-time path taken while the
    /// user is still talking, so it never blocks and never reports anything.
    pub fn speculate(&self, settings: &PolishSettings, text: &str) {
        if !worth_polishing(text) {
            return;
        }
        let start = {
            let mut st = self.state.lock();
            if st.ready.as_ref().is_some_and(|(input, _)| input == text)
                || st.inflight.as_deref() == Some(text)
            {
                return;
            }
            // One pause that added a few words is not worth another round
            // trip: the answer we already have covers all but the tail, and
            // `resolve` will ask for real if it turns out to matter. Without
            // this, a stop-start dictation fires a request per pause, each one
            // re-sending everything the last already covered.
            let answered = st
                .ready
                .as_ref()
                .map(|(input, _)| input.as_str())
                .or(st.inflight.as_deref());
            if let Some(answered) = answered {
                let grew = text
                    .chars()
                    .count()
                    .saturating_sub(answered.chars().count());
                if text.starts_with(answered) && grew < MIN_GROWTH_CHARS {
                    return;
                }
            }
            if st.inflight.is_some() {
                st.queued = Some(text.to_string());
                false
            } else {
                st.inflight = Some(text.to_string());
                true
            }
        };
        if start {
            self.spawn(settings.clone(), text.to_string());
        }
    }

    /// The polished form of `text`, or `None` to paste it as-is.
    ///
    /// Returns instantly when speculation already answered for exactly this
    /// text. Otherwise it starts a pass and blocks the paste thread for at
    /// most `settings.deadline` -- that bounded wait IS the feature, and a
    /// timeout is a normal outcome, not an error.
    pub fn resolve(&self, settings: &PolishSettings, text: &str) -> Option<String> {
        if !worth_polishing(text) {
            return None;
        }
        let (tx, rx) = std::sync::mpsc::sync_channel::<String>(1);
        let start = {
            let mut st = self.state.lock();
            if let Some((input, polished)) = st.ready.take() {
                if input == text {
                    tracing::debug!("polish: speculation already answered this, no wait");
                    return changed(text, polished);
                }
            }
            if st.inflight.as_deref() == Some(text) {
                // Already being computed -- wait on that one rather than
                // restarting the clock with a duplicate request.
                st.waiters.push(tx);
                false
            } else {
                st.inflight = Some(text.to_string());
                st.waiters = vec![tx];
                st.queued = None;
                true
            }
        };
        if start {
            self.spawn(settings.clone(), text.to_string());
        }

        let started = Instant::now();
        match rx.recv_timeout(settings.deadline) {
            Ok(polished) => {
                tracing::info!(
                    "polish: answered in {:?}, inside the deadline",
                    started.elapsed()
                );
                changed(text, polished)
            }
            Err(_) => {
                // Timed out, or the task gave up and dropped its sender.
                // Either way the raw text is pasted, exactly as it would have
                // been without any of this.
                tracing::info!(
                    "polish: deadline {:?} expired, pasting unpolished",
                    settings.deadline
                );
                None
            }
        }
    }

    /// Run one pass on the tokio runtime, publish the result, wake anyone
    /// blocked on it, and pick up whatever was queued behind it.
    fn spawn(&self, settings: PolishSettings, text: String) {
        let inner = Polisher {
            rt: self.rt.clone(),
            client: self.client.clone(),
            state: Arc::clone(&self.state),
            next_key: Arc::clone(&self.next_key),
        };
        let key = Self::take_key(&settings, &self.next_key);
        self.rt.spawn(async move {
            let started = Instant::now();
            let outcome = match key {
                Some(key) => request_edits(&inner.client, &settings, &key, &text).await,
                None => Err("no key configured".to_string()),
            };
            let polished = match outcome {
                Ok(edits) => apply_edits(&text, &edits),
                Err(e) => {
                    tracing::debug!("polish: request failed ({e}); leaving the text alone");
                    None
                }
            };
            tracing::debug!(
                "polish: pass over {} char(s) took {:?} and {} the text",
                text.chars().count(),
                started.elapsed(),
                if polished.is_some() {
                    "changed"
                } else {
                    "left"
                }
            );

            // Whatever came back, this input is answered: publish it (even
            // unchanged, stored as the text itself) so a later `resolve` for
            // the same text is instant instead of a second round trip.
            let settled = polished.unwrap_or_else(|| text.clone());
            let (waiters, next) = {
                let mut st = inner.state.lock();
                st.ready = Some((text.clone(), settled.clone()));
                if st.inflight.as_deref() == Some(text.as_str()) {
                    st.inflight = None;
                    (std::mem::take(&mut st.waiters), st.queued.take())
                } else {
                    // A newer pass superseded us and owns the waiters now.
                    // Our answer still goes in `ready`; it just isn't the one
                    // anybody is holding the paste for.
                    (Vec::new(), None)
                }
            };
            for tx in waiters {
                // The receiver may already have hit its deadline and gone;
                // that is the expected loss case, not an error.
                let _ = tx.try_send(settled.clone());
            }
            if let Some(next) = next {
                inner.speculate(&settings, &next);
            }
        });
    }
}
