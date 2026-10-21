//! A plugin's panel or page: the element tree it sent, drawn with the
//! app's components. Fields keep their text here, so typing does not wait
//! on the plugin; each change and Enter is sent to it as an event.

use super::draw::{self, Draw, FieldState};
use super::protocol::Node;
use super::{ExternalPlugins, Status};
use crate::agents2::registry::AgentView;
use crate::community::{AppState, Community};
use crate::ui::account_actions::Close;
use crate::ui::channel_panels::Opening;
use buzz_ui::{ActiveTheme, Button, IconName, Spinner, TypeStyled, tokens::space};
use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

struct Field {
    state: FieldState,
    /// The tree's `version` when its value was last put in.
    version: u64,
    clear_on_submit: bool,
    _subscription: Subscription,
}

pub struct ExternalView {
    plugin: String,
    contribution: String,
    /// `panel`, `page` or `settings`.
    kind: &'static str,
    id: String,
    /// A panel's opening; `None` for a page.
    opening: Option<Opening>,
    /// An agent tab's agent, and its type's id.
    agent: Option<(AgentView, &'static str)>,
    /// More `view/open` params, such as an accessory's channel.
    extra: Value,
    /// Closes a dialog (`ui/closeView`).
    close: Option<crate::ui::account_actions::Close>,
    /// Picks a reaction (`reaction/select`).
    select: Option<crate::ui::emoji_picker::Select>,
    /// The community a reaction picker reacts in.
    reacting_in: Option<WeakEntity<Community>>,
    /// Sets a new agent's config (`agent/createConfig`).
    change: Option<crate::agents2::registry::CreateChange>,
    /// Ends a composer capture (`composer/accept`).
    accept: Option<Accept>,
    /// A page's route params, from `ui/navigate`.
    route: Option<Value>,
    /// Whether the plugin has heard `view/open` for it yet.
    opened: bool,
    /// A panel's earlier targets, newest last, which Back returns to
    /// (`PanelContext.push`).
    back: Vec<String>,
    /// A `workspace` page: no padding around the plugin's tree.
    edge_to_edge: bool,
    tree: Option<Node>,
    /// The cached content of a framed view.
    cached: Option<Entity<Cached>>,
    /// The last tree the plugin sent could not be drawn.
    error: Option<String>,
    fields: HashMap<String, Field>,
    /// Buzz's composers and threads the tree embeds, by element id, with
    /// what they were made for.
    embeds: HashMap<String, (String, AnyView)>,
    /// Embedded composers, by element id, with their event subscription
    /// and the draft last put in.
    composers: HashMap<
        String,
        (
            Entity<crate::ui::composer::Composer>,
            Subscription,
            Option<String>,
        ),
    >,
    /// Drafts the plugin took (`submit: "plugin"`) and has not settled,
    /// by element id.
    unsettled: HashMap<String, crate::ui::composer::Submission>,
    _release: Subscription,
}

impl ExternalView {
    pub fn panel(
        plugin: &str,
        contribution: &str,
        opening: Opening,
        edge_to_edge: bool,
        _window: &mut Window,
        cx: &mut App,
    ) -> AnyView {
        Self::open(
            plugin,
            contribution,
            "panel",
            |view| {
                view.opening = Some(opening);
                view.edge_to_edge = edge_to_edge;
            },
            cx,
        )
        .into()
    }

    pub fn page(
        plugin: &str,
        contribution: &str,
        edge_to_edge: bool,
        _window: &mut Window,
        cx: &mut App,
    ) -> AnyView {
        Self::open(
            plugin,
            contribution,
            "page",
            |view| view.edge_to_edge = edge_to_edge,
            cx,
        )
        .into()
    }

    /// A Settings section the plugin added.
    pub fn settings(
        plugin: &str,
        contribution: &str,
        _window: &mut Window,
        cx: &mut App,
    ) -> AnyView {
        // The section belongs to the selected community, if any; an
        // account card (`group`) to none.
        let account = crate::ui::settings::cards::get(&format!("{plugin}/{contribution}"), cx)
            .is_some_and(|card| {
                matches!(
                    card.placement,
                    crate::ui::settings::cards::CardPlacement::Group(_)
                )
            });
        let community = crate::community::AppState::current(cx)
            .filter(|_| !account)
            .map(|community| community.read(cx).origin.websocket().to_string());
        let extra = json!({ "community": community });
        Self::open(
            plugin,
            contribution,
            "settings",
            |view| view.extra = extra,
            cx,
        )
        .into()
    }

    /// A settings tab of one of the plugin's agent types.
    pub fn agent_tab(
        plugin: &str,
        kind: &'static str,
        tab: &str,
        agent: AgentView,
        _window: &mut Window,
        cx: &mut App,
    ) -> AnyView {
        Self::open(
            plugin,
            tab,
            "agentTab",
            |view| view.agent = Some((agent, kind)),
            cx,
        )
        .into()
    }

    /// An account action's dialog, titled `title`, with the plugin's view
    /// as its body.
    pub fn dialog(
        plugin: &str,
        contribution: &str,
        title: SharedString,
        close: Close,
        _window: &mut Window,
        cx: &mut App,
    ) -> AnyView {
        Self::framed(
            plugin,
            contribution,
            title,
            DialogSize::Default,
            close,
            Value::Null,
            cx,
        )
        .0
    }

    /// A dialog titled `title` around a plugin view of kind `dialog`, with
    /// `extra` in its `view/open`; and the view's id.
    pub(super) fn framed(
        plugin: &str,
        contribution: &str,
        title: SharedString,
        size: DialogSize,
        close: Close,
        extra: Value,
        cx: &mut App,
    ) -> (AnyView, String) {
        let inner = close.clone();
        let view = Self::open(
            plugin,
            contribution,
            "dialog",
            |view| {
                view.close = Some(inner);
                view.extra = extra;
            },
            cx,
        );
        let id = view.read(cx).id.clone();
        (
            cx.new(|cx| Framed {
                title,
                size,
                view,
                close,
                focus: cx.focus_handle(),
            })
            .into(),
            id,
        )
    }

