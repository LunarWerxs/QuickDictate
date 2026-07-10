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

/// Decode `png` to raw RGBA8 at `size`² (Lanczos-resampled). Native transparency
/// is preserved. Returns `(rgba, size, size)`.
fn decode(png: &[u8], size: u32) -> (Vec<u8>, u32, u32) {
    let s = size.max(1);
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
