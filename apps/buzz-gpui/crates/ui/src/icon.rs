//! Icons: the Tabler glyphs buzz-app's icon gateway exports, buzz-app's
//! custom glyphs, and the harness brand marks on the Agents page.

use gpui_kit::{
    App, Hsla, ImageSource, IntoElement, ParentElement, Radians, Rems, RenderOnce, SharedString,
    Styled, Transformation, Window, div, img, percentage, prelude::FluentBuilder, rems, svg,
};

macro_rules! tabler_icons {
    ($( $name:ident => $file:literal, )*) => {
        /// A monochrome glyph. Tabler outline icons draw a 2px stroke on a 24px
        /// grid and follow the current text color.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum IconName {
            $( $name, )*
            /// buzz-app's "Send to channel" glyph.
            HashArrowIn,
            /// buzz-app's filled counterpart to `Notification`.
            NotificationFilled,
            /// Block Goose harness mark (monochrome).
            GooseLogo,
            /// OpenAI's Blossom mark for Codex (monochrome).
            CodexLogo,
            /// buzz-app's outline Microsoft OneDrive mark, for links.
            OneDriveLogo,
            /// buzz-app's GitHub open-issue mark.
            GitHubIssue,
            /// Bestie's raster mark (`BestieIcon`), drawn as an image.
            Bestie,
            /// buzz-app's agent mark (`RobotIcon`): the Soft & wide robot.
            Robot,
        }

        impl IconName {
            /// Every icon, for galleries.
            pub const ALL: &[IconName] = &[
                $( IconName::$name, )*
                IconName::HashArrowIn,
                IconName::NotificationFilled,
                IconName::GooseLogo,
                IconName::CodexLogo,
                IconName::OneDriveLogo,
                IconName::GitHubIssue,
                IconName::Bestie,
                IconName::Robot,
            ];

            /// The embedded asset path.
            pub fn path(self) -> &'static str {
                match self {
                    $( IconName::$name => concat!("icons/tabler/", $file, ".svg"), )*
                    IconName::HashArrowIn => "icons/custom/hash-arrow-in.svg",
                    IconName::NotificationFilled => "icons/custom/notification-filled.svg",
                    IconName::GooseLogo => "icons/brand/goose.svg",
                    IconName::CodexLogo => "icons/brand/codex.svg",
                    IconName::OneDriveLogo => "icons/custom/onedrive-logo.svg",
                    IconName::GitHubIssue => "icons/custom/github-issue.svg",
                    IconName::Bestie => "icons/custom/bestie.png",
                    IconName::Robot => "icons/custom/robot.svg",
                }
            }

            /// The buzz-app export name without the `Icon` suffix.
            pub fn name(self) -> &'static str {
                match self {
                    $( IconName::$name => stringify!($name), )*
                    IconName::HashArrowIn => "HashArrowIn",
                    IconName::NotificationFilled => "NotificationFilled",
                    IconName::GooseLogo => "GooseLogo",
                    IconName::CodexLogo => "CodexLogo",
                    IconName::OneDriveLogo => "OneDriveLogo",
                    IconName::GitHubIssue => "GitHubIssue",
                    IconName::Bestie => "Bestie",
                    IconName::Robot => "Robot",
                }
            }
        }
    };
}

