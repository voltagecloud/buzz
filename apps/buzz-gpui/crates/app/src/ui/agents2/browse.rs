//! Browse: the agents this community's members have shared, each
//! importable as a new agent (buzz-app's `AgentBrowse.tsx` and
//! `shared-agents.ts`). The catalog is read a page at a time; an
//! over-budget page is retried smaller.

use super::card::{self, AgentCard};
use crate::agents2::{Agents2, types};
use crate::community::{AppState, Community, IdentityState};
use crate::runtime::Runtime;
use crate::ui::overlay::{self, Align, Dismiss};
use buzz_relay_client::{HttpError, RelayHttp, Session};
use buzz_ui::{
    ActiveTheme, Avatar, AvatarSize, Brand, BrandMark, Button, ButtonVariant, Dialog, DialogSize,
    Icon, IconName, Menu, MenuItem, MenuLabel, Select, Surface, Tooltip, TypeStyled,
    tokens::{radius, space},
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use nostr::prelude::Event;
use regex::Regex;
use serde_json::{Map, Value, json};
use std::cmp::Ordering;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::{Arc, LazyLock};

/// NIP-AP agent definitions (`catalog-envelope.ts`).
pub const AGENT_CATALOG_KIND: u16 = 30175;
const PAGE_SIZE: usize = 200;
/// `avatarSource`'s bound on a remote picture URL.
const MAX_AVATAR_URL: usize = 2048;

/// The fields of a catalog event the list needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogEvent {
    pub id: String,
    pub pubkey: String,
    pub created_at: u64,
    pub kind: u16,
    pub tags: Vec<Vec<String>>,
    pub content: String,
}

impl From<&Event> for CatalogEvent {
    fn from(event: &Event) -> Self {
        Self {
            id: event.id.to_hex(),
            pubkey: event.pubkey.to_hex(),
            created_at: event.created_at.as_secs(),
            kind: event.kind.as_u16(),
            tags: event
                .tags
                .iter()
                .map(|tag| tag.as_slice().to_vec())
                .collect(),
            content: event.content.clone(),
        }
    }
}

/// One agent a member shares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SharedAgent {
    /// `owner:d`, unique per publisher and catalog slug.
    pub id: String,
    pub name: String,
    pub owner: String,
    pub picture: Option<String>,
    pub model: String,
    pub instructions: String,
}

/// The signer's admission rules for a catalog envelope
/// (`validCatalogEnvelope`); they do not depend on the community, so any
/// HTTPS origin selects them.
fn valid_envelope(event: &CatalogEvent) -> bool {
    let template = buzz_desktop::EventTemplate {
        created_at: event.created_at,
        kind: event.kind,
        tags: event.tags.clone(),
        content: event.content.clone(),
    };
    buzz_desktop::relay::events::validate_event("https://catalog.invalid", &template).is_ok()
}

/// `avatarSource`, keeping only HTTPS artwork without credentials.
fn https_picture(value: Option<&Value>) -> Option<String> {
    let source = value?.as_str()?.trim();
    if source.len() > MAX_AVATAR_URL {
        return None;
    }
    let url = url::Url::parse(source).ok()?;
    (url.scheme() == "https" && url.username().is_empty() && url.password().is_none())
        .then(|| url.to_string())
}

/// JavaScript's `localeCompare` in the root locale, approximately:
/// case-insensitive first, then lowercase before uppercase.
fn locale_compare(a: &str, b: &str) -> Ordering {
    a.to_lowercase()
        .cmp(&b.to_lowercase())
        .then_with(|| b.cmp(a))
}