    /// A composer accessory, shown above one composer.
    pub fn accessory(
        plugin: &str,
        contribution: &str,
        community: &Entity<Community>,
        channel: &str,
        thread: Option<&str>,
        _window: &mut Window,
        cx: &mut App,
    ) -> AnyView {
        let extra = json!({
            "community": community.read(cx).origin.websocket(),
            "channel": channel,
            "thread": thread,
        });
        Self::open(
            plugin,
            contribution,
            "accessory",
            |view| view.extra = extra,
            cx,
        )
        .into()
    }

    /// A plugin agent type's part of the New agent dialog.
    pub fn agent_create(
        plugin: &str,
        kind: &'static str,
        config: &Value,
        change: crate::agents2::registry::CreateChange,
        _window: &mut Window,
        cx: &mut App,
    ) -> AnyView {
        let extra = json!({ "type": kind, "config": config, "disabled": false });
        Self::open(
            plugin,
            kind,
            "agentCreate",
            |view| {
                view.extra = extra;
                view.change = Some(change);
            },
            cx,
        )
        .into()
    }

    /// The New agent dialog's config or its creating state changed
    /// (`Create`'s `config` and `disabled` props): an `agentCreate` view
    /// hears `view/agentCreate`, and a reopened one starts from it.
    pub fn agent_create_changed(view: &AnyView, config: &Value, disabled: bool, cx: &mut App) {
        let Ok(view) = view.clone().downcast::<Self>() else {
            return;
        };
        let changed = view.update(cx, |view, _| {
            let Value::Object(extra) = &mut view.extra else {
                return None;
            };
            if extra.get("config") == Some(config)
                && extra.get("disabled") == Some(&Value::Bool(disabled))
            {
                return None;
            }
            extra.insert("config".into(), config.clone());
            extra.insert("disabled".into(), Value::Bool(disabled));
            Some((view.plugin.clone(), view.id.clone()))
        });
        if let Some((plugin, id)) = changed {
            ExternalPlugins::notify_running(
                &plugin,
                "view/agentCreate",
                json!({ "view": id, "config": config, "disabled": disabled }),
                cx,
            );
        }
    }

    /// The agent an `agentTab` view shows changed (its `agent` prop): it
    /// hears `view/agent`, and a reopened one starts from it.
    pub fn agent_changed(view: &AnyView, agent: &crate::agents2::Agent, cx: &mut App) {
        let Ok(view) = view.clone().downcast::<Self>() else {
            return;
        };
        let changed = view.update(cx, |view, _| match &mut view.agent {
            Some((shown, _)) if shown.agent != *agent => {
                shown.agent = agent.clone();
                Some((view.plugin.clone(), view.id.clone()))
            }
            _ => None,
        });
        if let Some((plugin, id)) = changed {
            ExternalPlugins::notify_running(
                &plugin,
                "view/agent",
                json!({ "view": id, "agent": super::agents::agent_json(agent) }),
                cx,
            );
        }
    }

    /// A composer capture: the plugin's view in place of one composer's
    /// editor.
    pub(super) fn capture(
        plugin: &str,
        composer: u64,
        accept: Accept,
        cx: &mut App,
    ) -> (Entity<Self>, String) {
        let extra = json!({ "composer": composer });
        let view = Self::open(
            plugin,
            "capture",
            "capture",
            |view| {
                view.extra = extra;
                view.accept = Some(accept);
            },
            cx,
        );
        let id = view.read(cx).id.clone();
        (view, id)
    }

    /// Keeps `route` as the page's route, as its next `view/open` says.
    /// The composer a capture or popover belongs to.
    pub(super) fn composer_id(&self) -> Option<u64> {
        self.extra.get("composer").and_then(Value::as_u64)
    }

    /// It forgets its route, which a page at another version does not
    /// take (`routeVersion`).
    pub(super) fn clear_route(&mut self) {
        self.route = None;
    }

    pub(super) fn set_route(&mut self, route: Value) {
        self.route = Some(route);
    }

    /// What `composer/accept` calls: a capture's end.
    pub(super) fn acceptor(&self) -> Option<Accept> {
        self.accept.clone()
    }

    /// What `agent/createConfig` calls: a create form's change.
    pub(super) fn changer(&self) -> Option<crate::agents2::registry::CreateChange> {
        self.change.clone()
    }

    /// A plugin agent type's provider dialog body.
    pub fn agent_provider(
        plugin: &str,
        kind: &'static str,
        close: Close,
        _window: &mut Window,
        cx: &mut App,
    ) -> AnyView {
        let extra = json!({ "type": kind });
        Self::open(
            plugin,
            kind,
            "agentProvider",
            |view| {
                view.extra = extra;
                view.close = Some(close);
            },
            cx,
        )
        .into()
    }

    /// A composer tool's popover, anchored to its button
    /// (`ComposerTool.component`'s own popup).
    pub(super) fn tool_popover(
        plugin: &str,
        contribution: &str,
        close: Close,
        extra: Value,
        cx: &mut App,
    ) -> Entity<Self> {
        Self::open(
            plugin,
            contribution,
            "toolPopover",
            |view| {
                view.close = Some(close);
                view.extra = extra;
            },
            cx,
        )
    }

    /// A reaction picker, standing in for the emoji picker.
    pub fn reaction_picker(
        plugin: &str,
        contribution: &str,
        select: crate::ui::emoji_picker::Select,
        community: &Entity<Community>,
        _window: &mut Window,
        cx: &mut App,
    ) -> AnyView {
        // Where the reaction goes (`ReactionToolProps.scope`), and whether
        // it can be sent now: not while only the saved copy shows.
        let weak = community.downgrade();
        let community = community.read(cx);
        let extra = json!({
            "community": community.origin.websocket(),
            "disabled": community.snapshot.cached,
        });
        Self::open(
            plugin,
            contribution,
            "reactionPicker",
            |view| {
                view.select = Some(select);
                view.reacting_in = Some(weak);
                view.extra = extra;
            },
            cx,
        )
        .into()
    }

