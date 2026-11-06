//! Clipboard image normalization (original features/chat/image_clipboard.dart).

/// PNG, JPEG, GIF or WebP by signature.
pub fn sniff_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("image/jpeg");
    }
    if bytes.len() >= 6 && bytes.starts_with(b"GIF8") {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

/// Encodes straight (non-premultiplied) RGBA8 pixels as PNG.
pub fn encode_rgba_png(width: u32, height: u32, rgba: &[u8]) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(
        width > 0 && height > 0 && rgba.len() == width as usize * height as usize * 4,
        "Clipboard image has an invalid pixel buffer."
    );
    let mut out = Vec::with_capacity(rgba.len() / 2);
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(rgba)?;
        writer.finish()?;
    }
    Ok(out)
}

/// Decodes a PNG into straight RGBA8 (used to place images on desktop clipboards).
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn decode_png_rgba(bytes: &[u8]) -> anyhow::Result<(u32, u32, Vec<u8>)> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info()?;
    let mut buffer = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buffer)?;
    buffer.truncate(info.buffer_size());
    let rgba = match info.color_type {
        png::ColorType::Rgba => buffer,
        png::ColorType::Rgb => buffer.chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        png::ColorType::GrayscaleAlpha => buffer.chunks_exact(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        png::ColorType::Grayscale => buffer.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => anyhow::bail!("Indexed PNG was not expanded."),
    };
    Ok((info.width, info.height, rgba))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_round_trips_and_sniffs() {
        let pixels = [255u8, 0, 0, 255, 0, 255, 0, 128];
        let png = encode_rgba_png(2, 1, &pixels).unwrap();
        assert_eq!(sniff_image_mime(&png), Some("image/png"));
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        assert_eq!(decode_png_rgba(&png).unwrap(), (2, 1, pixels.to_vec()));
    }
}
