//! Minimal safe wrapper over libopus decoding.

use std::ptr::{self, NonNull};

use anyhow::{Result, bail};
use opus_head_sys as ffi;

pub const SAMPLE_RATE: u32 = 48_000;
/// 20 ms at 48 kHz: the frame size the phone sends.
pub const FRAME_SAMPLES: usize = 960;
/// Longest Opus packet: 120 ms.
pub const MAX_FRAME_SAMPLES: usize = 5760;

pub struct Decoder(NonNull<ffi::OpusDecoder>);

// The decoder state is plain memory owned by this value.
unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new_mono() -> Result<Self> {
        let mut err = 0;
        let raw = unsafe { ffi::opus_decoder_create(SAMPLE_RATE as i32, 1, &mut err) };
        match NonNull::new(raw) {
            Some(p) if err == ffi::OPUS_OK as i32 => Ok(Self(p)),
            _ => bail!("opus_decoder_create failed: {err}"),
        }
    }

    /// Decodes one packet into `out`; `None` asks libopus to conceal one lost frame.
    pub fn decode(&mut self, packet: Option<&[u8]>, out: &mut [f32]) -> Result<usize> {
        let (data, len, frame) = match packet {
            Some(p) => (p.as_ptr(), p.len() as i32, out.len()),
            None => (ptr::null(), 0, FRAME_SAMPLES.min(out.len())),
        };
        let n = unsafe {
            ffi::opus_decode_float(
                self.0.as_ptr(),
                data,
                len,
                out.as_mut_ptr(),
                frame as i32,
                0,
            )
        };
        if n < 0 {
            bail!("opus_decode_float failed: {n}");
        }
        Ok(n as usize)
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe { ffi::opus_decoder_destroy(self.0.as_ptr()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_silence_frame_and_conceals_loss() {
        let mut dec = Decoder::new_mono().unwrap();
        let mut pcm = [0f32; MAX_FRAME_SAMPLES];
        assert_eq!(
            dec.decode(Some(&[0xf8, 0xff, 0xfe]), &mut pcm).unwrap(),
            FRAME_SAMPLES
        );
        assert!(pcm[..FRAME_SAMPLES].iter().all(|s| s.abs() < 1e-3));
        assert_eq!(dec.decode(None, &mut pcm).unwrap(), FRAME_SAMPLES);
    }
}
