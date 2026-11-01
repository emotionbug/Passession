use crate::{HighlightRule, HighlightStyle};
use std::ops::Index;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ColorScheme {
    Light,
    Dark,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum SyntaxThemeId {
    Github,
    Catppuccin,
    Dracula,
    TokyoNight,
    One,
    Nord,
    Gruvbox,
    Solarized,
}

impl SyntaxThemeId {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Github => "github",
            Self::Catppuccin => "catppuccin",
            Self::Dracula => "dracula",
            Self::TokyoNight => "tokyo-night",
            Self::One => "one",
            Self::Nord => "nord",
            Self::Gruvbox => "gruvbox",
            Self::Solarized => "solarized",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        SYNTAX_THEME_IDS
            .into_iter()
            .find(|theme| theme.as_str() == name)
    }
}

pub const SYNTAX_THEME_IDS: [SyntaxThemeId; 8] = [
    SyntaxThemeId::Github,
    SyntaxThemeId::Catppuccin,
    SyntaxThemeId::Dracula,
    SyntaxThemeId::TokyoNight,
    SyntaxThemeId::One,
    SyntaxThemeId::Nord,
    SyntaxThemeId::Gruvbox,
    SyntaxThemeId::Solarized,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SyntaxThemeOption {
    pub id: SyntaxThemeId,
    pub label: &'static str,
}

pub const SYNTAX_THEME_OPTIONS: [SyntaxThemeOption; 8] = [
    SyntaxThemeOption {
        id: SyntaxThemeId::Github,
        label: "GitHub",
    },
    SyntaxThemeOption {
        id: SyntaxThemeId::Catppuccin,
        label: "Catppuccin",
    },
    SyntaxThemeOption {
        id: SyntaxThemeId::Dracula,
        label: "Dracula",
    },
    SyntaxThemeOption {
        id: SyntaxThemeId::TokyoNight,
        label: "Tokyo Night",
    },
    SyntaxThemeOption {
        id: SyntaxThemeId::One,
        label: "One",
    },
    SyntaxThemeOption {
        id: SyntaxThemeId::Nord,
        label: "Nord",
    },
    SyntaxThemeOption {
        id: SyntaxThemeId::Gruvbox,
        label: "Gruvbox",
    },
    SyntaxThemeOption {
        id: SyntaxThemeId::Solarized,
        label: "Solarized",
    },
];

/// Complete syntax palette, indexed by semantic role (never the editor background).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SyntaxColors(pub [&'static str; 20]);

impl Index<HighlightStyle> for SyntaxColors {
    type Output = str;
    fn index(&self, role: HighlightStyle) -> &str {
        self.0[role as usize]
    }
}

impl SyntaxColors {
    pub const fn color(self, role: HighlightStyle) -> &'static str {
        self.0[role as usize]
    }
}

pub const DARK_HIGHLIGHT_COLORS: SyntaxColors = SyntaxColors([
    "#ff7b72", "#8b949e", "#a5d6ff", "#79c0ff", "#79c0ff", "#d2a8ff", "#d2a8ff", "#ffa657",
    "#ff7b72", "#7ee787", "#79c0ff", "#79c0ff", "#c9d1d9", "#79c0ff", "#c9d1d9", "#a5d6ff",
    "#79c0ff", "#8b949e", "#79c0ff", "#a5d6ff",
]);

pub const LIGHT_HIGHLIGHT_COLORS: SyntaxColors = SyntaxColors([
    "#cf222e", "#6e7781", "#0a3069", "#0550ae", "#0550ae", "#8250df", "#8250df", "#953800",
    "#cf222e", "#116329", "#0550ae", "#0550ae", "#24292f", "#0550ae", "#24292f", "#0a3069",
    "#0550ae", "#6e7781", "#0550ae", "#0a3069",
]);

