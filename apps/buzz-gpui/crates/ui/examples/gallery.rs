//! Buzz design-system gallery: theme swatches, type ramp, icons and every
//! component, light and dark side by side.
//!
//! ```sh
//! cargo run -p buzz-ui --example gallery -- [page] [--scale 1.2]
//! ```
//!
//! Pages: `foundations` (default), `icons`, `buttons`, `controls`,
//! `overlays`, `navigation`, `shell`, `all`.

use buzz_ui::{
    ActiveTheme, AlertDialog, Appearance, AppearancePreference, Avatar, AvatarShape, AvatarSize,
    Badge, BadgeTone, Brand, BrandMark, Button, ButtonShape, ButtonSize, ButtonVariant, Checkbox,
    CheckboxState, Colors, Dialog, Divider, Icon, IconButton, IconButtonShape, IconButtonSize,
    IconName, InlineChip, Kbd, MaterialStyled, Menu, MenuItem, MenuLabel, MenuSeparator, MenuSize,
    NavigationItem, NavigationItemVariant, PanelHeader, Popover, Presence, SearchField,
    ShellBackground, Surface, Switch, Tab, TabVariant, Tabs, TextArea, TextField, TextRole, Theme,
    ThemeScope, Toast, ToastTone, Tooltip, TypeStyled,
};
use gpui_kit::{
    component::input::{InputState, TextareaState},
    *,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Foundations,
    Icons,
    Buttons,
    Controls,
    Overlays,
    Navigation,
    Shell,
    All,
}

impl Page {
    fn parse(arg: &str) -> Option<Self> {
        Some(match arg {
            "foundations" => Self::Foundations,
            "icons" => Self::Icons,
            "buttons" => Self::Buttons,
            "controls" => Self::Controls,
            "overlays" => Self::Overlays,
            "navigation" => Self::Navigation,
            "shell" => Self::Shell,
            "all" => Self::All,
            _ => return None,
        })
    }
}

/// Inputs per column, so light and dark each own their editing state.
#[derive(Clone)]
struct Fields {
    name: Entity<InputState>,
    empty: Entity<InputState>,
    invalid: Entity<InputState>,
    small: Entity<InputState>,
    search: Entity<InputState>,
    capsule: Entity<InputState>,
    notes: Entity<TextareaState>,
}

impl Fields {
    fn new(window: &mut Window, cx: &mut App) -> Self {
        let input =
            |placeholder: &'static str, value: &'static str, window: &mut Window, cx: &mut App| {
                cx.new(|cx| {
                    let mut state = InputState::new(window, cx).placeholder(placeholder);
                    state.set_value(value, window, cx);
                    state
                })
            };
        Self {
            name: input("Agent name", "Fixture Reader", window, cx),
            empty: input("Message #alpha", "", window, cx),
            invalid: input("Webhook URL", "not a url", window, cx),
            small: input("Filter", "", window, cx),
            search: input("Search", "design", window, cx),
            capsule: input("Search people", "", window, cx),
            notes: cx.new(|cx| {
                let mut state = TextareaState::new(window, cx).placeholder("Agent instructions");
                state.set_value("You review pull requests.\nBe concise.", window, cx);
                state
            }),
        }
    }
}

struct Gallery {
    page: Page,
    light: Fields,
    dark: Fields,
    switch_on: bool,
}

impl Gallery {
    fn new(page: Page, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            page,
            light: Fields::new(window, cx),
            dark: Fields::new(window, cx),
            switch_on: true,
        }
    }
}

fn heading(text: &'static str, cx: &App) -> impl IntoElement {
    div()
        .pt(rems(1.5))
        .pb(rems(0.75))
        .text_label()
        .text_color(cx.colors().text_standard)
        .child(text)
}

fn caption(text: impl Into<SharedString>, cx: &App) -> impl IntoElement {
    div()
        .text_caption()
        .text_color(cx.colors().text_metadata)
        .child(text.into())
}

fn row() -> Div {
    div().flex().flex_wrap().items_center().gap(rems(0.75))
}

