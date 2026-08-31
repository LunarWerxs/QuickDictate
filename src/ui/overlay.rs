//! The cursor-following status pip: a layered window over a 32-bit
//! premultiplied-alpha DIB, which is what makes the circle and its text
//! genuinely anti-aliased rather than a 1-bit octagon.

use std::sync::atomic::Ordering;

use anyhow::Result;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject, DrawTextW, GetDC,
    ReleaseDC, SelectObject, SetBkMode, SetTextColor, AC_SRC_ALPHA, AC_SRC_OVER,
    ANTIALIASED_QUALITY, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, CLIP_DEFAULT_PRECIS,
    DEFAULT_CHARSET, DIB_RGB_COLORS, DT_CENTER, DT_SINGLELINE, DT_VCENTER, FF_DONTCARE, FW_BOLD,
    HBITMAP, HDC, HFONT, OUT_DEFAULT_PRECIS, TRANSPARENT, VARIABLE_PITCH,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::HMENU;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, LoadCursorW, PostQuitMessage, RegisterClassExW, ShowWindow,
    UpdateLayeredWindow, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, IDC_ARROW, SW_HIDE, SW_SHOWNA,
    ULW_ALPHA, WM_DESTROY, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

use crate::state::{ErrorKind, Status};

use super::*;

// ===== Layered overlay =====

/// Owns the layered window plus an in-memory 32-bit BGRA bitmap we render
/// into and then ship to the screen via `UpdateLayeredWindow`. All fields
/// are accessed only from the UI thread.
pub(super) struct Overlay {
    pub(super) hwnd: HWND,
    pub(super) mem_dc: HDC,
    pub(super) bitmap: HBITMAP,
    pub(super) pixels: *mut u32,
    pub(super) size: i32,
    pub(super) visible: std::cell::Cell<bool>,
    /// Cached label fonts, created once in `create` and sized off `size`.
    /// `render` used to `CreateFontW`/`DeleteObject` a fresh font on every
    /// repaint -- up to every `ACTIVE_POLL_INTERVAL` (16ms) while the pip is
    /// visible -- which is pure per-frame churn for something that never
    /// changes. `size` is fixed for the overlay's whole lifetime (nothing in
    /// this file resizes it), so there's no cache-invalidation path here; a
    /// future resize feature would need to rebuild these alongside it.
    pub(super) font_ui: HFONT,
    pub(super) font_icon: HFONT,
}

impl Overlay {
    pub(super) unsafe fn create(size: i32) -> Result<Self> {
        let class_name: Vec<u16> = OVERLAY_CLASS_NAME
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let h_module = GetModuleHandleW(PCWSTR::null())?;
        let h_instance = HINSTANCE(h_module.0);

        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(overlay_wnd_proc),
            hInstance: h_instance,
            lpszClassName: PCWSTR(class_name.as_ptr()),
            hCursor: LoadCursorW(HINSTANCE::default(), IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        let atom = RegisterClassExW(&wc);
        if atom == 0 {
            let err = windows::Win32::Foundation::GetLastError();
            if err.0 != 1410 {
                anyhow::bail!("RegisterClassExW failed: {:?}", err);
            }
        }

        // WS_EX_LAYERED is required for UpdateLayeredWindow; we deliberately
        // DO NOT call SetLayeredWindowAttributes -- the two APIs are mutually
        // exclusive on a given window.
        let ex_style =
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW;
        let style = WS_POPUP;
        let hwnd = CreateWindowExW(
            ex_style,
            PCWSTR(class_name.as_ptr()),
            PCWSTR::null(),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            size,
            size,
            HWND::default(),
            HMENU::default(),
            h_instance,
            None,
        )?;

        // 32-bit top-down BGRA DIB section. CreateDIBSection hands us the raw
        // pixel buffer so we can write directly without GDI's rasterizer.
        let screen_dc = GetDC(HWND::default());
        let mem_dc = CreateCompatibleDC(screen_dc);
        let _ = ReleaseDC(HWND::default(), screen_dc);

        let mut bmi: BITMAPINFO = std::mem::zeroed();
        bmi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size,
            biHeight: -size, // negative => top-down rows
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            biSizeImage: 0,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        };
        let mut pixels_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
        let bitmap = CreateDIBSection(mem_dc, &bmi, DIB_RGB_COLORS, &mut pixels_ptr, None, 0)?;
        if pixels_ptr.is_null() {
            anyhow::bail!("CreateDIBSection returned null pixel pointer");
        }
        SelectObject(mem_dc, bitmap);

        // Built once here rather than per-repaint -- see the `font_ui`/
        // `font_icon` doc comment on the struct. Two distinct fonts: the
        // MDL2 icon font for the (single) icon glyph, and a plain bold UI
        // font for everything else (word counts and the short error labels
        // from `error_glyph`).
        let font_ui = create_label_font(size, 0.45, "Segoe UI\0");
        let font_icon = create_label_font(size, 0.52, "Segoe MDL2 Assets\0");

        Ok(Self {
            hwnd,
            mem_dc,
            bitmap,
            pixels: pixels_ptr as *mut u32,
            size,
            visible: std::cell::Cell::new(false),
            font_ui,
            font_icon,
        })
    }