/// The agents a community's catalog events currently share, by name.
/// NIP-33 picks the head per (owner, d) first, so an unshared or invalid
/// replacement hides older shared heads rather than reviving them.
pub fn shared_agents(events: &[CatalogEvent]) -> Vec<SharedAgent> {
    let mut sorted: Vec<&CatalogEvent> = events.iter().collect();
    sorted.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    let mut heads: Vec<(String, &CatalogEvent)> = Vec::new();
    let mut seen = HashSet::new();
    for event in sorted {
        let d: Vec<&Vec<String>> = event
            .tags
            .iter()
            .filter(|tag| tag.first().map(String::as_str) == Some("d"))
            .collect();
        let slug = match d.as_slice() {
            [tag] => tag.get(1).filter(|slug| !slug.is_empty()),
            _ => None,
        };
        let Some(slug) = slug.filter(|_| event.kind == AGENT_CATALOG_KIND) else {
            continue;
        };
        let key = format!("{}:{slug}", event.pubkey);
        if seen.insert(key.clone()) {
            heads.push((key, event));
        }
    }
    let mut agents: Vec<SharedAgent> = heads
        .into_iter()
        .filter_map(|(id, event)| {
            let body = serde_json::from_str::<Map<String, Value>>(&event.content).ok()?;
            let shared = event
                .tags
                .iter()
                .any(|tag| tag.first().map(String::as_str) == Some("shared"));
            if !shared || !valid_envelope(event) {
                return None;
            }
            let text = |key: &str| body.get(key).and_then(Value::as_str);
            Some(SharedAgent {
                id,
                name: text("display_name").unwrap_or_default().trim().to_owned(),
                owner: event.pubkey.clone(),
                picture: https_picture(body.get("avatar_url")),
                model: text("model").map(str::trim).unwrap_or_default().to_owned(),
                instructions: text("system_prompt").unwrap_or_default().to_owned(),
            })
        })
        .collect();
    agents.sort_by(|a, b| {
        locale_compare(&a.name, &b.name).then_with(|| locale_compare(&a.id, &b.id))
    });
    agents
}

/// A model's family, for its icon (`ModelIcon`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelFamily {
    Claude,
    OpenAi,
    Other,
}

static CLAUDE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("claude|anthropic|opus|sonnet|haiku").expect("valid regex"));
static OPENAI: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|[\s/._-])(?:gpt|codex|o[134](?:[\s/._-]|$))|openai").expect("valid regex")
});

pub fn model_family(model: &str) -> ModelFamily {
    let name = model.to_lowercase();
    if CLAUDE.is_match(&name) {
        ModelFamily::Claude
    } else if OPENAI.is_match(&name) {
        ModelFamily::OpenAi
    } else {
        ModelFamily::Other
    }
}

/// `ModelIcon`: the Claude or OpenAI mark, else the robot, in the
/// current text color.
pub(crate) fn model_icon(model: &str, size: f32, _: &App) -> AnyElement {
    match model_family(model) {
        ModelFamily::Claude => Brand::new(BrandMark::Claude).size(size).into_any_element(),
        ModelFamily::OpenAi => Brand::new(BrandMark::Codex).size(size).into_any_element(),
        ModelFamily::Other => Icon::new(IconName::Robot).size(size).into_any_element(),
    }
}

/// One catalog page at `cursor`, halving the limit while the relay's
/// answer exceeds the read budget; with the limit it was read at.
async fn read_page(
    http: &RelayHttp,
    cursor: Option<(u64, String)>,
) -> Result<(Vec<CatalogEvent>, usize), HttpError> {
    let mut limit = PAGE_SIZE;
    loop {
        let mut query = json!({ "kinds": [AGENT_CATALOG_KIND], "limit": limit });
        if let Some((until, before)) = &cursor {
            query["until"] = json!(until);
            query["before_id"] = json!(before);
        }
        let filter = query.as_object().cloned().unwrap_or_default();
        match http.query(&[filter]).await {
            Ok(page) => return Ok((page.iter().map(CatalogEvent::from).collect(), limit)),
            // Matches the community catalog's bounded retry at the same cursor.
            Err(HttpError::TooLarge) if limit > 1 => limit /= 2,
            Err(error) => return Err(error),
        }
    }
}

/// An import finished; the page shows Your agents.
pub enum BrowseEvent {
    Imported,
}

/// The import footer's state, reset for each agent opened.
#[derive(Default)]
struct Import {
    kind: Option<SharedString>,
    open: bool,
    pending: bool,
    error: Option<SharedString>,
}

pub struct AgentBrowse {
    /// The community browsed and its session; another starts over.
    community: Option<(Entity<Community>, Arc<Session>)>,
    events: Vec<CatalogEvent>,
    cursor: Option<(u64, String)>,
    more: bool,
    loading: bool,
    error: bool,
    selected: Option<SharedAgent>,
    import: Import,
    read: Option<Task<()>>,
    watch: Option<Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<BrowseEvent> for AgentBrowse {}

impl AgentBrowse {
    pub fn new(_: &mut Window, cx: &mut Context<Self>) -> Self {
        let subscriptions = vec![
            cx.observe_global::<AppState>(|this, cx| this.follow(cx)),
            cx.observe_global::<crate::agents2::registry::AgentTypes>(|_, cx| cx.notify()),
        ];
        let mut this = Self {
            community: None,
            events: Vec::new(),
            cursor: None,
            more: false,
            loading: false,
            error: false,
            selected: None,
            import: Import::default(),
            read: None,
            watch: None,
            _subscriptions: subscriptions,
        };
        this.follow(cx);
        this
    }

