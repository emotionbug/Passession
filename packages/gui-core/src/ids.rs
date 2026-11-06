use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};

/// Flutter `PaseoClient.newRequestId`: 18 random bytes, base64url without padding. Also the
/// format of a newly created persistent client identity.
pub(crate) fn new_id() -> String {
    let mut bytes = [0u8; 18];
    bytes[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes[16..].copy_from_slice(&uuid::Uuid::new_v4().as_bytes()[..2]);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Dart `Uri.encodeComponent`: everything except `A-Za-z0-9-_.!~*'()` is percent-encoded.
pub(crate) fn encode_component(value: &str) -> String {
    use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
    const COMPONENT: &AsciiSet = &NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'!')
        .remove(b'~')
        .remove(b'*')
        .remove(b'\'')
        .remove(b'(')
        .remove(b')');
    utf8_percent_encode(value, COMPONENT).to_string()
}

pub(crate) fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}