    /// Whether a reaction picker cannot react now: its community shows only
    /// its saved copy, or is gone (`ReactionToolProps.disabled`).
    pub(super) fn reacting_disabled(&self, cx: &App) -> bool {
        self.reacting_in
            .as_ref()
            .and_then(WeakEntity::upgrade)
            .is_none_or(|community| community.read(cx).snapshot.cached)
    }

    /// What `reaction/select` calls: a reaction picker's pick.
    pub(super) fn selector(&self) -> Option<crate::ui::emoji_picker::Select> {
        self.select.clone()
    }

    /// What `ui/closeView` calls: a dialog's close; `None` for other
    /// views.
    pub(super) fn closer(&self) -> Option<Close> {
        self.close
            .clone()
            .or_else(|| self.opening.as_ref().map(|opening| opening.close.clone()))
    }

    /// `panel/open` and `panel/push`: shows `target` in this panel, or,
    /// when another panel shows it, opens that one as a tab for the same
    /// conversation. `open` replaces this panel; `push` keeps it, and
    /// within this panel lets Back return. False for a view that is not
    /// a panel, or a target no panel shows.
    pub(super) fn open_target(this: &Entity<Self>, target: &str, push: bool, cx: &mut App) -> bool {
        let view = this.read(cx);
        let Some(opening) = &view.opening else {
            return false;
        };
        // Only the panel the user sees acts: a background tab answers
        // `false` (`panelActive`).
        let own = format!("{}/{}", view.plugin, view.contribution);
        let tab = format!("{own}:{}", opening.target);
        if crate::ui::navigation::workspace(cx).is_some_and(|workspace| {
            workspace
                .read(cx)
                .panel_tab_inactive(&tab, this.entity_id(), cx)
        }) {
            return false;
        }
        let Some(panel) = crate::ui::channel_panels::resolve(target, cx) else {
            return false;
        };
        if panel.key == own {
            this.update(cx, |view, cx| view.retarget(target.to_owned(), push, cx));
            return true;
        }
        // A push opens for the conversation alone, without the thread
        // (`push`: `{channelId, panel, target}`).
        // A panel beside the page has no conversation to pass on.
        let channel = Some(opening.channel.clone()).filter(|channel| !channel.is_empty());
        let root = (!push).then(|| opening.root.clone()).flatten();
        let close = (!push).then(|| opening.close.clone());
        let (key, target) = (panel.key, target.to_owned());
        crate::ui::navigation::with_workspace(cx, move |workspace, window, cx| {
            let opened = if close.is_some() {
                workspace.open_panel_replacing(key, &target, channel, root, &tab, window, cx)
            } else {
                workspace.open_panel_at(Some(key), &target, channel, root, window, cx)
            };
            if opened && let Some(close) = close {
                // A drawer's close updates the workspace, which is busy.
                window.defer(cx, move |window, cx| close(window, cx));
            }
        });
        true
    }

    /// Shows `target` in this panel, keeping the one showing for Back
    /// when `push`; the plugin hears `view/retarget`.
    fn retarget(&mut self, target: String, push: bool, cx: &mut Context<Self>) {
        let Some(opening) = &mut self.opening else {
            return;
        };
        let old = std::mem::replace(&mut opening.target, target.clone());
        // Its tab, if it is one, follows.
        let (panel, view) = (
            format!("{}/{}", self.plugin, self.contribution),
            cx.entity_id(),
        );
        let (from, to) = (old.clone(), target.clone());
        crate::ui::navigation::with_workspace(cx, move |workspace, _, cx| {
            workspace.retarget_panel(&panel, view, &from, &to, cx)
        });
        if push {
            self.back.push(old);
            self.back.truncate(64);
        }
        ExternalPlugins::notify_running(
            &self.plugin,
            "view/retarget",
            json!({ "view": self.id, "target": target, "canGoBack": !self.back.is_empty() }),
            cx,
        );
        cx.notify();
    }

    /// Back: the target shown before the last `panel/push`.
    fn go_back(&mut self, cx: &mut Context<Self>) {
        if let Some(target) = self.back.pop() {
            self.retarget(target, false, cx);
        }
    }

    /// `agent/save`: replaces this tab's agent's config.
    pub(super) fn save_agent(
        &self,
        config: Value,
        cx: &mut App,
    ) -> Option<Task<Result<(), String>>> {
        let (agent, _) = self.agent.as_ref()?;
        Some(agent.save(config, cx))
    }

    /// A view of `kind`, which `setup` fills in before the plugin hears of
    /// it.
    fn open(
        plugin: &str,
        contribution: &str,
        kind: &'static str,
        setup: impl FnOnce(&mut Self),
        cx: &mut App,
    ) -> Entity<Self> {
        let id = ExternalPlugins::next_view_id(cx);
        let view = cx.new(|cx| {
            let mut view = Self {
                plugin: plugin.to_owned(),
                contribution: contribution.to_owned(),
                kind,
                id: id.clone(),
                opening: None,
                agent: None,
                extra: Value::Null,
                close: None,
                select: None,
                reacting_in: None,
                change: None,
                accept: None,
                route: None,
                opened: false,
                back: Vec::new(),
                edge_to_edge: false,
                tree: None,
                cached: None,
                error: None,
                fields: HashMap::new(),
                embeds: HashMap::new(),
                composers: HashMap::new(),
                unsettled: HashMap::new(),
                _release: cx.on_release(|this: &mut Self, cx| {
                    ExternalPlugins::close_view(&this.plugin, &this.id, cx);
                }),
            };
            setup(&mut view);
            view
        });
        // A page hears `view/open` once the visit has handed it its
        // route, so the route is in it.
        let weak = view.downgrade();
        let (plugin, deferred) = (plugin.to_owned(), kind == "page");
        let open = move |cx: &mut App| {
            let Some(view) = weak.upgrade() else {
                return;
            };
            view.update(cx, |view, _| view.opened = true);
            let params = view.read(cx).open_params(cx);
            ExternalPlugins::open_view(&plugin, &id, weak, params, cx);
        };
        if deferred {
            cx.defer(open);
        } else {
            open(cx);
        }
        view
    }

