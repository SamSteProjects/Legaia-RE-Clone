//! Bind a parsed VAB sound bank ([`legaia_vab::VabReport`] + raw VAG body
//! bytes) into a [`crate::Spu`] instance.
//!
//! This is the bridge between the asset-extraction track (which parses VAB
//! files off disc) and the engine-reimplementation track (which plays them
//! through the clean-room SPU). One bank is uploaded once via
//! [`VabBank::upload`], then the engine triggers notes via
//! [`VabBank::play_note`] which:
//!  1. picks a tone from the program based on the requested key,
//!  2. allocates an idle SPU voice,
//!  3. sets sample address, ADSR, volume, pitch,
//!  4. fires `key_on`.
//!
//! Pitch math follows the standard libspu key-to-pitch formula:
//!
//! ```text
//!   semitones = (note - center) + (-fine / 128)
//!   pitch_ratio = 2^(semitones / 12)
//!   pitch_register = base_pitch * pitch_ratio  (clipped to 0..=0x3FFF)
//! ```
//!
//! `base_pitch` is the playback pitch when `note == center`: `0x1000`.
//! The sample's intended musical rate is captured by the tone's center/fine
//! key metadata, not by a global 22.05 kHz resampling factor.
//!
//! No Sony bytes - algorithm is the documented libspu surface.

use crate::Spu;
use crate::spu::{
    adsr::AdsrConfig,
    ram::{SpuAllocator, TransferDirection},
    voice::PITCH_UNITY,
};
use legaia_vab::{VabReport, VagAtr};

/// Default sample rate used when exporting/auditioning decoded VAG bodies as
/// standalone PCM. The SPU consumes one decoded ADPCM sample per 44.1 kHz
/// mixer tick when the pitch register is `0x1000`; sequenced playback still
/// derives pitch from MIDI key versus VAB tone center/fine metadata.
pub const VAB_SAMPLE_RATE: u32 = super::spu::voice::SPU_INTERNAL_RATE;

/// VAB tone `mode` bit observed on Legaia BGM tones that should route the
/// resulting SPU voice into the reverb send bus.
pub const TONE_MODE_REVERB_SEND: u8 = 0x04;

/// Per-VAG metadata after upload: where in SPU RAM the body lives.
#[derive(Debug, Clone, Copy)]
pub struct UploadedVag {
    /// Start address in SPU RAM (bytes).
    pub addr: u32,
    /// Body size in bytes.
    pub size: u32,
}

/// A VAB bank, ready for playback. Holds the per-VAG addresses + program
/// table needed to translate "play program P note N" into voice config.
#[derive(Debug, Clone)]
pub struct VabBank {
    pub master_vol: u8,
    pub samples: Vec<Option<UploadedVag>>,
    /// Per-program tone table. Index is program 0..=ps-1; each entry is
    /// the same Vec<VagAtr> that VabReport carries, copied so we don't
    /// need to keep VabReport alive.
    pub programs: Vec<Vec<VagAtr>>,
}

impl VabBank {
    /// Upload every VAG body in `report` into `spu`'s RAM, allocating
    /// through `alloc`. The raw `bank_buf` is the same byte slice that
    /// was passed to `legaia_vab::parse` so `VagSampleSpan::byte_offset`
    /// indexes are valid.
    pub fn upload(
        spu: &mut Spu,
        alloc: &mut SpuAllocator,
        report: &VabReport,
        bank_buf: &[u8],
    ) -> Self {
        let mut samples: Vec<Option<UploadedVag>> = Vec::with_capacity(report.vag_samples.len());
        spu.ram.set_direction(TransferDirection::CpuToSpu);
        for span in &report.vag_samples {
            if span.size == 0 {
                samples.push(None);
                continue;
            }
            let body = sample_body(bank_buf, report, span);
            let Some(body) = body else {
                log::warn!(
                    "vab_bind: sample index {} body OOB (offset 0x{:X}, size {})",
                    span.index,
                    span.byte_offset,
                    span.size
                );
                samples.push(None);
                continue;
            };
            let body = trim_leading_bad_adpcm_blocks(body);
            // Allocate aligned to 16 (one ADPCM block).
            match alloc.alloc(body.len() as u32) {
                Some(addr) => {
                    spu.ram.write_at(addr, body);
                    samples.push(Some(UploadedVag {
                        addr,
                        size: body.len() as u32,
                    }));
                }
                None => {
                    log::warn!(
                        "vab_bind: SPU RAM exhausted at sample index {} ({} bytes)",
                        span.index,
                        span.size
                    );
                    samples.push(None);
                }
            }
        }
        Self {
            master_vol: report.header.mvol,
            samples,
            programs: report.tones.clone(),
        }
    }