// Generated from the icon gateway table in docs/port/ui.md (section 7.2),
// plus the emoji picker category glyphs. Names follow buzz-app's exports
// without the `Icon` suffix.
tabler_icons! {
    Alarm => "outline/alarm",
    Archive => "outline/archive",
    ArchiveOff => "outline/archive-off",
    ArrowClockwise => "outline/rotate-clockwise",
    ArrowCounterClockwise => "outline/rotate",
    ArrowDown => "outline/arrow-down",
    ArrowLeft => "outline/arrow-left",
    ArrowRight => "outline/arrow-right",
    ArrowSquareLeft => "outline/square-arrow-left",
    ArrowSquareOut => "outline/external-link",
    ArrowSquareRight => "outline/square-arrow-right",
    ArrowUp => "outline/arrow-up",
    ArrowUpRight => "outline/arrow-up-right",
    ArrowsClockwise => "outline/refresh",
    ArrowsDownUp => "outline/arrows-down-up",
    ArrowsIn => "outline/arrows-minimize",
    ArrowsLeftRight => "outline/arrows-left-right",
    ArrowsOut => "outline/arrows-maximize",
    Asterisk => "outline/asterisk",
    At => "outline/at",
    Barbell => "outline/barbell",
    Bell => "outline/bell",
    BellSlash => "outline/bell-off",
    BoxArrowUp => "outline/upload",
    Browser => "outline/browser",
    Bulb => "outline/bulb",
    Calendar => "outline/calendar",
    Car => "outline/car",
    CaretDown => "outline/chevron-down",
    CaretLeft => "outline/chevron-left",
    CaretRight => "outline/chevron-right",
    CaretUp => "outline/chevron-up",
    Category => "outline/category",
    ChatCircle => "outline/message-circle",
    ChatsCircle => "outline/messages",
    Check => "outline/check",
    CheckCircle => "outline/circle-check",
    Checks => "outline/checks",
    Circle => "outline/circle",
    CircleDashed => "outline/circle-dashed",
    CircleHalf => "outline/circle-half",
    CircleNotch => "outline/loader-2",
    Clock => "outline/clock",
    CloudUpload => "outline/cloud-upload",
    Code => "outline/code",
    CodeBlock => "outline/source-code",
    Columns => "outline/columns",
    Copy => "outline/copy",
    Crown => "outline/crown",
    Detective => "outline/spy",
    DotsThree => "outline/dots",
    DotsThreeVertical => "outline/dots-vertical",
    Download => "outline/download",
    DropboxLogo => "outline/brand-dropbox",
    Envelope => "outline/mail",
    EnvelopeOpen => "outline/mail-opened",
    Eye => "outline/eye",
    EyeSlash => "outline/eye-off",
    FigmaLogo => "outline/brand-figma",
    FileText => "outline/file-text",
    Flag => "outline/flag",
    FolderOpen => "outline/folder-open",
    FolderSimple => "outline/folder",
    Gauge => "outline/gauge",
    Gear => "outline/settings",
    GitBranch => "outline/git-branch",
    GitCommit => "outline/git-commit",
    GitMerge => "outline/git-merge",
    GitPullRequest => "outline/git-pull-request",
    GithubLogo => "outline/brand-github",
    GitlabLogo => "outline/brand-gitlab",
    Globe => "outline/world",
    GoogleDriveLogo => "outline/brand-google-drive",
    Hash => "outline/hash",
    House => "outline/home",
    Info => "outline/info-circle",
    Keyboard => "outline/keyboard",
    Lemon => "outline/lemon-2",
    Lightning => "outline/bolt",
    Link => "outline/link",
    LinkBreak => "outline/unlink",
    ListBullets => "outline/list",
    ListChecks => "outline/list-check",
    ListNumbers => "outline/list-numbers",
    Lock => "outline/lock",
    MagnifyingGlass => "outline/search",
    Microphone => "outline/microphone",
    MicrosoftTeamsLogo => "outline/brand-teams",
    Minus => "outline/minus",
    Monitor => "outline/device-desktop",
    Moon => "outline/moon",
    Notification => "outline/notification",
    NotionLogo => "outline/brand-notion",
    Palette => "outline/palette",
    Paperclip => "outline/paperclip",
    Pause => "outline/player-pause",
    PauseFilled => "filled/player-pause",
    Paw => "outline/paw",
    PencilSimple => "outline/pencil",
    Play => "outline/player-play",
    PlayFilled => "filled/player-play",
    Plug => "outline/plug",
    Plus => "outline/plus",
    Presentation => "outline/presentation",
    Question => "outline/help-circle",
    Quotes => "outline/blockquote",
    Shield => "outline/shield",
    Sidebar => "outline/layout-sidebar",
    SidebarRight => "outline/layout-sidebar-right",
    SignOut => "outline/logout",
    SlackLogo => "outline/brand-slack",
    SlidersHorizontal => "outline/adjustments-horizontal",
    Smiley => "outline/mood-smile",
    SmileyPlus => "outline/mood-plus",
    SmileySticker => "outline/sticker-2",
    SpeakerHigh => "outline/volume",
    SpeakerSlash => "outline/volume-off",
    SquaresFour => "outline/layout-grid",
    Stop => "outline/player-stop",
    StopFilled => "filled/player-stop",
    Sun => "outline/sun",
    Table => "outline/table",
    TerminalWindow => "outline/terminal-2",
    TextAa => "outline/letter-case",
    TextB => "outline/bold",
    TextItalic => "outline/italic",
    TextStrikethrough => "outline/strikethrough",
    ThumbsUp => "outline/thumb-up",
    Ticket => "outline/ticket",
    Timer => "outline/stopwatch",
    Trash => "outline/trash",
    User => "outline/user",
    Users => "outline/users",
    VideoCamera => "outline/video",
    VideoConference => "outline/video-plus",
    WarningCircle => "outline/alert-circle",
    WebhooksLogo => "outline/webhook",
    Wrench => "outline/tool",
    X => "outline/x",
    XCircle => "outline/circle-x",
    YoutubeLogo => "outline/brand-youtube",
}

