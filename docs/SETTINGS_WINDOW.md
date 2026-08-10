# Settings window: developer notes

Read this before changing the sizing or the Save button in
[`src/settings_ui.rs`](../src/settings_ui.rs). The Settings window is an
egui/eframe app with a few non-obvious behaviors that make quick edits go
sideways if you do not know about them. This file exists because working these
out from scratch cost a long, painful session; the goal is that the next change
takes minutes, not hours.

## 1. The window runs at 0.9 zoom, so there are THREE coordinate systems

`SettingsApp` calls `cc.egui_ctx.set_zoom_factor(0.9)` at startup, so everything
renders 10% smaller. That one line means three different units are in play, and
mixing them is the number one cause of sizing bugs:

- **egui points (zoom-scaled).** What all layout code sees: widget sizes,
  `Ui`/panel `Rect`s, `ScrollAreaOutput::content_size`, `Margin` values, and the
  argument to `ViewportCommand::InnerSize`. `ctx.pixels_per_point()` (call it
  `egui_ppp`) converts these to physical pixels. With Windows scaling at 100%,
  `egui_ppp = 1.0 * 0.9 = 0.9`.
- **native points.** What `ViewportInfo` reports: `inner_rect`, `outer_rect`,
  `monitor_size`. These use `native_pixels_per_point()` (call it `native_ppp`),
  which ignores the egui zoom. At 100% Windows scaling `native_ppp = 1.0`.
- **physical pixels.** The actual framebuffer.
  `physical = egui_points * egui_ppp = native_points * native_ppp`.

Conversions you will need:

```
egui_points = native_points * native_ppp / egui_ppp
physical    = egui_points   * egui_ppp
zoom_factor = egui_ppp / native_ppp          (currently 0.9)
```

Real example from the debugging session (native 100%, zoom 0.9): a window whose
content area is 600 physical pixels wide reports `inner_rect.width() = 600`
(native points) but lays out at `600 / 0.9 = 666.7` egui points.

**Trap A.** `ViewportCommand::InnerSize` takes *egui* points, but
`ViewportInfo::inner_rect` is in *native* points. Feeding `inner_rect.width()`
straight back into `InnerSize` resizes the window by the zoom factor every frame
(one attempt shrank the window 600 -> 540; a different mistake blew it up past
10000 px wide). Always convert:
`width_egui = inner_rect.width() * native_ppp / egui_ppp`.

**Trap B.** A `CentralPanel`'s `response.rect.width()` is NOT the window width
(it reported ~11000 in testing). Use `ViewportInfo::inner_rect` for the real
window size, then convert units.

## 2. There is a nav rail, and the window does NOT auto-size itself

The window is a fixed-size box: a nav rail on the left (`nav.rs`), one category
page in the middle, and the action bar along the bottom. Only the active page
draws; the rest are not laid out at all. `with_inner_size([760, 600])` in the
viewport builder is the real size, not an estimate, and nothing resizes the
window afterwards.

**Do not reintroduce an auto-fit.** Until v0.5.5 `fn ui` measured the body every
frame and sent `ViewportCommand::InnerSize` to match. It had two problems, and
the second is the reason this rule exists:

1. Stacking every card into one column made the window about 1160 points tall,
   taller than a lot of laptop screens.
2. It fought the user. Dragging the window edge changes the width, which
   rewraps the content, which changes the measured height, which sent a new
   `InnerSize`, which the OS applied a frame later, mid-drag. Resizing snapped
   the window open and shut. Measured A/B: asked for 900x620, the old build
   settled at 757x1184, then 860x1184, then 812x1184. The current build returns
   exactly what it was asked for, every time.

If a page outgrows the window, let the page scroll (its `ScrollArea` already
does) or move settings into another category. Do not grow the window to fit.

To check a page you are editing, screenshot it directly:
`pwsh -File scripts\ui_shot.ps1 -Tab dictation`. `-Tab` takes a prefix of the
rail label (provider, dictation, application, history, settings).

## 3. The Save split button (`SPLIT_BTN_H`)

The Save button and its dropdown chevron are two separate widgets styled to read
as one control. Two things keep their heights equal:

- Both are pinned to `SPLIT_BTN_H` via `min_size`. Set it at or above the Save
  text button's natural height so both halves clamp to the same value.
  Otherwise Save renders at its taller natural height while the chevron sits
  shorter, and they look mismatched.
- The chevron (`accent_menu_button`) sets `bg_stroke = Stroke::NONE`. The Save
  half is drawn with `Stroke::NONE`, so its accent fill reaches the button
  edges. The chevron otherwise inherits a 1px border from the global style,
  which insets its fill about 1px top and bottom and makes it read ~2px shorter
  than Save. Dropping the border matches them to the pixel.

Chevron width comes from `button_padding.x` inside `accent_menu_button`, kept
small because it holds a single glyph.

## 4. How to verify a change fast (do not eyeball it)

Headless screenshot, no screen-control tooling needed:

```
pwsh -File scripts\refresh_test_exe.ps1        # rebuild release + copy to the root test exe
pwsh -File scripts\ui_shot.ps1 -Shot out.png   # open Settings, self-screenshot, kill the app
```

`ui_shot.ps1` captures the *physical* framebuffer, so at 0.9 zoom a
600-egui-point-wide window produces a 540 px wide PNG. The PNG dimensions are a
quick sanity check on the real window size.

To measure exact pixel sizes of a control (for example to confirm two buttons
match), load the PNG with `System.Drawing.Bitmap` in PowerShell and scan for the
accent-blue pixels; comparing the min/max y of each half is how the Save/chevron
height match was confirmed to the pixel.

To get ground-truth runtime values (both ppps, `inner_rect`, `content_size`,
computed sizes), append them to a plain file with `std::fs`:

```rust
use std::io::Write as _;
if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open("autofit_debug.txt") {
    let _ = writeln!(f, "egui_ppp={} native_ppp={} inner={:?} content_h={}", /* ... */);
}
```

Do NOT use `tracing::info!` for this. `ui_shot.ps1` force-kills the app right
after capture, and the non-blocking log appender loses buffered lines; a
synchronous file write survives. Remember to remove the debug write before
committing.

## Shared layout constants

Keep new controls consistent with these instead of hand-tuning per-widget sizes:

- `CTRL_PAD`: vertical inner padding shared by text wells and buttons so their
  heights line up.
- `SPLIT_BTN_H`: the Save split-button height (see section 3).
- `ROUND`: the standard corner rounding.
