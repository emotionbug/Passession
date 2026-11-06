//! QR Code Model 2 encoder (byte mode, error correction level M — the original
//! `errorCorrectionLevel: "M"`), following ISO/IEC 18004. Exact port of `_QrCode` in
//! `features/navigation/host_dialog.dart`.

const ECC_CODEWORDS_PER_BLOCK: [i32; 41] = [
    -1, 10, 16, 26, 18, 24, 16, 18, 22, 22, 26, 30, 22, 22, 24, 24, 28, 28, 26, 26, 26, //
    26, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28,
];
const NUM_ERROR_CORRECTION_BLOCKS: [i32; 41] = [
    -1, 1, 1, 1, 2, 2, 4, 4, 4, 5, 5, 5, 8, 9, 9, 10, 10, 11, 13, 14, 16, //
    17, 17, 18, 20, 21, 23, 25, 26, 28, 29, 31, 33, 35, 37, 38, 40, 43, 45, 47, 49,
];

fn raw_data_modules(version: i32) -> i32 {
    let mut result = (16 * version + 128) * version + 64;
    if version >= 2 {
        let align = version / 7 + 2;
        result -= (25 * align - 10) * align - 55;
        if version >= 7 {
            result -= 36;
        }
    }
    result
}

fn data_codewords(version: i32) -> i32 {
    raw_data_modules(version) / 8 - ECC_CODEWORDS_PER_BLOCK[version as usize] * NUM_ERROR_CORRECTION_BLOCKS[version as usize]
}

