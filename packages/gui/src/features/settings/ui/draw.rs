//! Native drawing primitives for the settings surface: the shared exact
//! rounded-rect `DrawBox` (fill + inside border), the shell `Painter` for
//! Lucide glyphs and rings, and the provider-glyph `DrawIcon`.

use makepad_widgets::*;

use crate::common::paint::Painter;
use crate::theme::DrawBox;

live_design! {
    use link::theme::*;
    use link::shaders::*;
    use crate::theme::*;
    use crate::common::paint::PaseoPainter;

    pub SettingsDraws = {{Draws}} {
        rect: <DrawBox> {}
        painter: <PaseoPainter> {}
        provider_icon: {}
        text: { text_style: <PASEO_FONT_REGULAR> { line_spacing: 1.3 } }
        medium: { text_style: <PASEO_FONT_MEDIUM> { line_spacing: 1.3 } }
        code: { text_style: <PASEO_FONT_MONO> { line_spacing: 1.4 } }
    }
}

/// Settings glyphs, drawn as Lucide w300 icons (the Flutter `LucideIcons.*300`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Icon {
    ChevronRight,
    ChevronDown,
    ArrowUp,
    ArrowDown,
    ArrowUpToLine,
    Plus,
    X,
    Check,
    Pencil,
    Trash,
    RotateCw,
    MoreHorizontal,
    Search,
    Info,
    ArrowUpRight,
    ArrowLeft,
    Settings,
    Settings2,
    Star,
    Pin,
    PinOff,
    CircleCheck,
    CircleX,
    TriangleAlert,
    Copy,
    FileText,
    Server,
    Spinner,
    Globe,
    Monitor,
    Sun,
    Moon,
    Palette,
    PanelLeft,
    MessageSquare,
    Code,
    Files,
    Keyboard,
    Bell,
    Activity,
    FolderGit,
    Network,
    Smartphone,
    Bot,
    Sparkles,
    Blocks,
    Boxes,
    Gauge,
    SquareTerminal,
    Plug,
    History,
    CalendarClock,
    Undo,
    PackagePlus,
    Circle,
    Stethoscope,
    Terminal,
    Bug,
    Wrench,
    Hammer,
    Flask,
    TestTube,
    Microscope,
    Eye,
    Feather,
    Book,
    Rocket,
    Package,
    Database,
    Cpu,
    Cloud,
    GitBranch,
    Layers,
    Compass,
    Brain,
    Shield,
}

impl Icon {
    /// Lucide kebab-case name (`crate::common::icons`).
    pub fn name(self) -> &'static str {
        use self::Icon::*;
        match self {
            ChevronRight => "chevron-right",
            ChevronDown => "chevron-down",
            ArrowUp => "arrow-up",
            ArrowDown => "arrow-down",
            ArrowUpToLine => "arrow-up-to-line",
            Plus => "plus",
            X => "x",
            Check => "check",
            Pencil => "pencil",
            Trash => "trash-2",
            RotateCw => "rotate-cw",
            MoreHorizontal => "ellipsis",
            Search => "search",
            Info => "info",
            ArrowUpRight => "arrow-up-right",
            ArrowLeft => "arrow-left",
            Settings => "settings",
            Settings2 => "settings-2",
            Star => "star",
            Pin => "pin",
            PinOff => "pin-off",
            CircleCheck => "circle-check",
            CircleX => "circle-x",
            TriangleAlert => "triangle-alert",
            Copy => "copy",
            FileText => "file-text",
            Server => "server",
            Spinner => "loader-circle",
            Globe => "globe",
            Monitor => "monitor",
            Sun => "sun",
            Moon => "moon",
            Palette => "palette",
            PanelLeft => "panel-left",
            MessageSquare => "message-square",
            Code => "code",
            Files => "files",
            Keyboard => "keyboard",
            Bell => "bell",
            Activity => "activity",
            FolderGit => "folder-git-2",
            Network => "network",
            Smartphone => "smartphone",
            Bot => "bot",
            Sparkles => "sparkles",
            Blocks => "blocks",
            Boxes => "boxes",
            Gauge => "gauge",
            SquareTerminal => "square-terminal",
            Plug => "plug",
            History => "history",
            CalendarClock => "calendar-clock",
            Undo => "undo-2",
            PackagePlus => "package-plus",
            Circle => "circle",
            Stethoscope => "stethoscope",
            Terminal => "terminal",
            Bug => "bug",
            Wrench => "wrench",
            Hammer => "hammer",
            Flask => "flask-conical",
            TestTube => "test-tube",
            Microscope => "microscope",
            Eye => "eye",
            Feather => "feather",
            Book => "book-open",
            Rocket => "rocket",
            Package => "package",
            Database => "database",
            Cpu => "cpu",
            Cloud => "cloud",
            GitBranch => "git-branch",
            Layers => "layers",
            Compass => "compass",
            Brain => "brain",
            Shield => "shield",
        }
    }
}

/// Original `agent-profile-glyph.tsx` icon registry (keys stored in profiles).
pub const PROFILE_ICONS: &[(&str, Icon)] = &[
    ("code", Icon::Code),
    ("terminal", Icon::Terminal),
    ("bug", Icon::Bug),
    ("wrench", Icon::Wrench),
    ("hammer", Icon::Hammer),
    ("flask", Icon::Flask),
    ("testTube", Icon::TestTube),
    ("microscope", Icon::Microscope),
    ("search", Icon::Search),
    ("eye", Icon::Eye),
    ("palette", Icon::Palette),
    ("feather", Icon::Feather),
    ("pencil", Icon::Pencil),
    ("fileText", Icon::FileText),
    ("book", Icon::Book),
    ("rocket", Icon::Rocket),
    ("package", Icon::Package),
    ("boxes", Icon::Boxes),
    ("server", Icon::Server),
    ("database", Icon::Database),
    ("cpu", Icon::Cpu),
    ("cloud", Icon::Cloud),
    ("globe", Icon::Globe),
    ("gitBranch", Icon::GitBranch),
    ("layers", Icon::Layers),
    ("compass", Icon::Compass),
    ("brain", Icon::Brain),
    ("sparkles", Icon::Sparkles),
    ("shield", Icon::Shield),
];

pub fn profile_icon(name: &str) -> Icon {
    PROFILE_ICONS.iter().find(|(key, _)| *key == name).map(|(_, icon)| *icon).unwrap_or(Icon::Star)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Font {
    Regular,
    Medium,
    Code,
}

/// The draw shaders a settings surface owns: one set for the page list, one
/// shared by the overlay layer lists (each layer draws within its own list).
#[derive(Live, LiveHook, LiveRegister)]
pub struct Draws {
    #[live]
    pub rect: DrawBox,
    #[live]
    pub painter: Painter,
    #[live]
    pub provider_icon: DrawIcon,
    #[live]
    pub text: DrawText,
    #[live]
    pub medium: DrawText,
    #[live]
    pub code: DrawText,
}

