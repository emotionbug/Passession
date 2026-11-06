//! Which built-in panel opens a file (Flutter `file_panel_resolve.dart`, original
//! `file-panels/resolve.ts` + `catalog.ts`) and how a text panel presents it (Flutter
//! `fileTextPresentation`, original `SOURCE_PRESENTATION_BUDGETS`).
//!
//! The native client has only Paseo's built-in panels (executable plugin panels are not
//! supported), so every file has at most one candidate; an association
//! (`ext:<extension>` / `name:<filename>` → panel id) applies only when it names a candidate.

use serde_json::Value;

/// One file panel: its id, the files it claims (`None` claims every file of its kind), and the
/// only kind of file it reads (`text` | `image` | `binary`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FilePanelCandidate {
    pub id: &'static str,
    pub file_kind: &'static str,
    pub extensions: Option<&'static [&'static str]>,
    pub filenames: &'static [&'static str],
}

impl FilePanelCandidate {
    pub fn claims_all(&self) -> bool {
        self.extensions.is_none()
    }
}

pub const MARKDOWN_FILE_PANEL: FilePanelCandidate = FilePanelCandidate {
    id: "builtin:markdown",
    file_kind: "text",
    extensions: Some(&["md", "markdown"]),
    filenames: &[],
};
pub const HTML_FILE_PANEL: FilePanelCandidate = FilePanelCandidate {
    id: "builtin:html",
    file_kind: "text",
    extensions: Some(&["html", "htm"]),
    filenames: &[],
};
pub const IMAGE_FILE_PANEL: FilePanelCandidate = FilePanelCandidate {
    id: "builtin:image",
    file_kind: "image",
    extensions: Some(&["png", "jpg", "jpeg", "gif", "webp", "svg"]),
    filenames: &[],
};
/// Paseo's editor, the floor for text files no other built-in claims.
pub const SOURCE_FILE_PANEL: FilePanelCandidate =
    FilePanelCandidate { id: "builtin:source", file_kind: "text", extensions: None, filenames: &[] };

/// Original `filePanelCatalog`, in resolver order: the built-ins that render a file, then Source.
pub const BUILTIN_FILE_PANELS: [FilePanelCandidate; 4] =
    [MARKDOWN_FILE_PANEL, HTML_FILE_PANEL, IMAGE_FILE_PANEL, SOURCE_FILE_PANEL];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FilePanelResolution {
    pub panel: Option<FilePanelCandidate>,
    pub candidates: Vec<FilePanelCandidate>,
    pub selector_key: String,
    pub associated_panel_id: Option<&'static str>,
}

fn basename(path: &str) -> &str {
    let trimmed = path.trim();
    trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed)
}

/// Original `filePanelExtension`: lowercase, without the dot.
pub fn file_panel_extension(path: &str) -> String {
    let name = basename(path);
    match name.rfind('.') {
        Some(dot) => name[dot + 1..].to_lowercase(),
        None => String::new(),
    }
}

fn matches(name: &str, extension: &str, candidate: &FilePanelCandidate) -> bool {
    match candidate.extensions {
        None => true,
        Some(extensions) => {
            candidate.filenames.contains(&name)
                || extensions.iter().any(|entry| entry.to_lowercase() == extension)
        }
    }
}

/// Original `resolveFilePanel` over the built-in catalog. `associations` is the device's
/// `filePanelAssociations` preference object.
pub fn resolve_file_panel(
    path: &str,
    kind: &str,
    associations: &Value,
    choice: Option<&str>,
) -> FilePanelResolution {
    let name = basename(path);
    let extension = file_panel_extension(path);
    let matching: Vec<FilePanelCandidate> = BUILTIN_FILE_PANELS
        .iter()
        .copied()
        .filter(|candidate| candidate.file_kind == kind && matches(name, &extension, candidate))
        .collect();
    let claimed_by_builtin = matching.iter().any(|candidate| !candidate.claims_all());
    let offered: Vec<FilePanelCandidate> = if claimed_by_builtin {
        matching.into_iter().filter(|candidate| !candidate.claims_all()).collect()
    } else {
        matching
    };
    let matched_filename = offered
        .iter()
        .any(|candidate| !candidate.claims_all() && candidate.filenames.contains(&name));
    let selector_key =
        if matched_filename { format!("name:{name}") } else { format!("ext:{extension}") };
    let associated_id = associations.get(&selector_key).and_then(Value::as_str);
    let associated = offered.iter().copied().find(|candidate| Some(candidate.id) == associated_id);
    let chosen = offered.iter().copied().find(|candidate| Some(candidate.id) == choice);
    FilePanelResolution {
        panel: chosen.or(associated).or_else(|| offered.first().copied()),
        candidates: offered,
        selector_key,
        associated_panel_id: associated.map(|candidate| candidate.id),
    }
}

