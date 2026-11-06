//! Project identity presentation (`project_settings_panel.dart` `projectViewKey`,
//! `_identityFill`, `ProjectIconSquare`).

use serde_json::{json, Value};

use super::rpc::str_of;

/// `createProjectViewKey`: the host's shared `projectKey`, otherwise the JSON
/// `[serverId, projectId]` placement.
pub fn project_view_key(server_id: &str, project: &Value) -> String {
    let key = str_of(project, "projectKey");
    if !key.is_empty() {
        return key.to_owned();
    }
    json!([server_id, str_of(project, "projectId")]).to_string()
}

/// IDENTITY_COLORS fill tones; the hash indexes into this order.
pub const IDENTITY_FILLS: [u32; 10] = [
    0x7a6aa8, // violet
    0x3d7ea6, // sky
    0x388068, // emerald
    0xa4673a, // orange
    0xb05c80, // pink
    0x6a70b8, // indigo
    0x368080, // teal
    0xb06260, // red
    0x8f7838, // amber
    0x5179b0, // blue
];

/// `deriveIdentityColorName`: `hash * 31 + code` per code point, unsigned 32-bit. Dart's
/// `String.fromCharCode(rune).codeUnitAt(0)` is the first UTF-16 unit of the code point.
pub fn identity_fill(key: &str) -> u32 {
    let mut hash: u32 = 0;
    for ch in key.chars() {
        let mut units = [0u16; 2];
        let first = ch.encode_utf16(&mut units)[0];
        hash = hash.wrapping_mul(31).wrapping_add(first as u32);
    }
    IDENTITY_FILLS[(hash as usize) % IDENTITY_FILLS.len()]
}

/// The white initial of the last path segment, `?` when empty.
pub fn project_initial(name: &str) -> String {
    let trimmed = name.trim();
    let label = trimmed.split('/').rfind(|s| !s.is_empty()).unwrap_or(trimmed);
    label
        .chars()
        .next()
        .map(|c| c.to_uppercase().collect())
        .unwrap_or_else(|| "?".to_owned())
}

/// `0xRRGGBB` as a Makepad `vec4` color.
pub fn rgb(color: u32) -> makepad_widgets::Vec4 {
    makepad_widgets::vec4(
        ((color >> 16) & 0xff) as f32 / 255.0,
        ((color >> 8) & 0xff) as f32 / 255.0,
        (color & 0xff) as f32 / 255.0,
        1.0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_key_prefers_project_key() {
        assert_eq!(project_view_key("s", &json!({"projectKey": "k", "projectId": "p"})), "k");
        assert_eq!(project_view_key("s", &json!({"projectId": "p"})), "[\"s\",\"p\"]");
    }

    #[test]
    fn identity_hash_matches_dart_reference() {
        // "a" = 97 → 97 % 10 = 7 (red); "ab" = 97*31+98 = 3105 → 5 (indigo).
        assert_eq!(identity_fill("a"), IDENTITY_FILLS[7]);
        assert_eq!(identity_fill("ab"), IDENTITY_FILLS[5]);
    }

    #[test]
    fn initial_uses_last_segment() {
        assert_eq!(project_initial("~/code/paseo"), "P");
        assert_eq!(project_initial("  "), "?");
        assert_eq!(project_initial("한글"), "한");
    }
}
