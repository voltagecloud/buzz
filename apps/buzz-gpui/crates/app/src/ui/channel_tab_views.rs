//! The views behind a conversation's tabs, shared by the Messages board
//! and a conversation open in Me: made when a tab first shows on a visit,
//! and dropped with it. A thread tab's view is the same entity in either
//! pane, so moving the tab keeps its reply, scroll position and branches.

use crate::community::Community;
use crate::ui::channel::{ChannelPage, OpenThread};
use crate::ui::channel_panels::{self, ChannelPanel};
use crate::ui::channel_settings::ChannelSettingsPanel;
use crate::ui::channel_tabs::{TabKind, TabPane, TabState, ThreadTab};
use crate::ui::message::channel_icon;
use crate::ui::thread::{self, ThreadPanel};
use buzz_relay_client::{ChannelSummary, ChannelType};
use buzz_ui::IconName;
use gpui_kit::*;
use nostr::prelude::EventId;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

/// A tab's view, made when the tab first shows on this visit.
pub enum TabView {
    /// The channel's settings; only the Messages board has this tab.
    Settings {
        panel: Entity<ChannelSettingsPanel>,
        _subscription: Subscription,
    },
    Conversation(Entity<ChannelPage>),
    Thread {
        panel: Entity<ThreadPanel>,
        /// Reports the reader's verified root to the tab.
        _root: Subscription,
    },
    Panel(AnyView),
}

impl TabView {
    pub fn thread(&self) -> Option<&Entity<ThreadPanel>> {
        match self {
            TabView::Thread { panel, .. } => Some(panel),
            _ => None,
        }
    }
}

/// A view that shows a conversation with its tabs.
pub trait TabHost: Sized + 'static {
    fn community(&self) -> Option<Entity<Community>>;
    /// The conversation the tabs belong to.
    fn channel_id(&self, cx: &App) -> Option<String>;
    /// That conversation's own page, whose rows name its threads' tabs.
    fn page(&self) -> Option<Entity<ChannelPage>>;
    fn tab_views(&mut self) -> &mut HashMap<String, TabView>;
    fn tabs(&self, cx: &App) -> TabState;
    /// Changes the conversation's tabs, then drops the views of tabs that
    /// closed.
    fn update_tabs(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut TabState));
    fn close_tab(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>);
    fn focus_tab(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>);
    /// Where a thread opened from another conversation's tab goes: the
    /// side pane, unless this host has a main pane for threads.
    fn thread_pane(&self, cx: &App) -> TabPane;
    /// Makes a registered panel's view, opened from `channel` (and the
    /// thread `root` there) to show `target` and closed through `close`.
    #[allow(clippy::too_many_arguments)]
    fn panel_view(
        &self,
        panel: &ChannelPanel,
        channel: String,
        target: String,
        root: Option<String>,
        close: channel_panels::Close,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyView>;
}

/// The conversation a thread tab reads: its own conversation even as a
/// preview (reading a thread is not write access), else one a tab may
/// show (`tabChannel`).
pub fn thread_channel<T: TabHost>(
    this: &T,
    thread: &ThreadTab,
    cx: &App,
) -> Option<ChannelSummary> {
    let community = this.community()?;
    if this.channel_id(cx).as_deref() == Some(thread.channel.as_str()) {
        let page = this.page()?;
        let channel = page.read(cx).channel();
        return (!channel.cached).then(|| channel.clone());
    }
    usable(&community, &thread.channel, cx)
}

/// Makes the views of tabs that have none yet. A thread tab whose
/// message changed has its view select it in place, keeping the reply
/// being written; until the view knows its root, a new view opens it.
/// Mounts the composers of the conversation and every tab, selected or
/// not, as buzz-app keeps each tab's content mounted.
pub fn mount_composers<T: TabHost>(this: &mut T, window: &mut Window, cx: &mut Context<T>) {
    if let Some(page) = this.page() {
        page.update(cx, |page, cx| page.mount_composer(window, cx));
    }
    let views: Vec<_> = this
        .tab_views()
        .values()
        .filter_map(|view| match view {
            TabView::Conversation(page) => Some((Some(page.clone()), None)),
            TabView::Thread { panel, .. } => Some((None, Some(panel.clone()))),
            _ => None,
        })
        .collect();
    for (page, panel) in views {
        if let Some(page) = page {
            page.update(cx, |page, cx| page.mount_composer(window, cx));
        }
        if let Some(panel) = panel {
            panel.update(cx, |panel, cx| panel.mount_composer(window, cx));
        }
    }
}