/// Original `MAX_EDITABLE_BYTES`: the daemon refuses to write larger files.
pub const MAX_EDITABLE_FILE_BYTES: u64 = 1024 * 1024;

/// How a text panel shows its file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileTextPresentation {
    Editable,
    Highlighted,
    Plain,
    Unsupported,
}

impl FileTextPresentation {
    pub fn name(self) -> &'static str {
        match self {
            Self::Editable => "editable",
            Self::Highlighted => "highlighted",
            Self::Plain => "plain",
            Self::Unsupported => "unsupported",
        }
    }
}

/// Files above the platform's display budget are refused before reading; the editor platform
/// (desktop) edits files up to 1 MiB; everything else is shown read-only.
pub fn file_text_presentation(size: u64, editor_platform: bool) -> FileTextPresentation {
    let highlighted = if editor_platform { 10 * 1024 * 1024 } else { 1024 * 1024 };
    let plain = if editor_platform { 50 * 1024 * 1024 } else { 10 * 1024 * 1024 };
    if size > plain {
        return FileTextPresentation::Unsupported;
    }
    if editor_platform && size <= MAX_EDITABLE_FILE_BYTES {
        return FileTextPresentation::Editable;
    }
    if size <= highlighted {
        FileTextPresentation::Highlighted
    } else {
        FileTextPresentation::Plain
    }
}

/// Original `TextFilePanel`: only desktop mounts the editable source; Android and iOS show every
/// text file in the read-only source view (the original native branch).
pub fn editor_platform() -> bool {
    !cfg!(any(target_os = "android", target_os = "ios"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn builtins_claim_their_extensions_and_source_is_the_text_floor() {
        let none = json!({});
        let panel = |path: &str, kind: &str| resolve_file_panel(path, kind, &none, None).panel;
        assert_eq!(panel("docs/README.MD", "text"), Some(MARKDOWN_FILE_PANEL));
        assert_eq!(panel("a/index.htm", "text"), Some(HTML_FILE_PANEL));
        assert_eq!(panel("logo.svg", "image"), Some(IMAGE_FILE_PANEL));
        assert_eq!(panel("main.rs", "text"), Some(SOURCE_FILE_PANEL));
        assert_eq!(panel("notes.mkd", "text"), Some(SOURCE_FILE_PANEL));
        assert_eq!(panel("icon.bmp", "image"), None);
        assert_eq!(panel("blob.bin", "binary"), None);
        assert_eq!(panel("logo.svg", "text"), Some(SOURCE_FILE_PANEL));
    }

    #[test]
    fn associations_only_pick_a_candidate() {
        let resolution = resolve_file_panel(
            "README.md",
            "text",
            &json!({"ext:md": "builtin:source", "md": "source"}),
            None,
        );
        assert_eq!(resolution.selector_key, "ext:md");
        assert_eq!(resolution.candidates, vec![MARKDOWN_FILE_PANEL]);
        assert_eq!(resolution.associated_panel_id, None);
        assert_eq!(resolution.panel, Some(MARKDOWN_FILE_PANEL));
    }

    #[test]
    fn presentation_budgets() {
        use FileTextPresentation::*;
        assert_eq!(file_text_presentation(1024 * 1024, true), Editable);
        assert_eq!(file_text_presentation(1024 * 1024 + 1, true), Highlighted);
        assert_eq!(file_text_presentation(20 * 1024 * 1024, true), Plain);
        assert_eq!(file_text_presentation(50 * 1024 * 1024 + 1, true), Unsupported);
        assert_eq!(file_text_presentation(10, false), Highlighted);
        assert_eq!(file_text_presentation(2 * 1024 * 1024, false), Plain);
        assert_eq!(file_text_presentation(10 * 1024 * 1024 + 1, false), Unsupported);
    }
}
