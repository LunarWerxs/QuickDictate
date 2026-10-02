//! The pure licence decisions. Everything here is deterministic over its
//! arguments, so the tests pin every boundary with no registry, no file and no
//! network in sight.

/// The Personal-or-Business answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Personal,
    Business,
}

/// `None`, garbage, casing: everything but an exact business marker is
/// Personal. Failing toward the quiet mode is the module's standing rule.
/// `None` comes back only for a value that is absent or empty, which is what
/// "never asked" looks like.
pub(crate) fn parse_mode(raw: Option<&str>) -> Option<Mode> {
    match raw.map(str::trim) {
        None | Some("") => None,
        Some(s) if s.eq_ignore_ascii_case("business") => Some(Mode::Business),
        Some(_) => Some(Mode::Personal),
    }
}

/// How a licence is sold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plan {
    Perpetual,
    Monthly,
}

impl Plan {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Plan::Perpetual => "perpetual",
            Plan::Monthly => "monthly",
        }
    }
}

const DAY: u64 = 24 * 60 * 60;

/// How long a Business copy that never held a licence may evaluate with
/// everything working.
pub(crate) const EVALUATION_SECS: u64 = 7 * DAY;

/// The notice period after the evaluation ends (or a licence is refused)
/// before dictation stops. Three days covers an evaluation that ends on a
/// Friday evening: whoever raises the purchase order is back at their desk
/// before anything stops.
pub(crate) const NOTICE_SECS: u64 = 3 * DAY;

/// Everything the decisions read, gathered by `store::Store::load_facts`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Facts {
    /// `None`: the question was never answered, which reads as Personal.
    pub mode: Option<Mode>,
    /// When the business evaluation started; `0` = not started.
    pub evaluation_started_unix: u64,
    /// When this copy first held a licence (a verified certificate, or an
    /// accepted redeem); `0` = never. Once set, silence never locks it.
    pub licensed_since_unix: u64,
    /// When Connections refused the stored key on replay; `0` = never. A new
    /// accepted key clears it.
    pub refused_unix: u64,
    /// The plan of the last licence held, for a copy whose certificate lapsed.
    pub last_plan: Option<Plan>,
    /// The stored certificate, verified when it was loaded.
    pub certificate: Option<CertFacts>,
    /// Display only.
    pub key_last4: Option<String>,
    /// Display only: why the last accepted redeem had no certificate.
    pub pending_reason: Option<String>,
}

/// What a verified certificate grants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CertFacts {
    pub plan: Plan,
    pub exp_unix: u64,
}

/// What the app should do about licensing right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Posture {
    /// Personal use: free, and never a licensing word.
    Personal,
    /// A valid certificate: licensed, total silence.
    Licensed { plan: Plan, cert_exp_unix: u64 },
    /// Held a licence, but the certificate has lapsed or is not here yet and
    /// Connections has not refused the key: keeps working while renewal
    /// retries. Never leads to the lock.
    LicensedRenewing { plan: Option<Plan> },
    /// Business, never licensed, inside the 7-day evaluation.
    Evaluation { ends_unix: u64 },
    /// The evaluation is over; dictation still works until `stops_unix`, with a
    /// notice at every start.
    EvaluationEnded { stops_unix: u64 },
    /// Connections refused the stored key. A Business copy gets the notice
    /// period from that moment (`stops_unix`); a Personal one just hears it in
    /// Settings (`None`) and keeps working.
    NoLongerActive { stops_unix: Option<u64> },
    /// Past the stop date: dictation does not start. `refused` tells "your
    /// evaluation ended" from "your licence is no longer active".
    Locked { refused: bool },
}

/// The whole matrix in one place. The order of the checks is the design:
///
/// 1. A known refusal outranks a certificate. A certificate cannot be
///    withdrawn (its only reach is its own expiry), and "Connections said no"
///    is newer information than a statement signed before it.
/// 2. A valid certificate licenses, whatever the mode.
/// 3. A copy that once held a licence keeps working through silence.
/// 4. Only a Business copy that never held one runs the evaluation clock.
pub(crate) fn posture(now: u64, f: &Facts) -> Posture {
    let mode = f.mode.unwrap_or(Mode::Personal);
    if f.refused_unix > 0 {
        return refused_posture(now, mode, f.refused_unix);
    }
    if let Some(c) = f.certificate.filter(|c| c.exp_unix > now) {
        return Posture::Licensed {
            plan: c.plan,
            cert_exp_unix: c.exp_unix,
        };
    }
    if f.licensed_since_unix > 0 {
        return Posture::LicensedRenewing {
            plan: f.certificate.map(|c| c.plan).or(f.last_plan),
        };
    }
    match mode {
        Mode::Personal => Posture::Personal,
        Mode::Business => evaluation_posture(now, f.evaluation_started_unix),
    }
}