fn swatch(name: &'static str, color: Hsla, cx: &App) -> impl IntoElement {
    let c = cx.colors();
    div()
        .flex()
        .items_center()
        .gap(rems(0.5))
        .w(rems(13.))
        .child(
            div()
                .size(rems(1.5))
                .rounded(rems(0.375))
                .border_1()
                .border_color(c.border_standard)
                .bg(color),
        )
        .child(
            div()
                .text_mono()
                .text_color(c.text_subtle)
                .truncate()
                .child(name),
        )
}

fn swatches(c: &Colors, cx: &App) -> impl IntoElement {
    let list: Vec<(&'static str, Hsla)> = vec![
        ("surface-base", c.surface_base),
        ("surface-panel", c.surface_panel),
        ("surface-popover", c.surface_popover),
        ("surface-inset", c.surface_inset),
        ("surface-inverse", c.surface_inverse),
        ("text-standard", c.text_standard),
        ("text-subtle", c.text_subtle),
        ("text-metadata", c.text_metadata),
        ("text-unavailable", c.text_unavailable),
        ("text-danger", c.text_danger),
        ("text-link", c.text_link),
        ("text-accent", c.text_accent),
        ("border-standard", c.border_standard),
        ("border-prominent", c.border_prominent),
        ("border-focus", c.border_focus),
        ("border-danger", c.border_danger),
        ("affordance-prominent", c.affordance_prominent),
        ("affordance-subtle", c.affordance_subtle),
        ("affordance-subtle-hover", c.affordance_subtle_hover),
        ("affordance-selected", c.affordance_selected),
        ("affordance-danger", c.affordance_danger),
        ("affordance-accent", c.affordance_accent),
        ("accent-prominent", c.affordance_accent_prominent),
        ("affordance-link-hover", c.affordance_link_hover),
        ("status-online", c.status_online),
        ("status-away", c.status_away),
        ("status-offline", c.status_offline),
        ("bg-scrim", c.bg_scrim),
    ];
    div()
        .flex()
        .flex_wrap()
        .gap(rems(0.5))
        .children(list.into_iter().map(|(n, col)| swatch(n, col, cx)))
}

fn type_ramp(cx: &App) -> impl IntoElement {
    let c = cx.colors();
    let roles = [
        (TextRole::Title, "Title 32/35.2 500"),
        (TextRole::Heading, "Heading 24/26.4 500"),
        (TextRole::BodyLg, "Body large 20/28"),
        (TextRole::Label, "Label 16/24 500"),
        (TextRole::Body, "Body 14/20 — the default reading text"),
        (TextRole::LabelSm, "Label small 14/20 500"),
        (TextRole::Caption, "Caption 12/16 — timestamps, tooltips"),
        (TextRole::Mono, "Mono 12/16 ChannelList.tsx"),
    ];
    div()
        .flex()
        .flex_col()
        .gap(rems(0.5))
        .text_color(c.text_standard)
        .child(div().text_display().child("Display 56"))
        .children(
            roles
                .into_iter()
                .map(|(role, text)| div().text_role(role).child(text)),
        )
}

fn foundations(cx: &App) -> Div {
    let c = cx.colors();
    div()
        .child(heading("Color roles", cx))
        .child(swatches(c, cx))
        .child(heading("Type ramp", cx))
        .child(type_ramp(cx))
        .child(heading("Radii", cx))
        .child(
            row().children([4., 8., 10., 12., 16., 20., 26.].into_iter().map(|r: f32| {
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(rems(0.25))
                    .child(
                        div()
                            .size(rems(3.))
                            .rounded(rems(r / 16.))
                            .bg(c.affordance_subtle)
                            .border_1()
                            .border_color(c.border_prominent),
                    )
                    .child(caption(format!("{r}"), cx))
            })),
        )
        .child(heading("Divider", cx))
        .child(Divider::horizontal())
        .child(heading("Elevation", cx))
        .child(
            row()
                .gap(rems(1.5))
                .child(div().size(rems(5.)).panel_surface(c))
                .child(div().size(rems(5.)).floating_surface(c)),
        )
}

