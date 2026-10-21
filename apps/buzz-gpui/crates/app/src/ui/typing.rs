//! Conversation activity above the composer (ui.md §4.10,
//! `TypingIndicator.tsx`): who is typing on the left, and the viewer's
//! own working agents on the right, as an avatar stack that opens a
//! "Working now" list.

use super::agent_activity::row::{Action, AgentActivityRow};
use super::identity::Names;
use super::{media, navigation};
use crate::agent_activity::current::{CurrentActivity, Root};
use buzz_relay_client::{EventId, Media, Profiles};
use buzz_ui::tokens::shadow;
use buzz_ui::{ActiveTheme, Avatar, AvatarSize, Popover, PopoverPadding, PopoverWidth, TypeStyled};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

/// The chooser opens after the pointer rests this long, and closes this
/// long after it leaves (`delay={250} closeDelay={150}`).
const OPEN_DELAY: Duration = Duration::from_millis(250);
const CLOSE_DELAY: Duration = Duration::from_millis(150);
/// Avatars shown in the stack before "+N".
const STACK: usize = 4;
/// Windows this narrow inset the indicator 12px instead of 16px.
pub const NARROW: Pixels = px(650.);

/// "Alice is typing", "Alice, Bob are typing", "Alice, Bob and 2 others
/// are typing".
pub fn label(names: &[SharedString]) -> String {
    let shown = names
        .iter()
        .take(2)
        .map(SharedString::as_ref)
        .collect::<Vec<&str>>()
        .join(", ");
    let others = match names.len() {
        0..=2 => String::new(),
        n => format!(" and {} others", n - 2),
    };
    let verb = if names.len() == 1 {
        " is typing"
    } else {
        " are typing"
    };
    format!("{shown}{others}{verb}")
}

/// The viewer's own agents, on every host (`agentChoices`).
pub fn owned_agents(cx: &App) -> HashSet<String> {
    super::identity::AgentLibrary::from_hosts(cx).keys()
}

/// Who is active in a conversation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Activity {
    /// Working agents by key, each with what it does and where:
    /// `Some(None)` is the channel conversation, `Some(Some(root))` a
    /// thread, `None` a thread not yet confirmed.
    pub agents: BTreeMap<String, CurrentActivity>,
    /// Everyone else typing here, in the order they started.
    pub humans: Vec<String>,
}

impl Activity {
    /// A channel: its own typists, plus the viewer's agents anywhere in it,
    /// threads included. `in_channel` says whether a thread root is one of
    /// this channel's loaded messages.
    pub fn channel(
        typing: &[String],
        threads: &HashMap<String, Vec<String>>,
        in_channel: impl Fn(&str) -> bool,
        owned: &HashSet<String>,
    ) -> Self {
        let mut activity = Self::default();
        for person in typing {
            if owned.contains(person) {
                activity.add(person, Some(None));
            }
        }
        let mut roots: Vec<&String> = threads.keys().filter(|root| in_channel(root)).collect();
        roots.sort();
        for root in roots {
            for person in &threads[root] {
                if owned.contains(person) {
                    activity.add(person, Some(Some(root.clone())));
                }
            }
        }
        activity.humans = typing
            .iter()
            .filter(|person| !activity.agents.contains_key(*person))
            .cloned()
            .collect();
        activity
    }

    /// A thread: only those typing in it.
    pub fn thread(typing: &[String], root: &str, owned: &HashSet<String>) -> Self {
        let mut activity = Self::default();
        for person in typing {
            if owned.contains(person) {
                activity.add(person, Some(Some(root.to_string())));
            } else {
                activity.humans.push(person.clone());
            }
        }
        activity
    }

    /// Adds what the agent activity feed shows working here: observed
    /// turns with their live labels ("Reading main.rs"), and typing from
    /// agents that send telemetry (`currentActivity`).
    pub fn observe(mut self, observed: BTreeMap<String, CurrentActivity>) -> Self {
        for (agent, detail) in observed {
            for root in detail.roots {
                self.add(&agent, root);
            }
            if let Some(entry) = self.agents.get_mut(&agent) {
                entry.label = detail.label;
            }
        }
        let agents = &self.agents;
        self.humans.retain(|person| !agents.contains_key(person));
        self
    }