    /// `view/open`'s params.
    /// Its id, as plugins name it.
    pub(super) fn view_id(&self) -> &str {
        &self.id
    }

    /// The contribution it shows, as `plugin/id`.
    pub(super) fn contribution_key(&self) -> String {
        format!("{}/{}", self.plugin, self.contribution)
    }

    /// What it is: `page`, `panel`, `dialog` and so on.
    pub(super) fn kind(&self) -> &'static str {
        self.kind
    }

    pub(super) fn open_params(&self, cx: &App) -> Value {
        let opening = self.opening.as_ref();
        let (agent, kind, conversation, pending) = match &self.agent {
            Some((view, kind)) => (
                Some(super::agents::agent_json(&view.agent)),
                Some(*kind),
                view.conversation.as_ref().map(|conversation| {
                    json!({ "channel": conversation.channel_id, "root": conversation.root_id })
                }),
                // Only Settings, with the Provider field, opens staged.
                view.pending()
                    && view
                        .settings_host
                        .as_ref()
                        .is_some_and(|host| host.type_control.is_some()),
            ),
            None => (None, None, None, false),
        };
        let mut params = json!({
            "conversation": conversation,
            "pending": pending,
            "route": self.route,
            "agent": agent,
            "type": kind,
            "view": self.id,
            "contribution": self.contribution,
            "kind": self.kind,
            "target": opening.map(|opening| opening.target.clone()),
            // None beside a page (`PanelCard` has no `context`).
            "channel": opening
                .map(|opening| opening.channel.clone())
                .filter(|channel| !channel.is_empty()),
            "thread": opening.and_then(|opening| opening.thread.clone()),
            // The thread of the message the opening link was in
            // (`PanelProps.context.rootId`).
            "root": opening.and_then(|opening| opening.root.clone()),
        });
        // The rest of `ChannelPanelContext`: the panel's community, which
        // may not be the selected one, who you are there, and the channel.
        // A panel beside a page has none of it (`channelContext` comes only
        // with a channel presentation).
        if let Some(opening) = opening.filter(|opening| !opening.channel.is_empty()) {
            let community = opening.community.read(cx);
            params["community"] = json!(community.origin.websocket());
            params["viewer"] = json!(community.session.viewer().to_string());
            // ChannelPanelContext's `scope`: the relay and you.
            params["scope"] = json!(format!(
                "{}#{}",
                community.origin.websocket(),
                community.session.viewer()
            ));
            params["channelName"] = json!(
                community
                    .snapshot
                    .channels
                    .iter()
                    .find(|channel| channel.id == opening.channel)
                    .map(|channel| channel.name.clone())
            );
        }
        // The visit that opened a page (`PageNavigation.entryId`, `focus`).
        if self.kind == "page" {
            let (entry, keeps_focus) = crate::ui::workspace::current_visit();
            params["entry"] = json!(entry);
            params["focus"] = json!(if keeps_focus { "preserve" } else { "content" });
        }
        if let Value::Object(extra) = &self.extra {
            for (key, value) in extra {
                params[key] = value.clone();
            }
        }
        params
    }

    pub(super) fn set_tree(&mut self, tree: Result<Node, String>, cx: &mut Context<Self>) {
        match tree {
            Ok(tree) => {
                self.tree = Some(tree);
                self.error = None;
            }
            Err(reason) => {
                tracing::warn!(plugin = %self.plugin, %reason, "plugin view not drawn");
                self.error = Some(reason);
            }
        }
        cx.notify();
    }

    fn community(&self, cx: &App) -> Option<Entity<Community>> {
        match &self.opening {
            Some(opening) => Some(opening.community.clone()),
            None => AppState::current(cx),
        }
    }

    /// After a restart, a draft the old run took with `submit` is handed
    /// back unsent, so its composer unlocks as a remount does.
    pub(super) fn release_unsettled(&mut self, cx: &mut Context<Self>) {
        for (element, submission) in std::mem::take(&mut self.unsettled) {
            let handed_back = submission.settle(&Err("Not sent".to_owned()), cx);
            if let Some((composer, _, _)) = self.composers.get(&element)
                && !handed_back
            {
                composer.update(cx, |composer, cx| composer.failed(cx));
            }
        }
    }

    /// After a restart, each embedded composer tells the new run its id
    /// again (`ready`), as a remount does.
    pub(super) fn announce_composers(&self, cx: &App) {
        for (id, (composer, _, _)) in &self.composers {
            let number = super::contribute::composer_id(&self.plugin, composer);
            self.event(id, "ready", Some(json!({ "composer": number })), cx);
        }
    }

    fn event(&self, id: &str, event: &str, value: Option<Value>, cx: &App) {
        let mut params = json!({ "view": self.id, "id": id, "event": event });
        if let Some(value) = value {
            params["value"] = value;
        }
        ExternalPlugins::send_event(&self.plugin, params, cx);
    }

