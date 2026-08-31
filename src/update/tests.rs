//! Tests for tag parsing, asset selection, and download verification.

use super::install_id::new_install_id;
use super::*;

#[test]
fn parses_plain_and_prefixed_tags() {
    assert_eq!(parse_ver("0.1.0"), Some((0, 1, 0, 1)));
    assert_eq!(parse_ver("v1.2.3"), Some((1, 2, 3, 1)));
    assert_eq!(parse_ver("V2.0"), Some((2, 0, 0, 1)));
    assert_eq!(parse_ver("1.2.3-rc1"), Some((1, 2, 3, 0)));
    assert_eq!(parse_ver("1.2.3+build7"), Some((1, 2, 3, 1)));
    assert_eq!(parse_ver("garbage"), None);
    assert_eq!(parse_ver(""), None);
}

#[test]
fn tuple_compare_orders_versions() {
    assert!(parse_ver("0.2.0") > parse_ver("0.1.9"));
    assert!(parse_ver("1.0.0") > parse_ver("0.99.99"));
    assert!(parse_ver("0.1.0") == parse_ver("v0.1.0"));
    assert!(parse_ver("0.1.1") > parse_ver("0.1.0"));
}

#[test]
fn a_final_release_outranks_its_own_prerelease() {
    // Semver: 1.0.0-rc1 < 1.0.0. Stripping the suffix made these compare
    // equal, so the real 1.0.0 was reported as "up to date" and never
    // delivered to anyone running the rc.
    assert!(parse_ver("1.0.0") > parse_ver("1.0.0-rc1"));
    assert!(parse_ver("1.0.0-rc2") > parse_ver("0.9.9"));
    // Build metadata does not affect precedence.
    assert_eq!(parse_ver("1.0.0+abc"), parse_ver("1.0.0"));
}

#[test]
fn install_id_is_a_lowercase_v4_uuid_and_unique() {
    let a = new_install_id().expect("system RNG available");
    let b = new_install_id().expect("system RNG available");
    assert_ne!(a, b, "two ids must not collide");
    assert_eq!(a.len(), 36);
    for (i, ch) in a.chars().enumerate() {
        match i {
            8 | 13 | 18 | 23 => assert_eq!(ch, '-', "dash expected at {i} in {a}"),
            _ => assert!(
                matches!(ch, '0'..='9' | 'a'..='f'),
                "lowercase hex expected at {i} in {a}"
            ),
        }
    }
    assert_eq!(&a[14..15], "4", "version nibble in {a}");
    assert!(
        matches!(&a[19..20], "8" | "9" | "a" | "b"),
        "RFC 4122 variant nibble in {a}"
    );
}

#[test]
#[ignore = "live network"]
fn live_studio_latest_release_parses() {
    // The Studio proxy must relay GitHub's releases/latest JSON verbatim —
    // the same fields check() and latest_exe_asset() consume. NOTE: each
    // run logs one anonymous analytics row on the endpoint.
    let resp = client()
        .unwrap()
        .get(RELEASES_API)
        .send()
        .expect("Studio endpoint reachable");
    assert!(resp.status().is_success(), "HTTP {}", resp.status());
    let json: serde_json::Value = resp.json().unwrap();
    let tag = json.get("tag_name").and_then(|v| v.as_str()).unwrap();
    println!("latest QuickDictate tag = {tag}");
    assert!(parse_ver(tag).is_some(), "tag {tag} should parse");
}

#[test]
#[ignore = "live network"]
fn live_github_fallback_parses() {
    // Exercises the REAL fallback URL a shipped binary uses when the Studio proxy fails,
    // not a stand-in. This used to point at the sibling SageThumbs repo purely to sample
    // GitHub's response shape, which validated the shape but never the address this binary
    // actually falls back to, and that address is the one thing here that can go wrong.
    let resp = client()
        .unwrap()
        .get(GITHUB_LATEST_API)
        .send()
        .expect("GitHub API reachable");
    assert!(resp.status().is_success(), "HTTP {}", resp.status());
    let json: serde_json::Value = resp.json().unwrap();
    let tag = json.get("tag_name").and_then(|v| v.as_str()).unwrap();
    println!("latest QuickDictate tag via GitHub fallback = {tag}");
    assert!(parse_ver(tag).is_some(), "tag {tag} should parse");
}

