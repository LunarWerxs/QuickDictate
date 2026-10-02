//! The words: what the Licence page, the banners and the notice say in each
//! posture, and the dates inside them. One place, so three surfaces cannot
//! drift into three descriptions of one clock.

use super::posture::{days_until, Plan, Posture};
use super::{RedeemReport, Snapshot};

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// Proleptic Gregorian (year, month 1-12, day 1-31) of a day count since
/// 1970-01-01 (Howard Hinnant's `civil_from_days`).
pub(crate) fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// "October 5, 2026".
pub(crate) fn date_words(y: i64, m: u32, d: u32) -> String {
    let month = MONTHS
        .get(m.saturating_sub(1) as usize)
        .copied()
        .unwrap_or("?");
    format!("{month} {d}, {y}")
}

/// `unix` as a calendar date in this machine's time zone, falling back to UTC
/// if Windows will not convert it.
pub(crate) fn local_date(unix: u64) -> String {
    let secs = i64::try_from(unix).unwrap_or(i64::MAX);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let (y, m, d) = to_local(y, m, d, secs.rem_euclid(86_400)).unwrap_or((y, m, d));
    date_words(y, m, d)
}

fn to_local(y: i64, m: u32, d: u32, secs_of_day: i64) -> Option<(i64, u32, u32)> {
    use windows::Win32::Foundation::SYSTEMTIME;
    use windows::Win32::System::Time::SystemTimeToTzSpecificLocalTime;
    let utc = SYSTEMTIME {
        wYear: u16::try_from(y).ok()?,
        wMonth: m as u16,
        wDayOfWeek: 0,
        wDay: d as u16,
        wHour: (secs_of_day / 3600) as u16,
        wMinute: (secs_of_day / 60 % 60) as u16,
        wSecond: (secs_of_day % 60) as u16,
        wMilliseconds: 0,
    };
    let mut local = SYSTEMTIME::default();
    unsafe { SystemTimeToTzSpecificLocalTime(None, &utc, &mut local) }.ok()?;
    Some((
        i64::from(local.wYear),
        u32::from(local.wMonth),
        u32::from(local.wDay),
    ))
}

fn plural_days(n: u64) -> String {
    if n == 1 {
        "1 day".into()
    } else {
        format!("{n} days")
    }
}

/// " (key ending ABCD)" or nothing.
fn key_suffix(last4: Option<&str>) -> String {
    last4
        .map(|k| format!(" (key ending {k})"))
        .unwrap_or_default()
}

/// The Licence page's status: a short headline and the sentence under it.
pub(crate) fn status_lines(s: &Snapshot) -> (String, String) {
    let key = key_suffix(s.key_last4.as_deref());
    match s.posture {
        Posture::Personal => (
            "Personal".into(),
            "Personal use: free, no licence needed.".into(),
        ),
        Posture::Evaluation { ends_unix } => (
            format!(
                "Business evaluation, {} left",
                plural_days(days_until(s.now_unix, ends_unix))
            ),
            format!(
                "Everything works until {}. After that, business use needs a licence.",
                local_date(ends_unix)
            ),
        ),
        Posture::EvaluationEnded { stops_unix } => (
            "Licence needed".into(),
            format!(
                "The 7-day business evaluation has ended. Dictation stops on {} unless a \
                 licence key is entered.",
                local_date(stops_unix)
            ),
        ),
        Posture::Locked { refused: false } => (
            "Licence needed".into(),
            "The business evaluation has ended. Dictation is paused until a licence key \
             is entered."
                .into(),
        ),
        Posture::Licensed {
            plan: Plan::Perpetual,
            ..
        } => (
            "Licensed: perpetual".into(),
            format!("This copy is licensed for business use{key}."),
        ),
        Posture::Licensed {
            plan: Plan::Monthly,
            cert_exp_unix,
        } => (
            format!("Licensed: monthly, renews by {}", local_date(cert_exp_unix)),
            format!("This copy is licensed for business use{key}."),
        ),
        Posture::LicensedRenewing { plan } => (
            match plan {
                Some(p) => format!("Licensed: {}", p.label()),
                None => "Licensed".into(),
            },
            match &s.pending_reason {
                Some(why) => format!(
                    "Connections accepted the key{key} but has not sent its certificate yet \
                     ({why}). QuickDictate will try again; dictation keeps working."
                ),
                None => format!(
                    "Licensed{key}. Its certificate is being renewed; dictation keeps working \
                     meanwhile."
                ),
            },
        ),
        Posture::NoLongerActive { stops_unix } => (
            "This licence is no longer active".into(),
            match stops_unix {
                Some(stops) => format!(
                    "Connections no longer accepts this copy's licence{key}. Dictation stops \
                     on {} unless another key is entered.",
                    local_date(stops)
                ),
                None => format!("Connections no longer accepts this copy's licence{key}."),
            },
        ),
        Posture::Locked { refused: true } => (
            "This licence is no longer active".into(),
            format!(
                "Connections no longer accepts this copy's licence{key}. Dictation is paused \
                 until another licence key is entered."
            ),
        ),
    }
}

/// The notice's headline and body, for the postures that show one.
pub(crate) fn notice_text(p: Posture, last4: Option<&str>) -> (String, String) {
    let key = key_suffix(last4);
    match p {
        Posture::EvaluationEnded { stops_unix } => (
            "QuickDictate needs a business licence".into(),
            format!(
                "The 7-day business evaluation has ended. Dictation stops on {} unless a \
                 licence key is entered.",
                local_date(stops_unix)
            ),
        ),
        Posture::NoLongerActive { stops_unix } => (
            "This licence is no longer active".into(),
            match stops_unix {
                Some(stops) => format!(
                    "Connections no longer accepts this copy's licence{key}. Dictation stops \
                     on {} unless another key is entered.",
                    local_date(stops)
                ),
                None => format!("Connections no longer accepts this copy's licence{key}."),
            },
        ),
        Posture::Locked { refused: true } => (
            "Dictation is paused: this licence is no longer active".into(),
            "Buy a licence, or enter another key, to keep dictating.".into(),
        ),
        _ => (
            "Dictation is paused: a licence is needed".into(),
            "This copy is set up for business use and its 7-day evaluation has ended. Buy a \
             licence, or enter your key, to keep dictating."
                .into(),
        ),
    }
}

/// A log line for a redeem outcome, with no key in it.
pub(crate) fn report_for_log(r: &RedeemReport) -> String {
    match r {
        RedeemReport::Licensed { plan } => format!("licensed ({})", plan.label()),
        RedeemReport::AcceptedNoCertificate { .. } => "accepted, no certificate yet".into(),
        RedeemReport::Rejected { .. } => "rejected".into(),
        RedeemReport::Unavailable => "no answer".into(),
    }
}

/// What the Licence page says after the user's own redeem.
pub(crate) fn report_line(r: &RedeemReport) -> (String, bool) {
    match r {
        RedeemReport::Licensed { plan } => (
            format!("Licence activated: {}. Thank you!", plan.label()),
            false,
        ),
        RedeemReport::AcceptedNoCertificate { reason } => (
            format!(
                "Key accepted, but Connections has not sent its certificate yet ({reason}). \
                 QuickDictate will try again."
            ),
            false,
        ),
        RedeemReport::Rejected { message } => (message.clone(), true),
        RedeemReport::Unavailable => (
            "Couldn't reach the licence service. Check your connection and try again.".into(),
            true,
        ),
    }
}
