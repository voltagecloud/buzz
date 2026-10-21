//! The Settings page (ui.md §5b): a shell destination, not a modal. The
//! settings sidebar takes the channel sidebar's place and the detail pane
//! shows one section at a time.

mod about;
mod agents;
mod appearance;
pub(crate) mod archive;
pub(crate) mod avatar;
pub mod cards;
mod communities;
pub(crate) use communities::left_text;
mod emoji;
pub(crate) mod form;
pub mod messages;
mod notifications;
pub mod pairing;
pub mod templates;
pub use pairing::close as close_pairing;
mod plugin_import;
mod plugins;
mod profile;
mod shortcuts;
mod updates;

use crate::community::{AppState, Community};
use crate::hosts::HostConnection;
use crate::ui::agents::AddHost;
use crate::ui::overlay;
pub use agents::{followup_draft, remember_mentioned_agents};
pub use archive::capture as capture_agent_activity;
pub use archive::opened as agent_archive;
use buzz_ui::{
    ActiveTheme, Button, ButtonShape, Icon, IconName, NavigationItem, Toast, ToastTone, TypeStyled,
    tokens::radius,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use profile::ProfileForm;
pub use profile::{SignOutDialog, default_profile, remember_profile};
use std::rc::Rc;
use std::time::Duration;

actions!(
    buzz,
    [
        /// Mod+, : open Settings.
        OpenSettings,
        /// Open Settings at Agents, where hosts are managed.
        OpenHostSettings,
        /// The settings sidebar's Back row.
        CloseSettings,
    ]
);

const TOAST_DURATION: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    Profile,
    CustomEmoji,
    Communities,
    Appearance,
    Notifications,
    Shortcuts,
    Agents,
    Plugins,
    Updates,
    About,
    /// A plugin's card, by its key (`cards::SettingsCard::key`).
    Card(&'static str),
}

/// Emitted when the page shows another section, so it is a history visit.
pub struct SectionChanged;

impl EventEmitter<SectionChanged> for SettingsPage {}

impl Section {
    /// The default section: Profile with a community, else Appearance
    /// (`defaultSection`).
    pub fn default_for(has_community: bool) -> Self {
        if has_community {
            Self::Profile
        } else {
            Self::Appearance
        }
    }

    /// Whether the sidebar lists the section: Custom emoji belongs to a
    /// community.
    fn listed(self, has_community: bool, cx: &App) -> bool {
        match self {
            // The Emoji plugin's card (`buzz.emoji/custom-emoji`).
            Self::CustomEmoji => has_community && crate::plugins::enabled("buzz.emoji"),
            // Only a card that shows is listed; one pending or failed is
            // left, as buzz-app lists only `true`.
            Self::Card(key) => cards::get(key, cx).is_some_and(|card| {
                (has_community || matches!(card.placement, cards::CardPlacement::Group(_)))
                    && card.visibility(cx) == cards::CardVisibility::Visible
            }),
            _ => true,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Profile => "Profile",
            Self::CustomEmoji => "Custom emoji",
            Self::Communities => "Communities",
            Self::Appearance => "Appearance",
            Self::Notifications => "Notifications",
            Self::Shortcuts => "Shortcuts",
            Self::Agents => "Agents",
            Self::Plugins => "Plugins",
            Self::Updates => "Updates",
            Self::About => "About",
            Self::Card(key) => key,
        }
    }

    fn icon(self) -> IconName {
        match self {
            Self::Profile => IconName::User,
            Self::CustomEmoji => IconName::Smiley,
            Self::Communities => IconName::Globe,
            Self::Appearance => IconName::Palette,
            Self::Notifications => IconName::Bell,
            Self::Shortcuts => IconName::Keyboard,
            Self::Agents => IconName::Robot,
            Self::Plugins => IconName::SquaresFour,
            Self::Updates => IconName::Download,
            Self::About => IconName::Info,
            Self::Card(_) => IconName::Plug,
        }
    }
}

enum Modal {
    AddHost {
        dialog: Entity<AddHost>,
        _subscription: Subscription,
    },
    RemoveHost {
        host: Entity<HostConnection>,
        error: Option<SharedString>,
    },
    Leave {
        community: Entity<Community>,
        busy: bool,
        error: Option<SharedString>,
    },
    SignOut {
        dialog: Entity<SignOutDialog>,
        _subscription: Subscription,
    },
    /// Shorten retention or clear saved agent activity.
    Archive(archive::Confirm),
    PluginImport {
        dialog: Entity<plugin_import::PluginImport>,
        _subscription: Subscription,
    },
    SignPlugin {
        path: std::path::PathBuf,
        name: String,
        busy: bool,
        error: Option<SharedString>,
    },
}

pub struct SettingsPage {
    section: Section,
    profile: ProfileForm,
    modal: Option<Modal>,
    toast: Option<(SharedString, usize)>,
    toast_tone: ToastTone,
    toasts: usize,
    /// Made on first visit; its saves raise this page's toast.
    custom_emoji: Option<(Entity<emoji::CustomEmojiSettings>, Subscription)>,
    /// The notification sound select that is open.
    sound_menu: Option<buzz_relay_client::Category>,
    /// The archive's "Activity retention" select is open.
    retention_menu: bool,
    /// The last archive save or clear failed.
    archive_error: Option<SharedString>,
    /// The archive settings the card shows, read off the UI thread.
    archive: Option<archive::Loaded>,
    /// Made on first visit; keeps the row being rebound and its notice.
    shortcuts: Option<Entity<shortcuts::ShortcutSettings>>,
    /// The open plugin card's view and the community it was made for.
    card: Option<(&'static str, Option<EntityId>, AnyView)>,
    /// Card visibility checks run while the page shows; a changed answer
    /// re-lists the sidebar.
    _card_checks: (Vec<Subscription>, Subscription),
    _app_observer: Subscription,
    _community_observer: Option<Subscription>,
}

impl SettingsPage {
    /// Opens at `section`, or the default: Profile with a community
    /// selected, otherwise Appearance.
    pub fn new(section: Option<Section>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let has_community = AppState::current(cx).is_some();
        let mut page = Self {
            section: section.unwrap_or(Section::default_for(has_community)),
            profile: ProfileForm::new(window, cx),
            modal: None,
            toast: None,
            toast_tone: ToastTone::Success,
            toasts: 0,
            custom_emoji: None,
            sound_menu: None,
            retention_menu: false,
            archive_error: None,
            archive: None,
            shortcuts: None,
            card: None,
            _card_checks: (
                cards::retain_visibility(cx),
                cards::observe(window, cx, |this, _, cx| this.fall_back(cx)),
            ),
            _app_observer: cx.observe_global_in::<AppState>(window, |this, window, cx| {
                this.follow_community(window, cx)
            }),
            _community_observer: None,
        };
        page.follow_community(window, cx);
        page
    }

    /// Shows `section`; a change is a history visit.
    pub fn open(&mut self, section: Section, cx: &mut Context<Self>) {
        if section != self.section {
            self.quiet(cx);
            self.section = section;
            // The archive card reads its settings again when it shows.
            if let Some(archive) = &mut self.archive {
                archive.stale = true;
            }
            cx.emit(SectionChanged);
        }
        cx.notify();
    }

    pub fn section(&self) -> Section {
        self.section
    }

    /// Keeps the profile form on the selected community.
    fn follow_community(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let community = AppState::current(cx);
        self._community_observer = community.as_ref().map(|community| {
            cx.observe_in(community, window, |this, _, window, cx| {
                this.sync_profile(window, cx);
                cx.notify();
            })
        });
        self.sync_profile(window, cx);
        self.fall_back(cx);
    }

    /// A section no longer listed falls back to the default
    /// (`Settings.tsx`), e.g. Custom emoji once no community is chosen.
    fn fall_back(&mut self, cx: &mut Context<Self>) {
        let has_community = AppState::current(cx).is_some();
        if !self.section.listed(has_community, cx) {
            self.open(Section::default_for(has_community), cx);
        }
        cx.notify();
    }

    fn show_toast(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.show_toast_tone(text, ToastTone::Success, cx);
    }

    fn show_toast_tone(
        &mut self,
        text: impl Into<SharedString>,
        tone: ToastTone,
        cx: &mut Context<Self>,
    ) {
        self.toast_tone = tone;
        self.toasts += 1;
        let generation = self.toasts;
        self.toast = Some((text.into(), generation));
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(TOAST_DURATION).await;
            let _ = this.update(cx, |this, cx| {
                if this.toast.as_ref().is_some_and(|(_, g)| *g == generation) {
                    this.toast = None;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    /// A dialog is open over the page.
    pub fn has_modal(&self) -> bool {
        self.modal.is_some()
    }

    fn close_modal(&mut self, cx: &mut Context<Self>) {
        self.modal = None;
        cx.notify();
    }

    /// Closes the sound menu, stops a preview and hides the private key,
    /// on leaving a section.
    fn quiet(&mut self, cx: &mut App) {
        self.sound_menu = None;
        self.retention_menu = false;
        crate::sounds::stop_preview();
        // Leaving Profile hides the private key, as unmounting it does.
        self.profile.hide_private_key(cx);
        // Leaving Shortcuts stops a capture and clears its notice.
        self.shortcuts = None;
        // Leaving a plugin card unmounts it.
        self.card = None;
    }

    /// The settings sidebar (§5b.2). The shell draws it in the navigation
    /// column, which sets its width and divider.
    pub(crate) fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let community = AppState::current(cx).map(|community| community.read(cx).name());
        let item = |section: Section, cx: &mut Context<Self>| {
            NavigationItem::new(
                SharedString::from(format!("settings-{}", section.label())),
                section.label(),
            )
            .sidebar()
            .leading(Icon::new(section.icon()).size(18.))
            .selected(self.section == section)
            .on_click(cx.listener(move |this, _, _, cx| this.open(section, cx)))
        };
        let group = |title: SharedString, items: Vec<NavigationItem>| {
            div()
                .flex()
                .flex_col()
                .child(
                    div()
                        .mb(rems(0.25))
                        .px(rems(0.5))
                        .text_caption()
                        .text_color(c.text_metadata)
                        .child(title),
                )
                .child(div().flex().flex_col().gap(rems(0.125)).children(items))
        };
        let card_item = |card: &cards::SettingsCard, cx: &mut Context<Self>| {
            let section = Section::Card(card.key);
            NavigationItem::new(
                SharedString::from(format!("settings-{}", card.key)),
                card.title,
            )
            .sidebar()
            .when_some(card.sidebar_icon(), |item, icon| {
                item.leading(Icon::new(icon).size(18.))
            })
            .selected(self.section == section)
            .on_click(cx.listener(move |this, _, _, cx| this.open(section, cx)))
        };
        let contributed = cards::visible(cx);
        let placed = |placement: cards::CardPlacement| {
            contributed
                .iter()
                .filter(move |card| card.placement == placement)
        };
        let mut groups = Vec::new();
        let mut app = Vec::new();
        match community {
            Some(name) => {
                let mut items = vec![item(Section::Profile, cx), item(Section::CustomEmoji, cx)];
                for card in placed(cards::CardPlacement::Community) {
                    items.push(card_item(card, cx));
                }
                groups.push(group(name.into(), items));
                let administration: Vec<_> = placed(cards::CardPlacement::Administration)
                    .map(|card| card_item(card, cx))
                    .collect();
                if !administration.is_empty() {
                    groups.push(group("Administration".into(), administration));
                }
            }
            None => app.push(item(Section::Profile, cx)),
        }
        groups.push(group(
            "Communities".into(),
            vec![item(Section::Communities, cx)],
        ));
        for label in cards::groups(&contributed) {
            let items: Vec<_> = placed(cards::CardPlacement::Group(label))
                .map(|card| card_item(card, cx))
                .collect();
            groups.push(group(label.into(), items));
        }

        for section in [
            Section::Appearance,
            Section::Notifications,
            Section::Shortcuts,
            Section::Agents,
            Section::Plugins,
            Section::Updates,
            Section::About,
        ] {
            app.push(item(section, cx));
        }
        groups.push(group("App".into(), app));
        div()
            .id("settings-sidebar")
            .flex()
            .flex_col()
            .size_full()
            .pt(rems(0.5))
            .px(rems(0.5))
            .overflow_y_scroll()
            .text_body_sm()
            .child(
                NavigationItem::new("settings-back", "Back")
                    .sidebar()
                    .leading(Icon::new(IconName::ArrowLeft).size(18.))
                    .on_click(|_, window, cx| window.dispatch_action(Box::new(CloseSettings), cx)),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(rems(1.))
                    .pt(rems(0.75))
                    .pb(rems(0.5))
                    .children(groups),
            )
    }

    fn detail(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        match self.section {
            Section::Profile => self.profile_section(window, cx),
            Section::CustomEmoji => {
                let (settings, _) = self.custom_emoji.get_or_insert_with(|| {
                    let settings = cx.new(|cx| emoji::CustomEmojiSettings::new(window, cx));
                    let added = cx.subscribe(&settings, |this, _, emoji::EmojiAdded(code), cx| {
                        this.show_toast(format!("Added :{code}:"), cx)
                    });
                    (settings, added)
                });
                settings.clone().into_any_element()
            }
            Section::Communities => self.communities_section(cx),
            Section::Appearance => appearance::render(window, cx),
            Section::Notifications => self.notifications_section(window, cx),
            Section::Shortcuts => self
                .shortcuts
                .get_or_insert_with(|| cx.new(|cx| shortcuts::ShortcutSettings::new(window, cx)))
                .clone()
                .into_any_element(),
            Section::Agents => self.agents_section(window, cx),
            Section::Plugins => self.plugins_section(cx),
            Section::Updates => self.updates_section(cx),
            Section::About => about::render(cx),
            Section::Card(key) => self.card_section(key, window, cx),
        }
    }

    /// A plugin card's view, kept while its section stays open.
    fn card_section(
        &mut self,
        key: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(card) = cards::get(key, cx) else {
            return div().into_any_element();
        };
        // An account card belongs to no community, so a switch keeps it.
        let community = AppState::current(cx)
            .map(|community| community.entity_id())
            .filter(|_| !matches!(card.placement, cards::CardPlacement::Group(_)));
        if let Some((open, made_for, view)) = &self.card
            && *open == key
            && *made_for == community
        {
            return view.clone().into_any_element();
        }
        let view = (card.build)(window, cx);
        self.card = Some((key, community, view.clone()));
        view.into_any_element()
    }

    fn modal(&self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let page = cx.entity().downgrade();
        let close: overlay::Dismiss = Rc::new(move |_, cx| {
            let _ = page.update(cx, |this, cx| this.close_modal(cx));
        });
        let (content, dismiss): (AnyElement, Option<overlay::Dismiss>) =
            match self.modal.as_ref()? {
                Modal::AddHost { dialog, .. } => (dialog.clone().into_any_element(), None),
                Modal::RemoveHost { host, error } => (
                    self.remove_host_dialog(host, error.clone(), cx),
                    Some(close),
                ),
                Modal::Leave {
                    community,
                    busy,
                    error,
                } => (
                    self.leave_dialog(community, *busy, error.clone(), cx),
                    (!*busy).then_some(close),
                ),
                Modal::SignOut { dialog, .. } => (
                    dialog.clone().into_any_element(),
                    (!dialog.read(cx).pending()).then_some(close),
                ),
                Modal::Archive(confirm) => (self.archive_dialog(*confirm, cx), Some(close)),
                Modal::PluginImport { dialog, .. } => (dialog.clone().into_any_element(), None),
                Modal::SignPlugin {
                    name, busy, error, ..
                } => (
                    self.sign_dialog(name, *busy, error.clone(), cx),
                    (!*busy).then_some(close),
                ),
            };
        Some(overlay::modal(window, content, dismiss).into_any_element())
    }
}

impl Render for SettingsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let detail = self.detail(window, cx);
        let modal = self.modal(window, cx);
        let toast = self.toast.as_ref().map(|(text, generation)| {
            crate::ui::toasts::place(
                window,
                Toast::new(("settings-toast", *generation), text.clone())
                    .tone(self.toast_tone)
                    .on_dismiss(cx.listener(|this, _, _, cx| {
                        this.toast = None;
                        cx.notify();
                    })),
            )
        });
        div()
            .flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .text_color(c.text_standard)
            .child(
                // `.detail`: 32 / 24 / 24 padding; every child is capped at
                // 768px and centered.
                div()
                    .id("settings-detail")
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .overflow_y_scroll()
                    .pt(rems(2.))
                    .px(rems(1.5))
                    .pb(rems(1.5))
                    .child(div().w_full().max_w(rems(48.)).mx_auto().child(detail)),
            )
            .children(modal)
            .children(toast)
    }
}

// ----- shared settings components (§5b.4) -----

/// The inset that lines titles up with group content (16 + 1px border).
fn inset() -> Rems {
    rems(1.0625)
}

/// The section `Header`: `text-heading` title, caption subtitle, 32px
/// below.
fn header(title: &'static str, subtitle: Option<SharedString>, cx: &App) -> Div {
    let c = cx.colors();
    div()
        .flex()
        .flex_col()
        .gap(rems(0.5))
        .px(inset())
        .child(div().text_heading().child(title))
        .children(subtitle.map(|text| div().text_caption().text_color(c.text_subtle).child(text)))
}

/// A subsection `InlineHeader`: `text-label-sm` title, caption subtitle,
/// actions on the right, 12px below.
fn inline_header(
    title: impl Into<SharedString>,
    subtitle: Option<SharedString>,
    actions: Vec<AnyElement>,
    cx: &App,
) -> Div {
    let c = cx.colors();
    div()
        .flex()
        .items_start()
        .gap(rems(1.))
        .mb(rems(0.75))
        .px(inset())
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .gap(rems(0.5))
                .child(div().text_label_sm().child(title.into()))
                .children(
                    subtitle.map(|text| div().text_caption().text_color(c.text_subtle).child(text)),
                ),
        )
        .child(div().flex().items_center().gap(rems(0.5)).children(actions))
}

/// `SettingsGroup layout="rows"`: an outlined card with a divider between
/// rows.
fn rows_group(rows: Vec<AnyElement>, cx: &App) -> Div {
    let c = cx.colors();
    let count = rows.len();
    div()
        .flex()
        .flex_col()
        .px(rems(1.))
        .border_1()
        .border_color(c.border_standard)
        .rounded(radius::CONTAINER)
        .children(rows.into_iter().enumerate().map(move |(index, row)| {
            div()
                .when(index + 1 < count, |this| {
                    this.border_b_1().border_color(c.border_standard)
                })
                .child(row)
        }))
}

/// `SettingsGroup layout="form"`: an outlined card with 16px gaps.
fn form_group(children: Vec<AnyElement>, cx: &App) -> Div {
    let c = cx.colors();
    div()
        .flex()
        .flex_col()
        .gap(rems(1.))
        .p(rems(1.))
        .border_1()
        .border_color(c.border_standard)
        .rounded(radius::CONTAINER)
        .children(children)
}

/// `PreferenceRow`: optional 20px icon, label and description, trailing
/// controls on the right.
fn preference_row(
    icon: Option<AnyElement>,
    title: impl Into<SharedString>,
    description: Option<AnyElement>,
    trailing: Vec<AnyElement>,
    cx: &App,
) -> Div {
    let c = cx.colors();
    div()
        .flex()
        .items_center()
        .gap(rems(1.))
        .py(rems(0.75))
        .children(icon.map(|icon| {
            div()
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .size(rems(1.25))
                .text_color(c.text_subtle)
                .child(icon)
        }))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .gap(rems(0.125))
                .child(div().text_label_sm().child(title.into()))
                .children(
                    description
                        .map(|text| div().text_caption().text_color(c.text_subtle).child(text)),
                ),
        )
        .child(
            div()
                .flex()
                .flex_none()
                .flex_wrap()
                .items_center()
                .justify_end()
                .gap(rems(0.5))
                .children(trailing),
        )
}

/// A `Field`: label, control, then the description and the error.
fn field(
    label: &'static str,
    control: impl IntoElement,
    description: Option<SharedString>,
    error: Option<SharedString>,
    cx: &App,
) -> Div {
    form::field(
        label,
        description,
        error,
        control,
        form::FieldStyle::default(),
        cx,
    )
}

/// Settings buttons use 12px corners instead of pills.
fn button(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Button {
    Button::new(id, label).shape(ButtonShape::Control)
}

/// A plain status line in place of a section's content.
fn note(text: impl Into<SharedString>, cx: &App) -> Div {
    let c = cx.colors();
    div()
        .px(inset())
        .text_body()
        .text_color(c.text_subtle)
        .child(text.into())
}

/// A bordered, centered card: a large icon, a title over a line of text,
/// then an optional action.
fn empty_card(
    icon: IconName,
    icon_color: Hsla,
    title: impl Into<SharedString>,
    text: impl IntoElement,
    action: Option<AnyElement>,
    cx: &App,
) -> Div {
    let c = cx.colors();
    div()
        .w_full()
        .border_1()
        .border_color(c.border_standard)
        .rounded(radius::CONTAINER)
        .flex()
        .flex_col()
        .items_center()
        .gap(rems(1.))
        .py(rems(2.))
        .px(rems(1.5))
        .text_center()
        .child(
            div()
                .flex()
                .text_color(icon_color)
                .child(Icon::new(icon).size(32.)),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap(rems(0.5))
                .max_w(rems(24.))
                .child(div().text_label_sm().child(title.into()))
                .child(text),
        )
        .children(action.map(|action| div().mt(rems(0.5)).child(action)))
}

fn section(children: Vec<AnyElement>) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(rems(2.))
        .children(children)
        .into_any_element()
}
