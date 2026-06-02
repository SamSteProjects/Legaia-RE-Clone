//! Clean-room PSX SPU reverb model.
//!
//! This implements the register-style reverb topology documented by PSXSPX:
//! SAME/DIFF reflections with IIR feedback, four comb taps, two all-pass
//! filters, and output return volumes over a circular SPU-RAM work area.
//!
//! References:
//! - https://problemkaputt.de/psxspx-spu-reverb-formula.htm
//! - https://problemkaputt.de/psxspx-spu-reverb-examples.htm
//!
//! The preset tables below are the public no$psx/PSXSPX register examples
//! for Room, Studio Small/Medium/Large, Hall, Half Echo, Space Echo, Chaos
//! Echo, Delay, and Off. Values are coefficients/offsets, not Sony program
//! bytes.
//!
//! DuckStation parity note:
//! DuckStation's `src/core/spu.cpp::ProcessReverb` uses the same public SPU
//! topology but names the registers after Mednafen (`IIR_ALPHA`, `ACC_COEF_*`,
//! `FB_ALPHA`, `FB_X`, `MIX_DEST_*`, `IN_COEF`). This module keeps the PSXSPX
//! names (`vIIR`, `vCOMB*`, `vAPF*`, `m*`, `d*`) because those are the names
//! used by the project documentation and preset tables. The arithmetic below
//! follows the DuckStation-shaped fixed-point stages: most reverb coefficients
//! are Q14 with explicit `>> 1` averaging at IIR/APF mix points, while final
//! APF output feedback uses Q15. The output return volume (`vLOUT/vROUT`) is
//! intentionally external here as `Spu::reverb_wet_gain_q14`; the route/debug
//! tools need to scale the wet return without changing internal feedback.

/// Standard PSX SPU reverb modes. Names match libspu's `SsSetReverbType`
/// enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReverbMode {
    /// Reverb disabled - voices with `reverb_send` produce no echo.
    Off,
    /// Small room.
    Room,
    /// Studio A.
    StudioA,
    /// Studio B.
    StudioB,
    /// Studio C.
    StudioC,
    /// Hall.
    Hall,
    /// Space / space echo.
    Space,
    /// Half echo.
    Echo,
    /// Long one-shot delay.
    Delay,
    /// Pipe-like resonant mode. PSXSPX's public table calls this "Chaos Echo";
    /// it is the closest documented resonant preset for the libspu slot.
    Pipe,
}