    /// Leaves out agents whose work shows on its message's bubble
    /// instead (`useReplacedTypers`).
    pub fn replace(&mut self, bubbled: &[String]) {
        self.agents.retain(|agent, _| !bubbled.contains(agent));
        self.humans.retain(|person| !bubbled.contains(person));
    }

    fn add(&mut self, agent: &str, root: Root) {
        let activity = self
            .agents
            .entry(agent.to_string())
            .or_insert_with(|| CurrentActivity {
                label: "Working".into(),
                roots: Vec::new(),
            });
        if !activity.roots.contains(&root) {
            activity.roots.push(root);
        }
    }
}

/// Where an agent works, for the chooser's rows in a channel:
/// "Channel conversation", "Thread · {excerpt}", "Work with unconfirmed
/// thread", at most two, then "+N more".
pub fn scope_label(roots: &[Root], excerpt: impl Fn(&str) -> Option<String>) -> String {
    let mut labels: Vec<String> = roots
        .iter()
        .take(2)
        .map(|root| match root {
            Some(None) => "Channel conversation".to_string(),
            None => "Work with unconfirmed thread".to_string(),
            Some(Some(root)) => {
                let text = excerpt(root)
                    .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "))
                    .map(|text| text.chars().take(60).collect::<String>())
                    .filter(|text| !text.is_empty())
                    .unwrap_or_else(|| root.chars().take(8).collect());
                format!("Thread · {text}")
            }
        })
        .collect();
    if roots.len() > 2 {
        labels.push(format!("+{} more", roots.len() - 2));
    }
    labels.join(" / ")
}

/// "Alice is working", "Alice, Bob are working".
pub fn working_label(names: &[SharedString]) -> String {
    let verb = if names.len() == 1 {
        " is working"
    } else {
        " are working"
    };
    let names: Vec<&str> = names.iter().map(SharedString::as_ref).collect();
    format!("{}{verb}", names.join(", "))
}

/// Down Arrow on a trigger whose click opens a panel previews the list.
pub fn opens_preview(key: &str, modified: bool, direct: bool) -> bool {
    direct && key == "down" && !modified
}

/// Three dots fading to 35% and back over 1.4 s, 0.2 s apart.
pub fn dots(color: Hsla, cx: &App) -> AnyElement {
    const PERIOD: f32 = 1.4;
    let row = div().flex().flex_none().items_center().gap(px(2.));
    let dot = move |opacity: f32| div().size(px(4.)).rounded_full().bg(color).opacity(opacity);
    if cx.reduce_motion() {
        return row.children((0..3).map(|_| dot(1.))).into_any_element();
    }
    row.with_animation(
        "typing-dots",
        Animation::new(Duration::from_millis(1400)).repeat(),
        move |row, delta| {
            row.children((0..3).map(|n| {
                let phase = (delta - n as f32 * 0.2 / PERIOD).rem_euclid(1.);
                // Down to the 50% keyframe and back, eased both ways.
                let fade = ease_in_out(1. - (2. * phase - 1.).abs());
                dot(1. - 0.65 * fade)
            }))
        },
    )
    .into_any_element()
}

/// Opens the conversation an agent works in: its thread, or the channel.
pub type OnOpen = Rc<dyn Fn(Option<EventId>, &mut Window, &mut App)>;

/// One agent in the chooser.
#[derive(Clone)]
struct Row {
    pubkey: String,
    name: SharedString,
    picture: Option<String>,
    /// What it does now ("Working", "Reading main.rs").
    status: String,
    /// Where it works, in a channel view.
    scope: Option<String>,
    /// The thread its conversation opens, when it works in exactly one.
    thread: Option<EventId>,
    /// Its activity panel: the thread when known, else the channel.
    activity: String,
}

