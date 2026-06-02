//! Placeholder interface for a hardware-shaped PSX reverb processor.
//!
//! The current live [`super::reverb::Reverb`] module is an approximate
//! diagnostic effect. The authentic path should port or clean-room reimplement
//! the reusable DSP behavior from:
//!
//! https://github.com/ipatix/lv2-psx-reverb
//! https://github.com/ipatix/lv2-psx-reverb/blob/master/psx-reverb.c
//!
//! Keep the LV2 plugin wrapper out of this crate. This module is the boundary
//! the mixer can call once the PSX register topology, delay memory, and mode
//! coefficient setup are implemented.

/// Minimal processor contract for an SPU reverb core.
pub trait PsxReverbProcessor {
    /// Reset delay memory and any mode-dependent runtime state.
    fn reset(&mut self);

    /// Process one 44.1 kHz stereo frame from the SPU reverb input bus.
    fn process_frame(&mut self, input_l: i16, input_r: i16) -> (i16, i16);
}

/// Silent placeholder used until the hardware-shaped DSP is ported.
#[derive(Debug, Default, Clone, Copy)]
pub struct SilentPsxReverb;

impl PsxReverbProcessor for SilentPsxReverb {
    fn reset(&mut self) {}

    fn process_frame(&mut self, _input_l: i16, _input_r: i16) -> (i16, i16) {
        (0, 0)
    }
}