#[derive(Debug, Clone, Copy)]
pub struct ReverbParams {
    pub size_bytes: usize,
    pub regs: ReverbRegs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReverbImpulseReport {
    pub mode: ReverbMode,
    pub peak: i16,
    pub first_nonzero_sample: Option<usize>,
    pub approximate_decay_sample: Option<usize>,
    pub major_tap_count: usize,
}

const FIR_TAPS: usize = 39;
const REVERB_FIR: [i16; FIR_TAPS] = [
    -0x0001, 0x0000, 0x0002, 0x0000, -0x000A, 0x0000, 0x0023, 0x0000, -0x0067, 0x0000, 0x010A,
    0x0000, -0x0268, 0x0000, 0x0534, 0x0000, -0x0B90, 0x0000, 0x2806, 0x4000, 0x2806, 0x0000,
    -0x0B90, 0x0000, 0x0534, 0x0000, -0x0268, 0x0000, 0x010A, 0x0000, -0x0067, 0x0000, 0x0023,
    0x0000, -0x000A, 0x0000, 0x0002, 0x0000, -0x0001,
];

#[derive(Debug, Clone, Copy)]
pub struct ReverbRegs {
    pub d_apf1: u16,
    pub d_apf2: u16,
    pub v_iir: i16,
    pub v_comb1: i16,
    pub v_comb2: i16,
    pub v_comb3: i16,
    pub v_comb4: i16,
    pub v_wall: i16,
    pub v_apf1: i16,
    pub v_apf2: i16,
    pub m_lsame: u16,
    pub m_rsame: u16,
    pub m_lcomb1: u16,
    pub m_rcomb1: u16,
    pub m_lcomb2: u16,
    pub m_rcomb2: u16,
    pub d_lsame: u16,
    pub d_rsame: u16,
    pub m_ldiff: u16,
    pub m_rdiff: u16,
    pub m_lcomb3: u16,
    pub m_rcomb3: u16,
    pub m_lcomb4: u16,
    pub m_rcomb4: u16,
    pub d_ldiff: u16,
    pub d_rdiff: u16,
    pub m_lapf1: u16,
    pub m_rapf1: u16,
    pub m_lapf2: u16,
    pub m_rapf2: u16,
    pub v_lin: i16,
    pub v_rin: i16,
}

macro_rules! regs {
    (
        $d_apf1:expr,$d_apf2:expr,$v_iir:expr,$v_comb1:expr,$v_comb2:expr,$v_comb3:expr,$v_comb4:expr,$v_wall:expr,
        $v_apf1:expr,$v_apf2:expr,$m_lsame:expr,$m_rsame:expr,$m_lcomb1:expr,$m_rcomb1:expr,$m_lcomb2:expr,$m_rcomb2:expr,
        $d_lsame:expr,$d_rsame:expr,$m_ldiff:expr,$m_rdiff:expr,$m_lcomb3:expr,$m_rcomb3:expr,$m_lcomb4:expr,$m_rcomb4:expr,
        $d_ldiff:expr,$d_rdiff:expr,$m_lapf1:expr,$m_rapf1:expr,$m_lapf2:expr,$m_rapf2:expr,$v_lin:expr,$v_rin:expr
    ) => {
        ReverbRegs {
            d_apf1: $d_apf1,
            d_apf2: $d_apf2,
            v_iir: $v_iir as u16 as i16,
            v_comb1: $v_comb1 as u16 as i16,
            v_comb2: $v_comb2 as u16 as i16,
            v_comb3: $v_comb3 as u16 as i16,
            v_comb4: $v_comb4 as u16 as i16,
            v_wall: $v_wall as u16 as i16,
            v_apf1: $v_apf1 as u16 as i16,
            v_apf2: $v_apf2 as u16 as i16,
            m_lsame: $m_lsame,
            m_rsame: $m_rsame,
            m_lcomb1: $m_lcomb1,
            m_rcomb1: $m_rcomb1,
            m_lcomb2: $m_lcomb2,
            m_rcomb2: $m_rcomb2,
            d_lsame: $d_lsame,
            d_rsame: $d_rsame,
            m_ldiff: $m_ldiff,
            m_rdiff: $m_rdiff,
            m_lcomb3: $m_lcomb3,
            m_rcomb3: $m_rcomb3,
            m_lcomb4: $m_lcomb4,
            m_rcomb4: $m_rcomb4,
            d_ldiff: $d_ldiff,
            d_rdiff: $d_rdiff,
            m_lapf1: $m_lapf1,
            m_rapf1: $m_rapf1,
            m_lapf2: $m_lapf2,
            m_rapf2: $m_rapf2,
            v_lin: $v_lin as u16 as i16,
            v_rin: $v_rin as u16 as i16,
        }
    };
}

impl ReverbMode {
    pub fn params(self) -> ReverbParams {
        match self {
            ReverbMode::Off => ReverbParams {
                size_bytes: 0x10,
                regs: regs!(
                    0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000,
                    0x0001, 0x0001, 0x0001, 0x0001, 0x0001, 0x0001, 0x0000, 0x0000, 0x0001, 0x0001,
                    0x0001, 0x0001, 0x0001, 0x0001, 0x0000, 0x0000, 0x0001, 0x0001, 0x0001, 0x0001,
                    0x0000, 0x0000
                ),
            },
            ReverbMode::Room => ReverbParams {
                size_bytes: 0x26C0,
                regs: regs!(
                    0x007D, 0x005B, 0x6D80, 0x54B8, 0xBED0, 0x0000, 0x0000, 0xBA80, 0x5800, 0x5300,
                    0x04D6, 0x0333, 0x03F0, 0x0227, 0x0374, 0x01EF, 0x0334, 0x01B5, 0x0000, 0x0000,
                    0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x01B4, 0x0136, 0x00B8, 0x005C,
                    0x8000, 0x8000
                ),
            },
            ReverbMode::StudioA => ReverbParams {
                size_bytes: 0x1F40,
                regs: regs!(
                    0x0033, 0x0025, 0x70F0, 0x4FA8, 0xBCE0, 0x4410, 0xC0F0, 0x9C00, 0x5280, 0x4EC0,
                    0x03E4, 0x031B, 0x03A4, 0x02AF, 0x0372, 0x0266, 0x031C, 0x025D, 0x025C, 0x018E,
                    0x022F, 0x0135, 0x01D2, 0x00B7, 0x018F, 0x00B5, 0x00B4, 0x0080, 0x004C, 0x0026,
                    0x8000, 0x8000
                ),
            },
            ReverbMode::StudioB => ReverbParams {
                size_bytes: 0x4840,
                regs: regs!(
                    0x00B1, 0x007F, 0x70F0, 0x4FA8, 0xBCE0, 0x4510, 0xBEF0, 0xB4C0, 0x5280, 0x4EC0,
                    0x0904, 0x076B, 0x0824, 0x065F, 0x07A2, 0x0616, 0x076C, 0x05ED, 0x05EC, 0x042E,
                    0x050F, 0x0305, 0x0462, 0x02B7, 0x042F, 0x0265, 0x0264, 0x01B2, 0x0100, 0x0080,
                    0x8000, 0x8000
                ),
            },
            ReverbMode::StudioC => ReverbParams {
                size_bytes: 0x6FE0,
                regs: regs!(
                    0x00E3, 0x00A9, 0x6F60, 0x4FA8, 0xBCE0, 0x4510, 0xBEF0, 0xA680, 0x5680, 0x52C0,
                    0x0DFB, 0x0B58, 0x0D09, 0x0A3C, 0x0BD9, 0x0973, 0x0B59, 0x08DA, 0x08D9, 0x05E9,
                    0x07EC, 0x04B0, 0x06EF, 0x03D2, 0x05EA, 0x031D, 0x031C, 0x0238, 0x0154, 0x00AA,
                    0x8000, 0x8000
                ),
            },
            ReverbMode::Hall => ReverbParams {
                size_bytes: 0xADE0,
                regs: regs!(
                    0x01A5, 0x0139, 0x6000, 0x5000, 0x4C00, 0xB800, 0xBC00, 0xC000, 0x6000, 0x5C00,
                    0x15BA, 0x11BB, 0x14C2, 0x10BD, 0x11BC, 0x0DC1, 0x11C0, 0x0DC3, 0x0DC0, 0x09C1,
                    0x0BC4, 0x07C1, 0x0A00, 0x06CD, 0x09C2, 0x05C1, 0x05C0, 0x041A, 0x0274, 0x013A,
                    0x8000, 0x8000
                ),
            },
            ReverbMode::Echo => ReverbParams {
                size_bytes: 0x3C00,
                regs: regs!(
                    0x0017, 0x0013, 0x70F0, 0x4FA8, 0xBCE0, 0x4510, 0xBEF0, 0x8500, 0x5F80, 0x54C0,
                    0x0371, 0x02AF, 0x02E5, 0x01DF, 0x02B0, 0x01D7, 0x0358, 0x026A, 0x01D6, 0x011E,
                    0x012D, 0x00B1, 0x011F, 0x0059, 0x01A0, 0x00E3, 0x0058, 0x0040, 0x0028, 0x0014,
                    0x8000, 0x8000
                ),
            },
            ReverbMode::Space => ReverbParams {
                size_bytes: 0xF6C0,
                regs: regs!(
                    0x033D, 0x0231, 0x7E00, 0x5000, 0xB400, 0xB000, 0x4C00, 0xB000, 0x6000, 0x5400,
                    0x1ED6, 0x1A31, 0x1D14, 0x183B, 0x1BC2, 0x16B2, 0x1A32, 0x15EF, 0x15EE, 0x1055,
                    0x1334, 0x0F2D, 0x11F6, 0x0C5D, 0x1056, 0x0AE1, 0x0AE0, 0x07A2, 0x0464, 0x0232,
                    0x8000, 0x8000
                ),
            },
            ReverbMode::Pipe => ReverbParams {
                size_bytes: 0x18040,
                regs: regs!(
                    0x0001, 0x0001, 0x7FFF, 0x7FFF, 0x0000, 0x0000, 0x0000, 0x8100, 0x0000, 0x0000,
                    0x1FFF, 0x0FFF, 0x1005, 0x0005, 0x0000, 0x0000, 0x1005, 0x0005, 0x0000, 0x0000,
                    0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x1004, 0x1002, 0x0004, 0x0002,
                    0x8000, 0x8000
                ),
            },
            ReverbMode::Delay => ReverbParams {
                size_bytes: 0x18040,
                regs: regs!(
                    0x0001, 0x0001, 0x7FFF, 0x7FFF, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000,
                    0x1FFF, 0x0FFF, 0x1005, 0x0005, 0x0000, 0x0000, 0x1005, 0x0005, 0x0000, 0x0000,
                    0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x1004, 0x1002, 0x0004, 0x0002,
                    0x8000, 0x8000
                ),
            },
        }
    }

