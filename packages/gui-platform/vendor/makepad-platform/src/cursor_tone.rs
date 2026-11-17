//! Paseo: pointer images toned to the app's light or dark theme.
//!
//! A system pointer set is drawn in one tone: Windows' stock arrow is white with a dark
//! outline, Yaru's is dark with a light one. Over a background of the same tone the pointer
//! all but disappears, so the desktop layers recolour the system's own images: the user's
//! shapes, sizes and coloured parts stay, and only the neutral black and white swap.

/// Alpha encoding of 0xAARRGGBB pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum PixelAlpha {
    /// Colour channels are independent of alpha (Windows cursor bitmaps).
    Straight,
    /// Colour channels are already multiplied by alpha (Xcursor images).
    Premultiplied,
}

/// Pixels at or above this alpha are part of the pointer's body; fainter ones are soft
/// shadows and outer anti-aliasing, which keep their tone.
const VISIBLE_ALPHA: u32 = 0x80;
/// Channel spread (max − min) up to which a pixel counts as grey rather than coloured.
const NEUTRAL_SPREAD: u32 = 48;

/// Inverts the pointer's neutral tones when its fill has the theme's own tone (a light
/// fill on a light theme, a dark fill on a dark one) and reports whether it did. A pointer
/// that already contrasts, or whose fill is mostly coloured, is the user's choice and is
/// left as it is.
pub fn tone_for_theme(pixels: &mut [u32], width: usize, height: usize, alpha: PixelAlpha, dark_theme: bool) -> bool {
    match fill_is_light(pixels, width, height, alpha) {
        Some(light) if light != dark_theme => {
            invert_neutral(pixels, alpha);
            true
        }
        _ => false,
    }
}

/// Whether the pointer's fill is light, or `None` when it is mostly coloured or empty.
///
/// The fill is the body pixels whose four neighbours are body pixels too; the outline is
/// the edge against transparency. A shape too thin to have an inside is judged by all of
/// its body pixels.
fn fill_is_light(pixels: &[u32], width: usize, height: usize, alpha: PixelAlpha) -> Option<bool> {
    debug_assert_eq!(pixels.len(), width * height);
    let body = |x: isize, y: isize| {
        x >= 0 && y >= 0 && (x as usize) < width && (y as usize) < height
            && pixels[y as usize * width + x as usize] >> 24 >= VISIBLE_ALPHA
    };
    let mut fill = Tally::default();
    let mut all = Tally::default();
    for y in 0..height as isize {
        for x in 0..width as isize {
            if !body(x, y) {
                continue;
            }
            let rgb = straight_rgb(pixels[y as usize * width + x as usize], alpha);
            all.add(rgb);
            if body(x - 1, y) && body(x + 1, y) && body(x, y - 1) && body(x, y + 1) {
                fill.add(rgb);
            }
        }
    }
    let tally = if fill.neutral + fill.coloured > 0 { fill } else { all };
    if tally.neutral == 0 || tally.coloured > tally.neutral {
        return None;
    }
    Some(tally.luma / tally.neutral as f32 > 0.5)
}

#[derive(Default, Clone, Copy)]
struct Tally {
    neutral: usize,
    coloured: usize,
    luma: f32,
}

impl Tally {
    fn add(&mut self, rgb: [u32; 3]) {
        if is_neutral(rgb) {
            self.neutral += 1;
            self.luma += (0.2126 * rgb[0] as f32 + 0.7152 * rgb[1] as f32 + 0.0722 * rgb[2] as f32) / 255.0;
        } else {
            self.coloured += 1;
        }
    }
}

/// Black becomes white and white black in every grey body pixel; coloured pixels (a busy
/// spinner, a "not allowed" sign) and faint pixels keep their colour.
pub(crate) fn invert_neutral(pixels: &mut [u32], alpha: PixelAlpha) {
    for pixel in pixels {
        let a = *pixel >> 24;
        if a < VISIBLE_ALPHA || !is_neutral(straight_rgb(*pixel, alpha)) {
            continue;
        }
        // A premultiplied channel lies in 0..=a, so its inverse is a − c.
        let full = match alpha {
            PixelAlpha::Straight => 0xff,
            PixelAlpha::Premultiplied => a,
        };
        let channel = |shift: u32| (full - ((*pixel >> shift) & 0xff).min(full)) << shift;
        *pixel = (a << 24) | channel(16) | channel(8) | channel(0);
    }
}

