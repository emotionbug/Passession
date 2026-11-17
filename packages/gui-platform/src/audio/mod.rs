//! Output-only PCM16 effect playback (original features/audio/audio_service.dart
//! and packages/flutter_audio). Microphone capture, voice mode, TTS input and
//! encoded (MP3/WAV/OGG/FLAC) sources stay excluded.

pub(crate) mod engine;

/// Shared PCM cue from the original native voice runtime (16 kHz, mono, PCM16 LE).
pub const THINKING_TONE_PCM16_16K: &[u8] =
    include_bytes!("../../assets/thinking_tone_16k_mono.pcm");
pub const THINKING_TONE_MIME: &str = "audio/pcm;rate=16000;bits=16";

/// The engine's output rate. Makepad 1.0.0 opens every output device at 48 kHz.
pub(crate) const OUTPUT_RATE: u32 = 48_000;

/// Blocking settings diagnostics test: stops current output, then plays the
/// thinking tone to completion. Call from a worker thread.
pub fn play_thinking_tone() -> Result<(), String> {
    crate::facade::play_thinking_tone()
}

/// Original `audio/pcm` `parsePcmSampleRate`.
pub fn parse_pcm_sample_rate(mime_type: &str) -> Option<u32> {
    mime_type.split(';').skip(1).find_map(|part| {
        let (key, value) = part.split_once('=')?;
        if key.trim().eq_ignore_ascii_case("rate") {
            value.trim().parse::<u32>().ok().filter(|rate| *rate > 0)
        } else {
            None
        }
    })
}

/// Original `audio/pcm` `resamplePcm16`: linear interpolation of
/// little-endian PCM16 between sample rates.
pub fn resample_pcm16(pcm: &[u8], from_rate: u32, to_rate: u32) -> Vec<u8> {
    let samples: Vec<i16> = pcm
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect();
    resample_i16(&samples, from_rate, to_rate)
        .into_iter()
        .flat_map(i16::to_le_bytes)
        .collect()
}

fn resample_i16(samples: &[i16], from_rate: u32, to_rate: u32) -> Vec<i16> {
    if from_rate == to_rate || samples.is_empty() {
        return samples.to_vec();
    }
    let ratio = from_rate as f64 / to_rate as f64;
    let length = ((samples.len() as f64) / ratio).floor().max(1.0) as usize;
    (0..length)
        .map(|i| {
            let position = i as f64 * ratio;
            let index = position.floor() as usize;
            let fraction = position - index as f64;
            let a = samples[index.min(samples.len() - 1)] as f64;
            let b = samples[(index + 1).min(samples.len() - 1)] as f64;
            (a + (b - a) * fraction)
                .round()
                .clamp(i16::MIN as f64, i16::MAX as f64) as i16
        })
        .collect()
}

/// Linear resampling of interleaved f32 frames.
pub(crate) fn resample_interleaved(
    samples: &[f32],
    channels: usize,
    from_rate: u32,
    to_rate: u32,
) -> Vec<f32> {
    if from_rate == to_rate || samples.is_empty() || channels == 0 {
        return samples.to_vec();
    }
    let frames = samples.len() / channels;
    let ratio = from_rate as f64 / to_rate as f64;
    let length = ((frames as f64) / ratio).floor().max(1.0) as usize;
    let mut out = Vec::with_capacity(length * channels);
    for i in 0..length {
        let position = i as f64 * ratio;
        let index = position.floor() as usize;
        let fraction = (position - index as f64) as f32;
        let a = index.min(frames - 1);
        let b = (index + 1).min(frames - 1);
        for c in 0..channels {
            let x = samples[a * channels + c];
            let y = samples[b * channels + c];
            out.push(x + (y - x) * fraction);
        }
    }
    out
}

/// Decoded audio ready for the output callback: interleaved f32 at [`OUTPUT_RATE`].
pub(crate) struct DecodedAudio {
    pub samples: Vec<f32>,
    pub channels: usize,
}

/// PCM16 LE mono (`audio/pcm;rate=N`, 24 kHz when no rate is given). Encoded
/// containers are rejected: no caller plays them.
pub(crate) fn decode_source(bytes: &[u8], mime_type: &str) -> anyhow::Result<DecodedAudio> {
    let lower = mime_type.to_ascii_lowercase();
    let essence = lower.split(';').next().unwrap_or(&lower).trim();
    anyhow::ensure!(
        essence == "audio/pcm",
        "Unsupported audio format {essence}; only audio/pcm plays."
    );
    let rate = parse_pcm_sample_rate(&lower).unwrap_or(24_000);
    let mono: Vec<f32> = bytes
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
        .collect();
    anyhow::ensure!(!mono.is_empty(), "The audio source is empty.");
    Ok(DecodedAudio {
        samples: resample_interleaved(&mono, 1, rate, OUTPUT_RATE),
        channels: 1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm_rate_rule_matches_original() {
        assert_eq!(
            parse_pcm_sample_rate("audio/pcm;rate=16000;bits=16"),
            Some(16000)
        );
        assert_eq!(parse_pcm_sample_rate("audio/pcm"), None);
    }

    #[test]
    fn resampling_keeps_endpoints_and_length() {
        let pcm: Vec<u8> = [0i16, 1000, 2000, 3000]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let out = resample_pcm16(&pcm, 24_000, 16_000);
        assert_eq!(out.len() / 2, 2);
        assert_eq!(i16::from_le_bytes([out[0], out[1]]), 0);
    }

    #[test]
    fn only_pcm_sources_decode() {
        let tone = decode_source(THINKING_TONE_PCM16_16K, THINKING_TONE_MIME).unwrap();
        assert_eq!(tone.channels, 1);
        assert!(!tone.samples.is_empty());
        assert!(decode_source(&[0, 0, 1, 0], "audio/mpeg").is_err());
        assert!(decode_source(&[0, 0, 1, 0], "audio/pcmx").is_err());
    }
}