    /// Decode from a libspu-style mode byte. Out-of-range values map to
    /// [`ReverbMode::Off`] to keep the engine silent rather than panic.
    pub fn from_byte(b: u8) -> Self {
        match b {
            0 => ReverbMode::Off,
            1 => ReverbMode::Room,
            2 => ReverbMode::StudioA,
            3 => ReverbMode::StudioB,
            4 => ReverbMode::StudioC,
            5 => ReverbMode::Hall,
            6 => ReverbMode::Space,
            7 => ReverbMode::Echo,
            8 => ReverbMode::Delay,
            9 => ReverbMode::Pipe,
            _ => ReverbMode::Off,
        }
    }
}

/// Stereo PSX-style reverb processor backed by a simulated SPU work area.
#[derive(Debug, Clone)]
pub struct Reverb {
    pub mode: ReverbMode,
    params: ReverbParams,
    work: Vec<i16>,
    pos: usize,
    phase_22050: bool,
    input_l: [i16; FIR_TAPS],
    input_r: [i16; FIR_TAPS],
    output_l: [i16; FIR_TAPS],
    output_r: [i16; FIR_TAPS],
}

impl Reverb {
    pub fn new(mode: ReverbMode) -> Self {
        let params = mode.params();
        let samples = (params.size_bytes / 2).max(1);
        Self {
            mode,
            params,
            work: vec![0; samples],
            pos: 0,
            phase_22050: false,
            input_l: [0; FIR_TAPS],
            input_r: [0; FIR_TAPS],
            output_l: [0; FIR_TAPS],
            output_r: [0; FIR_TAPS],
        }
    }