    /// The field `id`, made on first sight; `true` when just made.
    fn field(
        &mut self,
        id: &str,
        placeholder: Option<&String>,
        area: Option<(usize, usize)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (&mut Field, bool) {
        // A field that changed between one line and an area starts over.
        let kind_changed = self
            .fields
            .get(id)
            .is_some_and(|field| matches!(field.state, FieldState::Area(_)) != area.is_some());
        if kind_changed {
            self.fields.remove(id);
        }
        let created = !self.fields.contains_key(id);
        if created {
            let field_id = id.to_owned();
            let on_event = move |this: &mut Self,
                                 text: String,
                                 event: &InputEvent,
                                 window: &mut Window,
                                 cx: &mut Context<Self>| {
                match event {
                    InputEvent::Change => this.event(&field_id, "change", Some(json!(text)), cx),
                    InputEvent::PressEnter { .. } => {
                        this.event(&field_id, "submit", Some(json!(text)), cx);
                        if let Some(field) = this
                            .fields
                            .get(&field_id)
                            .filter(|field| field.clear_on_submit)
                        {
                            field.state.clone().set_text(String::new(), window, cx);
                        }
                    }
                    _ => {}
                }
            };
            let (state, subscription) = match area {
                None => {
                    let state = cx.new(|cx| {
                        let state = InputState::new(window, cx);
                        match placeholder {
                            Some(placeholder) => state.placeholder(placeholder.clone()),
                            None => state,
                        }
                    });
                    let subscription = cx.subscribe_in(
                        &state,
                        window,
                        move |this, state, event: &InputEvent, window, cx| {
                            let text = state.read(cx).value().to_string();
                            on_event(this, text, event, window, cx)
                        },
                    );
                    (FieldState::Line(state), subscription)
                }
                Some((least, most)) => {
                    let state = cx.new(|cx| {
                        let state = TextareaState::new(window, cx).auto_grow(least, most);
                        match placeholder {
                            Some(placeholder) => state.placeholder(placeholder.clone()),
                            None => state,
                        }
                    });
                    let subscription = cx.subscribe_in(
                        &state,
                        window,
                        move |this, state, event: &InputEvent, window, cx| {
                            let text = state.read(cx).value().to_string();
                            on_event(this, text, event, window, cx)
                        },
                    );
                    (FieldState::Area(state), subscription)
                }
            };
            self.fields.insert(
                id.to_owned(),
                Field {
                    state,
                    version: 0,
                    clear_on_submit: false,
                    _subscription: subscription,
                },
            );
        }
        (self.fields.get_mut(id).expect("just added"), created)
    }

    /// Makes the composers and threads the tree names, keeps those it
    /// still names for the same channel, and drops the rest.
    fn sync_embeds(&mut self, tree: &Node, window: &mut Window, cx: &mut Context<Self>) {
        let Some(community) = self.community(cx) else {
            self.embeds.clear();
            return;
        };
        let mut seen = HashSet::new();
        for embed in draw::embeds(tree) {
            let (id, made_for) = match &embed {
                draw::EmbedNode::Composer {
                    id,
                    channel,
                    thread,
                    parent,
                    plugin_submits,
                    disabled,
                    draft,
                    draft_key,
                    invite_agents,
                    ..
                } => {
                    // Disabling, `inviteAgents` and a new draft change the
                    // composer made already.
                    if let Some((composer, _, last)) = self.composers.get_mut(*id) {
                        let (can_send, invite) = (!disabled, *invite_agents);
                        composer.update(cx, |composer, cx| {
                            composer.set_can_send(can_send, cx);
                            composer.set_invite_agents(invite);
                        });
                        if let Some(draft) = draft.filter(|draft| last.as_deref() != Some(*draft)) {
                            *last = Some(draft.to_owned());
                            let input = composer.read(cx).input();
                            input.update(cx, |input, cx| {
                                input.set_value(draft.to_owned(), window, cx)
                            });
                        }
                    }
                    (
                        *id,
                        format!(
                            "composer:{channel}:{}:{}:{plugin_submits}:{}",
                            thread.unwrap_or_default(),
                            parent.unwrap_or_default(),
                            draft_key.unwrap_or_default()
                        ),
                    )
                }
                draw::EmbedNode::Thread { id, channel, root } => {
                    (*id, format!("thread:{channel}:{root}"))
                }
                draw::EmbedNode::Message {
                    id,
                    channel,
                    message,
                    style,
                } => (*id, format!("message:{channel}:{message}:{style:?}")),
            };
            seen.insert(id.to_owned());
            if self
                .embeds
                .get(id)
                .is_some_and(|(made, _)| *made == made_for)
            {
                continue;
            }
            // Only channels you can read.
            let readable = |channel: &str| {
                community
                    .read(cx)
                    .snapshot
                    .channels
                    .iter()
                    .any(|known| known.id == channel)
            };
            let view: Option<AnyView> = match embed {
                draw::EmbedNode::Composer {
                    id,
                    channel,
                    thread,
                    parent,
                    placeholder,
                    plugin_submits,
                    disabled,
                    auto_focus,
                    draft,
                    mentions,
                    invite_agents,
                    draft_key,
                } if readable(channel) || (channel.is_empty() && plugin_submits) => {
                    // Without a channel, the draft lives under a key of the
                    // plugin's, as New message and Me keep theirs.
                    let detached = channel.is_empty();
                    let channel = if detached {
                        format!("plugin:{}:{}", self.plugin, draft_key.unwrap_or(id))
                    } else {
                        channel.to_owned()
                    };
                    let channel = channel.as_str();
                    let event_id = |hex: Option<&str>| {
                        hex.and_then(|hex| buzz_relay_client::EventId::from_hex(hex).ok())
                    };
                    let root = event_id(thread);
                    let reply = root.map(|root| (root, event_id(parent).unwrap_or(root)));
                    let placeholder: SharedString =
                        placeholder.unwrap_or("Message").to_owned().into();
                    let (community, channel) = (community.clone(), channel.to_owned());
                    let composer = cx.new(|cx| {
                        crate::ui::composer::Composer::new(
                            community.clone(),
                            channel.clone(),
                            root,
                            placeholder,
                            window,
                            cx,
                        )
                    });
                    let profiles = community.read(cx).snapshot.profiles.clone();
                    let chips: Vec<(String, SharedString, Option<String>)> = mentions
                        .iter()
                        .filter_map(|mention| {
                            let key = mention
                                .get("pubkey")
                                .and_then(Value::as_str)
                                .filter(|key| {
                                    key.len() == 64 && key.bytes().all(|b| b.is_ascii_hexdigit())
                                })?
                                .to_ascii_lowercase();
                            let name = mention
                                .get("name")
                                .and_then(Value::as_str)
                                .map(str::to_owned)
                                .or_else(|| profiles.get(&key).map(|p| p.name.clone()))
                                .unwrap_or_else(|| key.chars().take(10).collect());
                            Some((key, name.into(), None))
                        })
                        .take(32)
                        .collect();
                    composer.update(cx, |composer, cx| {
                        composer.set_can_send(!disabled, cx);
                        if detached {
                            composer.without_attachments();
                        }
                        if invite_agents {
                            composer.invite_agents();
                        }
                        if let Some(draft) = draft {
                            let input = composer.input();
                            input.update(cx, |input, cx| {
                                input.set_value(draft.to_owned(), window, cx)
                            });
                        }
                        if !chips.is_empty() {
                            composer.insert_team(&chips, None, window, cx);
                        }
                        if auto_focus {
                            composer.focus(window, cx);
                        }
                    });
                    // The id `composer/insert` and the like take.
                    let number = super::contribute::composer_id(&self.plugin, &composer);
                    self.event(id, "ready", Some(json!({ "composer": number })), cx);
                    let element = id.to_owned();
                    let subscription = cx.subscribe_in(
                        &composer,
                        window,
                        move |this, composer, event, window, cx| {
                            if let crate::ui::composer::ComposerEvent::Submit(submission) = event {
                                this.submitted(
                                    &element,
                                    composer.clone(),
                                    submission.clone(),
                                    (community.clone(), channel.clone(), reply, plugin_submits),
                                    window,
                                    cx,
                                );
                            }
                        },
                    );
                    self.composers.insert(
                        id.to_owned(),
                        (composer.clone(), subscription, draft.map(str::to_owned)),
                    );
                    Some(composer.into())
                }
                draw::EmbedNode::Message {
                    id,
                    channel,
                    message,
                    style,
                } if readable(channel) => {
                    buzz_relay_client::EventId::from_hex(message)
                        .ok()
                        .map(|message| {
                            let (community, channel) = (community.clone(), channel.to_owned());
                            let (weak, element) = (cx.entity().downgrade(), id.to_owned());
                            let send: super::message_view::Send =
                                Rc::new(move |event, value, cx| {
                                    if let Some(this) = weak.upgrade() {
                                        this.read(cx).event(&element, event, Some(value), cx);
                                    }
                                });
                            cx.new(|cx| {
                                super::message_view::MessageView::new(
                                    community, channel, message, send, window, cx,
                                )
                                .styled(style)
                            })
                            .into()
                        })
                }
                draw::EmbedNode::Thread { id, channel, root } if readable(channel) => {
                    match buzz_relay_client::EventId::from_hex(root) {
                        Ok(root) => {
                            let weak = cx.entity().downgrade();
                            let element = id.to_owned();
                            let close: crate::ui::thread::OnClose =
                                std::sync::Arc::new(move |_, cx| {
                                    if let Some(this) = weak.upgrade() {
                                        this.read(cx).event(&element, "close", None, cx);
                                    }
                                });
                            let (community, channel) = (community.clone(), channel.to_owned());
                            Some(
                                cx.new(|cx| {
                                    crate::ui::thread::ThreadPanel::new(
                                        community, channel, root, close, window, cx,
                                    )
                                })
                                .into(),
                            )
                        }
                        Err(_) => None,
                    }
                }
                _ => None,
            };
            match view {
                Some(view) => {
                    self.embeds.insert(id.to_owned(), (made_for, view));
                }
                None => {
                    self.embeds.remove(id);
                }
            }
        }
        self.embeds.retain(|id, _| seen.contains(id));
        self.composers.retain(|id, _| seen.contains(id));
        self.unsettled.retain(|id, _| seen.contains(id));
    }

    /// `composer/settle`: the plugin sent a draft it took, or could not;
    /// the composer clears, or keeps the draft to try again.
    pub(super) fn settle(
        &mut self,
        element: &str,
        sent: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let (Some(submission), Some((composer, _, _))) =
            (self.unsettled.remove(element), self.composers.get(element))
        else {
            return false;
        };
        let result = if sent {
            Ok(())
        } else {
            Err("Not sent".to_owned())
        };
        let handed_back = submission.settle(&result, cx);
        composer.update(cx, |composer, cx| match sent {
            true => composer.sent(window, cx),
            false if handed_back => {}
            false => composer.failed(cx),
        });
        true
    }

    /// An embedded composer's Send: the plugin takes the draft
    /// (`submission.submit`), or Buzz sends it as the channel does and
    /// tells the plugin `sent` (`onSend`).
    fn submitted(
        &mut self,
        element: &str,
        composer: Entity<crate::ui::composer::Composer>,
        submission: crate::ui::composer::Submission,
        (community, channel, reply, plugin_submits): (
            Entity<Community>,
            String,
            Option<(buzz_relay_client::EventId, buzz_relay_client::EventId)>,
            bool,
        ),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Editing needs the conversation's own views.
        if submission.editing.is_some() {
            composer.update(cx, |composer, cx| composer.failed(cx));
            return;
        }
        let session = community.read(cx).session.clone();
        let element = element.to_owned();
        cx.spawn_in(window, async move |this, cx| {
            let attachments = if submission.upload.is_some() {
                match submission.attachments().await {
                    Some(attachments) => attachments,
                    None => return,
                }
            } else {
                Vec::new()
            };
            let content = submission.content.clone();
            let mentions = submission.mentions.clone();
            let result = if plugin_submits {
                let draft = json!({
                    "content": content,
                    "mentions": mentions.recipients,
                    "references": mentions.references,
                    "attachments": attachments.iter().map(|attachment| json!({
                        "url": attachment.url,
                        "mime": attachment.mime,
                        "size": attachment.size,
                        "name": attachment.name,
                        "sha256": attachment.sha256,
                    })).collect::<Vec<_>>(),
                });
                // The draft stays, locked, until `composer/settle`.
                let _ = this.update(cx, |this, cx| {
                    this.unsettled.insert(element.clone(), submission.clone());
                    this.event(&element, "submit", Some(draft), cx);
                });
                return;
            } else {
                let sent = session
                    .send(&channel, &content, mentions, attachments, reply)
                    .await;
                match sent {
                    Ok(()) => Ok(session.queued_message(&channel, &content).await),
                    Err(error) => Err(error),
                }
            };
            let handed_back = cx
                .update(|_, cx| {
                    submission.settle(&result.as_ref().map(|_| ()).map_err(Clone::clone), cx)
                })
                .unwrap_or(false);
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(id) => {
                    composer.update(cx, |composer, cx| composer.sent(window, cx));
                    if !plugin_submits {
                        let value = json!({ "id": id.map(|id| id.to_hex()) });
                        this.event(&element, "sent", Some(value), cx);
                    }
                }
                Err(_) if handed_back => {}
                Err(_) => composer.update(cx, |composer, cx| composer.failed(cx)),
            });
        })
        .detach();
    }

    /// What shows before the plugin's tree, or instead of it.
    fn placeholder(&self, status: Status, cx: &App) -> Option<AnyElement> {
        let c = cx.colors();
        let message = |text: String| {
            div()
                .flex()
                .flex_col()
                .gap(space::S2)
                .text_body_sm()
                .text_color(c.text_subtle)
                .child(text)
        };
        match status {
            Status::Off => Some(
                message("This plugin is off. Turn it on in Settings → Plugins.".into())
                    .into_any_element(),
            ),
            Status::Paused => Some(
                message("Plugins you added are paused for this launch (safe mode).".into())
                    .into_any_element(),
            ),
            Status::Failed(reason) => {
                let plugin = self.plugin.clone();
                Some(
                    div()
                        .flex()
                        .flex_col()
                        .gap(space::S3)
                        .child(div().text_label().child("This plugin stopped"))
                        .child(
                            div()
                                .text_body_sm()
                                .text_color(c.text_subtle)
                                .font_family(buzz_ui::tokens::font::MONO)
                                .child(reason),
                        )
                        .child(
                            div().flex().child(
                                Button::new("plugin-restart", "Restart")
                                    .icon(IconName::ArrowClockwise)
                                    .on_click(move |_, _, cx| {
                                        ExternalPlugins::restart(&plugin, cx)
                                    }),
                            ),
                        )
                        .into_any_element(),
                )
            }
            Status::Starting | Status::Running if self.tree.is_none() => Some(
                div()
                    .flex()
                    .items_center()
                    .gap(space::S2)
                    .text_body_sm()
                    .text_color(c.text_subtle)
                    .child(Spinner::new().size(14.))
                    .child("Loading…")
                    .into_any_element(),
            ),
            Status::Starting | Status::Running => None,
        }
    }
}

