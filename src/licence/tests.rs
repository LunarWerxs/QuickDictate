use super::cert::{self, CertError};
use super::posture::*;
use super::redeem::{self, Reply};
use super::*;

/// A REAL certificate, minted 2026-10-02 by Connections for a test licence on
/// QuickDictate's own Pay account, since cancelled. The signature stays valid
/// forever, so it pins the parser, the base64url handling and the Ed25519 check
/// against what Connections actually emits. Its claims: product = the perpetual
/// id, sub `qd-install-test-20261002b`, iat 1790952532, exp 1791038932
/// (2026-10-03T14:48:52Z).
const REAL_CERT: &str = concat!(
    "eyJhdWQiOiJjb25uZWN0aW9ucy1saWNlbmNlIiwibGljIjoiMmE1ODA2YjUtYjNlYi00Mjg0LTkyODUtM2Jm",
    "YjhlZjg0MDM4IiwicHJvZHVjdCI6ImJmOGRjYTgwLTQwMzYtNDhhZC05MWY0LTgwYmFhMWRiYjllMyIsInN1",
    "YiI6InFkLWluc3RhbGwtdGVzdC0yMDI2MTAwMmIiLCJ1bml0IjoiaW5zdGFsbGF0aW9uIiwidGVybSI6InBl",
    "cnBldHVhbCIsInVuaXRzIjoxLCJzZWF0IjoiZjAxYWQzZjUtNTk2ZC00ZDI3LThhMmUtMTYwZmZlMThjODUx",
    "IiwibWFpbnQiOm51bGwsImNlaWwiOm51bGwsImlhdCI6MTc5MDk1MjUzMiwiZXhwIjoxNzkxMDM4OTMyfQ.",
    "TMOr7sXe1a99fstmjiGApZYA8Id8lMRke3WBXkNijPYBl1uBF0pYqbgLtwV_nVtzpERIjggQW4JWHnJ2UuiJAQ"
);
const REAL_SUB: &str = "qd-install-test-20261002b";
const REAL_EXP: u64 = 1_791_038_932;
/// Inside the certificate's window. Pinned, never the real clock: a test that
/// passes until a date and then fails on its own is not a test.
const INSIDE: u64 = 1_790_960_000;

const DAY: u64 = 24 * 60 * 60;
/// 2026-10-03, past [`CLOCK_FLOOR_UNIX`]: a stamp below the floor reads as
/// one made by a clock that was behind.
const T0: u64 = 1_791_000_000;

// ---- The product table -----------------------------------------------------

/// The ids are the one thing that changes when the products move; each buy
/// link must move with its id, or Buy sells the wrong (or a dead) product.
#[test]
fn every_buy_link_points_at_its_own_product() {
    for p in &PRODUCTS {
        assert!(p.buy_url.ends_with(p.id), "{p:?}");
        assert_eq!(product_by_id(p.id), Some(p));
        assert_eq!(product_for(p.plan).id, p.id);
    }
}

// ---- The certificate ------------------------------------------------------

#[test]
fn the_real_certificate_verifies_inside_its_window() {
    let v = cert::verify(REAL_CERT, REAL_SUB, INSIDE).unwrap();
    assert_eq!(v.plan, Plan::Perpetual);
    assert_eq!(v.product_id, "bf8dca80-4036-48ad-91f4-80baa1dbb9e3");
    assert_eq!(v.exp_unix, REAL_EXP);
    assert!(cert::verify(REAL_CERT, REAL_SUB, REAL_EXP - 1).is_ok());
}

#[test]
fn an_expired_certificate_is_no_certificate() {
    assert_eq!(
        cert::verify(REAL_CERT, REAL_SUB, REAL_EXP),
        Err(CertError::Expired),
        "exp itself is already expired"
    );
    assert_eq!(
        cert::verify(REAL_CERT, REAL_SUB, REAL_EXP + DAY),
        Err(CertError::Expired)
    );
}

#[test]
fn a_tampered_payload_or_signature_fails_the_signature() {
    let (p, s) = REAL_CERT.split_once('.').unwrap();
    let mut bad_p = p.to_string();
    bad_p.replace_range(10..11, if &p[10..11] == "A" { "B" } else { "A" });
    assert_eq!(
        cert::verify(&format!("{bad_p}.{s}"), REAL_SUB, INSIDE),
        Err(CertError::BadSignature)
    );
    let mut bad_s = s.to_string();
    bad_s.replace_range(0..1, if s.starts_with('c') { "d" } else { "c" });
    assert_eq!(
        cert::verify(&format!("{p}.{bad_s}"), REAL_SUB, INSIDE),
        Err(CertError::BadSignature)
    );
}

#[test]
fn another_installs_certificate_is_not_this_ones() {
    assert_eq!(
        cert::verify(REAL_CERT, "qd-some-other-install", INSIDE),
        Err(CertError::NotThisInstall)
    );
}