fn icons(cx: &App) -> Div {
    let c = cx.colors();
    div()
        .child(heading("Icons (16px, stroke 2)", cx))
        .child(
            div()
                .flex()
                .flex_wrap()
                .gap(rems(0.25))
                .text_color(c.text_standard)
                .children(IconName::ALL.iter().map(|icon| {
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(rems(2.))
                        .rounded(rems(0.5))
                        .bg(c.surface_inset)
                        .child(Icon::new(*icon).size(16.))
                })),
        )
        .child(heading("Stroke 2.5 (sidebar) and 1.5 (rail globe)", cx))
        .child(
            row()
                .text_color(c.text_standard)
                .child(Icon::new(IconName::Bell).size(16.).stroke_width(2.5))
                .child(Icon::new(IconName::Alarm).size(16.).stroke_width(2.5))
                .child(
                    Icon::new(IconName::FolderSimple)
                        .size(16.)
                        .stroke_width(2.5),
                )
                .child(Icon::new(IconName::Robot).size(16.).stroke_width(2.5))
                .child(Icon::new(IconName::Lightning).size(16.).stroke_width(2.5))
                .child(Icon::new(IconName::Globe).size(24.).stroke_width(1.5))
                .child(Icon::new(IconName::Plus).size(24.)),
        )
        .child(heading("Harness marks", cx))
        .child(
            row()
                .text_color(c.text_standard)
                .children(BrandMark::ALL.iter().map(|m| Brand::new(*m))),
        )
}

fn buttons(cx: &App) -> Div {
    let variants = [
        ("prominent", ButtonVariant::Prominent),
        ("subtle", ButtonVariant::Subtle),
        ("ghost", ButtonVariant::Ghost),
        ("outline", ButtonVariant::Outline),
        ("destructive", ButtonVariant::Destructive),
        ("link", ButtonVariant::Link),
    ];
    let sizes = [
        ("xs", ButtonSize::Xs),
        ("sm", ButtonSize::Sm),
        ("md", ButtonSize::Md),
        ("lg", ButtonSize::Lg),
    ];
    let mut root = div().child(heading("Button", cx));
    for (name, variant) in variants {
        root = root.child(
            row()
                .pb(rems(0.75))
                .child(div().w(rems(5.5)).child(caption(name, cx)))
                .children(sizes.iter().map(|(s, size)| {
                    Button::new(SharedString::from(format!("{name}-{s}")), "Continue")
                        .variant(variant)
                        .size(*size)
                        .icon(IconName::ArrowRight)
                }))
                .child(
                    Button::new(SharedString::from(format!("{name}-disabled")), "Disabled")
                        .variant(variant)
                        .size(ButtonSize::Sm)
                        .disabled(true),
                ),
        );
    }
    root.child(
        row()
            .child(
                Button::new("loading", "Save changes")
                    .prominent()
                    .loading(true),
            )
            .child(Button::new("save", "Save changes").prominent())
            .child(Button::new("details", "Show details").size(ButtonSize::Sm))
            .child(
                Button::new("control", "Control shape")
                    .shape(ButtonShape::Control)
                    .size(ButtonSize::Sm),
            )
            .child(
                Button::new("tip", "Hover for tooltip")
                    .outline()
                    .size(ButtonSize::Sm)
                    .tooltip("Search Buzz (Ctrl+K)"),
            ),
    )
    .child(heading("IconButton", cx))
    .child(
        row()
            .children(
                [
                    ("g", ButtonVariant::Ghost, false),
                    ("gs", ButtonVariant::Ghost, true),
                    ("s", ButtonVariant::Subtle, false),
                    ("o", ButtonVariant::Outline, false),
                    ("p", ButtonVariant::Prominent, false),
                    ("t", ButtonVariant::Tint, false),
                    ("d", ButtonVariant::Destructive, false),
                ]
                .into_iter()
                .map(|(id, variant, selected)| {
                    IconButton::new(id, IconName::DotsThree)
                        .variant(variant)
                        .selected(selected)
                }),
            )
            .child(IconButton::new("xs", IconName::X).size(IconButtonSize::Xs))
            .child(IconButton::new("sm", IconName::MagnifyingGlass).size(IconButtonSize::Sm))
            .child(IconButton::new("lg", IconName::Plus).size(IconButtonSize::Lg))
            .child(
                IconButton::new("round", IconName::User)
                    .shape(IconButtonShape::Round)
                    .variant(ButtonVariant::Subtle),
            )
            .child(IconButton::new("dis", IconName::ArrowRight).disabled(true)),
    )
    .child(heading("Avatar", cx))
    .child(
        row()
            .child(Avatar::new("alice").size(AvatarSize::Small))
            .child(Avatar::new("Bob"))
            .child(Avatar::new("Carol").size(AvatarSize::Large))
            .child(Avatar::new("Goose").squircle().size(AvatarSize::Large))
            .child(
                Avatar::new("Ann")
                    .size(AvatarSize::Large)
                    .presence(Some(Presence::Online)),
            )
            .child(
                Avatar::new("Ann")
                    .size(AvatarSize::Large)
                    .presence(Some(Presence::Away)),
            )
            .child(
                Avatar::new("Agent")
                    .shape(AvatarShape::Squircle)
                    .size(AvatarSize::Large)
                    .presence(Some(Presence::Offline)),
            )
            .child(Avatar::new("").fallback_icon(IconName::Robot, 16.))
            .child(Avatar::new("Fixture").profile_disc()),
    )
    .child(heading("Badges, chips, kbd", cx))
    .child(
        row()
            .child(Badge::new("3"))
            .child(Badge::new("Online").tone(BadgeTone::Success))
            .child(Badge::new("Away").tone(BadgeTone::Warning))
            .child(Badge::new("Beta").tone(BadgeTone::Accent))
            .child(Badge::new("12").tone(BadgeTone::Prominent))
            .child(Badge::dot())
            .child(div().text_body().child(InlineChip::new("c1", "@Alice")))
            .child(div().text_body().child(InlineChip::new("c2", "#general")))
            .child(
                div()
                    .text_body()
                    .child(InlineChip::new("c3", "design doc").icon(IconName::Link)),
            )
            .child(
                div()
                    .text_body()
                    .child(InlineChip::new("c4", "@unknown").unresolved(true)),
            )
            .child(Kbd::new("Ctrl K")),
    )
}

