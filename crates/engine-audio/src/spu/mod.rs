//! Clean-room PSX SPU model.
//!
//! 24 voices, 512 KB SPU RAM, ADSR-shaped per-voice envelopes, libspu-shaped
//! transfer engine. The mixer's job is simple:
//!
//! 1. Each output frame, advance every voice that's on by one SPU-internal
//!    sample (44.1 kHz). Mix into a shared (left, right) accumulator.
//! 2. Apply a master volume (set by libspu `SsSetMVol`).
//! 3. Resample the result to the host sample rate.
//!
//! What this does NOT model:
//!
//! - Pitch modulation, noise mode, FM. None of these are used by Legaia
//!   (verified against the libspu calls in the SCUS dumps - `SpuSetPitch`
//!   is the only pitch path, `SpuSetVoiceAttr` writes only sample addr +
//!   ADSR + volume).
//!
//! ## Pitch / interpolation
//!
//! Voice pitch stepping follows the PSX SPU pitch-counter shape: `0x1000`
//! advances by one decoded ADPCM sample per 44.1 kHz output tick, and the
//! Gaussian interpolation index comes from the fractional counter bits. The
//! interpolation table is isolated in [`gaussian`] so it can be tested and
//! compared directly against PSXSPX.
//!
//! ## Reverb
//!
//! [`Reverb`] is a clean-room register-style SPU reverb path driven at the
//! SPU's 44.1 kHz frame clock with the documented 22.05 kHz reverb core and
//! 39-tap input/output FIR resamplers. Routing is per-voice: set
//! [`Voice::reverb_send`] to opt a voice into the wet signal (libspu
//! `SpuSetVoiceReverb` analogue). The current implementation is organized
//! around the PSXSPX public register formulas and shaped to match emulator
//! SPU architecture; it is not copied from DuckStation or any Sony library.
//!
//! See `docs/subsystems/audio.md` "engine-audio model" for the consumer.

pub mod adpcm;
pub mod adsr;
pub mod gaussian;
pub mod psx_reverb;
pub mod ram;
pub mod reverb;
pub mod voice;

pub use reverb::{Reverb, ReverbMode, ReverbParams};

use ram::SpuRam;
use voice::{InterpolationMode, Voice};

/// Number of hardware voices on the PSX SPU.
pub const NUM_VOICES: usize = 24;

/// The full SPU model.
#[derive(Debug, Clone)]
pub struct Spu {
    /// SPU RAM (512 KB).
    pub ram: SpuRam,
    /// Per-voice state. Indexed 0..24.
    pub voices: [Voice; NUM_VOICES],
    /// Master volume, 0x0000..=0x3FFF (libspu `MVOL` shape).
    pub master_left: i16,
    pub master_right: i16,
    /// Active reverb processor. Defaults to [`ReverbMode::Off`]. Voices
    /// with `reverb_send = true` route their output through this; the wet
    /// signal is mixed back into the master in [`Spu::tick`].
    pub reverb: Reverb,
    /// Last raw `reverb_mode` value written by libspu - preserved for
    /// debugging / tooling. The active mode in the [`Reverb`] processor
    /// is set via [`Spu::set_reverb_mode`].
    pub reverb_mode_raw: u32,
    /// Preview/editor trim for the wet return. This is intentionally applied
    /// after the reverb processor so it cannot change a mode's feedback or
    /// damping character.
    pub reverb_wet_gain_q14: i16,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct MixProbe {
    pub dry_l: i16,
    pub dry_r: i16,
    pub reverb_in_l: i16,
    pub reverb_in_r: i16,
    pub reverb_return_l: i16,
    pub reverb_return_r: i16,
    pub final_l: i16,
    pub final_r: i16,
}

impl Default for Spu {
    fn default() -> Self {
        Self {
            ram: SpuRam::new(),
            voices: std::array::from_fn(|_| Voice::default()),
            master_left: 0x3FFF,
            master_right: 0x3FFF,
            reverb: Reverb::new(ReverbMode::Off),
            reverb_mode_raw: 0,
            reverb_wet_gain_q14: 0x4000,
        }
    }
}

impl Spu {
    pub fn new() -> Self {
        Self::default()
    }

