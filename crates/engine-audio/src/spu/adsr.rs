//! PSX SPU ADSR envelope state machine.
//!
//! The PSX SPU drives each voice's volume through an Attack-Decay-Sustain-
//! Release envelope. The envelope counter is unsigned 16-bit (0..=0x7FFF
//! peak); each phase advances the counter by an amount derived from a
//! 7-bit "step + shift" rate plus a linear-vs-exponential mode bit.
//!
//! Layout of the two ADSR words (PSX libspu / nocash psx-spx):
//!
//! ```text
//!   ADSR1 (low 16 bits):
//!     bits 15:    attack mode      (0 = linear, 1 = exponential)
//!     bits 14..10: attack shift    (5 bits; larger -> slower)
//!     bits  9..8: attack step      (2 bits; +7 .. +4 added per tick: 7-step)
//!     bits  7..4: decay shift      (4 bits; decay always exponential, step=-8)
//!     bits  3..0: sustain level    (4 bits; (SL+1) << 11 = target counter)
//!
//!   ADSR2 (high 16 bits):
//!     bit 15: sustain mode         (0 = linear, 1 = exponential)
//!     bit 14: sustain direction    (0 = increase, 1 = decrease)
//!     bit 13: reserved
//!     bits 12..8: sustain shift    (5 bits)
//!     bits  7..6: sustain step     (2 bits; +7-step or -8+step depending on dir)
//!     bit  5: release mode         (0 = linear, 1 = exponential)
//!     bits  4..0: release shift    (5 bits)
//! ```
//!
//! Per-tick advance, given (mode, shift, step_bits, dir_sign):
//!
//! ```text
//!   step = (7 - step_bits) for increase, (-8 + step_bits) for decrease
//!   if shift < 11:  delta = step << (11 - shift)
//!   else:           delta = step >> (shift - 11)        (rounds toward 0)
//!   if exponential and increase and counter > 0x6000:
//!     delta >>= 2
//!   if exponential and decrease:
//!     delta = (delta * counter) >> 15
//!   counter = clamp(counter + dir_sign * delta, 0, 0x7FFF)
//! ```
//!
//! The increase/decrease distinction is carried by `step_bits` interpretation
//! plus the explicit direction sign (sustain can go either way).
//!
//! Source: this is the standard textbook PSX ADSR formula from the libspu
//! reference and nocash psx-spx; no Sony bytes here. The `crates/vab` parser
//! reads `adsr1`/`adsr2` directly off the VAB tone metadata (which is
//! game-data, not Sony-binary).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Attack,
    Decay,
    Sustain,
    Release,
    Off,
}

#[derive(Debug, Clone, Copy)]
pub struct AdsrConfig {
    pub attack_exp: bool,
    pub attack_shift: u8,
    pub attack_step: u8,
    pub decay_shift: u8,
    pub sustain_level: u16,
    pub sustain_exp: bool,
    pub sustain_decrease: bool,
    pub sustain_shift: u8,
    pub sustain_step: u8,
    pub release_exp: bool,
    pub release_shift: u8,
}

impl AdsrConfig {
    /// Decode from `(adsr1, adsr2)` words as stored in VAB tone metadata.
    pub fn from_words(adsr1: u16, adsr2: u16) -> Self {
        Self {
            attack_exp: (adsr1 >> 15) & 1 != 0,
            attack_shift: ((adsr1 >> 10) & 0x1F) as u8,
            attack_step: ((adsr1 >> 8) & 0x03) as u8,
            decay_shift: ((adsr1 >> 4) & 0x0F) as u8,
            sustain_level: ((adsr1 & 0x0F) + 1) << 11, // 0x0800 .. 0x8000
            sustain_exp: (adsr2 >> 15) & 1 != 0,
            sustain_decrease: (adsr2 >> 14) & 1 != 0,
            sustain_shift: ((adsr2 >> 8) & 0x1F) as u8,
            sustain_step: ((adsr2 >> 6) & 0x03) as u8,
            release_exp: (adsr2 >> 5) & 1 != 0,
            release_shift: (adsr2 & 0x1F) as u8,
        }
    }
}

impl Default for AdsrConfig {
    fn default() -> Self {
        // Hardware reset: linear-attack-fast, no decay, sustain=peak,
        // linear-release-fast. Matches what an "unconfigured" voice would
        // produce: instant attack, no envelope shaping.
        Self {
            attack_exp: false,
            attack_shift: 0,
            attack_step: 0,
            decay_shift: 0,
            sustain_level: 0x8000,
            sustain_exp: false,
            sustain_decrease: true,
            sustain_shift: 0,
            sustain_step: 0,
            release_exp: false,
            release_shift: 0,
        }
    }
}

/// Per-voice ADSR runtime state.
#[derive(Debug, Clone, Copy)]
pub struct AdsrState {
    pub phase: Phase,
    /// Envelope level, 0..=0x7FFF.
    pub level: u16,
    /// Remaining 44.1 kHz ticks before the next ADSR level step. PSXSPX
    /// documents this as `AdsrCycles = 1 << max(0, shift - 11)`: high shift
    /// values slow the envelope by spacing out fixed-size steps rather than
    /// only shrinking the step magnitude.
    step_wait: u32,
}