fn controls(fields: &Fields, switch_on: bool, cx: &App) -> Div {
    div()
        .child(heading("Switch", cx))
        .child(
            row()
                .gap(rems(1.5))
                .child(Switch::new("on", switch_on).label("Notifications"))
                .child(Switch::new("off", false).label("Sounds"))
                .child(Switch::new("dis", true).disabled(true).label("Disabled")),
        )
        .child(heading("Checkbox", cx))
        .child(
            row()
                .gap(rems(1.5))
                .child(Checkbox::new("c1", true))
                .child(Checkbox::new("c2", false))
                .child(Checkbox::new("c3", CheckboxState::Indeterminate))
                .child(Checkbox::new("c4", true).disabled(true))
                .child(
                    Checkbox::new("c5", true)
                        .label("Share with the channel")
                        .description("Everyone in #alpha can see it."),
                ),
        )
        .child(heading("Input", cx))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(rems(0.75))
                .w(rems(26.))
                .child(TextField::new(&fields.name))
                .child(TextField::new(&fields.empty))
                .child(TextField::new(&fields.invalid).invalid(true))
                .child(TextField::new(&fields.small).small())
                .child(SearchField::new(&fields.search))
                .child(SearchField::new(&fields.capsule).capsule())
                .child(TextArea::new(&fields.notes)),
        )
        .child(heading("Tabs", cx))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(rems(1.))
                .child(
                    Tabs::new(TabVariant::Panel)
                        .tab(Tab::new("p1", "Preview").selected(true))
                        .tab(Tab::new("p2", "Code")),
                )
                .child(
                    Tabs::new(TabVariant::Workspace)
                        .tab(Tab::new("w1", "Overview").selected(true))
                        .tab(Tab::new("w2", "Files"))
                        .tab(Tab::new("w3", "Preview").preview(true)),
                )
                .child(
                    Tabs::new(TabVariant::Navigation)
                        .tab(
                            Tab::new("n1", "Alpha")
                                .icon(IconName::Hash)
                                .selected(true)
                                .on_close(|_, _, _| {}),
                        )
                        .tab(
                            Tab::new("n2", "Thread")
                                .icon(IconName::ChatCircle)
                                .on_close(|_, _, _| {}),
                        ),
                ),
        )
}

