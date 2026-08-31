//! The artwork the card draws: the product tile, the LunarWerx wordmark, and
//! the GitHub mark, decoded and composited onto the theme background.

use windows::Win32::Foundation::COLORREF;
use windows::Win32::Graphics::Gdi::HBITMAP;

use crate::theme::{is_dark, DARK_BG};

use super::*;

/// LunarWerx Studios wordmark — the LIGHT (white) variant on transparent
/// (1680×273), for the dark card.
const LW_LOGO_PNG: &[u8] = include_bytes!("../../assets/lw_logo_white.png");
/// LunarWerx Studios wordmark — the DARK (navy) variant on transparent
/// (4911×941), for the light card.
const LW_LOGO_DARK_PNG: &[u8] = include_bytes!("../../assets/lw_logo_dark.png");
/// GitHub "mark" (white silhouette on transparent) for the version pill.
const GH_PNG: &[u8] = include_bytes!("../../assets/github_mark.png");

// ---- Artwork -------------------------------------------------------------

/// The QuickDictate logo (the blue mic tile — same art as the tray / exe icon,
/// see `crate::icon`), decoded and scaled to `size`² and flattened onto the
/// card background so the tile's rounded corners match the card. Mirrors
/// `lw_logo_hbitmap` above.
#[allow(
    clippy::expect_used,
    reason = "the bytes are include_bytes! of a committed PNG, and embedded_artwork_decodes forces this path in the test suite"
)]
pub(super) fn qd_logo_rgba(size: u32) -> Vec<u8> {
    let base = DARK_BG();
    let sz = size.max(1);
    let logo = image::load_from_memory(crate::icon::PNG)
        .expect("decode app icon")
        .resize_exact(sz, sz, image::imageops::FilterType::Lanczos3)
        .into_rgba8();
    let mut out = image::RgbaImage::from_pixel(
        sz,
        sz,
        image::Rgba([color_r(base), color_g(base), color_b(base), 255]),
    );
    image::imageops::overlay(&mut out, &logo, 0, 0);
    out.into_raw()
}

/// The LunarWerx Studios wordmark bytes + aspect ratio for the active theme.
pub(super) fn lw_logo() -> (&'static [u8], f32) {
    if is_dark() {
        (LW_LOGO_PNG, 1680.0 / 273.0)
    } else {
        (LW_LOGO_DARK_PNG, 4911.0 / 941.0)
    }
}

/// The themed LunarWerx wordmark sized to `w`×`h`, composited onto the card
/// background (SS_BITMAP BitBlts — no alpha — so we pre-composite).
pub(super) unsafe fn lw_logo_hbitmap(w: u32, h: u32) -> Option<HBITMAP> {
    let (bytes, _) = lw_logo();
    let logo = image::load_from_memory(bytes)
        .ok()?
        .resize_exact(w, h, image::imageops::FilterType::Lanczos3)
        .to_rgba8();
    let base = DARK_BG();
    let mut out = image::RgbaImage::from_pixel(
        w,
        h,
        image::Rgba([color_r(base), color_g(base), color_b(base), 255]),
    );
    image::imageops::overlay(&mut out, &logo, 0, 0);
    rgba_to_hbitmap(w, h, out.as_raw())
}

/// The GitHub mark at `px`², tinted `fg` and composited over `fill` (the pill
/// face), so it can be BitBlt'd straight onto the pill with no alpha-blend.
pub(super) unsafe fn github_icon_hbitmap(px: u32, fill: COLORREF, fg: COLORREF) -> Option<HBITMAP> {
    let src = image::load_from_memory(GH_PNG)
        .ok()?
        .resize_exact(px.max(1), px.max(1), image::imageops::FilterType::Lanczos3)
        .to_rgba8();
    let (fr, fgc, fb) = (color_r(fill), color_g(fill), color_b(fill));
    let (gr, gg, gb) = (color_r(fg), color_g(fg), color_b(fg));
    let mut out = image::RgbaImage::new(src.width(), src.height());
    for (o, p) in out.pixels_mut().zip(src.pixels()) {
        let a = p[3]; // octocat coverage
        *o = image::Rgba([blend(fr, gr, a), blend(fgc, gg, a), blend(fb, gb, a), 255]);
    }
    rgba_to_hbitmap(out.width(), out.height(), out.as_raw())
}