    /// Reconfigure the active mode and zero-fill the reverb work area. Retail
    /// code is expected to clear this area before enabling reverb; doing it on
    /// mode changes avoids stale tails from the prior preset.
    pub fn set_mode(&mut self, mode: ReverbMode) {
        if self.mode == mode {
            return;
        }
        *self = Self::new(mode);
    }

    /// Push one stereo sample of *reverb send* signal into the processor and
    /// pull one stereo sample of *reverb wet* signal out. PSXSPX documents
    /// that the reverb core runs at 22050 Hz and that input/output are
    /// resampled through a 39-tap halfband FIR filter. This function is called
    /// at the SPU's 44100 Hz clock: every tick updates the FIR histories, and
    /// every other tick advances the 22050 Hz reverb work-area formula.
    pub fn tick(&mut self, send_l: i16, send_r: i16) -> (i16, i16) {
        if self.mode == ReverbMode::Off {
            return (0, 0);
        }
        shift_in(&mut self.input_l, send_l);
        shift_in(&mut self.input_r, send_r);
        self.phase_22050 = !self.phase_22050;
        if self.phase_22050 {
            let in_l = fir_filter(&self.input_l);
            let in_r = fir_filter(&self.input_r);
            let (l, r) = self.process_22050(in_l, in_r);
            shift_in(&mut self.output_l, l);
            shift_in(&mut self.output_r, r);
            self.pos = (self.pos + 1) % self.work.len();
        } else {
            // Zero-stuff the 22050 Hz stream before the output FIR upsamples
            // it back to 44100 Hz.
            shift_in(&mut self.output_l, 0);
            shift_in(&mut self.output_r, 0);
        }
        let l = sat_i16(fir_filter_i32(&self.output_l) * 2);
        let r = sat_i16(fir_filter_i32(&self.output_r) * 2);
        (l, r)
    }

