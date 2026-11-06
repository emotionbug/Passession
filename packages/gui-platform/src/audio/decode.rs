use super::{resample_interleaved, DecodedAudio, OUTPUT_RATE};
use symphonia::core::{
    audio::SampleBuffer,
    codecs::{DecoderOptions, CODEC_TYPE_NULL},
    errors::Error,
    formats::FormatOptions,
    io::MediaSourceStream,
    meta::MetadataOptions,
    probe::Hint,
};

/// Decodes a complete encoded source (MP3, WAV, OGG/Vorbis, FLAC) to interleaved
/// f32 at the output rate.
pub(super) fn decode_encoded(bytes: &[u8], mime_type: &str) -> anyhow::Result<DecodedAudio> {
    let source = MediaSourceStream::new(Box::new(std::io::Cursor::new(bytes.to_vec())), Default::default());
    let mut hint = Hint::new();
    let essence = mime_type.split(';').next().unwrap_or(mime_type).trim();
    hint.mime_type(essence);
    if let Some(extension) = match essence {
        "audio/mpeg" | "audio/mp3" => Some("mp3"),
        "audio/wav" | "audio/x-wav" | "audio/wave" => Some("wav"),
        "audio/ogg" | "audio/vorbis" | "audio/opus" => Some("ogg"),
        "audio/flac" | "audio/x-flac" => Some("flac"),
        _ => None,
    } {
        hint.with_extension(extension);
    }
    let probed = symphonia::default::get_probe()
        .format(&hint, source, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|error| anyhow::anyhow!("Unsupported audio format {essence}: {error}"))?;
    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|track| track.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| anyhow::anyhow!("The audio source has no playable track."))?;
    let track_id = track.id;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|error| anyhow::anyhow!("Unsupported audio codec in {essence}: {error}"))?;
    let mut samples: Vec<f32> = Vec::new();
    let mut channels = 0usize;
    let mut rate = 0u32;
    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(Error::IoError(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(Error::ResetRequired) => break,
            Err(error) => return Err(anyhow::anyhow!("Audio demux failed: {error}")),
        };
        if packet.track_id() != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(decoded) => {
                let spec = *decoded.spec();
                if channels == 0 {
                    channels = spec.channels.count();
                    rate = spec.rate;
                }
                let mut buffer = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
                buffer.copy_interleaved_ref(decoded);
                samples.extend_from_slice(buffer.samples());
            }
            // A corrupt frame is skipped, as players do; the rest still plays.
            Err(Error::DecodeError(_)) => continue,
            Err(error) => return Err(anyhow::anyhow!("Audio decode failed: {error}")),
        }
    }
    anyhow::ensure!(channels > 0 && !samples.is_empty(), "The audio source decoded to silence.");
    Ok(DecodedAudio { samples: resample_interleaved(&samples, channels, rate, OUTPUT_RATE), channels })
}
