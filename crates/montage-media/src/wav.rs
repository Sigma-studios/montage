//! The audio-only export: every track mixed down with its gain, as the
//! MP4 exports do, written as a 16-bit stereo 48 kHz WAV. No video, no
//! captions.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::atomic::AtomicBool;

use montage_core::edit::Cut;

use crate::export::{PartFile, RATE, check, mix_audio};
use crate::{MediaError, init};

const CHANNELS: u16 = 2;
const BITS: u16 = 16;
/// The RIFF header before the samples.
pub(crate) const HEADER_BYTES: u64 = 44;
/// Bytes per second of audio.
pub(crate) const BYTE_RATE: u64 = RATE as u64 * CHANNELS as u64 * (BITS as u64 / 8);

/// Exports the audio of `cut` of `input` as a WAV. Returns how, for the
/// status line.
pub(crate) fn export_wav(
    input: &Path,
    output: &Path,
    cut: Cut,
    gains: &[f32],
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(f32),
) -> Result<String, MediaError> {
    init();
    let info = crate::probe(input)?;
    let cut = cut.clamped(info.duration);
    if cut.duration() <= 0.0 {
        return Err(MediaError::EmptyRange);
    }
    progress(0.0);
    let mix = mix_audio(input, cut, gains, cancel, &mut |p| progress(p * 0.9))?
        .ok_or_else(|| MediaError::NoAudio(input.into()))?;
    check(cancel)?;

    let mut part = PartFile::new(output);
    let io = |source| MediaError::Io {
        action: "can't write",
        path: output.into(),
        source,
    };
    let file = File::create(&part.path).map_err(io)?;
    let mut out = BufWriter::new(file);
    write_wav(&mut out, &mix).map_err(io)?;
    out.into_inner()
        .map_err(|e| io(e.into_error()))?
        .sync_all()
        .map_err(io)?;
    part.commit(output)?;
    progress(1.0);
    Ok(format!("PCM {BITS}-bit {} kHz stereo", RATE / 1000))
}

/// Writes `mix` (two channels of samples in -1.0..=1.0) as a RIFF WAVE.
fn write_wav(out: &mut impl Write, mix: &[Vec<f32>; 2]) -> std::io::Result<()> {
    let frames = mix[0].len();
    let data_bytes = frames as u64 * u64::from(CHANNELS) * u64::from(BITS / 8);
    // RIFF sizes are 32 bits: about 6 h 12 min at 48 kHz stereo 16-bit.
    let data_bytes = u32::try_from(data_bytes).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "the cut is too long for a WAV file (over 4 GB)",
        )
    })?;
    let block_align = CHANNELS * (BITS / 8);

    out.write_all(b"RIFF")?;
    out.write_all(&(36 + data_bytes).to_le_bytes())?;
    out.write_all(b"WAVE")?;
    out.write_all(b"fmt ")?;
    out.write_all(&16u32.to_le_bytes())?;
    out.write_all(&1u16.to_le_bytes())?; // PCM
    out.write_all(&CHANNELS.to_le_bytes())?;
    out.write_all(&(RATE as u32).to_le_bytes())?;
    out.write_all(&(BYTE_RATE as u32).to_le_bytes())?;
    out.write_all(&block_align.to_le_bytes())?;
    out.write_all(&BITS.to_le_bytes())?;
    out.write_all(b"data")?;
    out.write_all(&data_bytes.to_le_bytes())?;

    let [left, right] = mix;
    for (&l, &r) in left.iter().zip(right) {
        out.write_all(&to_i16(l).to_le_bytes())?;
        out.write_all(&to_i16(r).to_le_bytes())?;
    }
    Ok(())
}

fn to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_a_canonical_header_and_interleaved_samples() {
        let mix = [vec![0.0, 1.0, -1.0], vec![0.5, -0.5, 2.0]];
        let mut bytes = Vec::new();
        write_wav(&mut bytes, &mix).unwrap();
        assert_eq!(bytes.len() as u64, HEADER_BYTES + 3 * 4);
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 36 + 12);
        assert_eq!(&bytes[8..16], b"WAVEfmt ");
        assert_eq!(u16::from_le_bytes([bytes[22], bytes[23]]), 2);
        assert_eq!(
            u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
            48_000
        );
        assert_eq!(
            u32::from_le_bytes(bytes[28..32].try_into().unwrap()),
            192_000
        );
        assert_eq!(u16::from_le_bytes([bytes[34], bytes[35]]), 16);
        assert_eq!(&bytes[36..40], b"data");
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 12);
        let samples: Vec<i16> = bytes[44..]
            .chunks(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(samples, [0, 16384, 32767, -16384, -32767, 32767]);
    }
}