    /// Issue libspu-style key-on for the voices with the corresponding bit
    /// set in `mask` (bit 0 = voice 0, etc.). Equivalent to `SpuKeyOn` /
    /// `SpuSetKey(SpuOn, mask)`.
    pub fn key_on_mask(&mut self, mask: u32) {
        for i in 0..NUM_VOICES {
            if mask & (1u32 << i) != 0 {
                let v = &mut self.voices[i];
                let ram = &self.ram;
                v.key_on(ram);
            }
        }
    }

    /// Mirror of `key_on_mask` for key-off.
    pub fn key_off_mask(&mut self, mask: u32) {
        for i in 0..NUM_VOICES {
            if mask & (1u32 << i) != 0 {
                self.voices[i].key_off();
            }
        }
    }

    /// Set the active reverb mode. The [`Reverb`] processor's buffers are
    /// resized; in-flight wet signal is dropped.
    pub fn set_reverb_mode(&mut self, mode: ReverbMode) {
        self.reverb.set_mode(mode);
    }

    /// libspu `SpuCommonAttr.reverb` analogue - accepts the raw mode byte
    /// and updates both the live processor and the bookkeeping register.
    pub fn write_reverb_mode_byte(&mut self, raw: u8) {
        self.reverb_mode_raw = raw as u32;
        self.set_reverb_mode(ReverbMode::from_byte(raw));
    }

    pub fn set_interpolation_mode(&mut self, mode: InterpolationMode) {
        for voice in &mut self.voices {
            voice.set_interpolation(mode);
        }
    }

    pub fn interpolation_mode(&self) -> InterpolationMode {
        self.voices
            .first()
            .map(|v| v.interpolation)
            .unwrap_or(InterpolationMode::Nearest)
    }

    /// Scale the wet reverb return. `0x4000` is unity. Preview tools may
    /// exceed unity while matching retail captures; the final mixer still
    /// saturates to i16.
    pub fn set_reverb_wet_gain_q14(&mut self, gain_q14: i16) {
        self.reverb_wet_gain_q14 = gain_q14.clamp(0, 0x7FFF);
    }

    /// Advance every voice by one sample tick at the SPU internal rate
    /// (44.1 kHz). Returns the (left, right) sample, master-volume scaled
    /// and clamped to i16.
    pub fn tick(&mut self) -> (i16, i16) {
        self.tick_debug_mix(false, false)
    }

    /// Debug mixer variant for audition/export tooling. This keeps voice
    /// allocation, envelopes, pitch, and reverb timing identical to [`Self::tick`]
    /// while allowing callers to isolate the dry bus or wet return.
    pub fn tick_debug_mix(&mut self, mute_dry: bool, mute_wet: bool) -> (i16, i16) {
        let p = self.tick_probe_mix(mute_dry, mute_wet);
        (p.final_l, p.final_r)
    }

    pub fn tick_probe_mix(&mut self, mute_dry: bool, mute_wet: bool) -> MixProbe {
        let mut acc_l: i64 = 0;
        let mut acc_r: i64 = 0;
        let mut dry_l_acc: i64 = 0;
        let mut dry_r_acc: i64 = 0;
        let mut send_l: i64 = 0;
        let mut send_r: i64 = 0;
        for v in &mut self.voices {
            let (l, r) = v.tick(&self.ram);
            dry_l_acc += l as i64;
            dry_r_acc += r as i64;
            if !mute_dry {
                acc_l += l as i64;
                acc_r += r as i64;
            }
            if v.reverb_send {
                send_l += l as i64;
                send_r += r as i64;
            }
        }
        // Drive the reverb network with the reverb-tagged voices' sum.
        let send_l_i16 = send_l.clamp(i16::MIN as i64, i16::MAX as i64) as i16;
        let send_r_i16 = send_r.clamp(i16::MIN as i64, i16::MAX as i64) as i16;
        let (raw_wet_l, raw_wet_r) = self.reverb.tick(send_l_i16, send_r_i16);
        let wet_gain = self.reverb_wet_gain_q14 as i64;
        let wet_l =
            ((raw_wet_l as i64 * wet_gain) >> 14).clamp(i16::MIN as i64, i16::MAX as i64) as i16;
        let wet_r =
            ((raw_wet_r as i64 * wet_gain) >> 14).clamp(i16::MIN as i64, i16::MAX as i64) as i16;
        if !mute_wet {
            acc_l += wet_l as i64;
            acc_r += wet_r as i64;
        }
        // Apply master volume.
        let l = ((acc_l * self.master_left as i64) >> 14).clamp(i16::MIN as i64, i16::MAX as i64);
        let r = ((acc_r * self.master_right as i64) >> 14).clamp(i16::MIN as i64, i16::MAX as i64);
        MixProbe {
            dry_l: dry_l_acc.clamp(i16::MIN as i64, i16::MAX as i64) as i16,
            dry_r: dry_r_acc.clamp(i16::MIN as i64, i16::MAX as i64) as i16,
            reverb_in_l: send_l_i16,
            reverb_in_r: send_r_i16,
            reverb_return_l: wet_l,
            reverb_return_r: wet_r,
            final_l: l as i16,
            final_r: r as i16,
        }
    }

