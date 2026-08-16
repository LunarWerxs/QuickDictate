//! The QuickDictate product icon (the blue mic tile), embedded once and decoded
//! on demand. Shared by the tray (`ui`), the settings-window icon
//! (`settings_ui`), and the About card (`about`) so all three show the same
//! art as the exe's embedded .ico (see `build.rs`, which embeds the matching
//! `assets/quickdictate.ico`). Source of truth: `QuickDictate Icon.svg`,
//! rasterized to `assets/icon-256.png`.

use image::imageops::FilterType;

/// The main product icon as a 256² PNG (rasterized from `QuickDictate Icon.svg`
/// — the filled blue mic *tile*). Used for the exe, the settings window, and the
/// About card. The `image` crate is built with only the `png` feature, so these
/// must stay PNG.
pub const PNG: &[u8] = include_bytes!("../assets/icon-256.png");

/// The notification-area (system tray) variant — the mic glyph on a *transparent*
/// field, no filled tile — so it reads cleanly at ~16-24px in the tray instead of
/// looking like a solid blue box. Source: `QuickDictate Notification.svg`.
pub const NOTIFICATION_PNG: &[u8] = include_bytes!("../assets/notification-256.png");

/// Clamp a requested icon size to at least 1px — a 0x0 bitmap is invalid, and
/// callers may pass a size derived from a shrunk-to-nothing window/DPI calc.
fn clamp_size(size: u32) -> u32 {
    size.max(1)
}

/// Decode `png` to raw RGBA8 at `size`² (Lanczos-resampled). Native transparency
/// is preserved. Returns `(rgba, size, size)`.
#[allow(
    clippy::expect_used,
    reason = "the bytes are include_bytes! of a committed PNG, and embedded_artwork_decodes forces this path in the test suite"
)]
fn decode(png: &[u8], size: u32) -> (Vec<u8>, u32, u32) {
    let s = clamp_size(size);
    let img = image::load_from_memory(png)
        .expect("decode embedded app icon")
        .resize_exact(s, s, FilterType::Lanczos3)
        .into_rgba8();
    (img.into_raw(), s, s)
}

/// The main product icon (mic tile) at `size`².
pub fn rgba(size: u32) -> (Vec<u8>, u32, u32) {
    decode(PNG, size)
}

/// The system-tray / notification-area variant at `size`².
pub fn notification_rgba(size: u32) -> (Vec<u8>, u32, u32) {
    decode(NOTIFICATION_PNG, size)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `decode` is allowed to `.expect()` because its input is `include_bytes!`
    /// of a committed PNG. That is only true while SOMETHING forces the decode
    /// at build time -- otherwise a corrupt or replaced asset ships and aborts
    /// the tray thread on first paint, with no console to say why. This is that
    /// something; the same guarantee covers `about::qd_logo_rgba` and
    /// `ui::make_icon`, which decode the same two assets.
    #[test]
    fn embedded_artwork_decodes() {
        for (name, bytes) in [("icon-256", PNG), ("notification-256", NOTIFICATION_PNG)] {
            let img = image::load_from_memory(bytes)
                .unwrap_or_else(|e| panic!("embedded {name}.png failed to decode: {e}"));
            assert!(
                img.width() > 0 && img.height() > 0,
                "embedded {name}.png decoded to an empty image"
            );
        }
        // And through the real entry points, at the sizes the app asks for.
        let (rgba, w, h) = rgba(32);
        assert_eq!(rgba.len(), (w * h * 4) as usize);
        let (rgba, w, h) = notification_rgba(32);
        assert_eq!(rgba.len(), (w * h * 4) as usize);
    }

    #[test]
    fn clamp_size_raises_zero_to_one() {
        assert_eq!(clamp_size(0), 1);
    }

    #[test]
    fn clamp_size_leaves_a_positive_size_unchanged() {
        assert_eq!(clamp_size(256), 256);
        assert_eq!(clamp_size(1), 1);
    }
}
