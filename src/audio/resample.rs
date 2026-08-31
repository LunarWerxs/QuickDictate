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

        let mono = |i: usize| -> i16 {
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
        };

        let prev_mono = self.last_frame_mono;
        while self.pos < frame_end as f64 {
            let local = self.pos - frame_start as f64;
            if local < 0.0 {
                let p0 = prev_mono.unwrap_or(0) as f32;
                let p1 = mono(0) as f32;
                let frac = (local + 1.0).clamp(0.0, 1.0) as f32;
                let v = p0 * (1.0 - frac) + p1 * frac;
                out.push(v as i16);
            } else {
                let i = local as usize;
                let frac = (local - i as f64) as f32;
                if i + 1 < frames {
                    let a = mono(i) as f32;
                    let b = mono(i + 1) as f32;
                    out.push((a * (1.0 - frac) + b * frac) as i16);
                } else if i < frames {
                    out.push(mono(i));
                } else {
                    break;
                }
            }
            self.pos += self.step;
        }

        self.last_frame_mono = Some(mono(frames - 1));
        self.consumed = frame_end;
    }
}