    /// Follows the selected community, starting over from the first page
    /// when it (or its session) changes.
    fn follow(&mut self, cx: &mut Context<Self>) {
        let state = cx.global::<AppState>();
        let ready = matches!(state.identity, IdentityState::Ready(_));
        let current = state
            .selected
            .and_then(|index| state.communities.get(index))
            .filter(|_| ready)
            .map(|community| (community.clone(), community.read(cx).session.clone()));
        let same = match (&self.community, &current) {
            (Some((a, x)), Some((b, y))) => a == b && Arc::ptr_eq(x, y),
            (None, None) => true,
            _ => false,
        };
        if same {
            return;
        }
        self.watch = current.as_ref().map(|(community, _)| {
            // Profiles arrive on the community; its session may be replaced.
            cx.observe(community, |this, _, cx| {
                this.follow(cx);
                cx.notify();
            })
        });
        self.community = current;
        self.events.clear();
        self.cursor = None;
        self.more = false;
        self.error = false;
        self.selected = None;
        self.import = Import::default();
        self.read = None;
        self.loading = false;
        self.load(cx);
        cx.notify();
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some((community, _)) = &self.community else {
            return;
        };
        let IdentityState::Ready(identity) = &cx.global::<AppState>().identity else {
            return;
        };
        let http = RelayHttp::new(community.read(cx).origin.clone(), identity.clone());
        let cursor = self.cursor.clone();
        self.loading = true;
        self.error = false;
        let read = Runtime::handle(cx).spawn(async move { read_page(&http, cursor).await });
        self.read = Some(cx.spawn(async move |this, cx| {
            let result = read.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(Ok((page, limit))) => {
                        this.more = page.len() == limit;
                        if this.cursor.is_some() {
                            this.events.extend(page);
                        } else {
                            this.events = page;
                        }
                    }
                    _ => this.error = true,
                }
                this.loading = false;
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn retry(&mut self, cx: &mut Context<Self>) {
        self.load(cx);
    }

    /// The next page, before the oldest event read so far.
    fn load_more(&mut self, cx: &mut Context<Self>) {
        let last = self.events.iter().min_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        if let Some(last) = last {
            self.cursor = Some((last.created_at, last.id.clone()));
            self.load(cx);
        }
    }

    fn open(&mut self, agent: SharedAgent, cx: &mut Context<Self>) {
        if let Some((community, session)) = &self.community
            && !community
                .read(cx)
                .snapshot
                .profiles
                .contains_key(&agent.owner)
        {
            session.want_profiles(vec![agent.owner.clone()]);
        }
        self.selected = Some(agent);
        self.import = Import::default();
        cx.notify();
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        self.selected = None;
        self.import = Import::default();
        cx.notify();
    }

    /// Imports the shared agent as a new agent of the chosen type, through
    /// the same path that imports an earlier system's agents.
    fn run_import(&mut self, kind: SharedString, cx: &mut Context<Self>) {
        let (Some(shared), Some(service)) = (self.selected.clone(), Agents2::global(cx)) else {
            return;
        };
        self.import.pending = true;
        self.import.error = None;
        let task = service.update(cx, |service, cx| {
            service.port_shared(
                &kind,
                &shared.name,
                shared.picture.clone(),
                &shared.instructions,
                cx,
            )
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(_) => {
                        if this.selected.as_ref().map(|s| &s.id) == Some(&shared.id) {
                            this.close(cx);
                        }
                        cx.emit(BrowseEvent::Imported);
                    }
                    Err(error) => {
                        if this.selected.as_ref().map(|s| &s.id) == Some(&shared.id) {
                            this.import.error = Some(error.into());
                            this.import.pending = false;
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// The community's `https://` origin, for pictures through its media.
    fn relay(&self, cx: &App) -> String {
        self.community
            .as_ref()
            .map(|(community, _)| community.read(cx).origin.http())
            .unwrap_or_default()
    }

    fn details(&self, agent: &SharedAgent, cx: &App) -> Div {
        let c = *cx.colors();
        let relay = self.relay(cx);
        let row = || div().flex().items_center().h(rems(1.75)).min_w_0();
        div()
            .flex()
            .flex_col()
            .gap(rems(1.25))
            .min_w_0()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(rems(1.25))
                    .min_w_0()
                    .child(div().flex_none().size(rems(6.)).child(card::portrait(
                        &agent.name,
                        agent.picture.as_deref(),
                        &relay,
                        6.,
                        cx,
                    )))
                    .child(
                        div()
                            .flex()
                            .gap(rems(1.))
                            .min_w_0()
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(rems(0.5))
                                    .text_color(c.text_subtle)
                                    .child(row().child("Model"))
                                    .child(row().child("Shared by")),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(rems(0.5))
                                    .min_w_0()
                                    .child(
                                        row()
                                            .gap(rems(0.5))
                                            .child(model_icon(&agent.model, 18., cx))
                                            .child(div().min_w_0().truncate().child(
                                                if agent.model.is_empty() {
                                                    "Not given".to_owned()
                                                } else {
                                                    agent.model.clone()
                                                },
                                            )),
                                    )
                                    .child(row().child(self.sharer(&agent.owner, cx))),
                            ),
                    ),
            )
            .child(
                div()
                    .id("shared-agent-instructions")
                    .max_h(rems(22.))
                    .overflow_y_scroll()
                    .p(rems(1.))
                    .rounded(radius::CONTAINER)
                    .bg(c.surface_inset)
                    .child(if agent.instructions.is_empty() {
                        "No instructions included.".to_owned()
                    } else {
                        agent.instructions.clone()
                    }),
            )
    }

    /// Who shared it, by their profile; the key itself is a hover away.
    fn sharer(&self, pubkey: &str, cx: &App) -> AnyElement {
        let community = self.community.as_ref().map(|(c, _)| c.read(cx));
        let profile = community.and_then(|c| c.snapshot.profiles.get(pubkey));
        let npub = crate::ui::keys::npub(pubkey);
        let name = match profile {
            Some(profile) if !profile.name_is_fallback => profile.name.clone(),
            _ => crate::ui::keys::format_public_key(pubkey).unwrap_or_else(|| pubkey.to_owned()),
        };
        let mut avatar = Avatar::new(name.clone()).size(AvatarSize::Small).alt("");
        if let (Some(picture), Some(community)) =
            (profile.and_then(|p| p.picture.as_deref()), community)
        {
            avatar = avatar.image(crate::ui::media::avatar(picture, community.session.media()));
        }
        div()
            .id("shared-agent-sharer")
            .flex()
            .items_center()
            .gap(rems(0.5))
            .min_w_0()
            .tooltip(Tooltip::text(npub))
            .child(avatar)
            .child(div().min_w_0().truncate().child(name))
            .into_any_element()
    }

    /// The footer: "Import as" when there is a choice, any error, and
    /// Import.
    fn import_footer(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let c = *cx.colors();
        let all = types(cx);
        let Some(chosen) = self
            .import
            .kind
            .as_ref()
            .and_then(|key| all.iter().find(|kind| &kind.key == key))
            .or(all.first())
        else {
            return div()
                .text_body_sm()
                .text_color(c.text_subtle)
                .child("Enable an agent type plugin to import agents.")
                .into_any_element();
        };
        let pending = self.import.pending;
        let key = chosen.key.clone();
        let select = (all.len() > 1).then(|| {
            let mut menu = Menu::new().child(MenuLabel::new("Agent types"));
            for (index, kind) in all.iter().enumerate() {
                let pick = kind.key.clone();
                menu = menu.child(
                    MenuItem::new(("shared-import-type", index), kind.kind.title())
                        .checked(kind.key == chosen.key)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.import.kind = Some(pick.clone());
                            this.import.open = false;
                            cx.notify();
                        })),
                );
            }
            let weak = cx.entity().downgrade();
            let dismiss: Dismiss = Rc::new(move |_, cx| {
                let _ = weak.update(cx, |this, cx| {
                    this.import.open = false;
                    cx.notify();
                });
            });
            let select = Select::new("shared-import-type-select")
                .value(chosen.kind.title())
                .surface(Surface::Floating)
                .disabled(pending)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.import.open = !this.import.open;
                    cx.notify();
                }));
            div()
                .flex()
                .items_center()
                .gap(rems(0.5))
                .child(div().text_body_sm().child("Import as"))
                .child(
                    overlay::select_menu(
                        select,
                        self.import.open,
                        Align::End,
                        Some(rems(16.)),
                        || menu.into_any_element(),
                        dismiss,
                        window,
                    )
                    .min_w(rems(10.)),
                )
        });
        div()
            .flex()
            .items_center()
            .justify_end()
            .gap(rems(0.75))
            .min_w_0()
            .children(
                self.import
                    .error
                    .clone()
                    .map(|error| div().text_body_sm().text_color(c.text_danger).child(error)),
            )
            .children(select)
            .child(
                Button::new("shared-import", "Import")
                    .variant(ButtonVariant::Prominent)
                    .loading(pending)
                    .disabled(pending)
                    .on_click(cx.listener(move |this, _, _, cx| this.run_import(key.clone(), cx))),
            )
            .into_any_element()
    }
}

impl Render for AgentBrowse {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.community.is_none() {
            return div()
                .child("Connect to a community to browse shared agents.")
                .into_any_element();
        }
        let agents = shared_agents(&self.events);
        let relay = self.relay(cx);
        let cards = agents.iter().enumerate().map(|(index, agent)| {
            let open = agent.clone();
            AgentCard::new(
                ("shared-agent", index),
                agent.name.clone(),
                agent.picture.clone(),
                relay.clone(),
            )
            .detail(
                div()
                    .flex()
                    .items_center()
                    .gap(rems(0.25))
                    .min_w_0()
                    .child(model_icon(&agent.model, 18., cx))
                    .child(div().min_w_0().truncate().child(if agent.model.is_empty() {
                        "Shared agent".to_owned()
                    } else {
                        agent.model.clone()
                    })),
            )
            .action(
                Button::new(("shared-agent-view", index), "View details")
                    .on_click(cx.listener(move |this, _, _, cx| this.open(open.clone(), cx))),
            )
        });
        let cards: Vec<AgentCard> = cards.collect();
        let dialog = self.selected.clone().map(|agent| {
            let weak = cx.entity().downgrade();
            let dismiss: Dismiss = Rc::new(move |_, cx| {
                let _ = weak.update(cx, |this, cx| this.close(cx));
            });
            let dialog = Dialog::new("shared-agent-details", agent.name.clone())
                .description(
                    "Import makes it your own new agent, with this name, picture and \
                     instructions. The model is yours to choose after.",
                )
                .size(DialogSize::Wide)
                .on_close(cx.listener(|this, _, _, cx| this.close(cx)))
                .child(self.details(&agent, cx))
                .footer(self.import_footer(window, cx));
            overlay::modal(window, dialog, Some(dismiss))
        });
        div()
            .flex()
            .flex_col()
            .gap(space::SECTION_GAP)
            .when(self.loading, |this| this.child("Loading shared agents…"))
            .when(self.error, |this| {
                this.child(
                    div()
                        .flex()
                        .flex_col()
                        .items_start()
                        .gap(rems(0.5))
                        .child("Couldn’t load shared agents.")
                        .child(
                            Button::new("shared-agents-retry", "Try again")
                                .on_click(cx.listener(|this, _, _, cx| this.retry(cx))),
                        ),
                )
            })
            .when(!self.loading && !self.error && agents.is_empty(), |this| {
                this.child("No shared agents found in this community yet.")
            })
            .child(card::grid(window).children(cards))
            .when(self.more && !self.error, |this| {
                this.child(
                    div().child(
                        Button::new("shared-agents-more", "Load more")
                            .disabled(self.loading)
                            .on_click(cx.listener(|this, _, _, cx| this.load_more(cx))),
                    ),
                )
            })
            .children(dialog)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    const OWNER: &str = "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd";