impl Render for ExternalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // A page, panel or Settings section fills its place, so it can be
        // drawn from the last frame until it changes: a long plugin view is
        // not laid out again for every caret blink elsewhere.
        if matches!(self.kind, "panel" | "page" | "settings") {
            let weak = cx.entity().downgrade();
            let cached = self
                .cached
                .get_or_insert_with(|| {
                    let view = cx.entity();
                    cx.new(|cx| {
                        cx.observe(&view, |_, _, cx| cx.notify()).detach();
                        Cached { view: weak }
                    })
                })
                .clone();
            return div()
                .size_full()
                .child(cached.cached(StyleRefinement::default().size_full()))
                .into_any_element();
        }
        self.content(window, cx)
    }
}

/// A framed view's content, drawn from the last frame until the view
/// changes.
struct Cached {
    view: WeakEntity<ExternalView>,
}

impl Render for Cached {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        match self.view.upgrade() {
            Some(view) => view.update(cx, |view, cx| view.content(window, cx)),
            None => div().into_any_element(),
        }
    }
}

impl ExternalView {
    fn content(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let c = *cx.colors();
        let status = ExternalPlugins::status(&self.plugin, cx);
        // Panels, pages and Settings sections fill their place; agent tabs,
        // dialogs and accessories sit in the host's own frame.
        let framed = matches!(self.kind, "panel" | "page" | "settings");
        // A reaction picker sits in the picker's own padded surface.
        let mut body = div()
            .flex()
            .flex_col()
            .gap(space::S4)
            .when(framed && !self.edge_to_edge, |body| body.p(space::S6))
            // So a growing column can fill the page.
            .when(framed, |body| body.min_h_full());
        // A Settings tab starts with the app's Provider field.
        if let Some((agent, _)) = &self.agent
            && let Some(control) = agent.type_control(false, window, cx)
        {
            body = body.child(control);
        }
        if !self.back.is_empty() {
            let weak = cx.entity().downgrade();
            body = body.child(
                div().flex().child(
                    Button::new("plugin-panel-back", "Back")
                        .icon(IconName::ArrowLeft)
                        .on_click(move |_, _, cx| {
                            let _ = weak.update(cx, |view, cx| view.go_back(cx));
                        }),
                ),
            );
        }
        if let Some(placeholder) = self.placeholder(status, cx) {
            body = body.child(placeholder);
        } else if let Some(tree) = self.tree.clone() {
            let mut seen = HashSet::new();
            for input in draw::inputs(&tree) {
                seen.insert(input.id.to_owned());
                let (field, created) =
                    self.field(input.id, input.placeholder, input.area, window, cx);
                field.clear_on_submit = input.clear_on_submit;
                if created || field.version != input.version {
                    field.version = input.version;
                    let text = input.value.cloned().unwrap_or_default();
                    field.state.clone().set_text(text, window, cx);
                }
            }
            // Fields the tree no longer has start empty if they return.
            self.fields.retain(|id, _| seen.contains(id));
            self.sync_embeds(&tree, window, cx);
            let embeds: HashMap<String, AnyView> = self
                .embeds
                .iter()
                .map(|(id, (_, view))| (id.clone(), view.clone()))
                .collect();
            let fields: HashMap<String, FieldState> = self
                .fields
                .iter()
                .map(|(id, field)| (id.clone(), field.state.clone()))
                .collect();
            let weak = cx.entity().downgrade();
            let draw = Draw {
                send: Rc::new(move |id, event, value, cx| {
                    if let Some(this) = weak.upgrade() {
                        this.read(cx).event(id, event, value, cx);
                    }
                }),
                fields: &fields,
                embeds: &embeds,
                community: self.community(cx),
                place: &self.contribution,
                window: Some(window),
            };
            body = body.child(draw::node(&tree, &self.id, &draw, cx));
        }
        if let Some(error) = &self.error {
            body = body.child(
                div()
                    .text_body_sm()
                    .text_color(c.text_danger)
                    .child(error.clone()),
            );
        }
        div()
            .id(SharedString::from(format!("plugin-view-{}", self.id)))
            .w_full()
            .when(framed, |view| {
                view.size_full()
                    .min_h_0()
                    .overflow_y_scroll()
                    .bg(c.surface_panel)
            })
            .text_color(c.text_standard)
            .child(body)
            .into_any_element()
    }
}