impl Default for AdsrState {
    fn default() -> Self {
        Self {
            phase: Phase::Off,
            level: 0,
            step_wait: 0,
        }
    }
}

impl AdsrState {
    pub fn key_on(&mut self) {
        self.phase = Phase::Attack;
        self.level = 0;
        self.step_wait = 0;
    }

    pub fn key_off(&mut self) {
        // libspu KeyOff transitions any phase to Release.
        if self.phase != Phase::Off {
            self.phase = Phase::Release;
            self.step_wait = 0;
        }
    }

    /// Advance the envelope by one sample tick. Returns the new level.
    pub fn tick(&mut self, cfg: &AdsrConfig) -> u16 {
        if self.step_wait > 0 {
            self.step_wait -= 1;
            return self.level;
        }

        match self.phase {
            Phase::Off => {
                self.level = 0;
                self.step_wait = 0;
                return 0;
            }
            Phase::Attack => {
                let (delta, cycles) = compute_step(
                    cfg.attack_exp,
                    true,
                    cfg.attack_shift,
                    cfg.attack_step,
                    self.level,
                );
                self.step_wait = cycles.saturating_sub(1);
                self.level = self.level.saturating_add(delta as u16).min(0x7FFF);
                if self.level >= 0x7FFF {
                    self.level = 0x7FFF;
                    self.phase = Phase::Decay;
                    self.step_wait = 0;
                }
            }
            Phase::Decay => {
                // Decay is always exponential decrease with step=-8 (i.e. step_bits=0).
                let (delta, cycles) = compute_step(true, false, cfg.decay_shift, 0, self.level);
                self.step_wait = cycles.saturating_sub(1);
                self.apply_signed_delta(delta);
                if self.level <= cfg.sustain_level {
                    self.level = cfg.sustain_level;
                    self.phase = Phase::Sustain;
                    self.step_wait = 0;
                }
            }
            Phase::Sustain => {
                if cfg.sustain_decrease {
                    let (delta, cycles) = compute_step(
                        cfg.sustain_exp,
                        false,
                        cfg.sustain_shift,
                        cfg.sustain_step,
                        self.level,
                    );
                    self.step_wait = cycles.saturating_sub(1);
                    self.apply_signed_delta(delta);
                    if self.level == 0 {
                        self.phase = Phase::Off;
                        self.step_wait = 0;
                    }
                } else {
                    let (delta, cycles) = compute_step(
                        cfg.sustain_exp,
                        true,
                        cfg.sustain_shift,
                        cfg.sustain_step,
                        self.level,
                    );
                    self.step_wait = cycles.saturating_sub(1);
                    self.level = self.level.saturating_add(delta as u16).min(0x7FFF);
                }
            }
            Phase::Release => {
                let (delta, cycles) =
                    compute_step(cfg.release_exp, false, cfg.release_shift, 0, self.level);
                self.step_wait = cycles.saturating_sub(1);
                self.apply_signed_delta(delta);
                if self.level == 0 {
                    self.phase = Phase::Off;
                    self.step_wait = 0;
                }
            }
        }
        self.level
    }

    fn apply_signed_delta(&mut self, delta: i32) {
        let next = self.level as i32 + delta;
        self.level = next.clamp(0, 0x7FFF) as u16;
    }
}