/// The viewer's working agents: an avatar stack that opens "Working now"
/// on hover or click (`AgentChooser`).
pub struct AgentChooser {
    rows: Vec<Row>,
    media: Option<Media>,
    on_open: Option<OnOpen>,
    open: bool,
    /// The trigger or the list is under the pointer.
    hovered: (bool, bool),
    /// A click on a single agent opens its activity panel instead of the
    /// list (`clickOpensPanel`), as buzz-app's personal workspace does.
    pub click_opens_panel: bool,
    /// In a reader, where nothing opens: neither a thread nor activity.
    pub closed: bool,
    /// The list was opened from the keyboard and has focus.
    keyboard: bool,
    /// Focus goes back to the stack on the next frame.
    refocus: bool,
    trigger_focus: FocusHandle,
    list_focus: FocusHandle,
    _pending: Option<Task<()>>,
}

impl AgentChooser {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            rows: Vec::new(),
            media: None,
            on_open: None,
            open: false,
            hovered: (false, false),
            click_opens_panel: false,
            closed: false,
            keyboard: false,
            refocus: false,
            trigger_focus: cx.focus_handle(),
            list_focus: cx.focus_handle(),
            _pending: None,
        }
    }

    /// The activity a click opens at once: one agent, where clicks open
    /// panels. Unknown or several scopes open channel activity.
    fn direct_target(&self) -> Option<String> {
        match self.rows.as_slice() {
            [row] if self.click_opens_panel && !self.closed => Some(row.activity.clone()),
            _ => None,
        }
    }

    /// Follows the activity in `channel`. `thread` is the panel's root, or
    /// `None` in a channel, where `excerpt` names threads from the loaded rows. Names
    /// are the conversation's scoped names; pictures come from `profiles`.
    pub fn set(
        &mut self,
        activity: &Activity,
        (channel, thread): (&str, Option<&str>),
        (profiles, names, media): (&Profiles, &Names, &Media),
        excerpt: impl Fn(&str) -> Option<String>,
        on_open: OnOpen,
        cx: &mut Context<Self>,
    ) {
        let rows: Vec<Row> = activity
            .agents
            .iter()
            .map(|(pubkey, detail)| {
                let known: Vec<&String> = detail.roots.iter().flatten().flatten().collect();
                let conversation = thread
                    .map(str::to_string)
                    .or_else(|| (known.len() == 1).then(|| known[0].clone()));
                // Unknown or several scopes open channel activity.
                let root = thread
                    .map(str::to_string)
                    .or_else(|| match detail.roots.as_slice() {
                        [Some(Some(root))] => Some(root.clone()),
                        _ => None,
                    });
                Row {
                    pubkey: pubkey.clone(),
                    name: names.display(pubkey).into(),
                    picture: profiles.get(pubkey).and_then(|p| p.picture.clone()),
                    status: detail.label.clone(),
                    scope: thread
                        .is_none()
                        .then(|| scope_label(&detail.roots, &excerpt)),
                    thread: conversation.and_then(|root| EventId::from_hex(&root).ok()),
                    activity: crate::agent_activity::target(pubkey, Some(channel), root.as_deref()),
                }
            })
            .collect();
        if rows.is_empty() && self.open {
            self.open = false;
            self.keyboard = false;
        }
        self.rows = rows;
        self.media = Some(media.clone());
        self.on_open = Some(on_open);
        cx.notify();
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        self._pending = None;
        // A keyboard preview hands focus back to the stack.
        self.refocus |= std::mem::take(&mut self.keyboard);
        if self.open {
            self.open = false;
            cx.notify();
        }
    }

    /// Down Arrow: open the list now and move focus into it.
    fn preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self._pending = None;
        self.keyboard = true;
        self.open = true;
        window.focus(&self.list_focus, cx);
        cx.notify();
    }

    fn hover(&mut self, trigger: bool, hovered: bool, cx: &mut Context<Self>) {
        if trigger {
            self.hovered.0 = hovered;
        } else {
            self.hovered.1 = hovered;
        }
        let inside = self.hovered.0 || self.hovered.1;
        // A keyboard preview stays until Escape or a choice.
        if inside == self.open || self.keyboard {
            self._pending = None;
            return;
        }
        let delay = if inside { OPEN_DELAY } else { CLOSE_DELAY };
        self._pending = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update(cx, |this, cx| {
                let inside = this.hovered.0 || this.hovered.1;
                if this.open != inside && !this.rows.is_empty() {
                    this.open = inside;
                    cx.notify();
                }
            });
        }));
    }

    fn avatar(&self, row: &Row, size: f32) -> Avatar {
        let mut avatar = Avatar::new(row.name.clone())
            .size(AvatarSize::Px(size))
            .squircle();
        if let (Some(picture), Some(media)) = (&row.picture, &self.media) {
            avatar = avatar.image(media::avatar(picture, media));
        }
        avatar
    }

    fn list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let rows = self.rows.iter().map(|row| {
            let (target, on_open) = (row.thread, self.on_open.clone());
            let (chooser, viewer) = (cx.entity().downgrade(), cx.entity().downgrade());
            let activity = row.activity.clone();
            AgentActivityRow {
                id: SharedString::from(format!("working-agent-{}", row.pubkey)),
                name: row.name.clone(),
                picture: match (&row.picture, &self.media) {
                    (Some(picture), Some(media)) => Some(media::avatar(picture, media)),
                    _ => None,
                },
                status: row.status.clone().into(),
                meta: None,
                scope: row.scope.clone().map(Into::into),
                open_label: format!(
                    "Open {} for {}",
                    if row.thread.is_some() {
                        "thread"
                    } else {
                        "conversation"
                    },
                    row.name
                )
                .into(),
                on_open: (!self.closed).then(|| -> Action {
                    Rc::new(move |window, cx| {
                        let _ = chooser.update(cx, |this, cx| this.close(cx));
                        if let Some(on_open) = &on_open {
                            on_open(target, window, cx);
                        }
                    })
                }),
                on_open_activity: (!self.closed).then(|| -> Action {
                    Rc::new(move |window, cx| {
                        let _ = viewer.update(cx, |this, cx| this.close(cx));
                        navigation::open_url(&activity, window, cx);
                    })
                }),
            }
        });
        Popover::new()
            .width(PopoverWidth::Wide)
            .padding(PopoverPadding::List)
            .child(
                div()
                    .id("working-now")
                    .flex()
                    .flex_col()
                    .max_h(px(360.))
                    .overflow_y_scroll()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .min_h(px(28.))
                            .px(px(8.))
                            .py(px(2.))
                            .text_body_sm()
                            .text_color(c.text_metadata)
                            .child("Working now"),
                    )
                    .children(rows),
            )
    }
}

