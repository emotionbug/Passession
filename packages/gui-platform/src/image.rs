//! Image identification and clipboard pixel encoding.

/// Identify an image from its bytes without decoding or re-encoding it.
pub fn sniff_image_mime(bytes: &[u8]) -> Option<&'static str> {
    sniff_image_mime_with_hint(bytes, None)
}

/// A native MIME hint may refine a generic HEIF brand; specific byte signatures win.
pub fn sniff_image_mime_with_hint(bytes: &[u8], hint: Option<&str>) -> Option<&'static str> {
    let mime = image_header_mime(bytes, bytes.len(), hint)?;
    let framed = match mime {
        "image/png" => has_png_image_data(&bytes[33..]),
        "image/jpeg" => bytes[3..].windows(2).rev().any(|pair| pair == b"\xff\xd9"),
        "image/gif" => bytes.last() == Some(&0x3b),
        "image/heic" | "image/heic-sequence" | "image/heif" | "image/heif-sequence" | "image/avif" => {
            has_bmff_image_box(bytes)
        }
        _ => true,
    };
    framed.then_some(mime)
}

/// Probe a dropped file without reading its full contents. Image import checks the full bytes.
pub fn sniff_image_header(header: &[u8], file_len: usize) -> Option<&'static str> {
    image_header_mime(header, file_len, None)
}

fn image_header_mime(bytes: &[u8], len: usize, hint: Option<&str>) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        let ihdr = bytes.get(8..33)?;
        return (len >= 45
            && ihdr.starts_with(b"\0\0\0\x0dIHDR")
            && ihdr[8..12] != [0; 4]
            && ihdr[12..16] != [0; 4])
            .then_some("image/png");
    }
    if bytes.starts_with(b"\xff\xd8\xff") {
        let marker = bytes[3..].iter().position(|&b| b != 0xff)? + 3;
        let segment = u16::from_be_bytes(bytes.get(marker + 1..marker + 3)?.try_into().ok()?);
        return ((0xc0..=0xfe).contains(&bytes[marker])
            && !matches!(bytes[marker], 0xd0..=0xd9)
            && segment >= 2
            && marker + 1 + usize::from(segment) < len)
            .then_some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        let dimensions = bytes.get(6..10)?;
        return (len > 13 && dimensions[..2] != [0; 2] && dimensions[2..] != [0; 2])
            .then_some("image/gif");
    }
    if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        let size = u32::from_le_bytes(bytes[4..8].try_into().ok()?) as usize;
        let chunk = u32::from_le_bytes(bytes.get(16..20)?.try_into().ok()?) as usize;
        return (matches!(bytes.get(12..16)?, b"VP8 " | b"VP8L" | b"VP8X")
            && size <= len.saturating_sub(8)
            && chunk > 0
            && chunk <= size.saturating_sub(12))
            .then_some("image/webp");
    }
    if bytes.starts_with(b"BM") {
        let offset = u32::from_le_bytes(bytes.get(10..14)?.try_into().ok()?) as usize;
        let dib_size = u32::from_le_bytes(bytes.get(14..18)?.try_into().ok()?) as usize;
        return (matches!(dib_size, 12 | 16 | 40 | 52 | 56 | 64 | 108 | 124)
            && offset >= 14 + dib_size
            && offset < len)
            .then_some("image/bmp");
    }
    let little_endian = bytes.starts_with(b"II");
    let offset = match bytes.get(..4)? {
        b"II*\0" | b"MM\0*" => {
            let offset = bytes.get(4..8)?.try_into().ok()?;
            let offset = if little_endian {
                u32::from_le_bytes(offset)
            } else {
                u32::from_be_bytes(offset)
            } as usize;
            return (offset >= 8 && offset <= len.saturating_sub(2)).then_some("image/tiff");
        }
        b"II+\0" | b"MM\0+" => {
            let offset_len = if little_endian { b"\x08\0" } else { b"\0\x08" };
            if bytes.get(4..6)? != offset_len || bytes.get(6..8)? != b"\0\0" {
                return None;
            }
            let offset = bytes.get(8..16)?.try_into().ok()?;
            if little_endian {
                u64::from_le_bytes(offset)
            } else {
                u64::from_be_bytes(offset)
            }
        }
        _ => return bmff_image_mime(bytes, len, hint),
    };
    (offset >= 16 && offset <= len.saturating_sub(8) as u64).then_some("image/tiff")
}

fn bmff_box(bytes: &[u8], remaining: usize) -> Option<(&[u8], usize, usize)> {
    let size = u32::from_be_bytes(bytes.get(..4)?.try_into().ok()?);
    let kind = bytes.get(4..8)?;
    let (header, size) = match size {
        0 => (8, remaining),
        1 => {
            let long = u64::from_be_bytes(bytes.get(8..16)?.try_into().ok()?);
            (16, usize::try_from(long).ok()?)
        }
        _ => (8, size as usize),
    };
    (size >= header && size <= remaining).then_some((kind, header, size))
}