/// Whether `params` can be a plugin page's route: any JSON value but
/// null (the page's start) as buzz-app's targets keep one (`json()`): at
/// most 8 levels deep and 1024 values, arrays of up to 256 items, objects
/// of up to 128 keys of up to 128 characters, and 8 KiB in all.
pub fn route_valid(params: &Value) -> bool {
    fn walk(value: &Value, depth: usize, nodes: &mut usize) -> bool {
        if depth > 8 || *nodes == 0 {
            return false;
        }
        *nodes -= 1;
        match value {
            Value::Array(items) => {
                items.len() <= 256 && items.iter().all(|item| walk(item, depth + 1, nodes))
            }
            Value::Object(fields) => {
                fields.len() <= 128
                    && fields.iter().all(|(key, item)| {
                        key.encode_utf16().count() <= 128 && walk(item, depth + 1, nodes)
                    })
            }
            _ => true,
        }
    }
    !params.is_null() && params.to_string().len() <= 8_192 && walk(params, 0, &mut 1024)
}

/// Shows a plugin page at `params`, or its start: the page hears
/// `page/route`.
pub fn open_route(view: &AnyView, params: Option<&Value>, _: &mut Window, cx: &mut App) {
    let Ok(view) = view.clone().downcast::<ExternalView>() else {
        return;
    };
    view.update(cx, |view, cx| {
        view.route = params.cloned();
        // Before `view/open`, which carries the route itself.
        if !view.opened {
            return;
        }
        let (entry, keep) = crate::ui::workspace::current_visit();
        ExternalPlugins::notify_running(
            &view.plugin,
            "page/route",
            json!({
                "view": view.id,
                "route": view.route,
                "entry": entry,
                "focus": if keep { "preserve" } else { "content" },
            }),
            cx,
        );
    });
}

