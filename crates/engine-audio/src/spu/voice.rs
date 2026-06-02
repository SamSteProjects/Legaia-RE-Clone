//! Per-voice state for the SPU mixer.
//!
//! A voice owns:
//!  - a base sample address into SPU RAM (the start of its ADPCM stream)
//!  - a loop point (latched the first time a block with `loop_start` is seen,
//!    or set explicitly via [`Voice::set_loop_addr`])
//!  - a streaming ADPCM decoder (one 28-sample block at a time)
//!  - a fractional pitch counter - the SPU runs internally at 44.1 kHz and
//!    advances each voice by `pitch / 0x1000` samples per output tick
//!  - an ADSR envelope
//!  - linear left/right volume registers (signed 16-bit, peak 0x3FFF in
//!    libspu)
//!
//! The PSX SPU outputs 44.1 kHz; the host stream rate is decoupled by the
//! mixer.
//!
//! No Sony bytes - this is the standard PSX SPU model.

use super::adpcm::{AdpcmDecoder, BLOCK_BYTES, SAMPLES_PER_BLOCK};
use super::adsr::{AdsrConfig, AdsrState, Phase};
use super::gaussian;
use super::ram::SpuRam;

/// Internal SPU output rate (constant per hardware).
pub const SPU_INTERNAL_RATE: u32 = 44_100;

/// Pitch unit: 0x1000 = 1× sample rate.
pub const PITCH_UNITY: u16 = 0x1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterpolationMode {
    Nearest,
    Linear,
    /// PSX SPU 4-point Gaussian interpolation using the hardware table from
    /// PSXSPX. Kept as the value `3` because existing UI/debug exports use
    /// that selector for the Gaussian mode.
    GaussianApprox,
}

impl InterpolationMode {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Linear,
            3 => Self::GaussianApprox,
            _ => Self::Nearest,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Voice {
    /// Base address (bytes) into SPU RAM where this voice's ADPCM stream
    /// starts. Set by libspu `SpuSetVoiceStartAddr`.
    pub start_addr: u32,
    /// Loop-back address. Latched on the first block with `loop_start`,
    /// or set explicitly. `None` means "no loop set" - when an end-flag
    /// block plays out the voice will go silent.
    pub loop_addr: Option<u32>,
    /// Pitch, 0x1000 = 1.0×. Sample step = pitch / 0x1000.
    pub pitch: u16,
    /// Left output volume, 0..=0x3FFF (libspu convention).
    pub vol_left: i16,
    /// Right output volume.
    pub vol_right: i16,
    /// ADSR envelope settings.
    pub adsr_cfg: AdsrConfig,
    /// ADSR runtime state.
    pub adsr: AdsrState,
    /// Per-voice reverb routing flag. When `true` this voice's pre-master
    /// output is summed into the SPU's reverb send bus. libspu calls this
    /// `SpuSetVoiceReverb`. Defaults to `false` - engines opt voices in
    /// when starting a Spirit Art / echo-flagged sound effect.
    pub reverb_send: bool,
    /// Per-voice sample interpolation. Retail PS1 SPU uses a Gaussian table
    /// indexed by fractional pitch-counter bits; the Gaussian mode uses the
    /// PSXSPX coefficient table and tap order.
    pub interpolation: InterpolationMode,

    // --- runtime state --------------------------------------------------
    /// Absolute address of the *current* ADPCM block in SPU RAM.
    cur_block_addr: u32,
    /// Which sample within the current 28-sample block we're playing.
    sample_idx: u8,
    /// Fractional sample counter (in `1 / 0x1000` units of an input sample).
    sample_frac: u32,
    /// 28-sample buffer for the current block.
    block_pcm: [i16; SAMPLES_PER_BLOCK],
    /// Last three samples of the previous block so 4-tap interpolation can
    /// cross ADPCM block boundaries. DuckStation-style SPU implementations
    /// keep this history for the same reason.
    prev_block_tail: [i16; 3],
    /// Decoder state (carries `prev1`/`prev2` between blocks).
    decoder: AdpcmDecoder,
    /// True if the voice has at least one decoded block ready.
    has_block: bool,
}

impl Default for Voice {
    fn default() -> Self {
        Self {
            start_addr: 0,
            loop_addr: None,
            pitch: PITCH_UNITY,
            vol_left: 0x3FFF,
            vol_right: 0x3FFF,
            adsr_cfg: AdsrConfig::default(),
            adsr: AdsrState::default(),
            reverb_send: false,
            interpolation: InterpolationMode::Nearest,
            cur_block_addr: 0,
            sample_idx: 0,
            sample_frac: 0,
            block_pcm: [0; SAMPLES_PER_BLOCK],
            prev_block_tail: [0; 3],
            decoder: AdpcmDecoder::new(),
            has_block: false,
        }
    }
}

impl Voice {
    /// Set the explicit loop address. Equivalent to libspu's
    /// `SpuSetVoiceLoopStartAddr`.
    pub fn set_loop_addr(&mut self, addr: u32) {
        self.loop_addr = Some(addr);
    }