/// Modules (`[y][x]`, `true` = dark) of `text`, or an error when it does not fit version 40.
pub fn encode(text: &str) -> Result<Vec<Vec<bool>>, String> {
    let bytes = text.as_bytes();
    let count_bits_for = |version: i32| if version <= 9 { 8 } else { 16 };
    let mut version = 1;
    while 4 + count_bits_for(version) + bytes.len() as i32 * 8 > data_codewords(version) * 8 {
        version += 1;
        if version > 40 {
            return Err("Data too long for a QR code".into());
        }
    }
    let count_bits = count_bits_for(version);
    let mut bits: Vec<bool> = Vec::new();
    let append = |bits: &mut Vec<bool>, value: u32, length: i32| {
        for i in (0..length).rev() {
            bits.push((value >> i) & 1 != 0);
        }
    };
    append(&mut bits, 0x4, 4);
    append(&mut bits, bytes.len() as u32, count_bits);
    for byte in bytes {
        append(&mut bits, *byte as u32, 8);
    }
    let capacity = (data_codewords(version) * 8) as usize;
    let terminator = 4.min(capacity - bits.len()) as i32;
    append(&mut bits, 0, terminator);
    let pad_bits = ((8 - bits.len() % 8) % 8) as i32;
    append(&mut bits, 0, pad_bits);
    let mut pad = 0xECu32;
    while bits.len() < capacity {
        append(&mut bits, pad, 8);
        pad ^= 0xEC ^ 0x11;
    }
    let data: Vec<u8> = bits.chunks(8).map(|chunk| chunk.iter().fold(0u8, |a, b| (a << 1) | *b as u8)).collect();
    let codewords = add_ecc_and_interleave(&data, version);

    let size = (version * 4 + 17) as usize;
    let mut modules = vec![vec![false; size]; size];
    let mut function = vec![vec![false; size]; size];
    let set = |modules: &mut Vec<Vec<bool>>, function: &mut Vec<Vec<bool>>, x: usize, y: usize, dark: bool| {
        modules[y][x] = dark;
        function[y][x] = true;
    };

    // Timing patterns, then finders, alignment patterns, format and version areas.
    for i in 0..size {
        set(&mut modules, &mut function, 6, i, i % 2 == 0);
        set(&mut modules, &mut function, i, 6, i % 2 == 0);
    }
    for (cx, cy) in [(3i32, 3i32), (size as i32 - 4, 3), (3, size as i32 - 4)] {
        for dy in -4..=4i32 {
            for dx in -4..=4i32 {
                let distance = dx.abs().max(dy.abs());
                let (x, y) = (cx + dx, cy + dy);
                if x >= 0 && (x as usize) < size && y >= 0 && (y as usize) < size {
                    set(&mut modules, &mut function, x as usize, y as usize, distance != 2 && distance != 4);
                }
            }
        }
    }
    let positions = alignment_positions(version, size as i32);
    for i in 0..positions.len() {
        for j in 0..positions.len() {
            let last = positions.len() - 1;
            if (i, j) == (0, 0) || (i, j) == (0, last) || (i, j) == (last, 0) {
                continue;
            }
            for dy in -2..=2i32 {
                for dx in -2..=2i32 {
                    set(&mut modules, &mut function, (positions[i] + dx) as usize, (positions[j] + dy) as usize, dx.abs().max(dy.abs()) != 1);
                }
            }
        }
    }
    let draw_format = |modules: &mut Vec<Vec<bool>>, function: &mut Vec<Vec<bool>>, mask: u32| {
        // Level M format bits are 00.
        let value = mask;
        let mut remainder = value;
        for _ in 0..10 {
            remainder = (remainder << 1) ^ ((remainder >> 9) * 0x537);
        }
        let format = ((value << 10) | remainder) ^ 0x5412;
        let bit = |i: usize| (format >> i) & 1 != 0;
        for i in 0..=5 {
            set(modules, function, 8, i, bit(i));
        }
        set(modules, function, 8, 7, bit(6));
        set(modules, function, 8, 8, bit(7));
        set(modules, function, 7, 8, bit(8));
        for i in 9..15 {
            set(modules, function, 14 - i, 8, bit(i));
        }
        for i in 0..8 {
            set(modules, function, size - 1 - i, 8, bit(i));
        }
        for i in 8..15 {
            set(modules, function, 8, size - 15 + i, bit(i));
        }
        set(modules, function, 8, size - 8, true);
    };
    draw_format(&mut modules, &mut function, 0);
    if version >= 7 {
        let mut remainder = version as u32;
        for _ in 0..12 {
            remainder = (remainder << 1) ^ ((remainder >> 11) * 0x1F25);
        }
        let value = ((version as u32) << 12) | remainder;
        for i in 0..18 {
            let dark = (value >> i) & 1 != 0;
            let (a, b) = (size - 11 + i % 3, i / 3);
            set(&mut modules, &mut function, a, b, dark);
            set(&mut modules, &mut function, b, a, dark);
        }
    }

    // Codewords in the zigzag order, skipping function modules.
    let mut index = 0usize;
    let mut right = size as i32 - 1;
    while right >= 1 {
        if right == 6 {
            right = 5;
        }
        for vertical in 0..size {
            for j in 0..2 {
                let x = (right - j) as usize;
                let upward = (right + 1) & 2 == 0;
                let y = if upward { size - 1 - vertical } else { vertical };
                if !function[y][x] && index < codewords.len() * 8 {
                    modules[y][x] = (codewords[index >> 3] >> (7 - (index & 7))) & 1 != 0;
                    index += 1;
                }
            }
        }
        right -= 2;
    }

    let apply_mask = |modules: &mut Vec<Vec<bool>>, function: &Vec<Vec<bool>>, mask: u32| {
        for y in 0..size {
            for x in 0..size {
                if function[y][x] {
                    continue;
                }
                let invert = match mask {
                    0 => (x + y) % 2 == 0,
                    1 => y % 2 == 0,
                    2 => x % 3 == 0,
                    3 => (x + y) % 3 == 0,
                    4 => (x / 3 + y / 2) % 2 == 0,
                    5 => x * y % 2 + x * y % 3 == 0,
                    6 => (x * y % 2 + x * y % 3) % 2 == 0,
                    _ => ((x + y) % 2 + x * y % 3) % 2 == 0,
                };
                if invert {
                    modules[y][x] = !modules[y][x];
                }
            }
        }
    };
    let mut best = 0;
    let mut best_penalty: i64 = -1;
    for mask in 0..8 {
        apply_mask(&mut modules, &function, mask);
        draw_format(&mut modules, &mut function, mask);
        let penalty = penalty(&modules);
        if best_penalty < 0 || penalty < best_penalty {
            best = mask;
            best_penalty = penalty;
        }
        apply_mask(&mut modules, &function, mask);
    }
    apply_mask(&mut modules, &function, best);
    draw_format(&mut modules, &mut function, best);
    Ok(modules)
}

fn alignment_positions(version: i32, size: i32) -> Vec<i32> {
    if version == 1 {
        return Vec::new();
    }
    let count = version / 7 + 2;
    let step = (version * 8 + count * 3 + 5) / (count * 4 - 4) * 2;
    let mut positions = vec![6];
    for i in (0..=count - 2).rev() {
        positions.push(size - 7 - i * step);
    }
    positions
}

fn multiply(x: u32, y: u32) -> u32 {
    let mut z = 0u32;
    for i in (0..8).rev() {
        z = (z << 1) ^ ((z >> 7) * 0x11D);
        z ^= ((y >> i) & 1) * x;
    }
    z & 0xFF
}

fn divisor(degree: usize) -> Vec<u32> {
    let mut result = vec![0u32; degree];
    result[degree - 1] = 1;
    let mut root = 1u32;
    for _ in 0..degree {
        for j in 0..degree {
            result[j] = multiply(result[j], root);
            if j + 1 < degree {
                result[j] ^= result[j + 1];
            }
        }
        root = multiply(root, 0x02);
    }
    result
}