    fn shared_tags(slug: &str) -> Vec<Vec<String>> {
        vec![
            vec!["d".into(), slug.into()],
            vec!["shared".into(), "true".into()],
        ]
    }

    fn event(id: &str, created_at: u64, body: Value) -> CatalogEvent {
        event_with(id, created_at, body, shared_tags("helper"))
    }

    fn event_with(id: &str, created_at: u64, body: Value, tags: Vec<Vec<String>>) -> CatalogEvent {
        CatalogEvent {
            id: id.repeat(64),
            pubkey: OWNER.into(),
            created_at,
            kind: AGENT_CATALOG_KIND,
            tags,
            content: body.to_string(),
        }
    }

    fn helper() -> Value {
        json!({ "display_name": "Helper", "system_prompt": "Help." })
    }

    #[test]
    fn lists_the_newest_head_per_owner_and_slug() {
        let agents = shared_agents(&[
            event("a", 1, json!({ "display_name": "Old" })),
            event(
                "b",
                3,
                json!({ "display_name": "New", "model": " m ", "system_prompt": "Hi" }),
            ),
            event("c", 2, json!({ "display_name": "Middle" })),
        ]);
        assert_eq!(
            agents,
            vec![SharedAgent {
                id: format!("{OWNER}:helper"),
                name: "New".into(),
                owner: OWNER.into(),
                picture: None,
                model: "m".into(),
                instructions: "Hi".into(),
            }]
        );
    }

