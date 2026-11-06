//! Launch argument and link parsing shared by every platform.

use std::path::{Path, PathBuf};

/// `@getpaseo/protocol` agent-deep-link.ts `parseAgentDeepLink`: exactly
/// `paseo://h/<serverId>/agent/<agentId>` with no credentials, port, query or
/// fragment. Segments are percent-decoded and trimmed; a malformed or non-UTF-8
/// escape or a blank segment rejects the link.
pub fn parse_agent_deep_link(link: &str) -> Option<(String, String)> {
    let url = url::Url::parse(link.trim()).ok()?;
    if !url.scheme().eq_ignore_ascii_case("paseo")
        || url.host_str() != Some("h")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    let segments: Vec<&str> = url.path().split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() != 3 || segments[1] != "agent" {
        return None;
    }
    let decode = |segment: &str| -> Option<String> {
        if !valid_escapes(segment) {
            return None;
        }
        let decoded = percent_encoding::percent_decode_str(segment).decode_utf8().ok()?;
        let trimmed = decoded.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    };
    Some((decode(segments[0])?, decode(segments[2])?))
}

/// JS `decodeURIComponent` throws on `%` not followed by two hex digits.
fn valid_escapes(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if !(bytes.get(i + 1).is_some_and(u8::is_ascii_hexdigit)
                && bytes.get(i + 2).is_some_and(u8::is_ascii_hexdigit))
            {
                return false;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    true
}

/// desktop open-project-routing.ts `parseOpenProjectPathFromArgv`: ignoring
/// `-psn_*` and `--no-sandbox*`, the first non-flag existing absolute directory
/// wins, else the directory following `--open-project` (`--paseo-project`).
pub fn open_project_path_from_argv(argv: &[String]) -> Option<PathBuf> {
    let directory = |candidate: &str| {
        let path = Path::new(candidate);
        path.is_absolute() && path.is_dir()
    };
    let args: Vec<&str> = argv
        .iter()
        .map(String::as_str)
        .filter(|arg| !arg.starts_with("-psn_") && !arg.starts_with("--no-sandbox"))
        .collect();
    if let Some(found) = args.iter().find(|arg| !arg.starts_with('-') && directory(arg)) {
        return Some(PathBuf::from(found));
    }
    let index = args
        .iter()
        .position(|arg| *arg == "--open-project")
        .or_else(|| args.iter().position(|arg| *arg == "--paseo-project"))?;
    let flagged = args.get(index + 1)?;
    directory(flagged).then(|| PathBuf::from(flagged))
}

/// The first agent deep link in argv, if any (original
/// `parseAgentDeepLinkFromArgv`: first valid link only).
pub fn agent_deep_link_from_argv(argv: &[String]) -> Option<(String, String)> {
    argv.iter().find_map(|arg| parse_agent_deep_link(arg))
}

/// Original `pairingLinkFromScan`: only pairing links start an attempt.
pub fn pairing_link_from_scan(data: &str) -> Option<String> {
    let raw = data.trim();
    (raw.contains("#offer=") || raw.contains("#connect=") || raw.starts_with("relay://"))
        .then(|| raw.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_links_follow_the_protocol_parser() {
        assert_eq!(
            parse_agent_deep_link("paseo://h/srv%20one/agent/a1"),
            Some(("srv one".into(), "a1".into()))
        );
        assert_eq!(parse_agent_deep_link("paseo://h/srv/agent/%20"), None);
        assert_eq!(parse_agent_deep_link("paseo://h/srv/agent/a%2"), None);
        assert_eq!(parse_agent_deep_link("paseo://h/srv/agent/a?x=1"), None);
        assert_eq!(parse_agent_deep_link("paseo://user@h/srv/agent/a"), None);
        assert_eq!(parse_agent_deep_link("paseo://h:1/srv/agent/a"), None);
        assert_eq!(parse_agent_deep_link("paseo://h/srv/session/a"), None);
        assert_eq!(parse_agent_deep_link("paseo://h/srv/agent/%FF"), None);
    }

    #[test]
    fn pairing_links_require_offer_connect_or_relay() {
        assert!(pairing_link_from_scan(" https://x/#offer=abc ").is_some());
        assert!(pairing_link_from_scan("relay://abc").is_some());
        assert!(pairing_link_from_scan("https://example.com").is_none());
    }
}
