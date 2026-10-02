//! Business licensing: Personal or Business, the 7-day business evaluation,
//! redeeming a licence key, and the offline certificate that proves it.
//!
//! QuickDictate is PolyForm Noncommercial: free for personal use, and business
//! use needs a paid licence (see `LICENSE`). This module is everything inside
//! the app for that. The design is SageThumbs 2K's (its `license.rs` and
//! `licence_state.rs` headers record why each choice was made), adapted to a
//! portable exe with no installer. The choices worth restating so nobody
//! "fixes" them:
//!
//! * **Personal or Business is asked ONCE, and there is no Settings toggle.**
//!   A toggle is a switch lazy users flip. The question is self-declaration,
//!   not enforcement: it exists so a business cannot say nobody told them, and
//!   someone who answers Personal is believed. With no installer to ask it, the
//!   Settings window asks it on first run (and once, on the next launch, for a
//!   copy that predates this). Closing it unanswered leaves the copy Personal.
//! * **Personal is never nagged and never locked.** Everything here fails
//!   toward Personal: a missing, corrupt or unreadable value reads as the quiet
//!   mode. The one thing this module must never do is stop someone the design
//!   says should be left alone.
//! * **Business with no licence is an EVALUATION, and an evaluation ends.**
//!   Seven days with everything working, three days of a clear notice at each
//!   dictation start, then dictation stops until a key is redeemed. The
//!   arithmetic is pure, in [`posture`].
//! * **A copy that once held a licence is never locked by silence.** A
//!   certificate that expires while the machine is offline keeps it working
//!   while renewal retries. Only an explicit refusal from Connections (a 404
//!   `unknown_or_expired_key` on replaying the stored key) ends a licence.
//!
//! NO RELAY, AND NO CREDENTIAL OF OURS. SageThumbs talks to its own Worker
//! because its entitlement check needs a merchant key. QuickDictate talks only
//! to Connections' one public door, `POST .../seats/redeem`, which answers a
//! valid key with a signed certificate. Replaying the same key with the same
//! install id is how a certificate is renewed: there is no other endpoint and
//! nothing to store beyond the key itself.
//!
//! TRUST BOUNDARY, stated plainly: the mode and the evaluation clock live in
//! the user's own registry hive, so a user who edits them defeats the
//! evaluation, exactly as one who answers Personal does. The lock is a path
//! for the businesses that mean to pay, never a wall against the ones that
//! will not. The certificate is the one thing here that cannot be forged.
//!
//! ## Layout of this module
//!
//! - this file: the product table, the cached facts, and the app-facing calls
//!   (the question, the dictation gate, redeeming, renewal).
//! - [`cert`]: offline Ed25519 verification of a Connections certificate.
//! - [`redeem`]: key normalization, the redeem request, and reading its reply.
//! - [`posture`]: the pure decisions: posture, gate, days left, renewal due.
//! - [`store`]: HKCU persistence, DPAPI sealing, install id, device fingerprint.
//! - [`notice`]: the no-focus notice shown when a dictation needs a licence.
//! - [`mod@format`]: dates and the user-facing sentences for each posture.

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::state::App;

pub(crate) mod cert;
pub(crate) mod format;
mod notice;
pub(crate) mod posture;
pub(crate) mod redeem;
mod store;

#[cfg(test)]
mod tests;

pub(crate) use posture::{Gate, Mode, Plan, Posture};

// ---------------------------------------------------------------------------
// The product table. ONE place: these ids move if the products move to a new
// Connections Pay company, and every other line here reads them from this.
// ---------------------------------------------------------------------------

/// One Connections Pay catalog product that licenses QuickDictate.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Product {
    /// The catalog id, which is also the certificate's `product` claim.
    pub id: &'static str,
    pub plan: Plan,
    /// Shown on the Buy button's hover and in the notice.
    pub price: &'static str,
    /// The hosted checkout for this product.
    pub buy_url: &'static str,
}

/// Both products are sold per installation. Replace the ids and the URLs
/// together when the products move.
pub(crate) const PRODUCTS: [Product; 2] = [
    Product {
        id: "bf8dca80-4036-48ad-91f4-80baa1dbb9e3",
        plan: Plan::Perpetual,
        price: "US$19.99 once",
        buy_url: "https://checkout.connections.icu/licence/bf8dca80-4036-48ad-91f4-80baa1dbb9e3",
    },
    Product {
        id: "955db7f6-6561-4a30-85d7-569ea3b3ff20",
        plan: Plan::Monthly,
        price: "US$1.99 a month",
        buy_url: "https://checkout.connections.icu/licence/955db7f6-6561-4a30-85d7-569ea3b3ff20",
    },
];

/// Where a buyer manages (cancels, moves) a licence they already hold.
pub(crate) const MANAGE_URL: &str = "https://checkout.connections.icu/manage";

/// The public redeem door. No credential: the key is the whole request.
pub(crate) const REDEEM_URL: &str =
    "https://licensing.connections.icu/api/public/enterprise/seats/redeem";