fn overlays(cx: &App) -> Div {
    div()
        .child(heading("Menu and tooltip", cx))
        .child(
            row()
                .items_start()
                .gap(rems(1.5))
                .child(
                    Menu::new()
                        .child(MenuLabel::new("Channel"))
                        .child(MenuItem::new("m1", "Channel details").icon(IconName::Info))
                        .child(
                            MenuItem::new("m2", "Copy link")
                                .icon(IconName::Link)
                                .highlighted(true),
                        )
                        .child(
                            MenuItem::new("m3", "Sort")
                                .icon(IconName::ArrowsDownUp)
                                .submenu(),
                        )
                        .child(
                            MenuItem::new("m4", "Mute")
                                .icon(IconName::BellSlash)
                                .checked(true),
                        )
                        .child(MenuSeparator::new())
                        .child(
                            MenuItem::new("m5", "Leave channel")
                                .icon(IconName::SignOut)
                                .danger(),
                        ),
                )
                .child(
                    Menu::new()
                        .size(MenuSize::Compact)
                        .child(
                            MenuItem::new("k1", "Mark all as read")
                                .icon(IconName::Checks)
                                .icon_size(14.)
                                .size(MenuSize::Compact),
                        )
                        .child(
                            MenuItem::new("k2", "Copy community URL")
                                .icon(IconName::Link)
                                .icon_size(14.)
                                .size(MenuSize::Compact)
                                .highlighted(true),
                        )
                        .child(
                            MenuItem::new("k3", "Settings")
                                .icon(IconName::Gear)
                                .icon_size(14.)
                                .size(MenuSize::Compact)
                                .trailing(Kbd::new("Ctrl ,")),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .items_start()
                        .gap(rems(0.5))
                        .child(Tooltip::bubble("Search Buzz (Ctrl+K)", cx))
                        .child(Tooltip::bubble("Nov 15, 2023 at 12:15 AM", cx)),
                ),
        )
        .child(heading("Popover and toast", cx))
        .child(
            row()
                .items_start()
                .gap(rems(1.5))
                .child(
                    Popover::new()
                        .title("Agent activity")
                        .description("Two agents are working in this channel."),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .w(rems(22.5))
                        .gap(rems(0.75))
                        .child(
                            Toast::new("t1", "Message copied")
                                .description("The link is on your clipboard.")
                                .on_dismiss(|_, _, _| {}),
                        )
                        .child(
                            Toast::new("t2", "Couldn't send")
                                .tone(ToastTone::Error)
                                .description("The relay rejected the event.")
                                .action(
                                    Button::new("retry", "Retry")
                                        .size(ButtonSize::Sm)
                                        .surface(Surface::Floating),
                                ),
                        ),
                ),
        )
        .child(heading("Dialog", cx))
        .child(
            Dialog::new("dlg", "Leave #alpha?")
                .description("You can rejoin from channel discovery.")
                .on_close(|_, _, _| {})
                .child(
                    div()
                        .text_body()
                        .child("Your messages stay in the channel."),
                )
                .footer(Button::new("cancel", "Cancel").surface(Surface::Floating))
                .footer(Button::new("leave", "Leave channel").prominent()),
        )
        .child(div().h(rems(1.)))
        .child(
            AlertDialog::new("Delete agent?")
                .description("This removes its key from this computer.")
                .action(Button::new("c2", "Cancel").surface(Surface::Floating))
                .action(Button::new("d2", "Delete").destructive()),
        )
}

fn navigation(cx: &App) -> Div {
    let c = cx.colors();
    div()
        .child(heading("Sidebar rows", cx))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(rems(0.125))
                .w(rems(16.25))
                .p(rems(0.5))
                .panel_surface(c)
                .children(
                    [
                        ("Inbox", IconName::Bell),
                        ("Reminders", IconName::Alarm),
                        ("Projects", IconName::FolderSimple),
                        ("Agents", IconName::Robot),
                        ("Workflows", IconName::Lightning),
                    ]
                    .into_iter()
                    .map(|(label, icon)| {
                        NavigationItem::new(label, label)
                            .sidebar()
                            .icon(icon)
                            .icon_stroke(2.5)
                    }),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(rems(0.25))
                        .pl(rems(0.5))
                        .min_h(rems(2.))
                        .text_body_sm()
                        .text_color(c.text_unavailable)
                        .child("Channels")
                        .child(Icon::new(IconName::CaretDown).size(15.).stroke_width(2.5)),
                )
                .child(
                    NavigationItem::new("alpha", "Alpha")
                        .sidebar()
                        .icon(IconName::Hash)
                        .selected(true),
                )
                .child(
                    NavigationItem::new("beta", "Beta")
                        .sidebar()
                        .icon(IconName::Hash)
                        .unread(true)
                        .trailing(Badge::dot()),
                )
                .child(
                    NavigationItem::new("private", "Private")
                        .sidebar()
                        .icon(IconName::Lock),
                )
                .child(
                    NavigationItem::new("alice", "Alice Fixture")
                        .sidebar()
                        .leading(Avatar::new("Alice").size(AvatarSize::Px(22.))),
                ),
        )
        .child(heading("NavigationItem", cx))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(rems(0.125))
                .w(rems(16.))
                .child(NavigationItem::new("r1", "Profile").icon(IconName::User))
                .child(
                    NavigationItem::new("r2", "Appearance")
                        .icon(IconName::Palette)
                        .selected(true),
                )
                .child(
                    NavigationItem::new("r3", "Notifications")
                        .icon(IconName::Bell)
                        .trailing("3"),
                )
                .child(NavigationItem::new("r4", "Inset row").variant(NavigationItemVariant::Inset))
                .child(
                    NavigationItem::new("r5", "Pill")
                        .variant(NavigationItemVariant::Pill)
                        .selected(true),
                ),
        )
        .child(heading("PanelHeader", cx))
        .child(
            div()
                .w(rems(30.))
                .panel_surface(c)
                .child(
                    PanelHeader::new()
                        .label(Some(IconName::Hash), "Alpha")
                        .action(
                            IconButton::new("members", IconName::Users).size(IconButtonSize::Sm),
                        )
                        .action(
                            IconButton::new("more", IconName::DotsThree).size(IconButtonSize::Sm),
                        )
                        .action(
                            IconButton::new("side", IconName::SidebarRight)
                                .size(IconButtonSize::Sm),
                        ),
                )
                .child(div().h(rems(3.)))
                .child(PanelHeader::new().title("Thread").divider(false)),
        )
}