impl Render for AgentChooser {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.refocus) {
            window.focus(&self.trigger_focus, cx);
        }
        if self.rows.is_empty() {
            return div().into_any_element();
        }
        let c = *cx.colors();
        let overflow = self.rows.len().saturating_sub(STACK);
        // 24px tiles, each 4px over the last (`margin-inline-end:
        // -space-1`). Placed absolutely: Taffy lets negative margins push
        // the stack past the composer's edge.
        let shown = self.rows.len().min(STACK);
        let tiles = div()
            .relative()
            .flex_none()
            .h(px(24.))
            .w(px(24. + 20. * shown.saturating_sub(1) as f32))
            .children(
                self.rows
                    .iter()
                    .take(STACK)
                    .enumerate()
                    .map(|(index, row)| {
                        div()
                            .absolute()
                            .top_0()
                            .left(px(20. * index as f32))
                            .size(px(24.))
                            .p(px(2.))
                            .rounded(buzz_ui::tokens::radius::CONTROL)
                            .bg(c.surface_panel)
                            .shadow(shadow::sm())
                            .child(self.avatar(row, 20.))
                    }),
            );
        let stack = div()
            .flex()
            .items_center()
            .pr(px(4.))
            .child(tiles)
            .when(overflow > 0, |d| {
                d.child(
                    div()
                        .pl(px(8.))
                        .text_caption()
                        .text_color(c.text_subtle)
                        .child(format!("+{overflow}")),
                )
            });
        let label = match self.rows.as_slice() {
            [one] => format!("Activity: {} working", one.name),
            rows => format!("Activity: {} agents working", rows.len()),
        };
        let direct = self.direct_target();
        let has_direct = direct.is_some();
        let trigger = div()
            .id("agent-activity")
            .track_focus(&self.trigger_focus)
            .role(Role::Button)
            .aria_label(label)
            .when(has_direct, |d| {
                d.aria_description("Press Down Arrow for working agents and conversation actions.")
            })
            .flex()
            .items_center()
            .min_h(px(32.))
            .p(px(4.))
            .rounded(buzz_ui::tokens::radius::CONTROL)
            .cursor_pointer()
            .when(self.open, |d| d.bg(c.affordance_subtle_hover))
            .hover(|s| s.bg(c.affordance_subtle_hover))
            .child(stack)
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| this.hover(true, *hovered, cx)))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                let keys = &event.keystroke;
                if opens_preview(&keys.key, keys.modifiers.modified(), has_direct) {
                    cx.stop_propagation();
                    this.preview(window, cx);
                }
            }))
            .on_click(cx.listener(move |this, _, window, cx| {
                this._pending = None;
                match &direct {
                    // Hover previews the list; a click goes to the activity.
                    Some(target) => {
                        this.close(cx);
                        navigation::open_url(target, window, cx);
                    }
                    None => {
                        this.open = !this.open;
                        cx.notify();
                    }
                }
            }));
        div()
            .relative()
            .child(trigger)
            .when(self.open, |d| {
                d.child(
                    div().absolute().top_0().right_0().child(
                        deferred(
                            anchored()
                                .anchor(Anchor::BottomRight)
                                .offset(point(px(0.), px(-8.)))
                                .snap_to_window_with_margin(px(8.))
                                .child(
                                    div()
                                        .id("agent-activity-list")
                                        .track_focus(&self.list_focus)
                                        .occlude()
                                        .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                                            this.hover(false, *hovered, cx)
                                        }))
                                        .on_mouse_down_out(
                                            cx.listener(|this, _, _, cx| this.close(cx)),
                                        )
                                        .child(self.list(cx)),
                                ),
                        )
                        .with_priority(2),
                    ),
                )
            })
            .into_any_element()
    }
}