    pub(super) unsafe fn hide(&self) {
        if self.visible.get() {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
            self.visible.set(false);
        }
    }

    /// Software-render the disc + word count into the DIB, then ship it to
    /// the screen via UpdateLayeredWindow. Pixels are premultiplied BGRA as
    /// required by ULW_ALPHA.
    pub(super) unsafe fn render(
        &self,
        status: Status,
        error_kind: ErrorKind,
        word_count: u32,
        spinner_angle: Option<f32>,
        screen_x: i32,
        screen_y: i32,
    ) {
        let total = (self.size * self.size) as usize;
        let pixels = std::slice::from_raw_parts_mut(self.pixels, total);
        // Clear to fully transparent.
        for p in pixels.iter_mut() {
            *p = 0;
        }

        // Disc color picked by status. Values are (R, G, B).
        let (r, g, b) = match status {
            Status::Idle => return, // window will be hidden; nothing to draw
            Status::Starting => (0xFA, 0xB0, 0x05), // amber
            Status::Listening => (0x22, 0xC5, 0x5E), // green
            Status::Processing => (0x4A, 0x90, 0xF5), // blue
            Status::Error => (0xEF, 0x44, 0x44), // red
        };
        let cx = (self.size as f32 - 1.0) / 2.0;
        let cy = (self.size as f32 - 1.0) / 2.0;
        // Leave 1 px gutter so the soft edge doesn't get cropped by the window.
        let radius_outer = (self.size as f32 / 2.0) - 1.0;
        // 1 px feather for the anti-aliased edge.
        let edge = 1.0_f32;

        for y in 0..self.size {
            for x in 0..self.size {
                let dx = x as f32 - cx;
                let dy = y as f32 - cy;
                let dist = (dx * dx + dy * dy).sqrt();
                // Smooth alpha: 1.0 inside, ramps to 0 across `edge` pixels at the rim.
                let a = ((radius_outer - dist) / edge).clamp(0.0, 1.0);
                if a == 0.0 {
                    continue;
                }
                let alpha = (a * 255.0 + 0.5) as u32;
                // Premultiplied BGRA: BB GG RR AA stored as 0xAARRGGBB on
                // little-endian (which Windows expects).
                let pr = ((r as f32) * a + 0.5) as u32;
                let pg = ((g as f32) * a + 0.5) as u32;
                let pb = ((b as f32) * a + 0.5) as u32;
                let idx = (y * self.size + x) as usize;
                pixels[idx] = (alpha << 24) | (pr << 16) | (pg << 8) | pb;
            }
        }

        if let Some(start_angle) = spinner_angle {
            // Local providers are batch-only, so a word count sits at zero
            // until the final transcript. Draw a rotating 270° ring instead.
            draw_spinner_ring(pixels, self.size, cx, cy, start_angle);
        } else {
            // Draw the label on top. GDI doesn't touch the alpha channel, but
            // the disc interior already has alpha=255, so text stays opaque.
            let (label, use_icon_font): (String, bool) = match status {
                Status::Error => {
                    let (glyph, icon) = error_glyph(error_kind);
                    (glyph.to_string(), icon)
                }
                _ => (format!("{word_count}"), false),
            };
            let mut label_utf16: Vec<u16> = label.encode_utf16().collect();
            let font = if use_icon_font {
                self.font_icon
            } else {
                self.font_ui
            };
            let old_font = SelectObject(self.mem_dc, font);
            let _ = SetBkMode(self.mem_dc, TRANSPARENT);

            // Drop shadow: 1 px down-right, black.
            let _ = SetTextColor(self.mem_dc, COLORREF(0x00000000));
            let mut shadow_rect = RECT {
                left: 1,
                top: 1,
                right: self.size + 1,
                bottom: self.size + 1,
            };
            DrawTextW(
                self.mem_dc,
                &mut label_utf16,
                &mut shadow_rect,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE,
            );
            // Main text: white.
            let _ = SetTextColor(self.mem_dc, COLORREF(0x00FFFFFF));
            let mut text_rect = RECT {
                left: 0,
                top: 0,
                right: self.size,
                bottom: self.size,
            };
            DrawTextW(
                self.mem_dc,
                &mut label_utf16,
                &mut text_rect,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE,
            );
            // No DeleteObject here: `font` is one of the cached
            // `font_ui`/`font_icon` handles, freed once in `Drop` rather than
            // every repaint.
            SelectObject(self.mem_dc, old_font);
        }

        // Ship the bitmap to the screen, also moving the window.
        // UpdateLayeredWindow won't reveal a hidden window -- it only updates
        // an already-visible one. So show it on first use.
        let pt_dst = POINT {
            x: screen_x,
            y: screen_y,
        };
        let sz = SIZE {
            cx: self.size,
            cy: self.size,
        };
        let pt_src = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        if let Err(e) = UpdateLayeredWindow(
            self.hwnd,
            HDC::default(),
            Some(&pt_dst),
            Some(&sz),
            self.mem_dc,
            Some(&pt_src),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        ) {
            tracing::warn!("UpdateLayeredWindow failed: {e}");
        }
        if !self.visible.get() {
            // SW_SHOWNA shows without stealing focus.
            let _ = ShowWindow(self.hwnd, SW_SHOWNA);
            self.visible.set(true);
        }
    }
}