/// A full-window mock of the Messages shell (2.1), laid out like the
/// `app-channel-timeline` reference at 1200×800.
fn shell(cx: &App) -> impl IntoElement {
    let c = cx.colors();
    let header_button = |id: &'static str, icon: IconName| {
        IconButton::new(id, icon)
            .size(IconButtonSize::Sm)
            .surface(Surface::Header)
    };
    let header = div()
        .relative()
        .flex()
        .items_center()
        .justify_between()
        .h(rems(3.))
        .pl(rems(1.))
        .pr(rems(1.))
        .child(
            div()
                .flex()
                .pl(rems(1.))
                .child(header_button("tog", IconName::Sidebar))
                .child(header_button("back", IconName::ArrowLeft))
                .child(header_button("fwd", IconName::ArrowRight).disabled(true)),
        )
        .child(
            div()
                .absolute()
                .top(rems(0.25))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    Tabs::new(TabVariant::Chrome)
                        .tab(Tab::new("me", "Me"))
                        .tab(Tab::new("msg", "Messages").selected(true)),
                ),
        )
        .child(
            div()
                .flex()
                .gap(rems(0.5))
                .child(header_button("search", IconName::MagnifyingGlass).selected(true))
                .child(
                    IconButton::new("me-btn", IconName::User)
                        .size(IconButtonSize::Sm)
                        .variant(ButtonVariant::Avatar)
                        .shape(IconButtonShape::Round)
                        .child(
                            Avatar::new("Fixture")
                                .size(AvatarSize::Px(32.))
                                .profile_disc(),
                        ),
                ),
        );
    let rail = div()
        .flex()
        .flex_col()
        .items_center()
        .gap(rems(0.5))
        .w(rems(3.))
        .py(rems(0.5))
        .child(IconButton::new("world", IconName::Globe).stroke_width(1.5))
        .child(
            IconButton::new("primary", IconName::Globe)
                .selected(true)
                .child(Avatar::new("Primary").squircle().size(AvatarSize::Small)),
        )
        .child(
            IconButton::new("second", IconName::Globe)
                .child(Avatar::new("Second").squircle().size(AvatarSize::Small)),
        )
        .child(IconButton::new("add", IconName::Plus));
    let page_row = |label: &'static str, icon: IconName| {
        NavigationItem::new(label, label)
            .sidebar()
            .icon(icon)
            .icon_stroke(2.5)
    };
    let section = |label: &'static str| {
        div()
            .flex()
            .items_center()
            .gap(rems(0.25))
            .pl(rems(0.5))
            .min_h(rems(2.))
            .text_body_sm()
            .text_color(c.text_unavailable)
            .child(label)
            .child(Icon::new(IconName::CaretDown).size(15.).stroke_width(2.5))
    };
    let rows = || {
        div()
            .flex()
            .flex_col()
            .gap(rems(0.125))
            .mr(rems(0.625))
            .ml(rems(0.25))
    };
    let sidebar = div()
        .flex()
        .flex_col()
        .flex_none()
        .w(px(260.))
        .h_full()
        .px(rems(0.5))
        .py(rems(0.5))
        .border_r_1()
        .border_color(c.border_standard)
        .child(
            rows()
                .mb(rems(0.75))
                .child(page_row("Inbox", IconName::Bell))
                .child(page_row("Reminders", IconName::Alarm))
                .child(page_row("Projects", IconName::FolderSimple))
                .child(page_row("Agents", IconName::Robot))
                .child(page_row("Agents", IconName::Robot))
                .child(page_row("Workflows", IconName::Lightning)),
        )
        .child(section("Channels"))
        .child(
            rows()
                .child(
                    NavigationItem::new("alpha", "Alpha")
                        .sidebar()
                        .icon(IconName::Hash)
                        .selected(true)
                        .unread(true),
                )
                .child(
                    NavigationItem::new("beta", "Beta")
                        .sidebar()
                        .icon(IconName::Hash)
                        .unread(true),
                ),
        )
        .child(div().h(rems(1.)))
        .child(section("Direct messages"))
        .child(
            rows().child(
                NavigationItem::new("alice", "Alice Fixture")
                    .sidebar()
                    .leading(Avatar::new("Alice").size(AvatarSize::Px(22.))),
            ),
        );
    let main = div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .child(
            PanelHeader::new()
                .joined(true)
                .label(Some(IconName::Hash), "Alpha")
                .action(IconButton::new("members", IconName::Users).size(IconButtonSize::Sm))
                .action(IconButton::new("more", IconName::DotsThree).size(IconButtonSize::Sm))
                .action(IconButton::new("side", IconName::Sidebar).size(IconButtonSize::Sm)),
        )
        .child(
            div()
                .flex()
                .gap(rems(1.))
                .p(rems(1.5))
                .child(Avatar::new("Fixture Reader"))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(rems(0.5))
                                .child(div().text_label_sm().child("Fixture Reader"))
                                .child(
                                    div()
                                        .text_caption()
                                        .text_color(c.text_metadata)
                                        .child("Nov 15, 2023 at 12:15 AM"),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(rems(0.25))
                                .text_body()
                                .child("Shipping the sidebar fix today. See")
                                .child(InlineChip::new("chip", "ChannelList.tsx"))
                                .child("and the design doc."),
                        ),
                ),
        );
    ShellBackground::new().child(
        div()
            .flex()
            .flex_col()
            .size_full()
            .text_color(c.text_standard)
            .child(header)
            .child(
                div().flex().flex_1().min_h_0().child(rail).child(
                    div().flex_1().min_w_0().pr(rems(1.)).pb(rems(1.)).child(
                        div()
                            .flex()
                            .size_full()
                            .overflow_hidden()
                            .panel_surface(c)
                            .child(sidebar)
                            .child(main),
                    ),
                ),
            ),
    )
}