#[test]
fn github_fallback_url_targets_this_projects_releases() {
    // Offline guard so CI catches a typo in the fallback address without needing network.
    // A compiled binary cannot be repointed after release: if this constant is wrong,
    // every install that outlives the Studio proxy is stranded permanently, which is
    // precisely the YTSort failure this fallback exists to prevent.
    assert!(
        GITHUB_LATEST_API.starts_with("https://api.github.com/repos/"),
        "fallback must be GitHub's API, got {GITHUB_LATEST_API}"
    );
    assert!(
        GITHUB_LATEST_API.ends_with("/LunarWerxs/QuickDictate/releases/latest"),
        "fallback must target this project's releases, got {GITHUB_LATEST_API}"
    );
    // If the primary were also api.github.com there would be no second opinion at all.
    assert!(
        !RELEASES_API.starts_with("https://api.github.com/"),
        "primary and fallback must be different services"
    );
}

#[test]
fn sha256_matches_known_vector() {
    // SHA-256("abc") — canonical NIST test vector.
    assert_eq!(
        sha256_hex(b"abc").as_deref(),
        Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
    );
}

#[test]
fn verify_rejects_bad_bytes() {
    let good_hash = sha256_hex(b"MZ\x90\x00").expect("system SHA-256");
    let asset = Asset {
        url: String::new(),
        size: 4,
        sha256: good_hash,
    };
    assert!(!verify_exe_bytes(b"PK\x03\x04", &asset)); // not MZ
    assert!(verify_exe_bytes(b"MZ\x90\x00", &asset)); // MZ + right size
    let wrong_size = Asset {
        url: String::new(),
        size: 5,
        sha256: sha256_hex(b"MZ\x90\x00").expect("system SHA-256"),
    };
    assert!(!verify_exe_bytes(b"MZ\x90\x00", &wrong_size));
    let bad_hash = Asset {
        url: String::new(),
        size: 4,
        sha256: "00".repeat(32),
    };
    assert!(!verify_exe_bytes(b"MZ\x90\x00", &bad_hash));
}

#[test]
fn updater_selects_only_the_exact_portable_executable() {
    let digest = format!("sha256:{}", "ab".repeat(32));
    let json = serde_json::json!({
        "tag_name": "v0.5.2",
        "assets": [
            {
                "name": "quickdictate-debug.exe",
                "browser_download_url": "https://github.com/LunarWerxs/QuickDictate/releases/download/v0.5.2/quickdictate-debug.exe",
                "size": 99,
                "digest": digest
            },
            {
                "name": "quickdictate.exe",
                "browser_download_url": "https://github.com/LunarWerxs/QuickDictate/releases/download/v0.5.2/quickdictate.exe",
                "size": 42,
                "digest": format!("sha256:{}", "cd".repeat(32))
            }
        ]
    });
    let (tag, asset) = exe_asset_from_json(&json).expect("exact release executable");
    assert_eq!(tag, "0.5.2");
    assert_eq!(asset.size, 42);
    assert!(asset.url.ends_with("/quickdictate.exe"));
    assert_eq!(asset.sha256, "cd".repeat(32));
}

#[test]
fn updater_accepts_only_this_projects_github_release_assets() {
    assert!(trusted_asset_url(
        "https://github.com/LunarWerxs/QuickDictate/releases/download/v0.4.3/quickdictate.exe"
    ));
    assert!(!trusted_asset_url(
        "https://example.com/LunarWerxs/QuickDictate/quickdictate.exe"
    ));
    assert!(!trusted_asset_url(
        "http://github.com/LunarWerxs/QuickDictate/releases/download/v0.4.3/quickdictate.exe"
    ));
    assert!(!trusted_asset_url(
        "https://github.com/OtherOwner/QuickDictate/releases/download/v0.4.3/quickdictate.exe"
    ));
}