    /// Play `note` (MIDI key, 0..=127) through `program` index using `voice`
    /// on the SPU. Velocity 0..=127 scales the per-tone volume.
    ///
    /// Returns `false` when the program / tone / sample isn't valid (so the
    /// engine can log + skip without panicking on bad bank data).
    pub fn play_note(
        &self,
        spu: &mut Spu,
        voice: usize,
        program: usize,
        note: u8,
        velocity: u8,
    ) -> bool {
        self.play_note_with_bend(spu, voice, program, note, velocity, 0x2000)
    }

    /// Play `note` with a MIDI pitch-bend value (`0x2000` = centered).
    pub fn play_note_with_bend(
        &self,
        spu: &mut Spu,
        voice: usize,
        program: usize,
        note: u8,
        velocity: u8,
        pitch_bend: u16,
    ) -> bool {
        let Some(tones) = self.programs.get(program) else {
            return false;
        };
        let Some(tone) = tones.iter().find(|t| note >= t.min && note <= t.max) else {
            return false;
        };
        // tone.vag is 1-based in PSX VAB format. legaia_vab::VabReport's
        // `vag_samples` is 0-indexed (samples[0..vs]), so subtract 1.
        if tone.vag <= 0 {
            return false;
        }
        let sample_idx = (tone.vag - 1) as usize;
        let Some(Some(vag)) = self.samples.get(sample_idx) else {
            return false;
        };
        if voice >= spu.voices.len() {
            return false;
        }
        let pitch = pitch_register_for_tone(note, tone, pitch_bend);
        let bank_master = self.master_vol as i32;
        let prog_vol = tone.vol as i32;
        let vel = velocity as i32;
        // libspu mvol/vol/velocity all scale linearly into the 0..=0x3FFF
        // voice register: vol = bank * prog * vel / (127^3) * 0x3FFF.
        let combined = ((bank_master * prog_vol * vel) / (127 * 127)).min(0x3FFF) as i16;
        let pan = tone.pan as i32; // 0..=127, 64 = center
        let (vol_l, vol_r) = pan_split(combined, pan);
        {
            let v = &mut spu.voices[voice];
            v.start_addr = vag.addr;
            v.loop_addr = None;
            v.pitch = pitch;
            v.vol_left = vol_l;
            v.vol_right = vol_r;
            v.adsr_cfg = AdsrConfig::from_words(tone.adsr1, tone.adsr2);
            v.set_reverb_send(tone.mode & TONE_MODE_REVERB_SEND != 0);
        }
        let crate::spu::Spu {
            ref mut voices,
            ref ram,
            ..
        } = *spu;
        voices[voice].key_on(ram);
        true
    }

    /// Return the pitch register that would be used for this program/note at
    /// the supplied MIDI pitch-bend value. Used to update already-sounding
    /// voices when the SEQ emits `0xE0` events.
    pub fn pitch_for_note(&self, program: usize, note: u8, pitch_bend: u16) -> Option<u16> {
        let tones = self.programs.get(program)?;
        let tone = tones.iter().find(|t| note >= t.min && note <= t.max)?;
        Some(pitch_register_for_tone(note, tone, pitch_bend))
    }
}

fn sample_body<'a>(
    buf: &'a [u8],
    report: &VabReport,
    span: &legaia_vab::VagSampleSpan,
) -> Option<&'a [u8]> {
    buf.get(span.byte_offset..span.byte_offset.checked_add(span.size)?)
        .or_else(|| {
            let rel = span.byte_offset.checked_sub(report.header_offset)?;
            buf.get(rel..rel.checked_add(span.size)?)
        })
}

fn trim_leading_bad_adpcm_blocks(mut body: &[u8]) -> &[u8] {
    while body.len() >= 16 {
        let filter = (body[0] >> 4) & 0x0F;
        if filter <= 4 {
            break;
        }
        body = &body[16..];
    }
    body
}

/// Compute the SPU pitch register for one resolved VAB tone. Exposed for
/// debug tooling so a selected SEQ note can show the exact pitch value that
/// sequenced playback will write to the allocated SPU voice.
pub fn pitch_register_for_tone(note: u8, tone: &VagAtr, pitch_bend: u16) -> u16 {
    // PsyQ's key-to-pitch path works in 1/128-semitone fine units:
    // ((key1*0x80 + fine1) - (key2*0x80 + fine2)) / (12*0x80).
    // The VAB tone `shift` byte is the fine component of the sample's
    // center key, not a cents value.
    let fine_semitones = (tone.shift as i8 as f64) / 128.0;
    let bend = pitch_bend_semitones(pitch_bend, tone);
    let semitones = note as f64 - tone.center as f64 - fine_semitones + bend;
    let ratio = 2f64.powf(semitones / 12.0);
    let pitch = (PITCH_UNITY as f64 * ratio).round() as i64;
    pitch.clamp(1, 0x3FFF) as u16
}