impl IconName {
    /// Whether the glyph is drawn with strokes, so [`Icon::stroke_width`]
    /// applies.
    pub fn is_stroked(self) -> bool {
        self.path().starts_with("icons/tabler/outline/")
            || matches!(self, IconName::HashArrowIn | IconName::Robot)
    }
}

/// An icon at a size and color.
///
/// The default size is 1rem (16px at 100% interface size) and the default
/// color is the inherited text color, like buzz-app's `1em` /
/// `currentColor` defaults.
#[derive(IntoElement)]
pub struct Icon {
    name: IconName,
    size: Rems,
    color: Option<Hsla>,
    stroke: Option<f32>,
    rotation: Option<Radians>,
}

impl Icon {
    pub fn new(name: IconName) -> Self {
        Self {
            name,
            size: rems(1.),
            color: None,
            stroke: None,
            rotation: None,
        }
    }

    /// Size in CSS px at 100% interface size (buzz-app's numeric `size`).
    pub fn size(mut self, px: f32) -> Self {
        self.size = rems(px / 16.);
        self
    }

    /// Size in rems.
    pub fn size_rems(mut self, size: Rems) -> Self {
        self.size = size;
        self
    }

    pub fn color(mut self, color: impl Into<Hsla>) -> Self {
        self.color = Some(color.into());
        self
    }

    /// Overrides the Tabler stroke width (2 by default). The sidebar uses 2.5
    /// and the community rail globe 1.5.
    pub fn stroke_width(mut self, width: f32) -> Self {
        self.stroke = Some(width);
        self
    }

    /// Rotates the glyph around its center, e.g. a collapsed chevron.
    pub fn rotate(mut self, angle: impl Into<Radians>) -> Self {
        self.rotation = Some(angle.into());
        self
    }
}

impl RenderOnce for Icon {
    fn render(self, window: &mut Window, _: &mut App) -> impl IntoElement {
        if self.name == IconName::Bestie {
            return img(self.name.path())
                .flex_none()
                .size(self.size)
                .into_any_element();
        }
        let color = self.color.unwrap_or_else(|| window.text_style().color);
        let path: SharedString = match self.stroke {
            Some(width) if self.name.is_stroked() && width != 2. => {
                format!("{}?stroke={width}", self.name.path()).into()
            }
            _ => self.name.path().into(),
        };
        svg()
            .path(path)
            .flex_none()
            .size(self.size)
            .text_color(color)
            .when_some(self.rotation, |this, angle| {
                this.with_transformation(Transformation::rotate(angle))
            })
            .into_any_element()
    }
}

