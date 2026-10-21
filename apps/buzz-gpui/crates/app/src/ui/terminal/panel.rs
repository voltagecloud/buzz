//! The Terminal drawer (`TerminalPanel.tsx`): a "BuzzTerm" header naming
//! the channel, thread and status, Restart / End session / Hide, and the
//! channel's shell under it.
//!
//! The port adds one control: when remote hosts are configured, a host
//! button before Restart picks the machine the channel's shell runs on
//! ("This computer" until one is picked). Picking another host restarts
//! the shell there.

use super::sessions::{Key, PanelContext, Sessions, Source, Status};
use super::view::{Input, Sink, TerminalView};
use crate::ui::channel_panels::Close;
use buzz_ui::{
    ActiveTheme, Button, ButtonSize, ButtonVariant, Icon, IconButton, IconButtonSize, IconName,
    Menu, MenuItem, TypeStyled,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::rc::Rc;

pub struct TerminalPanel {
    context: PanelContext,
    key: Key,
    close: Close,
    sessions: Entity<Sessions>,
    /// The mounted screen and the view showing it.
    view: Option<(EntityId, Entity<TerminalView>)>,
    error: Option<String>,
    busy: bool,
    picking_host: bool,
    _observe: Subscription,
}

impl TerminalPanel {
    pub fn new(context: PanelContext, close: Close, cx: &mut Context<Self>) -> Self {
        let sessions = Sessions::global(cx);
        let observe = cx.observe(&sessions, |_, _, cx| cx.notify());
        let key = context.key();
        // Showing the panel starts the channel's shell, once.
        let error = sessions
            .update(cx, |sessions, cx| sessions.ensure(&context, true, cx))
            .err();
        Self {
            context,
            key,
            close,
            sessions,
            view: None,
            error,
            busy: false,
            picking_host: false,
            _observe: observe,
        }
    }

    /// End, then for Restart start again with the channel as it is now.
    fn end(&mut self, restart: bool, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.error = None;
        let ending = self
            .sessions
            .update(cx, |sessions, cx| sessions.end(&self.key, cx));
        cx.notify();
        cx.spawn(async move |this, cx| {
            let ended = ending.await;
            let _ = this.update(cx, |this, cx| {
                match ended {
                    Ok(()) if restart => {
                        let context = this.context.clone();
                        if let Err(error) = this
                            .sessions
                            .update(cx, |sessions, cx| sessions.ensure(&context, false, cx))
                        {
                            this.error = Some(error);
                        }
                    }
                    Ok(()) => {}
                    Err(error) => this.error = Some(error),
                }
                this.busy = false;
                cx.notify();
            });
        })
        .detach();
    }

    fn choose_host(&mut self, host: EntityId, cx: &mut Context<Self>) {
        self.picking_host = false;
        let running_elsewhere = self
            .sessions
            .read(cx)
            .get(&self.key)
            .is_some_and(|session| session.host != host);
        let key = self.key.clone();
        self.sessions
            .update(cx, |sessions, cx| sessions.choose_host(key, host, cx));
        if running_elsewhere {
            self.end(true, cx);
        }
        cx.notify();
    }

    /// The view for the session's screen, made when a screen mounts.
    fn screen_view(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<TerminalView>> {
        let screen = self
            .sessions
            .read(cx)
            .get(&self.key)
            .and_then(|session| session.screen.clone());
        let Some(screen) = screen else {
            self.view = None;
            return None;
        };
        if let Some((id, view)) = &self.view
            && *id == screen.entity_id()
        {
            return Some(view.clone());
        }
        let sessions = self.sessions.downgrade();
        let key = self.key.clone();
        let sink: Sink = Rc::new(move |input, cx| {
            let _ = sessions.update(cx, |sessions, cx| match input {
                Input::User(bytes) => sessions.write(&key, bytes, Source::User, cx),
                Input::Reply(bytes) => sessions.write(&key, bytes, Source::Reply, cx),
                Input::Resize(cols, rows) => sessions.resize(&key, cols, rows),
                Input::Cell(width, height) => sessions.cell = (width, height),
            });
        });
        let id = screen.entity_id();
        let view = cx.new(|cx| TerminalView::new(screen, sink, window, cx));
        // A screen takes focus when it mounts, as xterm does.
        let focus = view.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        self.view = Some((id, view.clone()));
        Some(view)
    }

    fn host_picker(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let hosts = Sessions::hosts(cx);
        if hosts.len() < 2 {
            return None;
        }
        let sessions = self.sessions.read(cx);
        let current = sessions
            .get(&self.key)
            .map(|session| session.host)
            .or_else(|| {
                sessions
                    .host_for(&self.key, cx)
                    .map(|host| host.entity_id())
            });
        let label = hosts
            .iter()
            .find(|host| Some(host.entity_id()) == current)
            .map(|host| host.read(cx).label.clone())
            .unwrap_or_default();
        let menu = self.picking_host.then(|| {
            let items = hosts.iter().map(|host| {
                let id = host.entity_id();
                MenuItem::new(
                    SharedString::from(format!("terminal-host-{id}")),
                    host.read(cx).label.clone(),
                )
                .checked(Some(id) == current)
                .on_click(cx.listener(move |this, _, _, cx| this.choose_host(id, cx)))
            });
            deferred(
                anchored()
                    .anchor(Anchor::TopRight)
                    .snap_to_window_with_margin(px(8.))
                    .child(
                        div()
                            .id("terminal-host-menu")
                            .occlude()
                            .mt(px(4.))
                            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                                this.picking_host = false;
                                cx.notify();
                            }))
                            .child(Menu::new().children(items)),
                    ),
            )
            .with_priority(2)
        });
        let mut button = Button::new("terminal-host", label)
            .size(ButtonSize::Sm)
            .variant(ButtonVariant::Ghost)
            .icon_end(IconName::CaretDown)
            .selected(self.picking_host)
            .on_click(cx.listener(|this, _, _, cx| {
                this.picking_host = !this.picking_host;
                cx.notify();
            }));
        if !self.picking_host {
            button = button.tooltip("Where this terminal runs");
        }
        Some(
            div()
                .relative()
                .child(button)
                .children(menu.map(|menu| div().absolute().bottom_0().right_0().child(menu)))
                .into_any_element(),
        )
    }
}