fn remainder(data: &[u8], divisor: &[u32]) -> Vec<u8> {
    let mut result: std::collections::VecDeque<u32> = std::iter::repeat_n(0, divisor.len()).collect();
    for byte in data {
        let factor = *byte as u32 ^ result.pop_front().unwrap_or(0);
        result.push_back(0);
        for (i, value) in result.iter_mut().enumerate() {
            *value ^= multiply(divisor[i], factor);
        }
    }
    result.into_iter().map(|v| v as u8).collect()
}

fn add_ecc_and_interleave(data: &[u8], version: i32) -> Vec<u8> {
    let blocks = NUM_ERROR_CORRECTION_BLOCKS[version as usize] as usize;
    let ecc_length = ECC_CODEWORDS_PER_BLOCK[version as usize] as usize;
    let raw = (raw_data_modules(version) / 8) as usize;
    let short_blocks = blocks - raw % blocks;
    let short_length = raw / blocks;
    let divisor = divisor(ecc_length);
    let mut all: Vec<Vec<u8>> = Vec::new();
    let mut offset = 0;
    for i in 0..blocks {
        let length = short_length - ecc_length + if i < short_blocks { 0 } else { 1 };
        let mut block = data[offset..offset + length].to_vec();
        offset += length;
        let ecc = remainder(&block, &divisor);
        if i < short_blocks {
            block.push(0);
        }
        block.extend(ecc);
        all.push(block);
    }
    let mut result = Vec::new();
    for i in 0..all[0].len() {
        for (j, block) in all.iter().enumerate() {
            if i != short_length - ecc_length || j >= short_blocks {
                result.push(block[i]);
            }
        }
    }
    result
}

/// ISO 18004 mask penalty rules 1 (runs), 2 (2×2 blocks), 3 (finder-like) and 4 (balance).
fn penalty(modules: &[Vec<bool>]) -> i64 {
    let size = modules.len();
    let runs = |at: &dyn Fn(usize, usize) -> bool| -> i64 {
        const PATTERN: [bool; 7] = [true, false, true, true, true, false, true];
        let mut penalty = 0i64;
        for i in 0..size {
            let mut run = 1i64;
            for j in 1..size {
                if at(i, j) == at(i, j - 1) {
                    run += 1;
                } else {
                    if run >= 5 {
                        penalty += 3 + run - 5;
                    }
                    run = 1;
                }
            }
            if run >= 5 {
                penalty += 3 + run - 5;
            }
            // Finder-like 1:1:3:1:1 runs with four light modules on either side.
            let mut j = 0;
            while j + 10 < size {
                if (0..7).all(|k| at(i, j + k + 4) == PATTERN[k]) && !at(i, j) && !at(i, j + 1) && !at(i, j + 2) && !at(i, j + 3) {
                    penalty += 40;
                }
                j += 1;
            }
            let mut j = 0;
            while j + 10 < size {
                if (0..7).all(|k| at(i, j + k) == PATTERN[k]) && !at(i, j + 7) && !at(i, j + 8) && !at(i, j + 9) && !at(i, j + 10) {
                    penalty += 40;
                }
                j += 1;
            }
        }
        penalty
    };
    let mut result = runs(&|i, j| modules[i][j]);
    result += runs(&|i, j| modules[j][i]);
    for y in 0..size.saturating_sub(1) {
        for x in 0..size.saturating_sub(1) {
            let color = modules[y][x];
            if color == modules[y][x + 1] && color == modules[y + 1][x] && color == modules[y + 1][x + 1] {
                result += 3;
            }
        }
    }
    let dark = modules.iter().flatten().filter(|m| **m).count() as i64;
    let total = (size * size) as i64;
    let k = ((dark * 20 - total * 10).abs() + total - 1) / total - 1;
    result + k.max(0) * 10
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_version_one_with_finders() {
        let modules = encode("HELLO").unwrap();
        assert_eq!(modules.len(), 21);
        // Finder pattern corners: dark ring, light ring, dark 3×3 core.
        assert!(modules[0][0] && modules[0][6] && modules[6][0] && modules[6][6]);
        assert!(!modules[1][1] && modules[2][2] && modules[3][3]);
        // The always-dark module above the bottom-left finder.
        assert!(modules[21 - 8][8]);
    }

    #[test]
    fn pairing_link_grows_the_version() {
        let link = format!("https://app.paseo.sh/#offer={}", "A".repeat(300));
        let modules = encode(&link).unwrap();
        assert!(modules.len() > 21);
        assert_eq!(modules.len() % 4, 1);
    }
}