    pub fn impulse_report(mode: ReverbMode, samples: usize) -> ReverbImpulseReport {
        let mut r = Reverb::new(mode);
        let mut peak = 0i16;
        let mut first_nonzero = None;
        let mut last_above_floor = None;
        let mut major_tap_count = 0usize;
        let mut was_below_major = true;
        for i in 0..samples {
            let send = if i == 0 { 0x4000 } else { 0 };
            let (l, rr) = r.tick(send, send);
            let p = l.abs().max(rr.abs());
            if p > 0 && first_nonzero.is_none() {
                first_nonzero = Some(i);
            }
            peak = peak.max(p);
            if p > 16 {
                last_above_floor = Some(i);
            }
            if p > 512 {
                if was_below_major {
                    major_tap_count += 1;
                    was_below_major = false;
                }
            } else {
                was_below_major = true;
            }
        }
        ReverbImpulseReport {
            mode,
            peak,
            first_nonzero_sample: first_nonzero,
            approximate_decay_sample: last_above_floor,
            major_tap_count,
        }
    }

    fn process_22050(&mut self, send_l: i16, send_r: i16) -> (i16, i16) {
        let r = self.params.regs;

        // DuckStation-shaped IIR input:
        // IIR_INPUT_A = clamp16(((IIR_SRC_A * IIR_COEF)>>14 + (input * IN_COEF)>>14) >> 1)
        // IIR_INPUT_B cross-feeds the opposite channel's DIFF source.
        let l_iir_input_a = sat_i16_i32(avg_or_half(
            mul_q14(self.read(r.d_lsame), r.v_wall) + mul_q14(send_l as i32, r.v_lin),
        )) as i32;
        let r_iir_input_a = sat_i16_i32(avg_or_half(
            mul_q14(self.read(r.d_rsame), r.v_wall) + mul_q14(send_r as i32, r.v_rin),
        )) as i32;
        let l_iir_input_b = sat_i16_i32(avg_or_half(
            mul_q14(self.read(r.d_rdiff), r.v_wall) + mul_q14(send_l as i32, r.v_lin),
        )) as i32;
        let r_iir_input_b = sat_i16_i32(avg_or_half(
            mul_q14(self.read(r.d_ldiff), r.v_wall) + mul_q14(send_r as i32, r.v_rin),
        )) as i32;

        // Same/different-side IIR:
        // IIR = clamp16(((input * IIR_ALPHA)>>14 + ((old * (0x8000-IIR_ALPHA))>>14)) >> 1)
        let l_same = sat_i16_i32(avg_or_half(
            mul_q14(l_iir_input_a, r.v_iir) + iir_feedback_q14(self.read_minus_samples(r.m_lsame, 1), r.v_iir),
        ));
        let r_same = sat_i16_i32(avg_or_half(
            mul_q14(r_iir_input_a, r.v_iir) + iir_feedback_q14(self.read_minus_samples(r.m_rsame, 1), r.v_iir),
        ));
        let l_diff = sat_i16_i32(avg_or_half(
            mul_q14(l_iir_input_b, r.v_iir) + iir_feedback_q14(self.read_minus_samples(r.m_ldiff, 1), r.v_iir),
        ));
        let r_diff = sat_i16_i32(avg_or_half(
            mul_q14(r_iir_input_b, r.v_iir) + iir_feedback_q14(self.read_minus_samples(r.m_rdiff, 1), r.v_iir),
        ));
        self.write(r.m_lsame, l_same as i32);
        self.write(r.m_rsame, r_same as i32);
        self.write(r.m_ldiff, l_diff as i32);
        self.write(r.m_rdiff, r_diff as i32);

        // ACC/comb taps: all four coefficients are Q14.
        let l_acc = mul_q14(self.read(r.m_lcomb1), r.v_comb1)
            + mul_q14(self.read(r.m_lcomb2), r.v_comb2)
            + mul_q14(self.read(r.m_lcomb3), r.v_comb3)
            + mul_q14(self.read(r.m_lcomb4), r.v_comb4);
        let r_acc = mul_q14(self.read(r.m_rcomb1), r.v_comb1)
            + mul_q14(self.read(r.m_rcomb2), r.v_comb2)
            + mul_q14(self.read(r.m_rcomb3), r.v_comb3)
            + mul_q14(self.read(r.m_rcomb4), r.v_comb4);

        // APF1/APF2 mapped from DuckStation MIX_DEST_A/B + FB_SRC_A/B:
        // MDA = clamp16((ACC + ((FB_A * -FB_ALPHA)>>14)) >> 1)
        // MDB = clamp16(FB_A + (((MDA*FB_ALPHA)>>14 + (FB_B*-FB_X)>>14) >> 1))
        // out = clamp16(FB_B + ((MDB*FB_X)>>15))
        let l_fb_a = self.read_wrapped_sub(r.m_lapf1, r.d_apf1);
        let r_fb_a = self.read_wrapped_sub(r.m_rapf1, r.d_apf1);
        let l_fb_b = self.read_wrapped_sub(r.m_lapf2, r.d_apf2);
        let r_fb_b = self.read_wrapped_sub(r.m_rapf2, r.d_apf2);

        let l_mda = sat_i16_i32(avg_or_half(l_acc + mul_q14(l_fb_a, neg_coeff(r.v_apf1))));
        let r_mda = sat_i16_i32(avg_or_half(r_acc + mul_q14(r_fb_a, neg_coeff(r.v_apf1))));
        let l_mdb = sat_i16_i32(
            l_fb_a
                + avg_or_half(mul_q14(l_mda as i32, r.v_apf1) + mul_q14(l_fb_b, neg_coeff(r.v_apf2))),
        );
        let r_mdb = sat_i16_i32(
            r_fb_a
                + avg_or_half(mul_q14(r_mda as i32, r.v_apf1) + mul_q14(r_fb_b, neg_coeff(r.v_apf2))),
        );
        let lout = sat_i16_i32(l_fb_b + mul_q15(l_mdb as i32, r.v_apf2));
        let rout = sat_i16_i32(r_fb_b + mul_q15(r_mdb as i32, r.v_apf2));

        self.write(r.m_lapf1, l_mda as i32);
        self.write(r.m_rapf1, r_mda as i32);
        self.write(r.m_lapf2, l_mdb as i32);
        self.write(r.m_rapf2, r_mdb as i32);

        (lout, rout)
    }