// base, keyword, comment, string, number, function, type, tag, attribute, operator.
const fn expand(r: [&'static str; 10]) -> SyntaxColors {
    SyntaxColors([
        r[1], r[2], r[3], r[4], r[4], r[5], r[5], r[6], r[6], r[7], r[8], r[8], r[0], r[9], r[0],
        r[3], r[4], r[2], r[5], r[3],
    ])
}

const CATPPUCCIN_LIGHT: SyntaxColors = expand([
    "#4c4f69", "#8839ef", "#8c8fa1", "#40a02b", "#fe640b", "#1e66f5", "#df8e1d", "#8839ef",
    "#df8e1d", "#04a5e5",
]);
const CATPPUCCIN_DARK: SyntaxColors = expand([
    "#cdd6f4", "#cba6f7", "#9399b2", "#a6e3a1", "#fab387", "#89b4fa", "#f9e2af", "#cba6f7",
    "#f9e2af", "#89dceb",
]);
const DRACULA: SyntaxColors = expand([
    "#f8f8f2", "#ff79c6", "#6272a4", "#f1fa8c", "#bd93f9", "#50fa7b", "#8be9fd", "#ff79c6",
    "#50fa7b", "#ff79c6",
]);
const TOKYO_LIGHT: SyntaxColors = expand([
    "#3760bf", "#9854f1", "#848cb5", "#587539", "#b15c00", "#2e7de9", "#007197", "#f52a65",
    "#8c6c3e", "#006a83",
]);
const TOKYO_DARK: SyntaxColors = expand([
    "#c0caf5", "#bb9af7", "#565f89", "#9ece6a", "#ff9e64", "#7aa2f7", "#2ac3de", "#f7768e",
    "#e0af68", "#89ddff",
]);
const ONE_LIGHT: SyntaxColors = expand([
    "#383a42", "#a626a4", "#a0a1a7", "#50a14f", "#986801", "#4078f2", "#c18401", "#e45649",
    "#986801", "#0184bc",
]);
const ONE_DARK: SyntaxColors = expand([
    "#abb2bf", "#c678dd", "#5c6370", "#98c379", "#d19a66", "#61afef", "#e5c07b", "#e06c75",
    "#d19a66", "#56b6c2",
]);
const NORD_LIGHT: SyntaxColors = expand([
    "#2e3440", "#5e81ac", "#6b7280", "#4f6f3a", "#8f5e91", "#2e6f8e", "#3b7f87", "#5e81ac",
    "#8f5e91", "#5e81ac",
]);
const NORD_DARK: SyntaxColors = expand([
    "#d8dee9", "#81a1c1", "#616e88", "#a3be8c", "#b48ead", "#88c0d0", "#8fbcbb", "#81a1c1",
    "#8fbcbb", "#81a1c1",
]);
const GRUVBOX_LIGHT: SyntaxColors = expand([
    "#3c3836", "#9d0006", "#928374", "#79740e", "#8f3f71", "#427b58", "#b57614", "#076678",
    "#b57614", "#af3a03",
]);
const GRUVBOX_DARK: SyntaxColors = expand([
    "#ebdbb2", "#fb4934", "#928374", "#b8bb26", "#d3869b", "#8ec07c", "#fabd2f", "#83a598",
    "#fabd2f", "#fe8019",
]);
const SOLARIZED_LIGHT: SyntaxColors = expand([
    "#657b83", "#859900", "#93a1a1", "#2aa198", "#d33682", "#268bd2", "#b58900", "#268bd2",
    "#b58900", "#859900",
]);
const SOLARIZED_DARK: SyntaxColors = expand([
    "#839496", "#859900", "#586e75", "#2aa198", "#d33682", "#268bd2", "#b58900", "#268bd2",
    "#b58900", "#859900",
]);

pub fn is_syntax_theme_id(value: &str) -> bool {
    SyntaxThemeId::from_name(value).is_some()
}

/// Dracula uses one palette; every other theme follows the light/dark axis.
/// Nord intentionally has both variants, matching the actual previous resolver.
pub const fn resolve_syntax_colors(id: SyntaxThemeId, scheme: ColorScheme) -> SyntaxColors {
    let dark = matches!(scheme, ColorScheme::Dark);
    match id {
        SyntaxThemeId::Github => {
            if dark {
                DARK_HIGHLIGHT_COLORS
            } else {
                LIGHT_HIGHLIGHT_COLORS
            }
        }
        SyntaxThemeId::Catppuccin => {
            if dark {
                CATPPUCCIN_DARK
            } else {
                CATPPUCCIN_LIGHT
            }
        }
        SyntaxThemeId::Dracula => DRACULA,
        SyntaxThemeId::TokyoNight => {
            if dark {
                TOKYO_DARK
            } else {
                TOKYO_LIGHT
            }
        }
        SyntaxThemeId::One => {
            if dark {
                ONE_DARK
            } else {
                ONE_LIGHT
            }
        }
        SyntaxThemeId::Nord => {
            if dark {
                NORD_DARK
            } else {
                NORD_LIGHT
            }
        }
        SyntaxThemeId::Gruvbox => {
            if dark {
                GRUVBOX_DARK
            } else {
                GRUVBOX_LIGHT
            }
        }
        SyntaxThemeId::Solarized => {
            if dark {
                SOLARIZED_DARK
            } else {
                SOLARIZED_LIGHT
            }
        }
    }
}

pub fn create_highlight_style(colors: SyntaxColors) -> [HighlightRule; 20] {
    HighlightStyle::ALL.map(|role| HighlightRule {
        role,
        color: colors.color(role),
    })
}