fn pitch_bend_semitones(value: u16, tone: &VagAtr) -> f64 {
    let value = value.min(0x3FFF);
    if value == 0x2000 {
        return 0.0;
    }
    if value < 0x2000 {
        let range = tone.pbmin.max(2) as f64;
        -((0x2000 - value) as f64 / 0x2000 as f64) * range
    } else {
        let range = tone.pbmax.max(2) as f64;
        ((value - 0x2000) as f64 / 0x1FFF as f64) * range
    }
}

/// Split a combined volume into (left, right) based on a 0..=127 pan value
/// (64 = center). Equal-power-ish: linear left/right scaling.
fn pan_split(vol: i16, pan: i32) -> (i16, i16) {
    let pan = pan.clamp(0, 127);
    let left = (vol as i32 * (127 - pan) / 64).min(0x3FFF) as i16;
    let right = (vol as i32 * pan / 64).min(0x3FFF) as i16;
    (left, right)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_tone(center: u8, vag: i16, vol: u8, pan: u8) -> VagAtr {
        VagAtr {
            prior: 0,
            mode: 0,
            vol,
            pan,
            center,
            shift: 0,
            min: 0,
            max: 127,
            vibw: 0,
            vibt: 0,
            porw: 0,
            port: 0,
            pbmin: 0,
            pbmax: 0,
            reserved1: 0,
            reserved2: 0,
            adsr1: 0,
            adsr2: 0,
            prog: 0,
            vag,
            reserved3: [0; 4],
        }
    }

    /// Note at center maps to SPU pitch unity. The VAB body is already SPU
    /// ADPCM; its musical rate is represented by tone center/fine metadata.
    #[test]
    fn pitch_at_center_matches_spu_unity() {
        let tone = dummy_tone(60, 1, 127, 64);
        let pitch = pitch_register_for_tone(60, &tone, 0x2000);
        assert_eq!(pitch, 0x1000);
    }

    /// One semitone above center bumps pitch by 2^(1/12) ≈ 1.0595.
    #[test]
    fn pitch_one_semitone_above_is_higher() {
        let tone = dummy_tone(60, 1, 127, 64);
        let p_center = pitch_register_for_tone(60, &tone, 0x2000);
        let p_above = pitch_register_for_tone(61, &tone, 0x2000);
        assert!(p_above > p_center);
        let ratio = p_above as f64 / p_center as f64;
        assert!((ratio - 2f64.powf(1.0 / 12.0)).abs() < 0.001);
    }

    #[test]
    fn tone_shift_uses_one_over_128_semitone_units() {
        let mut tone = dummy_tone(60, 1, 127, 64);
        tone.shift = 64;
        let shifted = pitch_register_for_tone(60, &tone, 0x2000);
        let center = pitch_register_for_tone(60, &dummy_tone(60, 1, 127, 64), 0x2000);
        let ratio = shifted as f64 / center as f64;
        assert!((ratio - 2f64.powf(-0.5 / 12.0)).abs() < 0.001);
    }

    #[test]
    fn pitch_bend_uses_tone_bend_range() {
        let mut tone = dummy_tone(60, 1, 127, 64);
        tone.pbmin = 2;
        tone.pbmax = 12;
        let center = pitch_register_for_tone(60, &tone, 0x2000);
        let up = pitch_register_for_tone(60, &tone, 0x3FFF);
        let down = pitch_register_for_tone(60, &tone, 0);
        assert!(up > center);
        assert!(down < center);
        assert!((up as f64 / center as f64 - 2.0).abs() < 0.01);
    }

    /// Pan=0 silences right; pan=127 silences left; pan=64 is roughly equal.
    #[test]
    fn pan_split_endpoints_silence_opposite_side() {
        let (l, r) = pan_split(0x3FFF, 0);
        assert!(l > 0);
        assert_eq!(r, 0);
        let (l, r) = pan_split(0x3FFF, 127);
        assert_eq!(l, 0);
        assert!(r > 0);
        let (l, r) = pan_split(0x3FFF, 64);
        // Center pan: left ≈ vol * 63/64, right = vol. Difference is ~vol/64.
        assert!((l as i32 - r as i32).abs() <= 0x100);
    }

    /// VabBank::play_note returns false for an invalid program index without
    /// panicking; voice state is left untouched.
    #[test]
    fn play_note_invalid_program_returns_false() {
        let mut spu = Spu::new();
        let bank = VabBank {
            master_vol: 127,
            samples: vec![],
            programs: vec![],
        };
        let ok = bank.play_note(&mut spu, 0, 99, 60, 100);
        assert!(!ok);
        // Voice 0 still in default Off state.
        assert!(spu.voices[0].is_off());
    }
}