/// A zero-height anchor for the top edge of the composer: the activity
/// sits 8px above it, inset 16px (12px in narrow windows), over the
/// bottom of the timeline. `None` when nobody is active.
pub fn overlay(
    humans: &[String],
    agents: Option<Entity<AgentChooser>>,
    names: &Names,
    narrow: bool,
    cx: &App,
) -> Option<AnyElement> {
    let agents = agents.filter(|chooser| !chooser.read(cx).rows.is_empty());
    if humans.is_empty() && agents.is_none() {
        return None;
    }
    let c = cx.colors();
    let names: Vec<SharedString> = humans
        .iter()
        .map(|pubkey| SharedString::from(names.display(pubkey)))
        .collect();
    // The live region reads "X is working. Y is typing".
    let working: Vec<SharedString> = agents
        .iter()
        .flat_map(|chooser| chooser.read(cx).rows.iter().map(|row| row.name.clone()))
        .collect();
    let status = [
        (!working.is_empty()).then(|| working_label(&working)),
        (!names.is_empty()).then(|| label(&names)),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(". ");
    let pill = (!humans.is_empty()).then(|| {
        div()
            .mr_auto()
            .max_w(relative(0.5))
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(8.))
            .py(px(4.))
            .rounded(px(12.))
            .bg(c.surface_panel)
            .text_caption()
            .text_color(c.text_subtle)
            .child(dots(c.text_subtle, cx))
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(label(&names)),
            )
    });
    let inset = if narrow { px(12.) } else { px(16.) };
    Some(
        div()
            .absolute()
            .top_0()
            .left(inset)
            .right(inset)
            .h_0()
            .child(
                div()
                    .id("conversation-activity")
                    .role(Role::Status)
                    .aria_label(status)
                    .absolute()
                    .bottom(px(8.))
                    .left_0()
                    .right_0()
                    .flex()
                    .flex_row_reverse()
                    .justify_between()
                    .items_center()
                    .gap(px(6.))
                    .children(agents)
                    .children(pill),
            )
            .into_any_element(),
    )
}

