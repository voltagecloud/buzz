//! The Agent Activity panel (`ActivityPanel.tsx`): one agent's captured
//! activity in a conversation scope, as a transcript or as raw records,
//! with live feed state, saved-history paging and capture notices.

use super::{agent_label, public_key_labels};
use crate::agent_activity::observer::{self, AgentActivity};
use crate::agent_activity::transcript_model::{Scope, Transcript, transcript};
use crate::agent_activity::turns::TurnState;
use crate::agent_activity::{
    Capture, FeedStatus, HistoryStatus, RawRecord, Selection, Snapshot, raw_records, selection,
};
use crate::community::Community;
use crate::ui::channel_panels::Opening;
use crate::ui::overlay::{self, Align};
use buzz_relay_client::SessionStatus;
use buzz_ui::{
    ActiveTheme, Button, ButtonSize, Icon, IconName, Menu, MenuItem, MenuLabel, Select, Surface,
    Tab, TabVariant, Tabs, TypeStyled,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum View {
    Transcript,
    Raw,
}

/// Which dropdown is open.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Open {
    Agent,
    Conversation,
}

/// `[channel]` is the whole channel, `[channel, root]` one thread.
pub(super) type Conversation = Option<(String, Option<String>)>;

/// Transcripts kept: the agent-wide one and the current scope's.
const TRANSCRIPTS_CACHED: usize = 2;

pub struct ActivityPanel {
    pub(super) community: Entity<Community>,
    pub(super) activity: Option<Entity<AgentActivity>>,
    /// The agent chosen in the select; "" follows the first observed.
    selected: String,
    pub(super) conversation: Conversation,
    pub(super) view: View,
    /// Open disclosures: raw records, tool calls and prompt sections.
    pub(super) expanded: HashSet<String>,
    open: Option<Open>,
    scroll: ScrollHandle,
    focus: FocusHandle,
    /// The last built transcripts, each for the snapshot and scope it came
    /// from: a render builds the agent-wide one and the current scope's.
    cache: Vec<(Arc<Snapshot>, Scope, Arc<Transcript>)>,
    /// The agent whose saved pages are shown.
    history_agent: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl ActivityPanel {
    pub fn new(opening: Opening, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let selection = selection(&opening.target).unwrap_or_default();
        let activity = observer::get(&opening.community, cx);
        let mut subscriptions = vec![cx.observe(&opening.community, |_, _, cx| cx.notify())];
        if let Some(activity) = &activity {
            subscriptions.push(cx.observe(activity, |_, _, cx| cx.notify()));
        }
        let focus = cx.focus_handle();
        // The panel takes focus when it opens.
        window.focus(&focus, cx);
        let Selection {
            agent,
            channel,
            thread,
        } = selection;
        Self {
            community: opening.community,
            activity,
            selected: agent,
            conversation: channel.map(|channel| (channel, thread)),
            view: View::Transcript,
            expanded: HashSet::new(),
            open: None,
            scroll: ScrollHandle::new(),
            focus,
            cache: Vec::new(),
            history_agent: None,
            _subscriptions: subscriptions,
        }
    }

    fn snapshot(&self, cx: &App) -> Arc<Snapshot> {
        self.activity
            .as_ref()
            .map(|activity| activity.read(cx).snapshot.clone())
            .unwrap_or_default()
    }

    /// Observed agents: saved-history agents, then those with records.
    fn agents(snapshot: &Snapshot) -> Vec<String> {
        let mut agents: Vec<String> = Vec::new();
        for agent in snapshot
            .history_agents
            .iter()
            .chain(snapshot.records.iter().map(|record| &record.agent))
        {
            if !agents.contains(agent) {
                agents.push(agent.clone());
            }
        }
        agents
    }

    pub(super) fn transcript(&mut self, snapshot: &Arc<Snapshot>, scope: Scope) -> Arc<Transcript> {
        if let Some((_, _, transcript)) = self.cache.iter().find(|(cached, cached_scope, _)| {
            Arc::ptr_eq(cached, snapshot) && *cached_scope == scope
        }) {
            return transcript.clone();
        }
        let built = Arc::new(transcript(&snapshot.records, &scope));
        if self.cache.len() == TRANSCRIPTS_CACHED {
            self.cache.remove(0);
        }
        self.cache.push((snapshot.clone(), scope, built.clone()));
        built
    }

    pub(super) fn toggle(&mut self, key: String, cx: &mut Context<Self>) {
        if !self.expanded.remove(&key) {
            self.expanded.insert(key);
        }
        cx.notify();
    }

    fn with_activity(
        &self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut AgentActivity, &mut Context<AgentActivity>),
    ) {
        if let Some(activity) = &self.activity {
            activity.update(cx, f);
        }
    }
}