fn straight_rgb(pixel: u32, alpha: PixelAlpha) -> [u32; 3] {
    let a = pixel >> 24;
    let channel = |shift: u32| {
        let c = (pixel >> shift) & 0xff;
        match alpha {
            PixelAlpha::Straight => c,
            PixelAlpha::Premultiplied if a == 0 => 0,
            PixelAlpha::Premultiplied => (c * 255 / a).min(255),
        }
    };
    [channel(16), channel(8), channel(0)]
}

fn is_neutral(rgb: [u32; 3]) -> bool {
    rgb.iter().max().unwrap() - rgb.iter().min().unwrap() <= NEUTRAL_SPREAD
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: u32 = 0x0000_0000; // transparent
    const K: u32 = 0xff00_0000; // black
    const W: u32 = 0xffff_ffff; // white
    const R: u32 = 0xffe0_2020; // red

    #[test]
    fn white_pointer_turns_dark_on_the_light_theme() {
        let mut pointer = vec![
            K, K, K, K, K,
            K, W, W, W, K,
            K, W, W, W, K,
            K, W, W, W, K,
            K, K, K, K, K,
        ];
        assert!(tone_for_theme(&mut pointer, 5, 5, PixelAlpha::Straight, false));
        assert_eq!(pointer, vec![
            W, W, W, W, W,
            W, K, K, K, W,
            W, K, K, K, W,
            W, K, K, K, W,
            W, W, W, W, W,
        ]);
    }

    #[test]
    fn dark_pointer_stays_as_it_is_on_the_light_theme() {
        let mut pointer = vec![
            W, W, W, W, W,
            W, K, K, K, W,
            W, K, K, K, W,
            W, K, K, K, W,
            W, W, W, W, W,
        ];
        let system = pointer.clone();
        assert!(!tone_for_theme(&mut pointer, 5, 5, PixelAlpha::Straight, false));
        assert_eq!(pointer, system);
    }

    #[test]
    fn dark_pointer_turns_light_on_the_dark_theme() {
        let mut pointer = vec![
            W, W, W, W, W,
            W, K, K, K, W,
            W, K, K, K, W,
            W, K, K, K, W,
            W, W, W, W, W,
        ];
        assert!(tone_for_theme(&mut pointer, 5, 5, PixelAlpha::Straight, true));
        assert_eq!(pointer[12], W);
        assert_eq!(pointer[0], K);
    }

    #[test]
    fn thin_dark_beam_with_light_outline_counts_as_dark() {
        // One pixel wide: its outline outnumbers it, but the beam is the inside.
        let mut beam = vec![
            T, W, W, W, T,
            T, W, K, W, T,
            T, W, K, W, T,
            T, W, K, W, T,
            T, W, W, W, T,
        ];
        assert!(!tone_for_theme(&mut beam, 5, 5, PixelAlpha::Straight, false));
    }

    #[test]
    fn coloured_parts_and_shadows_keep_their_colour() {
        let shadow = 0x4000_0000;
        let mut pointer = vec![
            K, K, K, K, shadow,
            K, W, W, W, K,
            K, W, R, W, K,
            K, W, W, W, K,
            K, K, K, K, K,
        ];
        assert!(tone_for_theme(&mut pointer, 5, 5, PixelAlpha::Straight, false));
        assert_eq!(pointer[12], R);
        assert_eq!(pointer[4], shadow);
        assert_eq!(pointer[6], K);
    }

    #[test]
    fn mostly_coloured_pointer_is_left_alone() {
        let mut pointer = vec![
            K, K, K, K, K,
            K, R, R, R, K,
            K, R, W, R, K,
            K, R, R, R, K,
            K, K, K, K, K,
        ];
        assert!(!tone_for_theme(&mut pointer, 5, 5, PixelAlpha::Straight, true));
    }

    #[test]
    fn premultiplied_grey_inverts_within_its_alpha() {
        // Half-opaque white is 0x80 in every premultiplied channel; inverted it is black.
        let mut pixels = vec![0x80_80_80_80];
        invert_neutral(&mut pixels, PixelAlpha::Premultiplied);
        assert_eq!(pixels, vec![0x80_00_00_00]);
    }
}