    /// Toggle the per-voice reverb send flag (libspu `SpuSetVoiceReverb`
    /// analogue).
    pub fn set_reverb_send(&mut self, on: bool) {
        self.reverb_send = on;
    }

    pub fn set_interpolation(&mut self, mode: InterpolationMode) {
        self.interpolation = mode;
    }

    /// Trigger a key-on: rewind to start, decode the first block, kick the
    /// envelope into the Attack phase.
    pub fn key_on(&mut self, ram: &SpuRam) {
        self.cur_block_addr = self.start_addr;
        self.sample_idx = 0;
        self.sample_frac = 0;
        self.prev_block_tail = [0; 3];
        self.decoder.reset();
        self.fetch_block(ram);
        self.adsr.key_on();
    }

    /// Trigger a key-off - the envelope transitions to Release. Voice keeps
    /// playing until the envelope drains.
    pub fn key_off(&mut self) {
        self.adsr.key_off();
    }

    /// Hard stop this voice immediately. Useful for preview tooling and for
    /// tearing down a sequencer before loading a new bank into the same SPU
    /// RAM addresses.
    pub fn reset(&mut self) {
        *self = Voice::default();
    }

    /// True when the envelope is in `Off` (envelope finished or never started).
    pub fn is_off(&self) -> bool {
        self.adsr.phase == Phase::Off
    }

    /// Sample-level tick. Produces one (left, right) output sample at the SPU
    /// internal rate, then advances envelope/pitch state. This matches the
    /// ordering used by emulator SPU cores such as DuckStation's `SampleVoice`:
    /// decode if needed, interpolate, apply current ADSR, tick ADSR, advance
    /// the pitch counter, then handle block loop/end flags and channel volume.
    pub fn tick(&mut self, ram: &SpuRam) -> (i32, i32) {
        if !self.has_block || self.adsr.phase == Phase::Off {
            return (0, 0);
        }
        let env = self.adsr.level as i32;
        let raw = self.interpolated_sample() as i32;

        // Mix: raw * env / 0x7FFF * vol / 0x3FFF.
        let enveloped = (raw * env) >> 15;
        let left = (enveloped * self.vol_left as i32) >> 14;
        let right = (enveloped * self.vol_right as i32) >> 14;

        self.adsr.tick(&self.adsr_cfg);

        // Advance fractional pitch counter and walk forward through samples.
        let step = self.pitch.min(0x3FFF) as u32;
        self.sample_frac += step;
        while self.sample_frac >= PITCH_UNITY as u32 {
            self.sample_frac -= PITCH_UNITY as u32;
            self.sample_idx += 1;
            if self.sample_idx as usize >= SAMPLES_PER_BLOCK {
                self.sample_idx = 0;
                self.advance_to_next_block(ram);
                if !self.has_block {
                    break;
                }
            }
        }

        (left, right)
    }

    /// Decode the block at `cur_block_addr` into `block_pcm`. Sets
    /// `has_block = false` on bad-header (treats as EOS).
    fn fetch_block(&mut self, ram: &SpuRam) {
        if self.has_block {
            self.prev_block_tail = [
                self.block_pcm[SAMPLES_PER_BLOCK - 3],
                self.block_pcm[SAMPLES_PER_BLOCK - 2],
                self.block_pcm[SAMPLES_PER_BLOCK - 1],
            ];
        }
        let bytes = ram.slice(self.cur_block_addr, BLOCK_BYTES as u32);
        if bytes.len() < BLOCK_BYTES {
            self.has_block = false;
            return;
        }
        let (pcm, flags) = self.decoder.decode_block(bytes);
        if flags.bad_header {
            self.has_block = false;
            return;
        }
        // Latch loop address if we don't have one yet and this block sets it.
        if self.loop_addr.is_none() && flags.loop_start {
            self.loop_addr = Some(self.cur_block_addr);
        }
        self.block_pcm = pcm;
        self.has_block = true;
    }

    fn sample_at_relative(&self, rel: i32) -> i16 {
        let idx = self.sample_idx as i32 + rel;
        if idx < 0 {
            let tail_idx = (idx + 3).clamp(0, 2) as usize;
            self.prev_block_tail[tail_idx]
        } else {
            self.block_pcm
                .get(idx as usize)
                .copied()
                .unwrap_or_else(|| self.block_pcm[SAMPLES_PER_BLOCK - 1])
        }
    }