fn text(text: impl Into<SharedString>) -> Div {
    div().child(text.into())
}

fn small(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .text_body_sm()
        .text_color(cx.colors().text_subtle)
        .child(text.into())
}

/// `10/10/2026, 12:00:00 PM` (`toLocaleString`).
pub fn date_time(ms: u64) -> String {
    use chrono::{Local, TimeZone};
    Local
        .timestamp_millis_opt(ms as i64)
        .single()
        .map(|time| time.format("%-m/%-d/%Y, %-I:%M:%S %p").to_string())
        .unwrap_or_default()
}

/// `10/10/26, 12:00 PM` (`dateStyle: short, timeStyle: short`).
fn short_date_time(ms: u64) -> String {
    use chrono::{Local, TimeZone};
    Local
        .timestamp_millis_opt(ms as i64)
        .single()
        .map(|time| time.format("%-m/%-d/%y, %-I:%M %p").to_string())
        .unwrap_or_default()
}

impl Render for ActivityPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let ready = self.community.read(cx).snapshot.status == SessionStatus::Ready;
        let body: Vec<AnyElement> = if !ready || self.activity.is_none() {
            vec![text("Connect to a community to view agent activity.").into_any_element()]
        } else {
            self.details(window, cx)
        };
        div()
            .id("agent-activity")
            .track_focus(&self.focus)
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .min_w_0()
                    .gap(rems(1.))
                    .p(rems(1.))
                    .text_body()
                    .text_color(c.text_standard)
                    .when(ready && self.activity.is_some(), |this| {
                        this.child(div().text_heading().child("Agent activity"))
                    })
                    .children(body),
            )
    }
}

