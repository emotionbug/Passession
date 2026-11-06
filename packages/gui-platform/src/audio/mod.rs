//! Output-only effect and daemon `audio_output` playback (original
//! features/audio/audio_service.dart + packages/flutter_audio). Microphone
//! capture, voice mode and TTS input stay excluded.

mod decode;
pub(crate) mod engine;

use base64::Engine as _;

/// Shared PCM cue from the original native voice runtime (16 kHz, mono, PCM16 LE).
pub const THINKING_TONE_PCM16_16K: &[u8] = include_bytes!("../../assets/thinking_tone_16k_mono.pcm");
pub const THINKING_TONE_MIME: &str = "audio/pcm;rate=16000;bits=16";

/// The engine's output rate. Makepad 1.0.0 opens every output device at 48 kHz.
pub(crate) const OUTPUT_RATE: u32 = 48_000;

/// Blocking settings diagnostics test: stops current output, then plays the
/// thinking tone to completion. Call from a worker thread.
pub fn play_thinking_tone() -> Result<(), String> {
    crate::facade::play_thinking_tone()
}

/// Original `buildAudioPlaybackSource` MIME rule: daemon formats are bare
/// names; `pcm` is 24 kHz PCM16.
pub fn audio_output_mime_type(format: &str) -> String {
    match format {
        "pcm" => "audio/pcm;rate=24000;bits=16".to_owned(),
        "mp3" => "audio/mpeg".to_owned(),
        other => format!("audio/{other}"),
    }
}

/// One daemon `audio_output` chunk.
#[derive(Debug, Clone)]
pub struct AudioOutputChunk {
    pub chunk_index: i64,
    /// Base64 audio bytes as sent by the daemon.
    pub audio: String,
    pub format: String,
}

/// Original `buildAudioPlaybackSource`: the group's chunks in index order,
/// concatenated into one source typed by the first chunk's format.
pub fn build_audio_playback_source(chunks: &[AudioOutputChunk]) -> anyhow::Result<(Vec<u8>, String)> {
    let mut ordered: Vec<&AudioOutputChunk> = chunks.iter().collect();
    ordered.sort_by_key(|chunk| chunk.chunk_index);
    let mut bytes = Vec::new();
    for chunk in &ordered {
        bytes.extend(base64::engine::general_purpose::STANDARD.decode(chunk.audio.as_bytes())?);
    }
    let mime = audio_output_mime_type(ordered.first().map(|c| c.format.as_str()).unwrap_or("pcm"));
    Ok((bytes, mime))
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
    let samples: Vec<i16> = pcm.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
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
            (a + (b - a) * fraction).round().clamp(i16::MIN as f64, i16::MAX as f64) as i16
        })
        .collect()
}

/// Linear resampling of interleaved f32 frames.
pub(crate) fn resample_interleaved(samples: &[f32], channels: usize, from_rate: u32, to_rate: u32) -> Vec<f32> {
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

/// PCM16 (`audio/pcm;rate=N`) or any container/codec symphonia supports.
pub(crate) fn decode_source(bytes: &[u8], mime_type: &str) -> anyhow::Result<DecodedAudio> {
    let lower = mime_type.to_ascii_lowercase();
    if lower.starts_with("audio/pcm") {
        let rate = parse_pcm_sample_rate(&lower).unwrap_or(24_000);
        let mono: Vec<f32> = bytes
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
            .collect();
        anyhow::ensure!(!mono.is_empty(), "The audio source is empty.");
        return Ok(DecodedAudio { samples: resample_interleaved(&mono, 1, rate, OUTPUT_RATE), channels: 1 });
    }
    decode::decode_encoded(bytes, &lower)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_and_rate_rules_match_original() {
        assert_eq!(audio_output_mime_type("pcm"), "audio/pcm;rate=24000;bits=16");
        assert_eq!(audio_output_mime_type("mp3"), "audio/mpeg");
        assert_eq!(audio_output_mime_type("wav"), "audio/wav");
        assert_eq!(parse_pcm_sample_rate("audio/pcm;rate=16000;bits=16"), Some(16000));
        assert_eq!(parse_pcm_sample_rate("audio/pcm"), None);
    }

    #[test]
    fn resampling_keeps_endpoints_and_length() {
        let pcm: Vec<u8> = [0i16, 1000, 2000, 3000].iter().flat_map(|s| s.to_le_bytes()).collect();
        let out = resample_pcm16(&pcm, 24_000, 16_000);
        assert_eq!(out.len() / 2, 2);
        assert_eq!(i16::from_le_bytes([out[0], out[1]]), 0);
    }

    #[test]
    fn chunks_play_in_index_order() {
        let chunks = [
            AudioOutputChunk { chunk_index: 1, audio: "Ag==".into(), format: "pcm".into() },
            AudioOutputChunk { chunk_index: 0, audio: "AQ==".into(), format: "pcm".into() },
        ];
        let (bytes, mime) = build_audio_playback_source(&chunks).unwrap();
        assert_eq!(bytes, vec![1, 2]);
        assert!(mime.starts_with("audio/pcm"));
    }
}
