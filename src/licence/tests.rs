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
const T0: u64 = 1_790_000_000;

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

/// Days 1-7 work quietly, days 8-10 work with a notice, day 11 on is locked.
#[test]
fn the_business_evaluation_runs_seven_days_then_three_of_notice_then_locks() {
    let f = business(T0);
    let ends = T0 + 7 * DAY;
    let stops = T0 + 10 * DAY;

    // Day 0: the moment it starts.
    assert_eq!(posture(T0, &f), Posture::Evaluation { ends_unix: ends });
    assert_eq!(days_until(T0, ends), 7);
    assert_eq!(gate(posture(T0, &f)), Gate::Allow);

    // Day 7: its last second is still the evaluation, with a day left.
    let day7 = ends - 1;
    assert_eq!(posture(day7, &f), Posture::Evaluation { ends_unix: ends });
    assert_eq!(days_until(day7, ends), 1);

    // Day 8: the notice period, dictation still starts.
    assert_eq!(
        posture(ends, &f),
        Posture::EvaluationEnded { stops_unix: stops }
    );
    assert_eq!(gate(posture(ends, &f)), Gate::AllowWithNotice);

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
            ends_unix: T0 + 7 * DAY
        }
    );
    assert_eq!(
        days_until(before, T0 + 7 * DAY),
        7,
        "never more than 7 days"
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
    };
    let (head, detail) = format::status_lines(&s);
    assert_eq!(head, "Licensed: perpetual");
    assert!(detail.contains("key ending 789K"), "{detail}");
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

    // Another key, accepted with no certificate yet: licensed again, with the
    // reason on record and the old certificate gone.
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
    assert_eq!(posture(later, &f), Posture::LicensedRenewing { plan: None });
    assert_eq!(f.pending_reason.as_deref(), Some("signing_key_unavailable"));
    assert_eq!(f.key_last4.as_deref(), Some("WWW1"));
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