impl Render for TerminalPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let available = Sessions::available(cx);
        let view = self.screen_view(window, cx);
        let sessions = self.sessions.read(cx);
        let entry = sessions.get(&self.key);
        let context = entry.map_or(&self.context, |entry| &entry.context);
        let channel_id = context.channel_id.clone();
        let channel_name = context.channel_name.clone();
        let thread = entry.and_then(|entry| entry.context.thread_id.clone());
        let status = entry.map(|entry| entry.status);
        let status_label = match status {
            Some(status) => status.label(),
            None if available => "ended",
            None => "desktop only",
        };
        let entry_error = entry.and_then(|entry| entry.error.clone());
        let has_entry = entry.is_some();
        let busy = self.busy;

        let secondary = |id: &'static str, text: String, tooltip: Option<String>| {
            div()
                .id(id)
                .flex_shrink(1.)
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_body_sm()
                .text_color(c.text_subtle)
                .when_some(tooltip, |this, tooltip| {
                    this.tooltip(buzz_ui::Tooltip::text(tooltip))
                })
                .child(text)
        };
        let identity = div()
            .flex()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .items_center()
            .gap(rems(0.5))
            .child(secondary(
                "terminal-channel",
                format!("#{channel_name}"),
                Some(channel_id),
            ))
            .when_some(thread, |this, thread| {
                this.child(
                    div()
                        .id("terminal-thread")
                        .flex_shrink(1.)
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_mono()
                        .text_color(c.text_subtle)
                        .tooltip(buzz_ui::Tooltip::text(thread.clone()))
                        .child(format!("Thread {}", &thread[..thread.len().min(8)])),
                )
            })
            .child(secondary("terminal-status", status_label.into(), None));
        let label = div()
            .flex()
            .flex_1()
            .min_w_0()
            .items_center()
            .gap(rems(0.5))
            .px(rems(0.5))
            .text_body_sm()
            .text_color(c.text_standard)
            .child(Icon::new(IconName::TerminalWindow).size(16.))
            .child(div().flex_none().child("BuzzTerm"))
            .child(identity);

        let mut actions: Vec<AnyElement> = Vec::new();
        if available {
            actions.extend(self.host_picker(cx));
            if has_entry {
                actions.push(
                    IconButton::new("terminal-restart", IconName::ArrowsClockwise)
                        .size(IconButtonSize::Sm)
                        .disabled(busy)
                        .tooltip("Restart terminal")
                        .on_click(cx.listener(|this, _, _, cx| this.end(true, cx)))
                        .into_any_element(),
                );
            } else {
                actions.push(
                    Button::new("terminal-start", "Start session")
                        .size(ButtonSize::Sm)
                        .disabled(busy)
                        .on_click(cx.listener(|this, _, _, cx| this.end(true, cx)))
                        .into_any_element(),
                );
            }
        }
        if has_entry {
            actions.push(
                IconButton::new("terminal-end", IconName::Stop)
                    .size(IconButtonSize::Sm)
                    .disabled(busy)
                    .tooltip("End session")
                    .on_click(cx.listener(|this, _, _, cx| this.end(false, cx)))
                    .into_any_element(),
            );
        }
        let close = self.close.clone();
        actions.push(
            IconButton::new("terminal-hide", IconName::CaretDown)
                .size(IconButtonSize::Sm)
                .tooltip("Hide terminal (Cmd/Ctrl+J)")
                .on_click(move |_, window, cx| close(window, cx))
                .into_any_element(),
        );

        // `PanelHeader`: 40px, with its hairline inset 4px at the bottom.
        let header = div()
            .relative()
            .flex()
            .flex_none()
            .items_center()
            .gap(rems(0.5))
            .min_h(rems(2.5))
            .px(rems(0.25))
            .child(label)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(rems(0.25))
                    .children(actions),
            )
            .child(
                div()
                    .absolute()
                    .bottom_0()
                    .left(rems(0.25))
                    .right(rems(0.25))
                    .h(px(1.))
                    .bg(c.border_standard),
            );

        let notice = |text: SharedString| {
            div()
                .px(rems(0.75))
                .py(rems(0.5))
                .text_body()
                .text_color(c.text_standard)
                .child(text)
        };
        let body = if !available {
            vec![
                notice(
                    "Open Buzz Desktop to use your local shell. The browser cannot start a terminal on your computer."
                        .into(),
                )
                .into_any_element(),
            ]
        } else {
            let mut body = Vec::new();
            if let Some(error) = self.error.clone().or(entry_error) {
                body.push(
                    notice(error.into())
                        .text_color(c.text_danger)
                        .into_any_element(),
                );
            }
            if status == Some(Status::Starting) {
                body.push(notice("Starting your login shell…".into()).into_any_element());
            }
            if !has_entry {
                body.push(
                    notice(
                        "Session ended. Start a new shell to use this channel’s current context."
                            .into(),
                    )
                    .into_any_element(),
                );
            }
            body.push(
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .p(rems(0.5))
                    // Escape belongs to the shell while it has focus.
                    .on_key_down(|event: &KeyDownEvent, _, cx| {
                        if event.keystroke.key == "escape" {
                            cx.stop_propagation();
                        }
                    })
                    .children(view)
                    .into_any_element(),
            );
            body
        };

        div()
            .id("terminal-panel")
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .bg(c.surface_panel)
            .text_color(c.text_standard)
            .child(header)
            .children(body)
    }
}