/// Audience and product are checked after the signature, so no signed
/// certificate can exercise them; the claims are driven directly.
#[test]
fn a_wrong_audience_or_product_is_refused() {
    let claims = |aud: &str, product: &str| {
        cert::Claims::from_json(&serde_json::json!({
            "aud": aud, "product": product, "sub": REAL_SUB, "exp": REAL_EXP,
        }))
        .unwrap()
    };
    let ours = PRODUCTS[1].id;
    assert_eq!(
        cert::check_claims(&claims("connections-session", ours), REAL_SUB, INSIDE),
        Err(CertError::WrongAudience)
    );
    assert_eq!(
        cert::check_claims(
            &claims(
                "connections-licence",
                "24544461-9530-4edb-84e5-4f3471876d98"
            ),
            REAL_SUB,
            INSIDE
        ),
        Err(CertError::WrongProduct),
        "a SageThumbs 2K certificate licenses nothing here"
    );
    let monthly = cert::check_claims(&claims("connections-licence", ours), REAL_SUB, INSIDE);
    assert_eq!(monthly.map(|v| v.plan), Ok(Plan::Monthly));
}

#[test]
fn garbage_certificates_are_refused_without_panicking() {
    for junk in ["", ".", "no-dot", "a.b", "a.b.c", "....", "!!!.!!!"] {
        assert!(cert::verify(junk, REAL_SUB, INSIDE).is_err(), "{junk:?}");
    }
    let (p, s) = REAL_CERT.split_once('.').unwrap();
    assert!(
        cert::verify(&format!("{p}=.{s}"), REAL_SUB, INSIDE).is_err(),
        "padded base64 is not what Connections emits"
    );
}

// ---- The redeem reply -------------------------------------------------------

#[test]
fn a_success_with_a_certificate_is_accepted() {
    let body = format!(
        r#"{{"ok":true,"replayed":false,"seat":{{"id":"s1"}},"certificate":"{REAL_CERT}",
            "certificateExpiresAt":"2026-10-03T12:40:21Z","certificateAlgorithm":"Ed25519",
            "certificateAudience":"connections-licence"}}"#
    );
    assert_eq!(
        redeem::parse_reply(200, body.as_bytes()),
        Reply::Accepted {
            replayed: false,
            certificate: Some(REAL_CERT.into()),
            certificate_error: None,
        }
    );
    let replay = body.replace(r#""replayed":false"#, r#""replayed":true"#);
    assert!(matches!(
        redeem::parse_reply(200, replay.as_bytes()),
        Reply::Accepted { replayed: true, .. }
    ));
}

#[test]
fn a_success_with_no_certificate_is_redeemed_with_the_reason() {
    let body = br#"{"ok":true,"replayed":true,"seat":{},"certificate":null,
                    "certificateError":"signing_key_unavailable"}"#;
    assert_eq!(
        redeem::parse_reply(200, body),
        Reply::Accepted {
            replayed: true,
            certificate: None,
            certificate_error: Some("signing_key_unavailable".into()),
        }
    );
}