    #[test]
    fn breaks_created_at_ties_by_lowest_id() {
        let names: Vec<String> = shared_agents(&[
            event("b", 1, json!({ "display_name": "Second" })),
            event("a", 1, json!({ "display_name": "First" })),
        ])
        .into_iter()
        .map(|agent| agent.name)
        .collect();
        assert_eq!(names, ["First"]);
    }

    #[test]
    fn hides_older_shared_heads_behind_an_unshared_or_invalid_newer_head() {
        let older = event("a", 1, helper());
        let unshared = event_with("b", 2, helper(), vec![vec!["d".into(), "helper".into()]]);
        assert_eq!(shared_agents(&[older.clone(), unshared]), vec![]);
        let invalid = CatalogEvent {
            content: "invalid".into(),
            ..event("b", 2, helper())
        };
        assert_eq!(shared_agents(&[older, invalid]), vec![]);
    }

    #[test]
    fn skips_invalid_envelopes_and_other_kinds() {
        let base = event("a", 1, helper());
        let mut extra_shared = shared_tags("helper");
        extra_shared.push(vec!["shared".into(), "false".into()]);
        let two_slugs = vec![
            vec!["d".into(), "helper".into()],
            vec!["d".into(), "other".into()],
            vec!["shared".into(), "true".into()],
        ];
        let invalid = [
            CatalogEvent {
                tags: extra_shared,
                ..base.clone()
            },
            CatalogEvent {
                tags: two_slugs,
                ..base.clone()
            },
            CatalogEvent {
                tags: shared_tags("invalid slug"),
                ..base.clone()
            },
            CatalogEvent {
                content: json!({ "system_prompt": "No name" }).to_string(),
                ..base.clone()
            },
            CatalogEvent {
                content: json!({ "display_name": "Helper", "env_vars": {} }).to_string(),
                ..base.clone()
            },
            CatalogEvent {
                content: "x".repeat(65_536),
                ..base.clone()
            },
            CatalogEvent {
                kind: 30177,
                ..base.clone()
            },
        ];
        for event in invalid {
            assert_eq!(
                shared_agents(std::slice::from_ref(&event)),
                vec![],
                "{event:?}"
            );
        }
        assert_eq!(shared_agents(&[base]).len(), 1);
    }