    fn interpolated_sample(&self) -> i16 {
        match self.interpolation {
            InterpolationMode::Nearest => self.block_pcm[self.sample_idx as usize],
            InterpolationMode::Linear => {
                let a = self.sample_at_relative(0) as i32;
                let b = self.sample_at_relative(1) as i32;
                let frac = self.sample_frac.min(PITCH_UNITY as u32 - 1) as i32;
                (a + (((b - a) * frac) >> 12)).clamp(i16::MIN as i32, i16::MAX as i32) as i16
            }
            InterpolationMode::GaussianApprox => {
                let i = gaussian::interpolation_index(self.sample_frac);
                gaussian::interpolate(
                    self.sample_at_relative(-3),
                    self.sample_at_relative(-2),
                    self.sample_at_relative(-1),
                    self.sample_at_relative(0),
                    i,
                )
            }
        }
    }

    /// Decide what comes after the current block: jump to loop, advance to
    /// next sequential block, or stop.
    fn advance_to_next_block(&mut self, ram: &SpuRam) {
        let bytes = ram.slice(self.cur_block_addr, BLOCK_BYTES as u32);
        let flag = if bytes.len() >= 2 { bytes[1] } else { 0 };
        let end = flag & 0x01 != 0;
        let repeat = flag & 0x02 != 0;
        if end {
            if repeat {
                // Loop back. Use loop_addr if latched, otherwise stop.
                if let Some(addr) = self.loop_addr {
                    self.cur_block_addr = addr;
                    self.fetch_block(ram);
                } else {
                    self.has_block = false;
                    self.adsr.phase = Phase::Off;
                }
            } else {
                self.has_block = false;
                self.adsr.phase = Phase::Off;
            }
        } else {
            self.cur_block_addr += BLOCK_BYTES as u32;
            self.fetch_block(ram);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::ram::SpuRam;
    use super::*;

    /// Build a synthetic ADPCM stream of `n_blocks` silence-blocks; final
    /// block carries an end flag and (optionally) the repeat flag.
    fn synth_silence_stream(n: usize, repeat_at_end: bool) -> Vec<u8> {
        let mut out = vec![0u8; n * BLOCK_BYTES];
        let last = (n - 1) * BLOCK_BYTES;
        out[last + 1] = if repeat_at_end { 0x03 } else { 0x01 };
        out
    }

    /// Voice with default ADSR (instant attack) plays the first block and
    /// envelope advances toward decay.
    #[test]
    fn voice_key_on_starts_envelope_and_yields_silence_for_silent_stream() {
        let stream = synth_silence_stream(1, false);
        let mut ram = SpuRam::new();
        ram.write_at(0x1000, &stream);
        let mut v = Voice {
            start_addr: 0x1000,
            ..Voice::default()
        };
        v.key_on(&ram);
        let mut sum = 0i64;
        for _ in 0..32 {
            let (l, _r) = v.tick(&ram);
            sum += l as i64;
        }
        // Silence block + any envelope -> still silence.
        assert_eq!(sum, 0);
    }

    /// End-flag without repeat halts the voice.
    #[test]
    fn voice_stops_at_end_flag_no_repeat() {
        let stream = synth_silence_stream(1, false);
        let mut ram = SpuRam::new();
        ram.write_at(0x1000, &stream);
        let mut v = Voice {
            start_addr: 0x1000,
            ..Voice::default()
        };
        v.key_on(&ram);
        // 28 samples at unity pitch consumes the whole block -> next tick stops.
        for _ in 0..32 {
            v.tick(&ram);
        }
        assert!(v.is_off());
    }

    /// Build an "infinite-sustain" ADSR config so the envelope doesn't
    /// drain the voice during a streaming-loop test.
    fn hold_forever_adsr() -> AdsrConfig {
        AdsrConfig {
            sustain_level: 0x8000,   // sustain at peak
            sustain_decrease: false, // not strictly needed at peak
            sustain_shift: 0x1F,     // huge shift -> tiny step
            decay_shift: 0,
            ..AdsrConfig::default()
        }
    }

    /// End-flag with repeat + an explicit loop addr keeps the voice's
    /// playback head looping forever.
    #[test]
    fn voice_loops_when_end_repeat_and_loop_addr_set() {
        let stream = synth_silence_stream(1, true);
        let mut ram = SpuRam::new();
        ram.write_at(0x1000, &stream);
        let mut v = Voice {
            start_addr: 0x1000,
            adsr_cfg: hold_forever_adsr(),
            ..Voice::default()
        };
        v.set_loop_addr(0x1000);
        v.key_on(&ram);
        for _ in 0..200 {
            v.tick(&ram);
        }
        // Voice's playback head should still be valid (repeat brought it
        // back to start). Envelope held by config -> not off.
        assert!(!v.is_off());
    }

    /// Pitch=0x800 means we walk through samples at half speed -> twice as
    /// many ticks needed to consume a block.
    #[test]
    fn voice_pitch_half_doubles_block_lifetime() {
        // Two-block stream so we can detect block-edge crossing.
        let stream = synth_silence_stream(2, false);
        let mut ram = SpuRam::new();
        ram.write_at(0x1000, &stream);
        let mut v = Voice {
            start_addr: 0x1000,
            pitch: 0x800,
            adsr_cfg: hold_forever_adsr(),
            ..Voice::default()
        };
        v.key_on(&ram);
        // After 55 ticks at half pitch, fractional advance = 55*0x800 =
        // 27.5 input samples -> still inside block 0. Voice active.
        for _ in 0..55 {
            v.tick(&ram);
        }
        assert!(!v.is_off());
    }

    #[test]
    fn unity_pitch_advances_one_sample_per_tick() {
        let stream = synth_silence_stream(2, false);
        let mut ram = SpuRam::new();
        ram.write_at(0x1000, &stream);
        let mut v = Voice {
            start_addr: 0x1000,
            pitch: PITCH_UNITY,
            adsr_cfg: hold_forever_adsr(),
            ..Voice::default()
        };
        v.key_on(&ram);
        assert_eq!(v.sample_idx, 0);
        assert_eq!(v.sample_frac, 0);
        v.tick(&ram);
        assert_eq!(v.sample_idx, 1);
        assert_eq!(v.sample_frac, 0);
    }

    #[test]
    fn gaussian_voice_render_uses_fractional_pitch_shape() {
        let mut stream = vec![0u8; BLOCK_BYTES * 2];
        stream[0] = 0x00;
        stream[1] = 0x00;
        stream[2] = 0x11;
        let end = BLOCK_BYTES;
        stream[end] = 0x00;
        stream[end + 1] = 0x01;

        let mut ram = SpuRam::new();
        ram.write_at(0x1000, &stream);
        let mut v = Voice {
            start_addr: 0x1000,
            pitch: 0x0800,
            adsr_cfg: hold_forever_adsr(),
            interpolation: InterpolationMode::GaussianApprox,
            ..Voice::default()
        };
        v.key_on(&ram);

        let frames: Vec<i32> = (0..8).map(|_| v.tick(&ram).0).collect();
        assert!(frames.iter().any(|&s| s != 0));
        assert_ne!(
            frames[0], frames[1],
            "fractional stepping should interpolate between samples"
        );
    }

    #[test]
    fn sample_uses_current_adsr_level_before_ticking_envelope() {
        let mut stream = vec![0u8; BLOCK_BYTES];
        stream[1] = 0x01;
        for b in &mut stream[2..] {
            *b = 0x11;
        }

        let mut ram = SpuRam::new();
        ram.write_at(0x1000, &stream);
        let mut v = Voice {
            start_addr: 0x1000,
            adsr_cfg: AdsrConfig {
                attack_shift: 0,
                decay_shift: 0,
                sustain_level: 0x7FFF,
                ..AdsrConfig::default()
            },
            ..Voice::default()
        };
        v.key_on(&ram);
        assert_eq!(v.adsr.level, 0);

        let first = v.tick(&ram).0;
        assert_eq!(
            first, 0,
            "DuckStation-style order applies the current ADSR level before ticking it"
        );
        assert!(v.adsr.level > 0);
    }

    #[test]
    fn gaussian_voice_taps_match_duckstation_order() {
        let mut v = Voice {
            interpolation: InterpolationMode::GaussianApprox,
            sample_idx: 0,
            sample_frac: 0x0800,
            prev_block_tail: [-3000, -2000, -1000],
            block_pcm: [0; SAMPLES_PER_BLOCK],
            has_block: true,
            ..Voice::default()
        };
        v.block_pcm[0] = 1000;
        let i = gaussian::interpolation_index(v.sample_frac);
        let expected = gaussian::interpolate(-3000, -2000, -1000, 1000, i);
        assert_eq!(v.interpolated_sample(), expected);
    }

    #[test]
    fn pitch_step_clamps_to_duckstation_max() {
        let stream = synth_silence_stream(2, false);
        let mut ram = SpuRam::new();
        ram.write_at(0x1000, &stream);
        let mut v = Voice {
            start_addr: 0x1000,
            pitch: 0xFFFF,
            adsr_cfg: hold_forever_adsr(),
            ..Voice::default()
        };
        v.key_on(&ram);
        v.tick(&ram);
        assert_eq!(v.sample_idx, 3);
        assert_eq!(v.sample_frac, 0x0FFF);
    }
}