pub fn ensure_views<T: TabHost>(this: &mut T, window: &mut Window, cx: &mut Context<T>) {
    let (Some(community), Some(_)) = (this.community(), this.channel_id(cx)) else {
        return;
    };
    let state = this.tabs(cx);
    for tab in &state.tabs {
        let Some(thread) = tab.thread() else {
            continue;
        };
        let Some(panel) = this
            .tab_views()
            .get(&tab.id)
            .and_then(TabView::thread)
            .cloned()
        else {
            continue;
        };
        if panel.read(cx).asked() == thread.message {
            continue;
        }
        let kept = panel.update(cx, |panel, cx| panel.select(thread.message, window, cx));
        if !kept {
            // Media hook: a clip started in the replaced reader stops.
            super::media::pause_audio(Some(panel.entity_id()), cx);
            this.tab_views().remove(&tab.id);
        }
    }
    for tab in &state.tabs {
        if this.tab_views().contains_key(&tab.id) {
            continue;
        }
        let view = match &tab.kind {
            TabKind::Conversation { channel } => {
                let Some(summary) = usable(&community, channel, cx) else {
                    continue;
                };
                let weak = cx.entity().downgrade();
                let channel_id = channel.clone();
                let open_thread: OpenThread = Arc::new(move |row, _, window, cx| {
                    let channel = channel_id.clone();
                    let root = row.thread_root_id.unwrap_or(row.id);
                    let _ = weak.update(cx, |this, cx| {
                        open_conversation_thread(this, channel, row.id, root, window, cx)
                    });
                });
                let community = community.clone();
                TabView::Conversation(cx.new(|cx| {
                    let mut page = ChannelPage::new(community, summary, open_thread, window, cx);
                    page.secondary = true;
                    page
                }))
            }
            TabKind::Thread(thread) => {
                let Some(summary) = thread_channel(this, thread, cx) else {
                    continue;
                };
                thread_view(&community, &tab.id, thread, summary, window, cx)
            }
        };
        this.tab_views().insert(tab.id.clone(), view);
    }
    // A channel-menu panel's tab stays only in its own channel while the
    // menu would offer it there (`channelContext` and `eligible`).
    let current = this.channel_id(cx).and_then(|id| {
        community
            .read(cx)
            .snapshot
            .channels
            .iter()
            .find(|channel| channel.id == id)
            .cloned()
    });
    let ineligible: Vec<String> = state
        .panels
        .iter()
        .filter(|opening| {
            channel_panels::get(opening.panel, cx)
                .and_then(|panel| panel.menu)
                .is_some_and(|menu| {
                    !current.as_ref().is_some_and(|channel| {
                        channel.id == opening.channel
                            && !channel.cached
                            && !channel.read_only
                            && (menu.eligible)(channel, cx)
                    })
                })
        })
        .map(|opening| opening.id())
        .collect();
    if !ineligible.is_empty() {
        this.update_tabs(cx, |state| {
            state.panels.retain(|tab| !ineligible.contains(&tab.id()));
            if ineligible.contains(&state.selected) {
                state.select(crate::ui::channel_tabs::THREAD);
            }
        });
    }
    for opening in &state.panels {
        let id = opening.id();
        if this.tab_views().contains_key(&id) || ineligible.contains(&id) {
            continue;
        }
        let Some(panel) = channel_panels::get(opening.panel, cx) else {
            continue;
        };
        let weak = cx.entity().downgrade();
        let close_id = id.clone();
        let close: channel_panels::Close = Rc::new(move |window, cx| {
            let id = close_id.clone();
            let _ = weak.update(cx, |this, cx| this.close_tab(&id, window, cx));
        });
        if let Some(view) = this.panel_view(
            &panel,
            opening.channel.clone(),
            opening.target.clone(),
            opening.root.clone(),
            close,
            window,
            cx,
        ) {
            this.tab_views().insert(id, TabView::Panel(view));
        }
    }
}

/// A thread tab's reader. The tab strip names and closes it; Escape in
/// it closes the tab. Its verified root merges a tab opened from a wrong
/// hint into the tab already holding the thread, and a message found in
/// another thread opens a tab of its own (`ConversationTab`).
fn thread_view<T: TabHost>(
    community: &Entity<Community>,
    id: &str,
    thread: &ThreadTab,
    summary: ChannelSummary,
    window: &mut Window,
    cx: &mut Context<T>,
) -> TabView {
    let weak = cx.entity().downgrade();
    let close_id = id.to_string();
    let on_close: thread::OnClose = Arc::new(move |window, cx| {
        let id = close_id.clone();
        let _ = weak.update(cx, |this, cx| this.close_tab(&id, window, cx));
    });
    let weak = cx.entity().downgrade();
    let divert_id = id.to_string();
    let on_different: thread::OnDifferent = Rc::new(move |requested, kept, _, cx| {
        let id = divert_id.clone();
        let _ = weak.update(cx, |this, cx| {
            this.update_tabs(cx, |state| state.divert_thread(&id, requested, kept))
        });
    });
    let (community, channel, message) = (community.clone(), thread.channel.clone(), thread.message);
    let exact = message != thread.root_id();
    let panel = cx.new(|cx| {
        let mut panel = match exact {
            true => ThreadPanel::exact(community, channel, message, on_close, window, cx),
            false => ThreadPanel::new(community, channel, message, on_close, window, cx),
        };
        panel.on_different = Some(on_different);
        // A public preview's thread offers Join, as its conversation does.
        panel.summary = Some(summary);
        panel
    });
    let root_id = id.to_string();
    let root = cx.observe(&panel, move |this, panel, cx| {
        let Some(root) = panel.read(cx).loaded_root() else {
            return;
        };
        let known = this
            .tabs(cx)
            .tab(&root_id)
            .and_then(|tab| tab.thread().map(|thread| thread.root));
        if known.is_some_and(|known| known != Some(root)) {
            this.update_tabs(cx, |state| state.resolve_thread_root(&root_id, root));
        }
    });
    TabView::Thread { panel, _root: root }
}