/// Compute one PSXSPX ADSR level step and the number of 44.1 kHz ticks to
/// wait before the following step.
fn compute_step(exp: bool, increase: bool, shift: u8, step_bits: u8, level: u16) -> (i32, u32) {
    let mut cycles = 1u32 << shift.saturating_sub(11);
    let step_value = if increase {
        7 - step_bits as i32
    } else {
        -8 + step_bits as i32
    };
    let mut step = if shift < 11 {
        step_value << (11 - shift) as u32
    } else {
        step_value
    };
    if exp && increase && level > 0x6000 {
        cycles = cycles.saturating_mul(4);
    }
    if exp && !increase {
        step = (step * level as i32) >> 15;
        if step == 0 && level > 0 {
            step = -1;
        }
    }
    (step, cycles.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Default ADSR (all-zero shifts) ramps to peak in three ticks: each
    /// tick adds delta = 7 << 11 = 0x3800, so 0 -> 0x3800 -> 0x7000 ->
    /// (0x7000 + 0x3800).min(0x7FFF) = 0x7FFF -> Decay.
    #[test]
    fn default_adsr_attacks_to_peak_in_three_ticks() {
        let cfg = AdsrConfig::default();
        let mut s = AdsrState::default();
        s.key_on();
        assert_eq!(s.phase, Phase::Attack);
        assert_eq!(s.tick(&cfg), 0x3800);
        assert_eq!(s.phase, Phase::Attack);
        assert_eq!(s.tick(&cfg), 0x7000);
        assert_eq!(s.phase, Phase::Attack);
        let lvl = s.tick(&cfg);
        assert_eq!(lvl, 0x7FFF);
        assert_eq!(s.phase, Phase::Decay);
    }

    /// A configured ADSR with slow attack actually ramps slowly.
    #[test]
    fn slow_attack_takes_many_ticks() {
        let cfg = AdsrConfig {
            attack_shift: 10, // delta = 7 << 1 = 14 per tick
            ..AdsrConfig::default()
        };
        let mut s = AdsrState::default();
        s.key_on();
        for _ in 0..100 {
            s.tick(&cfg);
        }
        assert!(s.level > 0);
        assert!(s.level < 0x7FFF);
        assert_eq!(s.phase, Phase::Attack);
    }

    /// PSXSPX: shifts above 11 don't shrink the step further; they insert
    /// wait cycles between steps (`1 << (shift - 11)` samples).
    #[test]
    fn high_shift_inserts_wait_cycles() {
        let cfg = AdsrConfig {
            attack_shift: 13, // cycles = 4, step = +7
            ..AdsrConfig::default()
        };
        let mut s = AdsrState::default();
        s.key_on();
        assert_eq!(s.tick(&cfg), 7);
        assert_eq!(s.tick(&cfg), 7);
        assert_eq!(s.tick(&cfg), 7);
        assert_eq!(s.tick(&cfg), 7);
        assert_eq!(s.tick(&cfg), 14);
    }

    /// Exponential attack above 0x6000 waits four times as long before the
    /// next step, matching PSXSPX's "fake exponential" behavior.
    #[test]
    fn exponential_attack_above_threshold_extends_wait() {
        let cfg = AdsrConfig {
            attack_exp: true,
            attack_shift: 11,
            attack_step: 0,
            ..AdsrConfig::default()
        };
        let mut s = AdsrState {
            phase: Phase::Attack,
            level: 0x6001,
            ..AdsrState::default()
        };
        assert_eq!(s.tick(&cfg), 0x6008);
        assert_eq!(s.tick(&cfg), 0x6008);
        assert_eq!(s.tick(&cfg), 0x6008);
        assert_eq!(s.tick(&cfg), 0x6008);
        assert_eq!(s.tick(&cfg), 0x600F);
    }

    /// Decay drops to sustain level then stops.
    #[test]
    fn decay_stops_at_sustain_level() {
        // SL field = 0xF -> sustain_level = (0xF+1) << 11 = 0x8000... but
        // peak is 0x7FFF. Use SL=7 -> level = 0x4000.
        let cfg = AdsrConfig {
            attack_shift: 0,
            attack_step: 0,
            sustain_level: 0x4000,
            decay_shift: 4, // moderate decay
            ..AdsrConfig::default()
        };
        let mut s = AdsrState::default();
        s.key_on();
        for _ in 0..3 {
            s.tick(&cfg);
        }
        assert_eq!(s.phase, Phase::Decay);
        for _ in 0..2000 {
            s.tick(&cfg);
            if s.phase == Phase::Sustain {
                break;
            }
        }
        assert_eq!(s.phase, Phase::Sustain);
        assert_eq!(s.level, 0x4000);
    }

    /// KeyOff during sustain transitions to release and eventually goes off.
    #[test]
    fn release_takes_voice_to_off() {
        let cfg = AdsrConfig {
            sustain_level: 0x4000,
            decay_shift: 4,
            release_shift: 8,
            ..AdsrConfig::default()
        };
        let mut s = AdsrState::default();
        s.key_on();
        for _ in 0..2200 {
            s.tick(&cfg);
            if s.phase == Phase::Sustain {
                break;
            }
        }
        s.key_off();
        assert_eq!(s.phase, Phase::Release);
        for _ in 0..50_000 {
            s.tick(&cfg);
            if s.phase == Phase::Off {
                break;
            }
        }
        assert_eq!(s.phase, Phase::Off);
        assert_eq!(s.level, 0);
    }

    /// AdsrConfig::from_words round-trips the bit layout we care about.
    #[test]
    fn adsr_config_decode_layout() {
        // adsr1 with attack_shift=5, attack_step=2, decay_shift=3, sl=4
        let adsr1 = (5u16 << 10) | (2 << 8) | (3 << 4) | 4;
        // adsr2 with sustain_dec=1, sustain_shift=10, release_shift=12
        let adsr2 = (1u16 << 14) | (10 << 8) | 12;
        let cfg = AdsrConfig::from_words(adsr1, adsr2);
        assert_eq!(cfg.attack_shift, 5);
        assert_eq!(cfg.attack_step, 2);
        assert_eq!(cfg.decay_shift, 3);
        assert_eq!(cfg.sustain_level, (4 + 1) << 11);
        assert!(cfg.sustain_decrease);
        assert_eq!(cfg.sustain_shift, 10);
        assert_eq!(cfg.release_shift, 12);
    }
}