/// Blend a rotating 270° ring onto an already-filled disc: `pixels` is the
/// `size`x`size` BGRA buffer, `(cx, cy)` its center, and `start_angle` the
/// ring's current rotation. Blended directly into the existing (opaque)
/// pixels so the layered window keeps correct premultiplied alpha at
/// antialiased edges. Split out of `Overlay::render` -- the nested scan plus
/// per-pixel blend closure was most of that function's branching.
fn draw_spinner_ring(pixels: &mut [u32], size: i32, cx: f32, cy: f32, start_angle: f32) {
    let ring_radius = size as f32 * 0.22;
    let ring_half_width = size as f32 * 0.034;
    let sweep = std::f32::consts::TAU * 0.75;
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - cx;
            let dy = y as f32 - cy;
            let dist = (dx * dx + dy * dy).sqrt();
            let radial = (ring_half_width + 0.8 - (dist - ring_radius).abs()).clamp(0.0, 1.0);
            if radial == 0.0 {
                continue;
            }
            let around = (dy.atan2(dx) - start_angle).rem_euclid(std::f32::consts::TAU);
            if around > sweep {
                continue;
            }
            let tip = (around.min(sweep - around) / 0.16).clamp(0.0, 1.0);
            let coverage = radial * tip;
            let idx = (y * size + x) as usize;
            let old = pixels[idx];
            let blend = |channel: u32| -> u32 {
                (channel as f32 + (255.0 - channel as f32) * coverage + 0.5) as u32
            };
            let red = blend((old >> 16) & 0xff);
            let green = blend((old >> 8) & 0xff);
            let blue = blend(old & 0xff);
            pixels[idx] = (old & 0xff00_0000) | (red << 16) | (green << 8) | blue;
        }
    }
}

/// Pip glyph for a given error cause: the label text and whether it needs
/// `Overlay::font_icon` (the Segoe MDL2 Assets icon font) instead of the
/// plain `font_ui`. Pure and separate from the GDI calls in `Overlay::render`
/// so the mapping itself is unit-testable. Every named `ErrorKind` variant is
/// handled explicitly -- collapsing them behind a wildcard is exactly how
/// this went stale before (everything but `DeadKeys` used to render as a
/// bare "!").
pub(super) fn error_glyph(kind: ErrorKind) -> (&'static str, bool) {
    match kind {
        ErrorKind::Generic => ("!", false),
        ErrorKind::DeadKeys => ("\u{E8D7}", true), // key glyph, MDL2 icon font
        ErrorKind::Quota => ("$", false),
        ErrorKind::RateLimited => ("429", false),
        ErrorKind::Network => ("net", false),
        ErrorKind::Elevated => ("UAC", false),
        ErrorKind::HotkeyBlocked => ("hk", false),
    }
}

/// Creates a bold, antialiased GDI font sized as `height_factor` of `size`
/// (the pip's diameter). `face_name` must be nul-terminated (GDI wants a wide
/// C string). Called twice total, once per cached font, in `Overlay::create`
/// -- this used to run on every repaint (see the `font_ui`/`font_icon` doc
/// comment on `Overlay`).
unsafe fn create_label_font(size: i32, height_factor: f32, face_name: &str) -> HFONT {
    let font_height = (size as f32 * height_factor) as i32;
    let face: Vec<u16> = face_name.encode_utf16().collect();
    CreateFontW(
        -font_height,
        0,
        0,
        0,
        FW_BOLD.0 as i32,
        0u32,
        0u32,
        0u32,
        DEFAULT_CHARSET.0 as u32,
        OUT_DEFAULT_PRECIS.0 as u32,
        CLIP_DEFAULT_PRECIS.0 as u32,
        ANTIALIASED_QUALITY.0 as u32,
        (VARIABLE_PITCH.0 as u32) | (FF_DONTCARE.0 as u32),
        PCWSTR(face.as_ptr()),
    )
}

impl Drop for Overlay {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.font_ui);
            let _ = DeleteObject(self.font_icon);
            let _ = DeleteObject(self.bitmap);
            let _ = DeleteDC(self.mem_dc);
        }
    }
}

unsafe extern "system" fn overlay_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        // A second QuickDictate launch found us via FindWindowW (see
        // `main.rs`'s single-instance guard) and posted the registered
        // activate message. We can't thread the `App`/`Arc` through a raw
        // wnd_proc, so just flip a flag the poll loop in `run` picks up and
        // acts on via the normal `settings_ui::show_settings` path.
        m if m == activate_message_id() => {
            SHOW_SETTINGS_REQUESTED.store(true, Ordering::Release);
            LRESULT(0)
        }
        // No WM_PAINT handler: UpdateLayeredWindow drives all visuals.
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