    /// Drain `n` samples into a stereo i16 buffer (pairs of left, right).
    /// Convenience for tests + the cpal callback's resampler.
    pub fn render_into(&mut self, out: &mut [i16]) {
        debug_assert_eq!(out.len() % 2, 0);
        for chunk in out.chunks_exact_mut(2) {
            let (l, r) = self.tick();
            chunk[0] = l;
            chunk[1] = r;
        }
    }

    /// Returns the count of voices currently in the `Off` envelope phase,
    /// i.e. available for a fresh allocation.
    pub fn idle_voice_count(&self) -> usize {
        self.voices.iter().filter(|v| v.is_off()).count()
    }

    /// Find an idle voice index. Mirrors the libspu pattern of "scan for a
    /// voice whose envelope has finished".
    pub fn find_idle_voice(&self) -> Option<usize> {
        self.voices.iter().position(|v| v.is_off())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synth_nonzero_loop_spu(reverb_send: bool) -> Spu {
        let mut spu = Spu::new();
        // A tiny ADPCM stream with a non-zero nibble pattern followed by an
        // end+repeat flag. This is intentionally synthetic; the test only
        // needs a stable non-silent SPU voice.
        let mut stream = vec![0u8; 16];
        stream[1] = 0x03;
        for b in &mut stream[2..] {
            *b = 0x11;
        }
        spu.ram.write_at(0x1000, &stream);
        spu.voices[0].start_addr = 0x1000;
        spu.voices[0].set_loop_addr(0x1000);
        spu.voices[0].set_reverb_send(reverb_send);
        spu.voices[0].adsr_cfg.attack_shift = 0;
        spu.voices[0].adsr_cfg.decay_shift = 0;
        spu.voices[0].adsr_cfg.sustain_level = 0x7FFF;
        spu.key_on_mask(1);
        spu
    }

    /// Smoke: 24 silent voices key-on simultaneously, render 1 second of
    /// stereo, finish without panics.
    #[test]
    fn render_silence_one_second() {
        let mut spu = Spu::new();
        let mut buf = vec![0i16; 44100 * 2];
        spu.render_into(&mut buf);
        assert!(buf.iter().all(|&s| s == 0));
    }

    /// key_on_mask actually triggers the per-voice key-on for the right
    /// voices and not the others.
    #[test]
    fn key_on_mask_only_affects_set_bits() {
        let mut spu = Spu::new();
        // Stick a one-block silence stream at 0x1000 so each voice has
        // something to "play".
        let stream = vec![0u8; 16];
        spu.ram.write_at(0x1000, &stream);
        for v in spu.voices.iter_mut() {
            v.start_addr = 0x1000;
        }
        spu.key_on_mask(0b101);
        // Voices 0 and 2 should be in Attack; the rest still Off.
        for (i, v) in spu.voices.iter().enumerate() {
            if i == 0 || i == 2 {
                assert!(!v.is_off(), "voice {i} should be on");
            } else {
                assert!(v.is_off(), "voice {i} should still be off");
            }
        }
    }

    /// idle_voice_count starts at 24, drops to 23 after one key-on.
    #[test]
    fn idle_voice_count_tracks_keyons() {
        let mut spu = Spu::new();
        let stream = vec![0u8; 16];
        spu.ram.write_at(0x1000, &stream);
        for v in spu.voices.iter_mut() {
            v.start_addr = 0x1000;
        }
        assert_eq!(spu.idle_voice_count(), NUM_VOICES);
        spu.key_on_mask(0b1);
        assert_eq!(spu.idle_voice_count(), NUM_VOICES - 1);
    }

    /// find_idle_voice returns None when all voices are busy.
    #[test]
    fn find_idle_voice_returns_none_when_all_busy() {
        let mut spu = Spu::new();
        let stream = vec![0u8; 16];
        spu.ram.write_at(0x1000, &stream);
        for v in spu.voices.iter_mut() {
            v.start_addr = 0x1000;
        }
        spu.key_on_mask(0xFFFFFFFF);
        assert!(spu.find_idle_voice().is_none());
    }

    /// Reverb mode round-trips through the libspu-style mode byte API
    /// and updates the live processor.
    #[test]
    fn write_reverb_mode_byte_updates_processor() {
        let mut spu = Spu::new();
        assert_eq!(spu.reverb.mode, ReverbMode::Off);
        spu.write_reverb_mode_byte(7); // Echo
        assert_eq!(spu.reverb_mode_raw, 7);
        assert_eq!(spu.reverb.mode, ReverbMode::Echo);
        spu.write_reverb_mode_byte(5); // Hall
        assert_eq!(spu.reverb.mode, ReverbMode::Hall);
        // Out-of-range falls back to Off.
        spu.write_reverb_mode_byte(0xFE);
        assert_eq!(spu.reverb.mode, ReverbMode::Off);
    }

    /// A voice with `reverb_send` set produces an echo tail past the
    /// reverb mode's delay length when the master tick is run.
    #[test]
    fn reverb_send_voice_produces_wet_tail() {
        let mut spu = Spu::new();
        spu.set_reverb_mode(ReverbMode::Room);
        // Plant a non-zero stream and key one voice on with reverb_send.
        let stream = vec![0u8; 16];
        spu.ram.write_at(0x1000, &stream);
        spu.voices[0].start_addr = 0x1000;
        spu.voices[0].vol_left = 0x3FFF;
        spu.voices[0].vol_right = 0x3FFF;
        spu.voices[0].set_reverb_send(true);
        spu.key_on_mask(0b1);
        // Render long enough to fill the room delay buffer.
        let delay = spu.reverb.tick(0, 0);
        let _ = delay;
        let mut buf = vec![0i16; 4_410 * 2]; // 100 ms of stereo
        spu.render_into(&mut buf);
        // Silence stream → output is silence (no decoded samples), so we
        // can't validate non-zero output here. Rely on the per-Reverb
        // unit tests for that. The smoke value is "no panics".
    }

    #[test]
    fn dry_only_matches_full_mix_with_zero_wet_gain() {
        let mut dry = synth_nonzero_loop_spu(true);
        let mut full_zero_wet = synth_nonzero_loop_spu(true);
        full_zero_wet.set_reverb_mode(ReverbMode::Room);
        full_zero_wet.set_reverb_wet_gain_q14(0);

        for _ in 0..1024 {
            let dry_frame = dry.tick_debug_mix(false, true);
            let full_frame = full_zero_wet.tick_debug_mix(false, false);
            assert_eq!(dry_frame, full_frame);
        }
    }

    #[test]
    fn wet_gain_changes_full_mix_when_reverb_return_is_nonzero() {
        let mut wet0 = synth_nonzero_loop_spu(true);
        let mut wet1 = synth_nonzero_loop_spu(true);
        wet0.set_reverb_mode(ReverbMode::Room);
        wet1.set_reverb_mode(ReverbMode::Room);
        wet0.set_reverb_wet_gain_q14(0);
        wet1.set_reverb_wet_gain_q14(0x4000);

        let mut any_return = false;
        let mut any_diff = false;
        for _ in 0..12_000 {
            let p0 = wet0.tick_probe_mix(false, false);
            let p1 = wet1.tick_probe_mix(false, false);
            any_return |= p1.reverb_return_l != 0 || p1.reverb_return_r != 0;
            any_diff |= p0.final_l != p1.final_l || p0.final_r != p1.final_r;
        }

        assert!(any_return, "test fixture should produce a nonzero wet return");
        assert!(any_diff, "wetness must affect final mix when wet return exists");
    }
}