impl ActivityPanel {
    fn details(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let snapshot = self.snapshot(cx);
        if snapshot.status == FeedStatus::Unavailable {
            return vec![
                text(
                    "This host cannot decode agent activity. Use the desktop app or development \
                     broker.",
                )
                .into_any_element(),
            ];
        }
        let agents = Self::agents(&snapshot);
        let agent = if self.selected.is_empty() {
            agents.first().cloned().unwrap_or_default()
        } else {
            self.selected.clone()
        };
        // Saved pages follow the shown agent.
        if self.history_agent.as_deref() != Some(agent.as_str()) {
            self.history_agent = Some(agent.clone());
            let chosen = agent.clone();
            self.with_activity(cx, move |activity, cx| activity.select_history(&chosen, cx));
        }
        let mut out = Vec::new();
        if snapshot.status == FeedStatus::Interrupted {
            let activity = self.activity.clone();
            out.push(
                div()
                    .flex()
                    .child(
                        Button::new("activity-retry", "Retry live feed")
                            .size(ButtonSize::Sm)
                            .on_click(move |_, _, cx| {
                                if let Some(activity) = &activity {
                                    activity.read(cx).retry();
                                }
                            }),
                    )
                    .into_any_element(),
            );
        }
        if agents.is_empty() && self.selected.is_empty() {
            out.extend(self.diagnostics(&snapshot, cx));
            out.push(
                text(
                    "No captured activity yet. New activity appears when an agent publishes \
                     telemetry while capture is enabled. There is no relay backfill.",
                )
                .into_any_element(),
            );
            out.push(footer(cx).into_any_element());
            return out;
        }

        let mut choices = agents.clone();
        if !self.selected.is_empty() && !choices.contains(&self.selected) {
            choices.push(self.selected.clone());
        }
        let keys = public_key_labels(choices.iter());
        let community = self.community.read(cx);
        let labels: HashMap<String, String> = choices
            .iter()
            .map(|key| (key.clone(), agent_label(community, key, &keys, cx)))
            .collect();
        let channel_names: HashMap<String, String> = community
            .snapshot
            .channels
            .iter()
            .map(|channel| (channel.id.clone(), channel.name.clone()))
            .collect();
        let channel_name = |id: &str| format!("#{}", channel_names.get(id).map_or(id, |n| n));

        out.push(self.agent_select(&agent, &choices, &labels, window, cx));
        out.push(
            div()
                .text_mono()
                .text_color(c(cx).text_standard)
                .overflow_hidden()
                .child(agent.clone())
                .into_any_element(),
        );

        let (channel, thread) = self.conversation.clone().unzip();
        let thread = thread.flatten();
        let mut channel_choices: Vec<String> = Vec::new();
        for record in snapshot.records.iter().filter(|r| r.agent == agent) {
            for id in &record.channel_ids {
                if !channel_choices.contains(id) {
                    channel_choices.push(id.clone());
                }
            }
        }
        if let Some(channel) = &channel
            && !channel_choices.contains(channel)
        {
            channel_choices.push(channel.clone());
        }
        // Threads are named by their first loaded prompt.
        let all = self.transcript(
            &snapshot,
            Scope {
                agent: agent.clone(),
                ..Default::default()
            },
        );
        let mut threads: BTreeMap<(String, String), (Option<String>, Option<String>)> =
            BTreeMap::new();
        let mut order: Vec<(String, String)> = Vec::new();
        for turn in &all.turns {
            let (Some(channel), Some(Some(root))) = (&turn.channel_id, &turn.thread_root) else {
                continue;
            };
            let key = (channel.clone(), root.clone());
            let entry = threads.entry(key.clone()).or_insert_with(|| {
                order.push(key.clone());
                (Some(short_date_time(turn.started_at)), None)
            });
            if entry.1.is_none() {
                entry.1 = turn.items.iter().find_map(|item| match &item.kind {
                    crate::agent_activity::transcript_model::ItemKind::Prompt { text, .. } => text
                        .split('\n')
                        .find(|line| !line.is_empty())
                        .map(|line| line.chars().take(60).collect()),
                    _ => None,
                });
            }
        }
        if let (Some(channel), Some(root)) = (&channel, &thread) {
            let key = (channel.clone(), root.clone());
            if !threads.contains_key(&key) {
                threads.insert(key.clone(), (None, None));
                order.push(key);
            }
        }
        let thread_label = |root: &str, (time, text): &(Option<String>, Option<String>)| {
            let text = text
                .clone()
                .unwrap_or_else(|| format!("thread {}", &root[..8.min(root.len())]));
            match time {
                Some(time) => format!("{time} · {text}"),
                None => text,
            }
        };
        let current_label = match (&channel, &thread) {
            (None, _) => "All conversations (including unscoped records)".to_string(),
            (Some(channel), None) => {
                format!(
                    "{} · whole channel, including threads",
                    channel_name(channel)
                )
            }
            (Some(channel), Some(root)) => format!(
                "{} › {}",
                channel_name(channel),
                thread_label(
                    root,
                    threads
                        .get(&(channel.clone(), root.clone()))
                        .unwrap_or(&(None, None))
                )
            ),
        };
        let mut groups: Vec<(String, Vec<(Conversation, String)>)> = vec![(
            "Activity scope".into(),
            vec![(
                None,
                "All conversations (including unscoped records)".into(),
            )],
        )];
        for id in &channel_choices {
            let mut options = vec![(
                Some((id.clone(), None)),
                format!("{} · whole channel, including threads", channel_name(id)),
            )];
            for key in order.iter().filter(|(c, _)| c == id) {
                options.push((
                    Some((id.clone(), Some(key.1.clone()))),
                    format!(
                        "{} › {}",
                        channel_name(id),
                        thread_label(&key.1, &threads[key])
                    ),
                ));
            }
            groups.push((channel_name(id), options));
        }
        out.push(self.conversation_select(current_label, groups, window, cx));

        let transcript = self.transcript(
            &snapshot,
            Scope {
                agent: agent.clone(),
                channel: channel.clone(),
                thread: thread.clone(),
            },
        );
        // A thread counts only its own known turns.
        let shown: Option<HashSet<&str>> = thread.as_ref().map(|_| {
            transcript
                .turns
                .iter()
                .map(|t| t.turn_id.as_str())
                .collect()
        });
        let turns: Vec<_> = snapshot
            .turns
            .iter()
            .filter(|turn| {
                turn.agent == agent
                    && (channel.is_none() || turn.channel_id == channel)
                    && shown
                        .as_ref()
                        .is_none_or(|shown| shown.contains(turn.turn_id.as_str()))
            })
            .collect();
        let working = turns
            .iter()
            .filter(|t| t.state == TurnState::Working)
            .count();
        let unknown = turns
            .iter()
            .filter(|t| t.state == TurnState::Unknown)
            .count();
        let mut status = if working > 0 {
            format!("{working} observed working turn(s).")
        } else {
            "No fresh working evidence.".to_string()
        };
        status.push(' ');
        if unknown > 0 {
            status.push_str(&format!("{unknown} turn(s) have unknown current state."));
        }
        out.push(text(status).into_any_element());

        let view = self.view;
        out.push(
            Tabs::new(TabVariant::Panel)
                .tab(
                    Tab::new("activity-view-transcript", "Transcript")
                        .selected(view == View::Transcript)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.view = View::Transcript;
                            cx.notify();
                        })),
                )
                .tab(
                    Tab::new("activity-view-raw", "Raw")
                        .selected(view == View::Raw)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.view = View::Raw;
                            cx.notify();
                        })),
                )
                .into_any_element(),
        );
        let states: HashMap<String, TurnState> = turns
            .iter()
            .map(|turn| (turn.turn_id.clone(), turn.state))
            .collect();
        let name = labels.get(&agent).cloned().unwrap_or_default();
        match view {
            View::Raw => out.extend(self.raw(&snapshot, &agent, channel.as_deref(), cx)),
            View::Transcript => {
                let mut before = self.notices(&snapshot, cx);
                if snapshot.history == HistoryStatus::Loading {
                    before.push(small("Loading saved history…", cx).into_any_element());
                }
                if snapshot.history == HistoryStatus::Error {
                    before.push(text("Saved history could not be read.").into_any_element());
                }
                if snapshot.history_older {
                    before.push(
                        small(
                            "Showing an older saved page; newer saved activity is hidden.",
                            cx,
                        )
                        .into_any_element(),
                    );
                }
                if snapshot.has_older || snapshot.history_older {
                    before.push(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap(rems(0.5))
                            .children(self.older_button(&snapshot, cx))
                            .when(snapshot.history_older, |this| {
                                this.children(self.latest_button(&snapshot, cx))
                            })
                            .into_any_element(),
                    );
                }
                if transcript.unknown_thread > 0 {
                    before.push(
                        small(
                            format!(
                                "{} turn(s) with an unknown thread appear only in the whole \
                                 channel.",
                                transcript.unknown_thread
                            ),
                            cx,
                        )
                        .into_any_element(),
                    );
                }
                out.push(super::transcript::timeline(
                    self,
                    &transcript,
                    &name,
                    &states,
                    before,
                    cx,
                ));
            }
        }
        out
    }

    fn agent_select(
        &mut self,
        agent: &str,
        choices: &[String],
        labels: &HashMap<String, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self.open == Some(Open::Agent);
        let items: Vec<AnyElement> = choices
            .iter()
            .map(|key| {
                let chosen = key.clone();
                MenuItem::new(
                    SharedString::from(format!("activity-agent-{key}")),
                    labels.get(key).cloned().unwrap_or_default(),
                )
                .checked(key == agent)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.selected = chosen.clone();
                    this.expanded.clear();
                    this.open = None;
                    cx.notify();
                }))
                .into_any_element()
            })
            .collect();
        self.select(
            "activity-agent",
            "Agent",
            labels.get(agent).cloned().unwrap_or_default(),
            Open::Agent,
            open.then(|| {
                vec![MenuLabel::new("Observed agents").into_any_element()]
                    .into_iter()
                    .chain(items)
                    .collect()
            }),
            window,
            cx,
        )
    }

    fn conversation_select(
        &mut self,
        current: String,
        groups: Vec<(String, Vec<(Conversation, String)>)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self.open == Some(Open::Conversation);
        let items = open.then(|| {
            let mut items = Vec::new();
            for (index, (label, options)) in groups.into_iter().enumerate() {
                items.push(MenuLabel::new(label).into_any_element());
                for (n, (value, label)) in options.into_iter().enumerate() {
                    let checked = value == self.conversation;
                    items.push(
                        MenuItem::new(
                            SharedString::from(format!("activity-scope-{index}-{n}")),
                            label,
                        )
                        .checked(checked)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.conversation = value.clone();
                            this.expanded.clear();
                            this.open = None;
                            cx.notify();
                        }))
                        .into_any_element(),
                    );
                }
            }
            items
        });
        self.select(
            "activity-conversation",
            "Conversation",
            current,
            Open::Conversation,
            items,
            window,
            cx,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn select(
        &mut self,
        id: &'static str,
        label: &'static str,
        value: String,
        which: Open,
        items: Option<Vec<AnyElement>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = items.is_some();
        let mut trigger = div().relative().child(
            Select::new(id)
                .value(value)
                .open(open)
                .surface(Surface::Panel)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.open = if this.open == Some(which) {
                        None
                    } else {
                        Some(which)
                    };
                    cx.notify();
                })),
        );
        if let Some(items) = items {
            let panel = cx.entity().downgrade();
            trigger = trigger.child(overlay::dropdown(
                window,
                Align::Start,
                div()
                    .min_w(rems(16.))
                    .max_w(rems(28.))
                    .max_h(rems(24.))
                    .child(Menu::new().children(items)),
                Rc::new(move |_, cx| {
                    let _ = panel.update(cx, |this, cx| {
                        this.open = None;
                        cx.notify();
                    });
                }),
            ));
        }
        div()
            .flex()
            .flex_col()
            .gap(rems(0.5))
            .child(div().text_label_sm().child(label))
            .child(trigger)
            .into_any_element()
    }

    fn latest_button(&self, snapshot: &Snapshot, cx: &mut Context<Self>) -> Option<AnyElement> {
        (snapshot.history != HistoryStatus::Unavailable).then(|| {
            Button::new("activity-latest", "Show latest saved activity")
                .size(ButtonSize::Sm)
                .disabled(snapshot.history == HistoryStatus::Loading)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.with_activity(cx, |activity, cx| activity.latest_history(cx))
                }))
                .into_any_element()
        })
    }

    fn older_button(&self, snapshot: &Snapshot, cx: &mut Context<Self>) -> Option<AnyElement> {
        snapshot.has_older.then(|| {
            Button::new("activity-older", "Load older activity")
                .size(ButtonSize::Sm)
                .disabled(snapshot.history == HistoryStatus::Loading)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.with_activity(cx, |activity, cx| activity.load_older(cx))
                }))
                .into_any_element()
        })
    }

    /// Capture and completeness notices; both views show them.
    fn notices(&self, snapshot: &Snapshot, cx: &App) -> Vec<AnyElement> {
        let mut out = Vec::new();
        match snapshot.capture {
            Capture::Off => out.push(text(
                "Saving activity is off. Live records are not being saved; existing history is \
                 kept.",
            )),
            Capture::Unknown => out.push(text("Activity saving status is unknown.")),
            Capture::Error => out.push(text(
                "Some new archive records could not be saved. Check archive settings and retry \
                 the connection.",
            )),
            Capture::Saving => {}
        }
        if snapshot.history_skipped > 0 {
            out.push(text(format!(
                "{} saved records could not be decoded on this page.",
                snapshot.history_skipped
            )));
        }
        if snapshot.trimmed > 0 {
            out.push(text(format!(
                "Live display limited: {} records or turn states left the RAM window. Saved \
                 history is paged separately.",
                snapshot.trimmed
            )));
        }
        let _ = cx;
        out.into_iter().map(IntoElement::into_any_element).collect()
    }

    fn diagnostics(&self, snapshot: &Snapshot, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut out = vec![
            small(
                "Owner-only activity. Capture is controlled in Settings → Agents, independently \
                 of this plugin. Saved records are history, not current working status. This is \
                 not a complete ACP recording; the agent must publish telemetry.",
                cx,
            )
            .into_any_element(),
            text(format!(
                "Feed: {}. Quiet or disconnected means unknown, not stopped.",
                snapshot.status.label()
            ))
            .into_any_element(),
        ];
        out.extend(self.notices(snapshot, cx));
        out.push(
            text(match snapshot.history {
                HistoryStatus::Loading => "Loading saved history…",
                HistoryStatus::Error => {
                    "Saved history could not be read. Live activity may still appear."
                }
                HistoryStatus::Unavailable => "Saved history is unavailable on this host.",
                HistoryStatus::Ready => "Saved history loaded.",
            })
            .into_any_element(),
        );
        out.extend(
            self.latest_button(snapshot, cx)
                .map(|b| div().flex().child(b).into_any_element()),
        );
        if snapshot.history_older {
            out.push(
                text(
                    "Showing an older saved page. Newer saved pages are not shown; live records \
                     may still appear. Use Show latest saved activity to return.",
                )
                .into_any_element(),
            );
        }
        out.extend(
            self.older_button(snapshot, cx)
                .map(|b| div().flex().child(b).into_any_element()),
        );
        out
    }

    /// The Raw view: diagnostics and one disclosure per record.
    fn raw(
        &mut self,
        snapshot: &Snapshot,
        agent: &str,
        channel: Option<&str>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let c = *cx.colors();
        let records: Vec<RawRecord> = raw_records(&snapshot.records, agent, channel);
        // Expansion stays within the records shown.
        let ids: HashSet<String> = records.iter().map(|r| format!("raw:{}", r.id)).collect();
        self.expanded
            .retain(|key| !key.starts_with("raw:") || ids.contains(key));
        let mut out = self.diagnostics(snapshot, cx);
        if channel.is_some() {
            out.push(
                small(
                    "Channel-wide, including threads. Batches show only matching channel \
                     entries; unscoped entries are omitted.",
                    cx,
                )
                .into_any_element(),
            );
        }
        out.push(
            small(
                "Working evidence expires after 30 seconds without a fresh turn record. \
                 Completed means ended, not necessarily succeeded.",
                cx,
            )
            .into_any_element(),
        );
        if records.is_empty() {
            out.push(
                text(format!(
                    "No captured records for this identity{}. Only owner-visible agent telemetry \
                     appears. Activity published while capture was off cannot be recovered.",
                    if channel.is_some() {
                        " in this channel"
                    } else {
                        ""
                    }
                ))
                .into_any_element(),
            );
        }
        if snapshot.history_older {
            out.push(
                small(
                    "Gap: this saved page is not continuous with the live display. Loading \
                     another page replaces this saved page.",
                    cx,
                )
                .into_any_element(),
            );
        }
        let rows = records.into_iter().map(|record| {
            let key = format!("raw:{}", record.id);
            let open = self.expanded.contains(&key);
            let toggle = key.clone();
            div()
                .flex()
                .flex_col()
                .border_b_1()
                .border_color(c.border_standard)
                .child(
                    div()
                        .id(SharedString::from(key))
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap(rems(0.5))
                        .py(rems(0.75))
                        .cursor_pointer()
                        .text_label()
                        .child(div().min_w_0().child(format!(
                            "{} · {}",
                            record.kind,
                            date_time(record.received_at)
                        )))
                        .child(
                            Icon::new(if open {
                                IconName::CaretUp
                            } else {
                                IconName::CaretDown
                            })
                            .size(16.)
                            .color(c.text_subtle),
                        )
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.toggle(toggle.clone(), cx)),
                        ),
                )
                .when(open, |this| {
                    this.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(rems(0.5))
                            .pb(rems(0.75))
                            .child(small(format!("Event {}", record.envelope_id), cx))
                            .child(code_block(&record.plaintext, cx)),
                    )
                })
                .into_any_element()
        });
        out.push(div().flex().flex_col().children(rows).into_any_element());
        out.push(footer(cx).into_any_element());
        out
    }
}

/// The live window and paging note.
fn footer(cx: &App) -> Div {
    small(
        "The live display keeps up to 200 records / 2 MiB plus one saved page. Use Load older \
         activity to browse earlier pages, even when a page contains no entries for this \
         channel. Retention and capture are controlled in Settings → Agents; disabling this \
         plugin only clears its live display.",
        cx,
    )
}

/// A monospace block on the inset surface, scrolling past 24rem.
pub(super) fn code_block(text: &str, cx: &App) -> AnyElement {
    let c = cx.colors();
    div()
        .id(SharedString::from(format!("pre-{:x}", fxhash(text))))
        .max_h(rems(24.))
        .overflow_y_scroll()
        .p(rems(0.5))
        .rounded(buzz_ui::tokens::radius::CONTROL)
        .bg(c.surface_inset)
        .text_mono()
        .text_color(c.text_standard)
        .child(text.to_string())
        .into_any_element()
}

fn fxhash(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

fn c(cx: &App) -> buzz_ui::Colors {
    *cx.colors()
}