    #[test]
    fn keeps_only_https_artwork() {
        let picture = |url: &str| {
            shared_agents(&[event(
                "a",
                1,
                json!({ "display_name": "Helper", "avatar_url": url }),
            )])[0]
                .picture
                .clone()
        };
        assert_eq!(
            picture("https://example.com/a.png").as_deref(),
            Some("https://example.com/a.png")
        );
        assert_eq!(picture("javascript:alert(1)"), None);
        assert_eq!(picture("data:image/png;base64,AAAA"), None);
    }

    #[test]
    fn sorts_by_name_then_id_and_separates_publishers() {
        let named = |id: &str, name: &str| {
            event_with(
                id,
                1,
                json!({ "display_name": name }),
                shared_tags(&name.to_lowercase()),
            )
        };
        let other = CatalogEvent {
            pubkey: "ef".repeat(32),
            ..named("e", "Alpha")
        };
        let names: Vec<String> = shared_agents(&[named("a", "Zed"), named("b", "Alpha"), other])
            .into_iter()
            .map(|agent| format!("{}:{}", agent.name, &agent.owner[..2]))
            .collect();
        assert_eq!(names, ["Alpha:cd", "Alpha:ef", "Zed:cd"]);
    }

    #[test]
    fn picks_the_icon_by_model_family() {
        assert_eq!(model_family("claude-sonnet-4-5"), ModelFamily::Claude);
        assert_eq!(model_family("Anthropic/Opus"), ModelFamily::Claude);
        assert_eq!(model_family("gpt-5"), ModelFamily::OpenAi);
        assert_eq!(model_family("openai/o3"), ModelFamily::OpenAi);
        assert_eq!(model_family("o4-mini"), ModelFamily::OpenAi);
        assert_eq!(model_family("gpt-5-codex"), ModelFamily::OpenAi);
        assert_eq!(model_family("llama-3"), ModelFamily::Other);
        assert_eq!(model_family("solo3"), ModelFamily::Other);
        assert_eq!(model_family(""), ModelFamily::Other);
    }
}
