//! Build script: embeds the committed multi-resolution Windows .ico
//! (`assets/quickdictate.ico` — the blue mic tile) into the .exe via a tiny
//! .rc file. Without an icon, Explorer/Taskbar show the generic "no icon"
//! placeholder, which looks sketchy.
//!
//! The .ico is generated offline from `QuickDictate Icon.svg` (the same art the
//! tray / settings window / About card load at runtime — see `src/icon.rs` and
//! `assets/icon-256.png`), so this script just points the resource at it; there
//! is no procedural fallback art here anymore.

use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/quickdictate.ico");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let ico_path = manifest.join("assets").join("quickdictate.ico");
    assert!(
        ico_path.exists(),
        "app icon missing: {} (regenerate it from `QuickDictate Icon.svg`)",
        ico_path.display()
    );

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let rc_path = out_dir.join("app.rc");
    // Resource ID 1 with type ICON is the convention Windows Explorer uses for
    // "the application icon".
    //
    // The VERSIONINFO block matters beyond cosmetics: an unsigned exe that
    // registers global hotkeys and synthesizes keystrokes trips AV/SmartScreen
    // heuristics, and a populated version resource measurably reduces those
    // false positives (same rationale as SageThumbs 2K's installer).
    let version = env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION");
    let mut parts = version.split('.');
    let (maj, min, pat) = (
        parts.next().unwrap_or("0"),
        parts.next().unwrap_or("0"),
        parts.next().unwrap_or("0"),
    );
    let rc_contents = format!(
        r#"1 ICON "{ico}"

1 VERSIONINFO
FILEVERSION {maj},{min},{pat},0
PRODUCTVERSION {maj},{min},{pat},0
FILEOS 0x40004L
FILETYPE 0x1L
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "Lunarwerx"
      VALUE "FileDescription", "QuickDictate - bring-your-own-key dictation for Windows"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "quickdictate"
      VALUE "LegalCopyright", "(c) 2026 Lunarwerx. MIT License."
      VALUE "OriginalFilename", "quickdictate.exe"
      VALUE "ProductName", "QuickDictate"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x0409, 0x04B0
  END
END
"#,
        ico = ico_path.display().to_string().replace('\\', "/"),
    );
    fs::write(&rc_path, rc_contents).expect("write app.rc");

    embed_resource::compile(&rc_path, embed_resource::NONE);
}