/// The spinning `loader-2` glyph used by loading buttons (1s per turn).
#[derive(IntoElement)]
pub struct Spinner {
    size: Rems,
    color: Option<Hsla>,
}

impl Spinner {
    pub fn new() -> Self {
        Self {
            size: rems(1.),
            color: None,
        }
    }

    pub fn size(mut self, px: f32) -> Self {
        self.size = rems(px / 16.);
        self
    }

    pub fn color(mut self, color: impl Into<Hsla>) -> Self {
        self.color = Some(color.into());
        self
    }
}

impl Default for Spinner {
    fn default() -> Self {
        Self::new()
    }
}

impl RenderOnce for Spinner {
    fn render(self, window: &mut Window, _: &mut App) -> impl IntoElement {
        use gpui_kit::{Animation, AnimationExt};
        let color = self.color.unwrap_or_else(|| window.text_style().color);
        svg()
            .path(IconName::CircleNotch.path())
            .flex_none()
            .size(self.size)
            .text_color(color)
            .with_animation(
                "spinner",
                Animation::new(crate::tokens::duration::SPINNER_TURN).repeat(),
                |svg, delta| svg.with_transformation(Transformation::rotate(percentage(delta))),
            )
    }
}

/// Full-color harness marks (Agents page and agent settings).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BrandMark {
    /// Goose, monochrome; drawn in the current text color.
    Goose,
    /// Pi: white glyph on a near-black rounded square.
    Pi,
    /// Claude Code.
    Claude,
    /// Hermes Agent.
    Hermes,
    /// Codex, monochrome; drawn in the current text color.
    Codex,
}

impl BrandMark {
    pub const ALL: &[BrandMark] = &[
        Self::Goose,
        Self::Pi,
        Self::Claude,
        Self::Hermes,
        Self::Codex,
    ];

    fn path(self) -> &'static str {
        match self {
            Self::Goose => "icons/brand/goose.svg",
            Self::Pi => "icons/brand/pi.svg",
            Self::Claude => "icons/brand/claude.png",
            Self::Hermes => "icons/brand/hermes.png",
            Self::Codex => "icons/brand/codex.svg",
        }
    }
}

/// A [`BrandMark`] at a size (32px by default, as on the Agents page).
#[derive(IntoElement)]
pub struct Brand {
    mark: BrandMark,
    size: Rems,
    color: Option<Hsla>,
}

impl Brand {
    pub fn new(mark: BrandMark) -> Self {
        Self {
            mark,
            size: rems(2.),
            color: None,
        }
    }

    pub fn size(mut self, px: f32) -> Self {
        self.size = rems(px / 16.);
        self
    }

    /// Color for the monochrome Goose and Codex marks; ignored by the
    /// others.
    pub fn color(mut self, color: impl Into<Hsla>) -> Self {
        self.color = Some(color.into());
        self
    }
}

impl RenderOnce for Brand {
    fn render(self, window: &mut Window, _: &mut App) -> impl IntoElement {
        match self.mark {
            mark @ (BrandMark::Goose | BrandMark::Codex) => Icon::new(match mark {
                BrandMark::Goose => IconName::GooseLogo,
                _ => IconName::CodexLogo,
            })
            .size_rems(self.size)
            .color(self.color.unwrap_or_else(|| window.text_style().color))
            .into_any_element(),
            mark => div()
                .flex_none()
                .size(self.size)
                .child(
                    img(ImageSource::Resource(gpui_kit::Resource::Embedded(
                        mark.path().into(),
                    )))
                    .size_full(),
                )
                .into_any_element(),
        }
    }
}