/// The product with this catalog id, if it is one of ours.
pub(crate) fn product_by_id(id: &str) -> Option<&'static Product> {
    PRODUCTS.iter().find(|p| p.id == id)
}

/// The product sold on this plan.
pub(crate) fn product_for(plan: Plan) -> &'static Product {
    match plan {
        Plan::Perpetual => &PRODUCTS[0],
        Plan::Monthly => &PRODUCTS[1],
    }
}

// ---------------------------------------------------------------------------
// Cached facts. The dictation gate runs on every hotkey press, so it reads a
// snapshot that is at most a few minutes old instead of the registry, DPAPI and
// a signature check every time. Every write invalidates it.
// ---------------------------------------------------------------------------

/// How long a loaded [`posture::Facts`] is trusted before it is re-read. Short
/// enough that an outside change (another process, a deleted value) lands
/// within minutes; long enough that a burst of presses costs one read.
const FACTS_TTL: Duration = Duration::from_secs(300);

static FACTS: Mutex<Option<(Instant, posture::Facts)>> = Mutex::new(None);

/// Serialises every read-modify-write of the store: a redeem, a renewal's
/// apply, the question, the evaluation clock. Held from re-reading what is
/// stored through the write, NEVER across a network call (a redeem can take
/// 30 s, and the Settings window and the renewal worker would queue behind it).
static STORE_LOCK: Mutex<()> = Mutex::new(());

/// The app handle, for the notice's "Enter key" button to open Settings with.
static APP: OnceLock<Arc<App>> = OnceLock::new();

/// Seconds since the Unix epoch, saturating to 0 for a clock before 1970.
pub(crate) fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// This install's facts, from the cache or freshly read.
fn facts() -> posture::Facts {
    let mut guard = FACTS.lock();
    if let Some((at, f)) = guard.as_ref() {
        if at.elapsed() < FACTS_TTL {
            return f.clone();
        }
    }
    let f = store::Store::user().load_facts(now_unix());
    *guard = Some((Instant::now(), f.clone()));
    f
}

/// Drop the cached facts so the next read sees what was just written.
fn invalidate() {
    *FACTS.lock() = None;
}

// ---------------------------------------------------------------------------
// The app-facing calls.
// ---------------------------------------------------------------------------

/// Start-up: remember the app handle, start the evaluation clock if a Business
/// copy has none, and start the background renewal worker. Never blocks on the
/// network: the first renewal runs on the worker thread.
pub(crate) fn init(app: &Arc<App>) {
    let _ = APP.set(Arc::clone(app));
    let settled = {
        let _held = STORE_LOCK.lock();
        store::Store::user().settle_clock(now_unix())
    };
    if settled {
        invalidate();
    }
    let f = facts();
    tracing::info!(
        "licence: mode={:?} posture={:?}",
        f.mode,
        posture::posture(now_unix(), &f)
    );
    spawn_renewal_worker(Arc::clone(app));
}

/// Whether the Personal-or-Business question has never been answered on this
/// copy. The Settings window shows it while this is true.
pub(crate) fn question_pending() -> bool {
    facts().mode.is_none()
}

/// Record the answer to the Personal-or-Business question. Business starts the
/// evaluation clock at the same moment. Closing the question without
/// answering is recorded as Personal by the caller.
pub(crate) fn answer_question(mode: Mode) {
    let s = store::Store::user();
    let _held = STORE_LOCK.lock();
    s.set_mode(mode);
    let _ = s.start_evaluation_if_due(now_unix());
    tracing::info!("licence: answered {mode:?}");
    invalidate();
}

/// What the Settings window shows: the posture plus the display-only details
/// around it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Snapshot {
    pub now_unix: u64,
    pub posture: Posture,
    /// The last four characters of the stored key, never more.
    pub key_last4: Option<String>,
    /// Why the last redeem came back with no certificate, when it did.
    pub pending_reason: Option<String>,
    /// The last four characters of a key accepted with no certificate yet.
    pub pending_key_last4: Option<String>,
}

pub(crate) fn snapshot() -> Snapshot {
    let f = facts();
    let now = now_unix();
    Snapshot {
        now_unix: now,
        posture: posture::posture(now, &f),
        key_last4: f.key_last4.clone(),
        pending_reason: f.pending_reason.clone(),
        pending_key_last4: f.pending_key_last4.clone(),
    }
}

/// Called at every dictation start. `true` lets the dictation start. Shows the
/// licence notice when the posture asks for one (the evaluation has ended, or
/// a licence was refused), and refuses the start once it is locked. Reads the
/// cache only: never the network, never a modal.
pub(crate) fn allow_dictation_start() -> bool {
    let now = now_unix();
    let f = facts();
    let p = posture::posture(now, &f);
    match posture::gate(p) {
        Gate::Allow => true,
        Gate::AllowWithNotice => {
            notice::show(format::notice_text(p, f.key_last4.as_deref()));
            true
        }
        Gate::Block => {
            tracing::info!("licence: dictation not started ({p:?})");
            notice::show(format::notice_text(p, f.key_last4.as_deref()));
            false
        }
    }
}