fn bmff_image_mime(bytes: &[u8], len: usize, hint: Option<&str>) -> Option<&'static str> {
    let (kind, header, size) = bmff_box(bytes, len)?;
    if kind != b"ftyp" || size < header + 8 || !(size - header).is_multiple_of(4) || size == len {
        return None;
    }
    let brands = bytes.get(header..size.min(bytes.len()))?;
    let major = brands.get(..4)?;
    // Skip the minor version between the major and compatible brands.
    let compatible = brands.get(8..)?.chunks_exact(4);
    let specific = |brand: &[u8]| match brand {
        b"avif" | b"avis" => Some("image/avif"),
        b"heic" | b"heix" | b"heim" | b"heis" => Some("image/heic"),
        b"hevc" | b"hevx" | b"hevm" | b"hevs" => Some("image/heic-sequence"),
        _ => None,
    };
    if let Some(mime) = specific(major).or_else(|| compatible.clone().find_map(specific)) {
        return Some(mime);
    }
    let generic = std::iter::once(major)
        .chain(compatible)
        .find(|brand| matches!(*brand, b"mif1" | b"msf1"))?;
    let hint = hint.and_then(|hint| hint.split(';').next()).map(str::trim);
    if generic == b"msf1" {
        Some(if hint.is_some_and(|mime| mime.eq_ignore_ascii_case("image/heic-sequence")) {
            "image/heic-sequence"
        } else {
            "image/heif-sequence"
        })
    } else {
        Some(if hint.is_some_and(|mime| mime.eq_ignore_ascii_case("image/heic")) {
            "image/heic"
        } else {
            "image/heif"
        })
    }
}

fn has_bmff_image_box(mut bytes: &[u8]) -> bool {
    let mut has_image = false;
    while !bytes.is_empty() {
        let Some((kind, header, size)) = bmff_box(bytes, bytes.len()) else {
            return false;
        };
        has_image |= (kind == b"meta" && size > header + 4) || (kind == b"moov" && size > header);
        bytes = &bytes[size..];
    }
    has_image
}

fn has_png_image_data(mut chunks: &[u8]) -> bool {
    let mut has_data = false;
    while let Some(header) = chunks.get(..8) {
        let size = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
        let Some(next) = size.checked_add(12).and_then(|end| chunks.get(end..)) else {
            return false;
        };
        match &header[4..] {
            b"IDAT" => has_data |= size > 0,
            b"IEND" => return has_data && size == 0 && next.is_empty(),
            _ => {}
        }
        chunks = next;
    }
    false
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
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
pub fn decode_png_rgba(bytes: &[u8]) -> anyhow::Result<(u32, u32, Vec<u8>)> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info()?;
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| anyhow::anyhow!("PNG dimensions exceed addressable memory."))?;
    let mut buffer = vec![0; size];
    let info = reader.next_frame(&mut buffer)?;
    buffer.truncate(info.buffer_size());
    let rgba = match info.color_type {
        png::ColorType::Rgba => buffer,
        png::ColorType::Rgb => buffer
            .chunks_exact(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => buffer
            .chunks_exact(2)
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
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
        #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
        assert_eq!(decode_png_rgba(&png).unwrap(), (2, 1, pixels.to_vec()));
    }

    fn bmff_fixture(major: &[u8; 4], compatible: &[[u8; 4]]) -> Vec<u8> {
        let mut bytes = (16 + 4 * compatible.len() as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(b"ftyp");
        bytes.extend_from_slice(major);
        bytes.extend_from_slice(&[0; 4]);
        for brand in compatible {
            bytes.extend_from_slice(brand);
        }
        // Container framing only; MIME recognition does not decode the pixel payload.
        bytes.extend_from_slice(b"\0\0\0\x14meta\0\0\0\0\0\0\0\x08free");
        bytes
    }

    #[test]
    fn image_mime_identifies_bmff_major_and_compatible_brands() {
        for (major, compatible, expected) in [
            (*b"heic", vec![], "image/heic"),
            (*b"mif1", vec![*b"heix"], "image/heic"),
            (*b"mif1", vec![*b"mihe"], "image/heif"),
            (*b"msf1", vec![*b"hevc"], "image/heic-sequence"),
            (*b"msf1", vec![], "image/heif-sequence"),
            (*b"mif1", vec![*b"avif"], "image/avif"),
            (*b"avis", vec![], "image/avif"),
        ] {
            let bytes = bmff_fixture(&major, &compatible);
            assert_eq!(sniff_image_mime_with_hint(&bytes, Some("image/png")), Some(expected));
            assert_eq!(sniff_image_header(&bytes[..16 + 4 * compatible.len()], bytes.len()), Some(expected));
        }
    }

    #[test]
    fn image_mime_uses_only_corroborated_native_hints() {
        let heif = bmff_fixture(b"mif1", &[]);
        assert_eq!(
            sniff_image_mime_with_hint(&heif, Some("IMAGE/HEIC; x-native=yes")),
            Some("image/heic")
        );
        assert_eq!(
            sniff_image_mime_with_hint(&heif, Some("text/plain")),
            Some("image/heif")
        );
        assert_eq!(
            sniff_image_mime_with_hint(&heif, Some("image/heic-sequence")),
            Some("image/heif")
        );
    }

    #[test]
    fn image_mime_rejects_unknown_and_truncated_payloads() {
        let heic = bmff_fixture(b"heic", &[]);
        let movie = bmff_fixture(b"mp42", &[*b"isom"]);
        let png = encode_rgba_png(1, 1, &[255, 0, 0, 255]).unwrap();
        for bytes in [
            &heic[..16],
            &heic[..heic.len() - 1],
            movie.as_slice(),
            &png[..png.len() - 1],
            &b"\xff\xd8\xff"[..],
            &b"GIF89a"[..],
            &b"unknown"[..],
        ] {
            assert_eq!(sniff_image_mime_with_hint(bytes, Some("image/heic")), None);
            assert_eq!(sniff_image_mime_with_hint(bytes, Some("application/pdf")), None);
        }
    }
}
