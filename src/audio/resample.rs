//! Linear-interpolation resampling from the device rate to the provider's,
//! and the f32 to i16 conversion in front of it.

// ---------------------------------------------------------------------------
// Resampler (unchanged algorithm — linear interpolation, mono mix)
// ---------------------------------------------------------------------------

#[inline]
pub(super) fn f32_to_i16(s: f32) -> i16 {
    let v = (s * 32767.0).clamp(-32768.0, 32767.0);
    v as i16
}

/// Unsigned 16-bit PCM (silence at 32768) to signed (silence at 0).
#[inline]
pub(super) fn u16_to_i16(s: u16) -> i16 {
    (s as i32 - 32768) as i16
}

#[derive(Default)]
pub(super) struct LinearResampler {
    pub(super) step: f64,
    pub(super) channels: usize,
    pub(super) pos: f64,
    pub(super) last_frame_mono: Option<i16>,
    pub(super) consumed: u64,
}

impl LinearResampler {
    pub(super) fn new(step: f64, channels: usize) -> Self {
        Self {
            step,
            channels: channels.max(1),
            pos: 0.0,
            last_frame_mono: None,
            consumed: 0,
        }
    }

    pub(super) fn feed_and_emit(&mut self, data: &[i16], out: &mut Vec<i16>) {
        if data.is_empty() {
            return;
        }
        let ch = self.channels;
        let frames = data.len() / ch;
        if frames == 0 {
            return;
        }

        let frame_start = self.consumed;
        let frame_end = self.consumed + frames as u64;

        let prev_mono = self.last_frame_mono;
        while self.pos < frame_end as f64 {
            let local = self.pos - frame_start as f64;
            let Some(v) = interpolate(data, ch, frames, prev_mono, local) else {
                break;
            };
            out.push(v);
            self.pos += self.step;
        }

        self.last_frame_mono = Some(mono_frame(data, ch, frames - 1));
        self.consumed = frame_end;
    }
}

/// Frame `i` of interleaved `data` with `ch` channels, mixed down to mono.
#[inline]
fn mono_frame(data: &[i16], ch: usize, i: usize) -> i16 {
    if ch == 1 {
        data[i]
    } else {
        let mut acc: i32 = 0;
        let base = i * ch;
        for c in 0..ch {
            acc += data[base + c] as i32;
        }
        (acc / ch as i32) as i16
    }
}

/// The output sample `local` frames into this buffer: interpolated between
/// the two frames around it, or (negative `local`) between the previous
/// buffer's last frame and this one's first. `None` once `local` is past the
/// last frame.
#[inline]
fn interpolate(
    data: &[i16],
    ch: usize,
    frames: usize,
    prev_mono: Option<i16>,
    local: f64,
) -> Option<i16> {
    if local < 0.0 {
        let p0 = prev_mono.unwrap_or(0) as f32;
        let p1 = mono_frame(data, ch, 0) as f32;
        let frac = (local + 1.0).clamp(0.0, 1.0) as f32;
        let v = p0 * (1.0 - frac) + p1 * frac;
        return Some(v as i16);
    }
    let i = local as usize;
    let frac = (local - i as f64) as f32;
    if i + 1 < frames {
        let a = mono_frame(data, ch, i) as f32;
        let b = mono_frame(data, ch, i + 1) as f32;
        Some((a * (1.0 - frac) + b * frac) as i16)
    } else if i < frames {
        Some(mono_frame(data, ch, i))
    } else {
        None
    }
}