/// Ends a composer capture, attaching a file `(name, mime, bytes)` if
/// the plugin made one.
pub type Accept = Rc<dyn Fn(Option<crate::ui::composer::Captured>, &mut Window, &mut App) -> bool>;

/// How wide a plugin's dialog is (`ui/openDialog` `size`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DialogSize {
    Default,
    Wide,
    /// The window less 24px a side, the body scrolling, as a diff's
    /// expanded view (`size="expanded"`).
    Expanded,
}

/// An account action's dialog around the plugin's view.
struct Framed {
    title: SharedString,
    size: DialogSize,
    view: Entity<ExternalView>,
    close: Close,
    focus: FocusHandle,
}

impl Render for Framed {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let close = self.close.clone();
        if self.size != DialogSize::Expanded {
            let dialog = buzz_ui::Dialog::new("plugin-dialog", self.title.clone())
                .on_close(move |_, window, cx| close(window, cx));
            let dialog = if self.size == DialogSize::Wide {
                dialog.size(buzz_ui::DialogSize::Wide)
            } else {
                dialog
            };
            return dialog.child(self.view.clone()).into_any_element();
        }
        // Like buzz-app's expanded dialog, only Escape and the close
        // button dismiss it.
        if !self.focus.contains_focused(window, cx) {
            self.focus.focus(window, cx);
        }
        let c = *cx.colors();
        let inset = rems(3.).to_pixels(window.rem_size());
        let viewport = window.viewport_size();
        let on_escape = close.clone();
        div()
            .id("plugin-dialog-expanded")
            .key_context(buzz_ui::DIALOG_CONTEXT)
            .track_focus(&self.focus)
            .on_action(move |_: &buzz_ui::Cancel, window, cx| on_escape(window, cx))
            .flex()
            .flex_col()
            .w(viewport.width - inset)
            .h(viewport.height - inset)
            .overflow_hidden()
            .p(rems(1.5))
            .bg(c.surface_popover)
            .border_1()
            .border_color(c.border_standard)
            .rounded(buzz_ui::tokens::radius::PANEL)
            .shadow(buzz_ui::tokens::shadow::sm())
            .text_color(c.text_standard)
            .text_body()
            .child(
                div()
                    .flex()
                    .items_start()
                    .justify_between()
                    .gap(rems(1.))
                    .child(div().min_w_0().text_label().child(self.title.clone()))
                    .child(
                        buzz_ui::IconButton::new("plugin-dialog-close", IconName::X)
                            .size(buzz_ui::IconButtonSize::Sm)
                            .surface(buzz_ui::Surface::Floating)
                            .on_click(move |_, window, cx| close(window, cx)),
                    ),
            )
            .child(
                div()
                    .id("plugin-dialog-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .py(rems(1.5))
                    .child(self.view.clone()),
            )
            .into_any_element()
    }
}