    fn offset_samples(offset_words: u16) -> usize {
        // The PSXSPX preset table values are reverb-buffer sample indices
        // (16-bit words), not byte counts. Several documented delay values
        // are odd (for example Hall dAPF1=0x01A5), so dividing by two halves
        // the real delay lengths and destroys the intended diffusion.
        offset_words as usize
    }

    fn addr(&self, offset_words: u16) -> usize {
        (self.pos + Self::offset_samples(offset_words)) % self.work.len()
    }

    fn read(&self, offset_words: u16) -> i32 {
        self.work[self.addr(offset_words)] as i32
    }

    fn read_wrapped_sub(&self, offset_words: u16, delay_words: u16) -> i32 {
        let offset = offset_words.wrapping_sub(delay_words);
        self.read(offset)
    }

    fn read_minus_samples(&self, offset_words: u16, samples: usize) -> i32 {
        let base = self.addr(offset_words);
        let delay = samples % self.work.len();
        self.work[(base + self.work.len() - delay) % self.work.len()] as i32
    }

    fn write(&mut self, offset_words: u16, value: i32) {
        let addr = self.addr(offset_words);
        self.work[addr] = sat_i16(value);
    }
}

fn mul_q14(sample: i32, coeff: i16) -> i32 {
    (sample * coeff as i32) >> 14
}

fn mul_q15(sample: i32, coeff: i16) -> i32 {
    (sample * coeff as i32) >> 15
}

fn avg_or_half(value: i32) -> i32 {
    value >> 1
}

fn iir_feedback_q14(sample: i32, iir_alpha: i16) -> i32 {
    if iir_alpha == i16::MIN {
        if sample == i16::MIN as i32 {
            0
        } else {
            sample * -65536 >> 14
        }
    } else {
        (sample * (32768 - iir_alpha as i32)) >> 14
    }
}

fn neg_coeff(coeff: i16) -> i16 {
    if coeff == i16::MIN {
        i16::MAX
    } else {
        -coeff
    }
}

fn shift_in(history: &mut [i16; FIR_TAPS], sample: i16) {
    history.copy_within(0..FIR_TAPS - 1, 1);
    history[0] = sample;
}

fn fir_filter(history: &[i16; FIR_TAPS]) -> i16 {
    sat_i16(fir_filter_i32(history))
}

fn fir_filter_i32(history: &[i16; FIR_TAPS]) -> i32 {
    let mut acc = 0i64;
    for i in 0..FIR_TAPS {
        acc += history[i] as i64 * REVERB_FIR[i] as i64;
    }
    (acc >> 15).clamp(i32::MIN as i64, i32::MAX as i64) as i32
}

fn sat_i16(v: i32) -> i16 {
    v.clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

fn sat_i16_i32(v: i32) -> i16 {
    sat_i16(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn impulse_peaks(mode: ReverbMode, samples: usize) -> (i16, usize) {
        let mut r = Reverb::new(mode);
        let mut peak = 0i16;
        let mut first_nonzero = usize::MAX;
        for i in 0..samples {
            let send = if i == 0 { 0x4000 } else { 0 };
            let (l, rr) = r.tick(send, send);
            let p = l.abs().max(rr.abs());
            if p > 0 && first_nonzero == usize::MAX {
                first_nonzero = i;
            }
            peak = peak.max(p);
        }
        (peak, first_nonzero)
    }

    fn impulse_signature(mode: ReverbMode, samples: usize) -> (i64, usize, usize) {
        let mut r = Reverb::new(mode);
        let mut energy = 0i64;
        let mut nonzero = 0usize;
        let mut first_loud = usize::MAX;
        for i in 0..samples {
            let send = if i == 0 { 0x4000 } else { 0 };
            let (l, rr) = r.tick(send, send);
            let p = l.abs().max(rr.abs()) as i64;
            energy += p * p;
            if p > 0 {
                nonzero += 1;
            }
            if p > 256 && first_loud == usize::MAX {
                first_loud = i;
            }
        }
        (energy, nonzero, first_loud)
    }

    #[test]
    fn off_mode_is_silent() {
        let mut r = Reverb::new(ReverbMode::Off);
        for _ in 0..1000 {
            assert_eq!(r.tick(0x4000, 0x4000), (0, 0));
        }
    }

    #[test]
    fn room_produces_delayed_output() {
        let (peak, first) = impulse_peaks(ReverbMode::Room, 8000);
        assert!(peak > 0);
        assert!(first > 0);
    }

    #[test]
    fn hall_is_diffuse_not_immediate_single_tap() {
        let mut r = Reverb::new(ReverbMode::Hall);
        let mut peak = 0i16;
        let mut nonzero = 0usize;
        for i in 0..30_000 {
            let send = if i == 0 { 0x4000 } else { 0 };
            let (l, rr) = r.tick(send, send);
            let p = l.abs().max(rr.abs());
            if p > 0 {
                nonzero += 1;
            }
            peak = peak.max(p);
        }
        assert!(peak > 0);
        assert!(nonzero > 1000);
    }

    #[test]
    fn modes_have_different_work_area_sizes() {
        assert_eq!(ReverbMode::Room.params().size_bytes, 0x26C0);
        assert_eq!(ReverbMode::Hall.params().size_bytes, 0xADE0);
        assert_eq!(ReverbMode::Space.params().size_bytes, 0xF6C0);
    }

    #[test]
    fn room_hall_echo_impulses_are_distinct() {
        let room = impulse_signature(ReverbMode::Room, 30_000);
        let hall = impulse_signature(ReverbMode::Hall, 30_000);
        let echo = impulse_signature(ReverbMode::Echo, 30_000);

        assert_ne!(room, hall);
        assert_ne!(hall, echo);
        assert_ne!(room, echo);
    }

    #[test]
    fn impulse_reports_cover_room_hall_echo_delay() {
        let room = Reverb::impulse_report(ReverbMode::Room, 40_000);
        let hall = Reverb::impulse_report(ReverbMode::Hall, 40_000);
        let echo = Reverb::impulse_report(ReverbMode::Echo, 40_000);
        let delay = Reverb::impulse_report(ReverbMode::Delay, 40_000);

        assert!(room.peak > 0);
        assert!(hall.peak > 0);
        assert!(echo.peak > 0);
        assert!(delay.peak > 0);
        assert_ne!(room, hall);
        assert_ne!(hall, echo);
        assert_ne!(hall, delay);
        assert_ne!(echo, delay);
    }

    #[test]
    fn preset_offsets_are_16bit_word_indices() {
        let r = Reverb::new(ReverbMode::Hall);
        assert_eq!(r.work.len(), 0xADE0 / 2);
        assert_eq!(
            Reverb::offset_samples(ReverbMode::Hall.params().regs.d_apf1),
            0x01A5
        );
    }

    #[test]
    fn mode_change_resets_buffers() {
        let mut r = Reverb::new(ReverbMode::Hall);
        for i in 0..5000 {
            let send = if i == 0 { 0x4000 } else { 0 };
            r.tick(send, send);
        }
        r.set_mode(ReverbMode::Room);
        assert!(r.work.iter().all(|&x| x == 0));
        assert!(r.input_l.iter().all(|&x| x == 0));
        assert!(r.output_l.iter().all(|&x| x == 0));
    }

    #[test]
    fn from_byte_matches_known_modes() {
        assert_eq!(ReverbMode::from_byte(0), ReverbMode::Off);
        assert_eq!(ReverbMode::from_byte(7), ReverbMode::Echo);
        assert_eq!(ReverbMode::from_byte(9), ReverbMode::Pipe);
        assert_eq!(ReverbMode::from_byte(0xFF), ReverbMode::Off);
    }
}