/// Opens a thread of another conversation as a tab.
fn open_conversation_thread<T: TabHost>(
    this: &mut T,
    channel: String,
    message: EventId,
    root: EventId,
    window: &mut Window,
    cx: &mut Context<T>,
) {
    let pane = this.thread_pane(cx);
    let mut id = String::new();
    this.update_tabs(cx, |state| {
        id = state.open_thread_tab(&channel, message, root, pane);
    });
    this.focus_tab(&id, window, cx);
}

/// A thread tab's label: the start of its root's text, so several open
/// threads stay apart, else "Thread · channel" (`threadLabel`). `content`
/// is the root's Markdown when the conversation's rows hold it.
pub fn thread_label(content: Option<&str>, channel: Option<&str>) -> String {
    let text = content.map(plain).unwrap_or_default();
    if !text.is_empty() {
        return text;
    }
    match channel {
        Some(name) => format!("Thread · {name}"),
        None => "Thread".into(),
    }
}

/// Markdown reduced to its words: links to their text, emphasis, code
/// and heading marks dropped, whitespace collapsed, at most 80 characters.
fn plain(markdown: &str) -> String {
    let mut text = String::new();
    let mut rest = markdown;
    while let Some(start) = rest.find('[') {
        text.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let link = after.find(']').and_then(|close| {
            let label = &after[..close];
            let tail = after[close + 1..].strip_prefix('(')?;
            let end = tail.find(')')?;
            Some((label, &tail[end + 1..]))
        });
        match link {
            Some((label, tail)) => {
                text.push_str(label);
                rest = tail;
            }
            None => {
                text.push('[');
                rest = after;
            }
        }
    }
    text.push_str(rest);
    let stripped: String = text
        .chars()
        .filter(|c| !matches!(c, '*' | '_' | '`' | '~' | '>' | '#'))
        .collect();
    stripped
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(80)
        .collect()
}

/// The root's Markdown, from the rows of the conversation it is in when
/// this host shows them.
pub fn root_content<T: TabHost>(this: &T, thread: &ThreadTab, cx: &App) -> Option<String> {
    let root = thread.root_id();
    let page = if this.channel_id(cx).as_deref() == Some(thread.channel.as_str()) {
        this.page()
    } else {
        None
    };
    page.and_then(|page| page.read(cx).row_content(&root))
}

/// The conversation `id` when a tab may show it: joined, writable, not
/// archived, and for sessions only when the viewer is a member.
pub fn usable(community: &Entity<Community>, id: &str, cx: &App) -> Option<ChannelSummary> {
    let community = community.read(cx);
    let viewer = community.session.viewer().to_string();
    community
        .snapshot
        .channels
        .iter()
        .find(|channel| channel.id == id)
        .filter(|channel| {
            !channel.read_only
                && !channel.archived
                && !channel.cached
                && (channel.channel_type != Some(ChannelType::Session)
                    || channel.members.contains(&viewer))
        })
        .cloned()
}

/// A conversation tab's icon: the speech bubble for DMs, else the
/// channel's hash or lock.
pub fn tab_icon(channel: &ChannelSummary) -> IconName {
    match channel.channel_type {
        Some(ChannelType::Dm) => IconName::ChatCircle,
        _ => channel_icon(channel),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn thread_labels_read_the_roots_words() {
        assert_eq!(
            thread_label(
                Some("**Deploy** the [new build](https://x.dev)\n\n> soon"),
                None
            ),
            "Deploy the new build soon"
        );
        assert_eq!(
            thread_label(Some("# `a` ~b~ _c_"), Some("general")),
            "a b c"
        );
        assert_eq!(
            thread_label(Some("  "), Some("general")),
            "Thread · general"
        );
        assert_eq!(thread_label(None, None), "Thread");
        assert_eq!(thread_label(Some("[unclosed link"), None), "[unclosed link");
        let long = "word ".repeat(40);
        assert_eq!(thread_label(Some(&long), None).chars().count(), 80);
    }
}