fn column(page: Page, fields: Fields, switch_on: bool, cx: &App) -> Div {
    let c = cx.colors();
    let mut col = div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .min_h(px(900.))
        .p(rems(1.5))
        .bg(c.surface_panel)
        .text_color(c.text_standard)
        .text_body();
    let all = page == Page::All;
    if all || page == Page::Foundations {
        col = col.child(foundations(cx));
    }
    if all || page == Page::Icons {
        col = col.child(icons(cx));
    }
    if all || page == Page::Buttons {
        col = col.child(buttons(cx));
    }
    if all || page == Page::Controls {
        col = col.child(controls(&fields, switch_on, cx));
    }
    if all || page == Page::Overlays {
        col = col.child(overlays(cx));
    }
    if all || page == Page::Navigation {
        col = col.child(navigation(cx));
    }
    col
}

impl Render for Gallery {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let page = self.page;
        let (light, dark, on) = (self.light.clone(), self.dark.clone(), self.switch_on);
        let scale = Theme::base(cx).interface_scale();
        if page == Page::Shell {
            return div()
                .size_full()
                .font_family("Inter")
                .child(shell(cx))
                .into_any_element();
        }
        div()
            .id("gallery")
            .size_full()
            .overflow_y_scroll()
            .font_family("Inter")
            .child(
                div()
                    .flex()
                    .w_full()
                    .child(ThemeScope::new(Appearance::Light, move |_, cx| {
                        column(page, light, on, cx)
                    }))
                    .child(ThemeScope::new(Appearance::Dark, move |_, cx| {
                        column(page, dark, on, cx)
                    })),
            )
            .child(
                div()
                    .absolute()
                    .top(rems(0.5))
                    .right(rems(1.))
                    .text_caption()
                    .text_color(cx.colors().text_metadata)
                    .child(format!("interface {:.0}%", scale * 100.)),
            )
            .into_any_element()
    }
}

fn main() {
    let mut page = Page::Foundations;
    let mut scale = 1.;
    let mut dark = false;
    let mut window_size = size(px(1400.), px(900.));
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--scale" {
            scale = args.next().and_then(|s| s.parse().ok()).unwrap_or(1.);
        } else if arg == "--dark" {
            dark = true;
        } else if arg == "--size" {
            if let Some((w, h)) = args.next().as_deref().and_then(|s| s.split_once('x')) {
                window_size = size(
                    px(w.parse().unwrap_or(1400.)),
                    px(h.parse().unwrap_or(900.)),
                );
            }
        } else if let Some(p) = Page::parse(&arg) {
            page = p;
        }
    }
    gpui_kit::application()
        .with_assets(buzz_ui::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            buzz_ui::init(cx);
            Theme::set_preference(
                if dark {
                    AppearancePreference::Dark
                } else {
                    AppearancePreference::Light
                },
                cx,
            );
            Theme::set_interface_scale(scale, cx);
            let bounds = Bounds::new(point(px(0.), px(0.)), window_size);
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                cx,
                move |window, cx| cx.new(|cx| Gallery::new(page, window, cx)),
            )
            .expect("open gallery window");
        });
}