/// Open the hosted checkout for `plan` in the browser.
pub(crate) fn open_buy(plan: Plan) {
    crate::about::open_url(product_for(plan).buy_url);
}

/// Open the licence-management page in the browser.
pub(crate) fn open_manage() {
    crate::about::open_url(MANAGE_URL);
}

/// Open Settings on its Licence page (the notice's "Enter key").
fn open_licence_page() {
    if let Some(app) = APP.get() {
        crate::settings_ui::show_settings_on_licence(Arc::clone(app));
    }
}

// ---------------------------------------------------------------------------
// Redeeming and renewing.
// ---------------------------------------------------------------------------

/// What happened when the user redeemed a key, in words for the Licence page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RedeemReport {
    /// Licensed, with a certificate that verified.
    Licensed { plan: Plan },
    /// Connections accepted the key but sent no certificate; it is kept and
    /// retried, and licenses nothing until a certificate verifies. Carries
    /// Connections' reason.
    AcceptedNoCertificate { reason: String },
    /// The key was refused, or is not shaped like a key. Carries the sentence.
    Rejected { message: String },
    /// No answer: offline, a server error, a reply we could not read.
    Unavailable,
}

/// Redeem a key the user typed. BLOCKING: call it from a worker thread.
///
/// Board #7129 (buy in the app with a claim token, no key to paste) would add a
/// second way into this same function's success path; the key field stays the
/// only way in until Connections ships it.
pub(crate) fn redeem_entered_key(raw: &str) -> RedeemReport {
    let Some(key) = redeem::normalize_key(raw) else {
        return RedeemReport::Rejected {
            message: "That doesn't look like a QuickDictate licence key. It starts with \
                      esk_ and has four groups of five letters and numbers."
                .into(),
        };
    };
    let s = store::Store::user();
    let report = match exchange(&s, &key) {
        Exchange::Reply(reply) => {
            let _held = STORE_LOCK.lock();
            s.apply_redeem(&key, reply, now_unix(), false)
        }
        Exchange::NoIdentity => RedeemReport::Unavailable,
    };
    invalidate();
    report
}

enum Exchange {
    Reply(redeem::Reply),
    /// This install has no id and none could be made, or the machine id could
    /// not be read. Treated as "try again", never as a bad key.
    NoIdentity,
}

/// One round trip to the redeem door for `key`, as this install. The install
/// id may be created here, so it is read under the store lock; the round trip
/// itself is not.
fn exchange(s: &store::Store, key: &str) -> Exchange {
    let subject = {
        let _held = STORE_LOCK.lock();
        s.install_id()
    };
    let (Some(subject), Some(fingerprint)) = (subject, store::device_fingerprint()) else {
        return Exchange::NoIdentity;
    };
    Exchange::Reply(redeem::post(key, &subject, &fingerprint))
}

/// How often the renewal worker wakes to ask whether a renewal is due.
const RENEWAL_TICK: Duration = Duration::from_secs(15 * 60);

/// Replay the stored key on start-up, then whenever [`posture::renewal_due`]
/// says so (about once a day, sooner after a failure), on its own thread.
/// Exits with the app.
fn spawn_renewal_worker(app: Arc<App>) {
    let spawned = std::thread::Builder::new()
        .name("qd-licence".into())
        .spawn(move || {
            let mut first = true;
            while !app.shutdown.load(std::sync::atomic::Ordering::Acquire) {
                renew_if_due(first);
                first = false;
                // Sleep in short slices so Quit is never held up by this thread.
                let until = Instant::now() + RENEWAL_TICK;
                while Instant::now() < until {
                    if app.shutdown.load(std::sync::atomic::Ordering::Acquire) {
                        return;
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
            }
        });
    if let Err(e) = spawned {
        tracing::warn!("licence: renewal worker could not start ({e})");
    }
}

/// One renewal decision, and the replays that are due. `at_startup` skips the
/// throttle: the brief is to renew on every start-up when the certificate is
/// within its renewal window.
///
/// Each reply is applied only if its key is still held when it lands
/// ([`store::Store::apply_renewal`]): the user may have redeemed another key
/// during the round trip.
fn renew_if_due(at_startup: bool) {
    let s = store::Store::user();
    let keys = {
        let _held = STORE_LOCK.lock();
        let now = now_unix();
        if s.settle_clock(now) {
            invalidate();
        }
        s.keys_due_for_renewal(now, at_startup)
    };
    for key in keys {
        // Board #7130: when replay starts naming WHY a licence ended, carry the
        // reason from here into the "no longer active" line.
        let exchanged = exchange(&s, &key);
        let _held = STORE_LOCK.lock();
        let now = now_unix();
        match exchanged {
            Exchange::Reply(reply) => match s.apply_renewal(&key, reply, now) {
                Some(report) => {
                    tracing::info!("licence: renewal {}", format::report_for_log(&report));
                }
                None => tracing::info!("licence: renewal reply dropped, the key changed meanwhile"),
            },
            Exchange::NoIdentity => {
                s.note_renewal_attempt(now, false);
                tracing::info!("licence: renewal skipped, no install identity");
            }
        }
        invalidate();
    }
}