/// Whether the composer shows an agent chooser, which moves the jump pill
/// into its own lane.
pub fn has_agents(chooser: &Entity<AgentChooser>, cx: &App) -> bool {
    !chooser.read(cx).rows.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;
    use gpui_kit::SharedString;

    #[test]
    fn labels_follow_the_reference() {
        let names: Vec<SharedString> = ["Alice", "Bob", "Carol", "Dan"]
            .into_iter()
            .map(SharedString::from)
            .collect();
        assert_eq!(label(&names[..1]), "Alice is typing");
        assert_eq!(label(&names[..2]), "Alice, Bob are typing");
        assert_eq!(label(&names), "Alice, Bob and 2 others are typing");
    }

    #[test]
    fn splits_owned_agents_from_people() {
        let owned: HashSet<String> = ["agent".to_string()].into();
        let typing = vec!["alice".to_string(), "agent".to_string()];
        let threads = HashMap::from([
            (
                "root".to_string(),
                vec!["agent".to_string(), "bob".to_string()],
            ),
            ("elsewhere".to_string(), vec!["agent".to_string()]),
        ]);
        let activity = Activity::channel(&typing, &threads, |root| root == "root", &owned);
        assert_eq!(activity.humans, vec!["alice".to_string()]);
        assert_eq!(
            activity.agents["agent"].roots,
            vec![Some(None), Some(Some("root".to_string()))]
        );
        let thread = Activity::thread(&threads["root"], "root", &owned);
        assert_eq!(thread.humans, vec!["bob".to_string()]);
        assert_eq!(
            thread.agents["agent"].roots,
            vec![Some(Some("root".to_string()))]
        );
    }

    #[test]
    fn observed_turns_join_typing_with_their_labels() {
        let owned: HashSet<String> = ["agent".to_string()].into();
        let typing = vec!["agent".to_string(), "helper".to_string()];
        let observed = BTreeMap::from([
            (
                "agent".to_string(),
                CurrentActivity {
                    label: "Reading main.rs".into(),
                    roots: vec![Some(None)],
                },
            ),
            (
                "helper".to_string(),
                CurrentActivity {
                    label: "Working".into(),
                    roots: vec![None],
                },
            ),
        ]);
        let activity =
            Activity::channel(&typing, &HashMap::new(), |_| false, &owned).observe(observed);
        assert_eq!(activity.agents["agent"].label, "Reading main.rs");
        assert_eq!(activity.agents["agent"].roots, vec![Some(None)]);
        // An agent at work is not "typing", even when it is not the viewer's.
        assert!(activity.humans.is_empty());
        assert_eq!(activity.agents["helper"].roots, vec![None]);
        assert_eq!(
            working_label(&["Ada".into(), "Bo".into()]),
            "Ada, Bo are working"
        );
    }

    #[test]
    fn down_arrow_previews_only_a_clickable_stack() {
        assert!(opens_preview("down", false, true));
        assert!(!opens_preview("down", false, false), "hover shows the list");
        assert!(!opens_preview("down", true, true));
        assert!(!opens_preview("up", false, true));
    }

    #[test]
    fn scopes_name_the_channel_or_threads() {
        let excerpt = |root: &str| (root == "a").then(|| "Fix   the\nbuild".to_string());
        let thread = |root: &str| Some(Some(root.to_string()));
        assert_eq!(scope_label(&[Some(None)], excerpt), "Channel conversation");
        assert_eq!(
            scope_label(&[None], excerpt),
            "Work with unconfirmed thread"
        );
        assert_eq!(
            scope_label(&[thread("a"), thread("bcdefghijk")], excerpt),
            "Thread · Fix the build / Thread · bcdefghi"
        );
        assert_eq!(
            scope_label(&[Some(None), thread("a"), thread("c")], excerpt),
            "Channel conversation / Thread · Fix the build / +1 more"
        );
    }
}