#[test]
fn unknown_or_expired_key_is_the_one_refusal() {
    let body = br#"{"error":"unknown_or_expired_key","message":"Key not found or expired."}"#;
    assert_eq!(redeem::parse_reply(404, body), Reply::Refused);
    // A 404 that is not that answer (a proxy page, a moved route) is no verdict.
    assert_eq!(
        redeem::parse_reply(404, b"<html>Not Found</html>"),
        Reply::Unavailable
    );
    assert_eq!(
        redeem::parse_reply(404, br#"{"error":"not_found"}"#),
        Reply::Unavailable
    );
}

#[test]
fn everything_else_is_a_reason_or_no_answer_never_a_refusal() {
    assert_eq!(
        redeem::parse_reply(
            409,
            br#"{"error":"seat_bound","message":"This key is in use on another computer."}"#
        ),
        Reply::Rejected {
            message: "This key is in use on another computer.".into()
        }
    );
    assert!(matches!(
        redeem::parse_reply(429, b""),
        Reply::Rejected { .. }
    ));
    assert_eq!(redeem::parse_reply(400, b"nope"), Reply::Unavailable);
    assert_eq!(
        redeem::parse_reply(500, br#"{"error":"x"}"#),
        Reply::Unavailable
    );
    assert_eq!(
        redeem::parse_reply(200, br#"{"ok":false}"#),
        Reply::Unavailable
    );
    assert_eq!(redeem::parse_reply(200, b"{truncat"), Reply::Unavailable);
}

#[test]
fn keys_normalize_case_and_dash_insensitively() {
    let canon = "esk_ABCDE-12345-FGHIJ-6789K";
    for typed in [
        canon,
        "esk_abcde-12345-fghij-6789k",
        "  ESK_ABCDE12345FGHIJ6789K ",
        "esk_ab-cde12-345fg-hij67-89k",
    ] {
        assert_eq!(
            redeem::normalize_key(typed).as_deref(),
            Some(canon),
            "{typed}"
        );
    }
    for bad in [
        "",
        "ABCDE-12345-FGHIJ-6789K",
        "esk_ABCDE-12345-FGHIJ-6789",
        "esk_ABCDE-12345-FGHIJ-6789KX",
        "esk_ABCDE-12345-FGHIJ-6789!",
    ] {
        assert_eq!(redeem::normalize_key(bad), None, "{bad}");
    }
    assert_eq!(redeem::key_last4(canon), "789K");
}

/// The install id goes in `externalSubjectId`; `sub` means a Connections
/// account id and must never be sent.
#[test]
fn the_request_names_the_install_as_an_external_subject() {
    let body = redeem::request_body("esk_K", "qd-1", "fp");
    assert_eq!(body["externalSubjectId"], "qd-1");
    assert_eq!(body["deviceFingerprint"], "fp");
    assert_eq!(body["label"], "QuickDictate");
    assert!(body.get("sub").is_none());
}

#[test]
fn the_fingerprint_is_salted_hex_and_stable() {
    let a = store::fingerprint_from_guid("6ba7b810-9dad-11d1-80b4-00c04fd430c8");
    assert_eq!(a.len(), 64);
    assert!(a
        .bytes()
        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
    assert_eq!(
        a,
        store::fingerprint_from_guid("6ba7b810-9dad-11d1-80b4-00c04fd430c8")
    );
    assert_ne!(a, store::fingerprint_from_guid("another-guid"));
}

// ---- The posture --------------------------------------------------------------

fn business(started: u64) -> Facts {
    Facts {
        mode: Some(Mode::Business),
        evaluation_started_unix: started,
        ..Default::default()
    }
}

#[test]
fn personal_is_never_nagged_or_locked() {
    for mode in [None, Some(Mode::Personal)] {
        let f = Facts {
            mode,
            evaluation_started_unix: T0,
            ..Default::default()
        };
        for now in [T0, T0 + 11 * DAY, T0 + 400 * DAY] {
            assert_eq!(posture(now, &f), Posture::Personal);
            assert_eq!(gate(posture(now, &f)), Gate::Allow);
        }
    }
}

#[test]
fn only_an_exact_business_answer_is_business() {
    assert_eq!(parse_mode(None), None);
    assert_eq!(parse_mode(Some("  ")), None, "unanswered");
    assert_eq!(parse_mode(Some("personal")), Some(Mode::Personal));
    assert_eq!(parse_mode(Some("corporate")), Some(Mode::Personal));
    assert_eq!(parse_mode(Some(" Business ")), Some(Mode::Business));
}

/// The 10-day trial: days 1-7 work quietly, days 8-10 work with a notice, day
/// 11 on is locked. Every count the user reads runs to the trial's end.
#[test]
fn the_business_trial_runs_ten_days_the_last_three_with_a_notice_then_locks() {
    let f = business(T0);
    let quiet_ends = T0 + 7 * DAY;
    let stops = T0 + 10 * DAY;

    // Day 0: the moment it starts, with the whole trial ahead.
    assert_eq!(posture(T0, &f), Posture::Evaluation { ends_unix: stops });
    assert_eq!(days_until(T0, stops), 10);
    assert_eq!(gate(posture(T0, &f)), Gate::Allow);

    // Day 7: its last second is still quiet, with three days left.
    let day7 = quiet_ends - 1;
    assert_eq!(posture(day7, &f), Posture::Evaluation { ends_unix: stops });
    assert_eq!(days_until(day7, stops), 4);

    // Day 8: the notice days, dictation still starts.
    assert_eq!(
        posture(quiet_ends, &f),
        Posture::EvaluationEnded { stops_unix: stops }
    );
    assert_eq!(gate(posture(quiet_ends, &f)), Gate::AllowWithNotice);
    assert_eq!(days_until(quiet_ends, stops), 3);

    // Day 10: its last second is still the notice.
    assert_eq!(
        posture(stops - 1, &f),
        Posture::EvaluationEnded { stops_unix: stops }
    );

    // Day 11: dictation does not start.
    assert_eq!(posture(stops, &f), Posture::Locked { refused: false });
    assert_eq!(gate(posture(stops, &f)), Gate::Block);
    assert_eq!(
        posture(stops + 365 * DAY, &f),
        Posture::Locked { refused: false }
    );
}

#[test]
fn an_unstarted_or_backdated_evaluation_fails_open() {
    assert!(matches!(
        posture(T0 + 400 * DAY, &business(0)),
        Posture::Evaluation { .. }
    ));
    let before = T0 - 30 * DAY;
    let p = posture(before, &business(T0));
    assert_eq!(
        p,
        Posture::Evaluation {
            ends_unix: T0 + 10 * DAY
        }
    );
    assert_eq!(
        days_until(before, T0 + 10 * DAY),
        10,
        "never more than the 10-day trial"
    );
}

#[test]
fn a_valid_certificate_licenses_any_mode() {
    for mode in [None, Some(Mode::Personal), Some(Mode::Business)] {
        let f = Facts {
            mode,
            evaluation_started_unix: T0,
            licensed_since_unix: T0 + DAY,
            certificate: Some(CertFacts {
                plan: Plan::Monthly,
                exp_unix: T0 + 30 * DAY,
            }),
            ..Default::default()
        };
        let p = posture(T0 + 20 * DAY, &f);
        assert_eq!(
            p,
            Posture::Licensed {
                plan: Plan::Monthly,
                cert_exp_unix: T0 + 30 * DAY
            }
        );
        assert_eq!(gate(p), Gate::Allow);
    }
}

/// The certificate lapsed while offline: the copy keeps working, however long
/// the silence, and never reaches the evaluation's lock.
#[test]
fn a_once_licensed_copy_whose_certificate_expired_offline_keeps_working() {
    let f = Facts {
        certificate: Some(CertFacts {
            plan: Plan::Perpetual,
            exp_unix: T0 + 2 * DAY,
        }),
        licensed_since_unix: T0 + DAY,
        last_plan: Some(Plan::Perpetual),
        ..business(T0)
    };
    for now in [T0 + 3 * DAY, T0 + 11 * DAY, T0 + 400 * DAY] {
        let p = posture(now, &f);
        assert_eq!(
            p,
            Posture::LicensedRenewing {
                plan: Some(Plan::Perpetual)
            },
            "at {now}"
        );
        assert_eq!(gate(p), Gate::Allow);
    }
    // With the expired certificate no longer even loadable, the plan on
    // record still names it.
    let gone = Facts {
        certificate: None,
        ..f
    };
    assert_eq!(
        posture(T0 + 400 * DAY, &gone),
        Posture::LicensedRenewing {
            plan: Some(Plan::Perpetual)
        }
    );
}

/// Connections refused the stored key on replay: that ends the licence, even
/// over a certificate that has not expired yet, with the notice period first.
#[test]
fn a_refused_replay_ends_the_licence() {
    let refused = T0 + 20 * DAY;
    let f = Facts {
        licensed_since_unix: T0 + DAY,
        refused_unix: refused,
        certificate: Some(CertFacts {
            plan: Plan::Monthly,
            exp_unix: refused + DAY,
        }),
        ..business(T0)
    };
    let stops = refused + 3 * DAY;
    let p = posture(refused, &f);
    assert_eq!(
        p,
        Posture::NoLongerActive {
            stops_unix: Some(stops)
        }
    );
    assert_eq!(gate(p), Gate::AllowWithNotice);
    assert_eq!(posture(stops, &f), Posture::Locked { refused: true });
    assert_eq!(gate(posture(stops, &f)), Gate::Block);

    // A Personal copy hears it in Settings and keeps working.
    let personal = Facts {
        mode: Some(Mode::Personal),
        ..f
    };
    let p = posture(stops + 100 * DAY, &personal);
    assert_eq!(p, Posture::NoLongerActive { stops_unix: None });
    assert_eq!(gate(p), Gate::Allow);
}

#[test]
fn renewal_is_due_near_expiry_throttled_and_never_after_a_refusal() {
    let now = T0;
    let base = RenewalInputs {
        has_key: true,
        has_pending_key: false,
        refused: false,
        cert_exp_unix: Some(now + DAY),
        last_attempt_unix: now - 2 * DAY,
        last_attempt_ok: true,
    };
    assert!(renewal_due(now, &base, false));
    assert!(!renewal_due(
        now,
        &RenewalInputs {
            cert_exp_unix: Some(now + 30 * DAY),
            ..base
        },
        true
    ));
    assert!(renewal_due(
        now,
        &RenewalInputs {
            cert_exp_unix: None,
            ..base
        },
        false
    ));
    // Throttle: a success an hour ago waits; a failure an hour ago retries.
    let recent = RenewalInputs {
        last_attempt_unix: now - 3600,
        ..base
    };
    assert!(!renewal_due(now, &recent, false));
    assert!(
        renewal_due(now, &recent, true),
        "start-up skips the throttle"
    );
    assert!(renewal_due(
        now,
        &RenewalInputs {
            last_attempt_ok: false,
            ..recent
        },
        false
    ));
    assert!(!renewal_due(
        now,
        &RenewalInputs {
            refused: true,
            ..base
        },
        true
    ));
    assert!(!renewal_due(
        now,
        &RenewalInputs {
            has_key: false,
            ..base
        },
        true
    ));
}

// ---- The words ----------------------------------------------------------------

#[test]
fn calendar_dates_come_out_right() {
    assert_eq!(format::civil_from_days(0), (1970, 1, 1));
    assert_eq!(
        format::civil_from_days((REAL_EXP / DAY) as i64),
        (2026, 10, 3)
    );
    assert_eq!(format::civil_from_days(11_016), (2000, 2, 29));
    assert_eq!(format::date_words(2026, 10, 3), "October 3, 2026");
}

#[test]
fn the_licensed_line_shows_only_the_last_four_key_characters() {
    let s = Snapshot {
        now_unix: T0,
        posture: Posture::Licensed {
            plan: Plan::Perpetual,
            cert_exp_unix: T0 + DAY,
        },
        key_last4: Some("789K".into()),
        pending_reason: None,
        pending_key_last4: None,
    };
    let (head, detail) = format::status_lines(&s);
    assert_eq!(head, "Licensed: perpetual");
    assert!(detail.contains("key ending 789K"), "{detail}");
}

/// The certificate's expiry is days away at most, and not when the
/// subscription bills: a date there reads as "renews tomorrow".
#[test]
fn the_monthly_line_names_no_date() {
    let s = Snapshot {
        now_unix: T0,
        posture: Posture::Licensed {
            plan: Plan::Monthly,
            cert_exp_unix: T0 + DAY,
        },
        key_last4: None,
        pending_reason: None,
        pending_key_last4: None,
    };
    assert_eq!(format::status_lines(&s).0, "Licensed: monthly subscription");
}

/// A key waiting for its certificate is mentioned, under a posture it has not
/// changed.
#[test]
fn a_pending_key_is_mentioned_without_claiming_a_licence() {
    let s = Snapshot {
        now_unix: T0,
        posture: Posture::Evaluation {
            ends_unix: T0 + 10 * DAY,
        },
        key_last4: None,
        pending_reason: Some("signing_key_unavailable".into()),
        pending_key_last4: Some("WWW1".into()),
    };
    let (head, detail) = format::status_lines(&s);
    assert_eq!(head, "Business trial, 10 days left");
    assert!(detail.contains("key ending WWW1"), "{detail}");
    assert!(detail.contains("signing_key_unavailable"), "{detail}");
    let (line, _) = format::report_line(&RedeemReport::AcceptedNoCertificate {
        reason: "signing_key_unavailable".into(),
    });
    assert!(!line.to_lowercase().contains("licensed"), "{line}");
}

/// The notice's headline is one line in a fixed box; past the limit the end is
/// cut off, and that is where "no longer active" was.
#[test]
fn every_notice_headline_fits_its_line() {
    for p in [
        Posture::EvaluationEnded {
            stops_unix: T0 + DAY,
        },
        Posture::NoLongerActive {
            stops_unix: Some(T0 + DAY),
        },
        Posture::NoLongerActive { stops_unix: None },
        Posture::Locked { refused: true },
        Posture::Locked { refused: false },
    ] {
        let (head, _) = format::notice_text(p, Some("789K"));
        assert!(
            head.chars().count() <= format::NOTICE_HEADLINE_MAX_CHARS,
            "{p:?}: {head:?}"
        );
    }
    let (head, body) = format::notice_text(Posture::Locked { refused: true }, None);
    assert!(head.contains("no longer active"), "{head}");
    assert!(body.contains("Dictation is paused"), "{body}");
}

// ---- The store, end to end ----------------------------------------------------

/// A scratch HKCU key, removed when the test ends however it ends.
struct ScratchKey(String);
impl ScratchKey {
    fn new(tag: &str) -> Self {
        let path = format!(
            r"Software\LunarWerx\QuickDictate\LicenceTest-{tag}-{}",
            std::process::id()
        );
        let _ = windows_registry::CURRENT_USER.remove_tree(&path);
        Self(path)
    }
}
impl Drop for ScratchKey {
    fn drop(&mut self) {
        let _ = windows_registry::CURRENT_USER.remove_tree(&self.0);
    }
}

/// Redeem, go quiet past the certificate's expiry, get refused on replay, then
/// redeem another key: every step through the real registry and DPAPI.
#[test]
fn the_store_carries_a_licence_through_expiry_refusal_and_a_new_key() {
    let scratch = ScratchKey::new("flow");
    let s = store::Store::at(&scratch.0);
    assert_eq!(s.load_facts(INSIDE), Facts::default(), "nothing stored yet");

    s.set_mode(Mode::Business);
    assert!(s.start_evaluation_if_due(T0));
    assert!(!s.start_evaluation_if_due(T0 + DAY), "set once");
    // The test certificate names this subject.
    windows_registry::CURRENT_USER
        .create(&scratch.0)
        .unwrap()
        .set_string("InstallId", REAL_SUB)
        .unwrap();

    let key = "esk_ABCDE-12345-FGHIJ-6789K";
    let accepted = Reply::Accepted {
        replayed: false,
        certificate: Some(REAL_CERT.into()),
        certificate_error: None,
    };
    assert_eq!(
        s.apply_redeem(key, accepted, INSIDE, false),
        RedeemReport::Licensed {
            plan: Plan::Perpetual
        }
    );
    let f = s.load_facts(INSIDE);
    assert_eq!(
        posture(INSIDE, &f),
        Posture::Licensed {
            plan: Plan::Perpetual,
            cert_exp_unix: REAL_EXP
        }
    );
    assert_eq!(f.key_last4.as_deref(), Some("789K"));
    assert_eq!(s.stored_key().as_deref(), Some(key), "sealed and unsealed");
    let raw = windows_registry::CURRENT_USER
        .open(&scratch.0)
        .unwrap()
        .get_string("Key")
        .unwrap();
    assert!(
        !raw.contains("ABCDE"),
        "the key is never stored in the clear"
    );

    // Offline past expiry: still working, and renewal wants to run.
    let later = REAL_EXP + 30 * DAY;
    assert_eq!(
        posture(later, &s.load_facts(later)),
        Posture::LicensedRenewing {
            plan: Some(Plan::Perpetual)
        }
    );
    assert!(renewal_due(later, &s.renewal_inputs(later), false));

    // A no-answer renewal changes nothing; a refusal ends it.
    s.apply_redeem(key, Reply::Unavailable, later, true);
    assert!(matches!(
        posture(later, &s.load_facts(later)),
        Posture::LicensedRenewing { .. }
    ));
    s.apply_redeem(key, Reply::Refused, later, true);
    assert_eq!(
        posture(later, &s.load_facts(later)),
        Posture::NoLongerActive {
            stops_unix: Some(later + 3 * DAY)
        }
    );
    assert!(!renewal_due(later, &s.renewal_inputs(later), true));

    // Another key, accepted with no certificate yet: it waits beside the
    // refused one and changes nothing until a certificate verifies.
    let other = "esk_ZZZZZ-YYYYY-XXXXX-WWWW1";
    let report = s.apply_redeem(
        other,
        Reply::Accepted {
            replayed: false,
            certificate: None,
            certificate_error: Some("signing_key_unavailable".into()),
        },
        later,
        false,
    );
    assert_eq!(
        report,
        RedeemReport::AcceptedNoCertificate {
            reason: "signing_key_unavailable".into()
        }
    );
    let f = s.load_facts(later);
    assert_eq!(
        posture(later, &f),
        Posture::NoLongerActive {
            stops_unix: Some(later + 3 * DAY)
        }
    );
    assert_eq!(f.pending_reason.as_deref(), Some("signing_key_unavailable"));
    assert_eq!(f.key_last4.as_deref(), Some("789K"));
    assert_eq!(f.pending_key_last4.as_deref(), Some("WWW1"));
    assert_eq!(s.stored_key().as_deref(), Some(key));
    // Renewal retries the pending key, hourly after that failure, and never
    // the refused one.
    assert!(s.keys_due_for_renewal(later + 60, false).is_empty());
    assert_eq!(
        s.keys_due_for_renewal(later + 3600, false),
        vec![other.to_string()]
    );
}

#[test]
fn a_different_key_refused_leaves_the_held_licence_alone() {
    let scratch = ScratchKey::new("other");
    let s = store::Store::at(&scratch.0);
    windows_registry::CURRENT_USER
        .create(&scratch.0)
        .unwrap()
        .set_string("InstallId", REAL_SUB)
        .unwrap();
    let key = "esk_ABCDE-12345-FGHIJ-6789K";
    s.apply_redeem(
        key,
        Reply::Accepted {
            replayed: false,
            certificate: Some(REAL_CERT.into()),
            certificate_error: None,
        },
        INSIDE,
        false,
    );
    let report = s.apply_redeem("esk_QQQQQ-QQQQQ-QQQQQ-QQQQQ", Reply::Refused, INSIDE, false);
    assert!(matches!(report, RedeemReport::Rejected { .. }));
    assert!(matches!(
        posture(INSIDE, &s.load_facts(INSIDE)),
        Posture::Licensed { .. }
    ));
}

// ---- An accepted redeem: verify first, write after (review findings 1,4,5,10)

const K1: &str = "esk_ABCDE-12345-FGHIJ-6789K";
const K2: &str = "esk_ZZZZZ-YYYYY-XXXXX-WWWW1";

/// The way back from Business changes the mode and nothing else: answering
/// Business again later finds the old evaluation stamp, so flipping cannot
/// buy a fresh week.
#[test]
fn switching_to_free_use_keeps_the_evaluation_clock() {
    let scratch = ScratchKey::new("free-switch");
    let s = store::Store::at(&scratch.0);
    s.set_mode(Mode::Business);
    assert!(s.start_evaluation_if_due(T0));
    switch_store_to_free_use(&s);
    assert_eq!(s.mode(), Some(Mode::Personal));
    let free = s.load_facts(T0 + 30 * DAY);
    assert_eq!(posture(T0 + 30 * DAY, &free), Posture::Personal);
    s.set_mode(Mode::Business);
    assert!(!s.start_evaluation_if_due(T0 + 30 * DAY), "no fresh week");
    let again = s.load_facts(T0 + 30 * DAY);
    assert_eq!(again.evaluation_started_unix, T0);
    assert!(matches!(
        posture(T0 + 30 * DAY, &again),
        Posture::Locked { refused: false }
    ));
}

/// A store holding nothing but this install's id, the one the test
/// certificate names.
fn scratch_store(tag: &str, sub: &str) -> (ScratchKey, store::Store) {
    let scratch = ScratchKey::new(tag);
    let s = store::Store::at(&scratch.0);
    windows_registry::CURRENT_USER
        .create(&scratch.0)
        .unwrap()
        .set_string("InstallId", sub)
        .unwrap();
    (scratch, s)
}

fn with_cert(cert: &str) -> Reply {
    Reply::Accepted {
        replayed: false,
        certificate: Some(cert.into()),
        certificate_error: None,
    }
}

fn no_cert() -> Reply {
    Reply::Accepted {
        replayed: false,
        certificate: None,
        certificate_error: Some("signing_key_unavailable".into()),
    }
}

/// REAL_CERT with one payload character changed: a 200 whose certificate
/// fails the signature, as anything not signed for us does.
fn tampered_cert() -> String {
    let (p, sig) = REAL_CERT.split_once('.').unwrap();
    let mut bad = p.to_string();
    bad.replace_range(10..11, if &p[10..11] == "A" { "B" } else { "A" });
    format!("{bad}.{sig}")
}

fn holding_k1(tag: &str) -> (ScratchKey, store::Store) {
    let (scratch, s) = scratch_store(tag, REAL_SUB);
    assert_eq!(
        s.apply_redeem(K1, with_cert(REAL_CERT), INSIDE, false),
        RedeemReport::Licensed {
            plan: Plan::Perpetual
        }
    );
    (scratch, s)
}

fn assert_holds_k1(s: &store::Store) {
    let f = s.load_facts(INSIDE);
    assert_eq!(
        posture(INSIDE, &f),
        Posture::Licensed {
            plan: Plan::Perpetual,
            cert_exp_unix: REAL_EXP
        }
    );
    assert_eq!(s.stored_key().as_deref(), Some(K1));
    assert_eq!(f.key_last4.as_deref(), Some("789K"));
    assert_eq!(f.refused_unix, 0);
}

fn rejected_with(report: &RedeemReport, words: &str) {
    match report {
        RedeemReport::Rejected { message } => assert!(message.contains(words), "{message}"),
        other => panic!("expected a rejection, got {other:?}"),
    }
}

#[test]
fn an_accepted_certificate_is_judged_before_anything_is_written() {
    let ok = cert::verify(REAL_CERT, REAL_SUB, INSIDE).unwrap();
    assert_eq!(
        store::judge_accepted(Some(REAL_CERT.into()), None, |_| Ok(ok)),
        store::Accepted::Licence {
            certificate: REAL_CERT.into(),
            verified: ok
        }
    );
    assert_eq!(
        store::judge_accepted(None, Some("signing_key_unavailable".into()), |_| {
            panic!("nothing to verify")
        }),
        store::Accepted::Pending {
            reason: "signing_key_unavailable".into()
        }
    );
    for error in [
        CertError::Malformed,
        CertError::BadEncoding,
        CertError::BadSignature,
        CertError::BadPayload,
        CertError::WrongAudience,
        CertError::WrongProduct,
    ] {
        assert_eq!(
            store::judge_accepted(Some(REAL_CERT.into()), None, |_| Err(error)),
            store::Accepted::Refuse { error }
        );
        assert_eq!(
            store::certificate_refusal(error),
            "That key is for a different product, not QuickDictate."
        );
    }
    assert!(store::certificate_refusal(CertError::NotThisInstall).contains("another installation"));
    assert!(store::certificate_refusal(CertError::Expired).contains("clock"));
}

/// A valid key for another Connections product gets a 200 from the
/// platform-wide door, with that product's certificate. The licence held
/// must come through untouched.
#[test]
fn a_foreign_product_certificate_leaves_a_held_licence_untouched() {
    let (_scratch, s) = holding_k1("foreign-held");
    let report = s.apply_accepted(
        K2,
        store::judge_accepted(Some("x.y".into()), None, |_| Err(CertError::WrongProduct)),
        INSIDE,
        false,
    );
    rejected_with(&report, "different product");
    assert_holds_k1(&s);

    // The same through the real verify: a certificate signed for nothing here.
    let report = s.apply_redeem(K2, with_cert(&tampered_cert()), INSIDE, false);
    rejected_with(&report, "different product");
    assert_holds_k1(&s);
}

#[test]
fn a_foreign_product_certificate_does_not_unlock_a_locked_evaluation() {
    let (_scratch, s) = scratch_store("foreign-locked", REAL_SUB);
    s.set_mode(Mode::Business);
    assert!(s.start_evaluation_if_due(T0));
    let day11 = T0 + 11 * DAY;
    assert_eq!(
        posture(day11, &s.load_facts(day11)),
        Posture::Locked { refused: false }
    );
    let report = s.apply_accepted(
        K2,
        store::judge_accepted(Some("x.y".into()), None, |_| Err(CertError::WrongProduct)),
        day11,
        false,
    );
    rejected_with(&report, "different product");
    s.apply_redeem(K2, with_cert(&tampered_cert()), day11, false);
    let f = s.load_facts(day11);
    assert_eq!(posture(day11, &f), Posture::Locked { refused: false });
    assert_eq!(f.licensed_since_unix, 0);
    assert_eq!(s.stored_key(), None);
    assert!(s.keys_due_for_renewal(day11, true).is_empty());
}

#[test]
fn another_installs_or_an_already_expired_certificate_stores_nothing() {
    let (_scratch, s) = scratch_store("not-ours", "qd-some-other-install");
    let report = s.apply_redeem(K1, with_cert(REAL_CERT), INSIDE, false);
    rejected_with(&report, "another installation");
    assert_eq!(s.stored_key(), None);
    assert_eq!(s.load_facts(INSIDE).licensed_since_unix, 0);

    let (_scratch, s) = scratch_store("clock-ahead", REAL_SUB);
    let ahead = REAL_EXP + 400 * DAY;
    let report = s.apply_redeem(K1, with_cert(REAL_CERT), ahead, false);
    rejected_with(&report, "clock");
    assert_eq!(s.stored_key(), None);
    assert_eq!(s.load_facts(ahead).licensed_since_unix, 0);
}

/// A 200 with no certificate proves the key is good for something, not that
/// it is good for QuickDictate: it waits, licensing nothing and displacing
/// nothing, until renewal brings a certificate that verifies.
#[test]
fn an_accept_with_no_certificate_licenses_nothing_until_one_verifies() {
    let (_scratch, s) = scratch_store("pending", REAL_SUB);
    s.set_mode(Mode::Business);
    assert!(s.start_evaluation_if_due(CLOCK_FLOOR_UNIX));
    let evaluating = posture(INSIDE, &s.load_facts(INSIDE));
    assert!(matches!(evaluating, Posture::Evaluation { .. }));

    assert!(matches!(
        s.apply_redeem(K2, no_cert(), INSIDE, false),
        RedeemReport::AcceptedNoCertificate { .. }
    ));
    let f = s.load_facts(INSIDE);
    assert_eq!(posture(INSIDE, &f), evaluating);
    assert_eq!(f.licensed_since_unix, 0);
    assert_eq!(f.pending_key_last4.as_deref(), Some("WWW1"));
    assert_eq!(s.stored_key(), None);

    // Renewal replays it, and a certificate that verifies promotes it.
    assert_eq!(s.keys_due_for_renewal(INSIDE, true), vec![K2.to_string()]);
    assert!(matches!(
        s.apply_renewal(K2, with_cert(REAL_CERT), INSIDE),
        Some(RedeemReport::Licensed { .. })
    ));
    let f = s.load_facts(INSIDE);
    assert!(matches!(posture(INSIDE, &f), Posture::Licensed { .. }));
    assert_eq!(s.stored_key().as_deref(), Some(K2));
    assert_eq!(f.pending_key_last4, None);
    assert_eq!(f.pending_reason, None);
}

#[test]
fn an_accept_with_no_certificate_does_not_displace_a_held_licence() {
    let (_scratch, s) = holding_k1("pending-held");
    s.apply_redeem(K2, no_cert(), INSIDE, false);
    assert_holds_k1(&s);
    assert_eq!(
        s.load_facts(INSIDE).pending_key_last4.as_deref(),
        Some("WWW1")
    );
    // A refusal of the pending key forgets it and leaves the licence alone.
    s.apply_renewal(K2, Reply::Refused, INSIDE);
    assert_holds_k1(&s);
    assert_eq!(s.load_facts(INSIDE).pending_key_last4, None);
}

#[test]
fn a_renewal_whose_certificate_fails_changes_only_the_attempt_note() {
    let (_scratch, s) = holding_k1("renew-bad");
    let before = s.load_facts(INSIDE);
    let report = s.apply_renewal(K1, with_cert(&tampered_cert()), INSIDE + 60);
    assert!(matches!(report, Some(RedeemReport::Rejected { .. })));
    assert_eq!(s.load_facts(INSIDE), before);
    assert_holds_k1(&s);
    let r = s.renewal_inputs(INSIDE + 60);
    assert_eq!(r.last_attempt_unix, INSIDE + 60);
    assert!(!r.last_attempt_ok);
}

// ---- Renewal against a key redeemed meanwhile (review findings 2,11) ---------

/// The renewal worker read K1, then spent up to 30 s on the network while the
/// user redeemed K2. Its reply is about a key no longer held and must change
/// nothing, whatever it says.
#[test]
fn a_renewal_reply_for_a_replaced_key_is_dropped() {
    let (_scratch, s) = holding_k1("cas");
    s.apply_redeem(K2, with_cert(REAL_CERT), INSIDE, false);
    assert_eq!(s.stored_key().as_deref(), Some(K2));
    let before = s.load_facts(INSIDE);

    assert_eq!(s.apply_renewal(K1, Reply::Refused, INSIDE), None);
    assert_eq!(s.apply_renewal(K1, with_cert(REAL_CERT), INSIDE), None);
    assert_eq!(s.apply_renewal(K1, no_cert(), INSIDE), None);
    assert_eq!(s.load_facts(INSIDE), before);
    assert_eq!(s.stored_key().as_deref(), Some(K2));
    assert!(matches!(posture(INSIDE, &before), Posture::Licensed { .. }));
}

/// The user typed the key already held and Connections refused it: that is
/// news about the stored licence, the same as a refused replay (finding 12).
#[test]
fn a_typed_refusal_of_the_stored_key_ends_the_licence() {
    let (_scratch, s) = holding_k1("typed-refusal");
    s.set_mode(Mode::Business);
    s.apply_redeem(K1, Reply::Refused, INSIDE, false);
    assert_eq!(
        posture(INSIDE, &s.load_facts(INSIDE)),
        Posture::NoLongerActive {
            stops_unix: Some(INSIDE + 3 * DAY)
        }
    );
    assert!(!renewal_due(INSIDE, &s.renewal_inputs(INSIDE), true));
}

// ---- A clock that was behind (review finding 3) -------------------------------

#[test]
fn a_stamp_from_a_clock_that_was_behind_never_locks() {
    let behind = CLOCK_FLOOR_UNIX - 400 * DAY;
    let now = T0 + 30 * DAY;
    assert_eq!(
        posture(now, &business(behind)),
        Posture::Evaluation {
            ends_unix: now + 10 * DAY
        }
    );
    let refused = Facts {
        licensed_since_unix: behind,
        refused_unix: behind,
        ..business(behind)
    };
    assert_eq!(
        posture(now, &refused),
        Posture::NoLongerActive {
            stops_unix: Some(now + 3 * DAY)
        }
    );
}

#[test]
fn the_store_restamps_what_a_clock_that_was_behind_recorded() {
    let (scratch, s) = scratch_store("clock-behind", REAL_SUB);
    s.set_mode(Mode::Business);
    let behind = CLOCK_FLOOR_UNIX - 400 * DAY;
    assert!(s.start_evaluation_if_due(behind));
    // Still behind: nothing to restamp yet.
    assert!(!s.settle_clock(behind + DAY));
    // Put right: the evaluation runs from now.
    assert!(s.settle_clock(T0));
    assert_eq!(s.load_facts(T0).evaluation_started_unix, T0);
    assert!(!s.settle_clock(T0 + DAY), "set once");

    let key = windows_registry::CURRENT_USER.create(&scratch.0).unwrap();
    key.set_u64("Refused", behind).unwrap();
    assert!(s.settle_clock(T0));
    assert_eq!(s.load_facts(T0).refused_unix, T0);
    assert!(!s.settle_clock(T0 + DAY));
}