fn refused_posture(now: u64, mode: Mode, refused_unix: u64) -> Posture {
    match mode {
        Mode::Personal => Posture::NoLongerActive { stops_unix: None },
        Mode::Business => {
            let stops = refused_unix.saturating_add(NOTICE_SECS);
            if now < stops {
                Posture::NoLongerActive {
                    stops_unix: Some(stops),
                }
            } else {
                Posture::Locked { refused: true }
            }
        }
    }
}

/// The evaluation path for a Business copy that never held a licence. A clock
/// nobody started (`0`) fails open to a full evaluation from now; a clock set
/// back before the start is still inside the evaluation.
fn evaluation_posture(now: u64, started: u64) -> Posture {
    if started == 0 {
        return Posture::Evaluation {
            ends_unix: now.saturating_add(EVALUATION_SECS),
        };
    }
    let ends = started.saturating_add(EVALUATION_SECS);
    if now < ends {
        return Posture::Evaluation { ends_unix: ends };
    }
    let stops = ends.saturating_add(NOTICE_SECS);
    if now < stops {
        Posture::EvaluationEnded { stops_unix: stops }
    } else {
        Posture::Locked { refused: false }
    }
}

/// What a dictation start does in a posture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Gate {
    Allow,
    /// Start, and show the licence notice.
    AllowWithNotice,
    /// Do not start; show the licence notice instead.
    Block,
}

pub(crate) fn gate(p: Posture) -> Gate {
    match p {
        Posture::Personal
        | Posture::Licensed { .. }
        | Posture::LicensedRenewing { .. }
        | Posture::Evaluation { .. }
        | Posture::NoLongerActive { stops_unix: None } => Gate::Allow,
        Posture::EvaluationEnded { .. }
        | Posture::NoLongerActive {
            stops_unix: Some(_),
        } => Gate::AllowWithNotice,
        Posture::Locked { .. } => Gate::Block,
    }
}

/// Whole days from `now` until `until`, rounded UP and never below 1 while
/// `until` is still ahead ("1 day left" is what a person expects to read four
/// hours before the end), capped at the evaluation's own length so a clock set
/// backwards cannot promise more. `0` once `until` has passed.
pub(crate) fn days_until(now: u64, until: u64) -> u64 {
    if until <= now {
        return 0;
    }
    (until - now).div_ceil(DAY).clamp(1, EVALUATION_SECS / DAY)
}

/// Replay the stored key when its certificate expires within this window.
pub(crate) const RENEW_WITHIN_SECS: u64 = 7 * DAY;
/// After a renewal that worked, wait about a day before the next.
pub(crate) const RENEW_AFTER_SUCCESS_SECS: u64 = 20 * 60 * 60;
/// After a renewal that got no answer (or no certificate), try again sooner.
pub(crate) const RENEW_AFTER_FAILURE_SECS: u64 = 60 * 60;

/// What the renewal decision reads.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RenewalInputs {
    pub has_key: bool,
    pub refused: bool,
    /// The stored certificate's expiry, when one verifies.
    pub cert_exp_unix: Option<u64>,
    pub last_attempt_unix: u64,
    pub last_attempt_ok: bool,
}

/// Is a replay of the stored key due? Never with no key, and never after
/// Connections refused it (only a new key the user enters can change that).
/// Otherwise when the certificate is missing or within [`RENEW_WITHIN_SECS`]
/// of expiring, throttled to about once a day after a success and hourly
/// after a failure. `at_startup` skips the throttle.
pub(crate) fn renewal_due(now: u64, r: &RenewalInputs, at_startup: bool) -> bool {
    if !r.has_key || r.refused {
        return false;
    }
    let needs = r
        .cert_exp_unix
        .is_none_or(|exp| exp.saturating_sub(now) <= RENEW_WITHIN_SECS);
    if !needs {
        return false;
    }
    if at_startup {
        return true;
    }
    let wait = if r.last_attempt_ok {
        RENEW_AFTER_SUCCESS_SECS
    } else {
        RENEW_AFTER_FAILURE_SECS
    };
    // A last attempt "in the future" means the clock was moved back; waiting
    // for it to catch up could stall renewal for as long as it was wrong.
    now < r.last_attempt_unix || now - r.last_attempt_unix >= wait
}
