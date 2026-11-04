//! Owner actions every surface showing an Agents2 agent offers: its Build
//! view and the profile panel (buzz-app's `AgentManage.tsx`).

use crate::agents2::Agents2;
use crate::ui::overlay::{self, Dismiss};
use buzz_ui::{ActiveTheme, AlertDialog, Button, ButtonSize, IconName, Surface, TypeStyled};
use gpui_kit::*;
use std::rc::Rc;

/// The agent was deleted.
pub struct Removed;

/// "Delete agent", after a confirmation that says what it does.
pub struct AgentDelete {
    pubkey: String,
    /// Draws its own "Delete agent" button; without it, another view opens
    /// the confirmation with [`Self::ask`].
    button: bool,
    confirming: bool,
    removing: bool,
    error: Option<SharedString>,
}

impl EventEmitter<Removed> for AgentDelete {}

impl AgentDelete {
    pub fn new(pubkey: String, _: &mut Context<Self>) -> Self {
        Self {
            pubkey,
            button: true,
            confirming: false,
            removing: false,
            error: None,
        }
    }

    /// Only the confirmation, for a view that offers "Delete agent" in its
    /// own menu.
    pub fn dialog_only(pubkey: String, cx: &mut Context<Self>) -> Self {
        Self {
            button: false,
            ..Self::new(pubkey, cx)
        }
    }

    /// Asks to confirm the deletion.
    pub fn ask(&mut self, cx: &mut Context<Self>) {
        self.confirming = true;
        cx.notify();
    }

    /// Its confirmation shows.
    pub fn open(&self) -> bool {
        self.confirming
    }

    fn remove(&mut self, cx: &mut Context<Self>) {
        let Some(service) = Agents2::global(cx) else {
            return;
        };
        self.removing = true;
        self.error = None;
        let task = service.update(cx, |service, cx| service.remove(&self.pubkey, cx));
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        this.confirming = false;
                        this.removing = false;
                        cx.emit(Removed);
                    }
                    Err(error) => {
                        this.error = Some(error.into());
                        this.removing = false;
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        if !self.removing {
            self.confirming = false;
            self.error = None;
            cx.notify();
        }
    }
}

impl Render for AgentDelete {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        // Any running agent, not only the selected community's: Mission
        // control deletes agents whose community never connects.
        let name = Agents2::global(cx)
            .and_then(|service| {
                let service = service.read(cx);
                service
                    .running()
                    .iter()
                    .find(|agent| agent.pubkey == self.pubkey)
                    .map(|agent| agent.name.clone())
            })
            .unwrap_or_else(|| "this agent".into());
        let button = self.button.then(|| {
            Button::new("agent-delete", "Delete agent")
                .size(ButtonSize::Sm)
                .ghost()
                .icon(IconName::Trash)
                .on_click(cx.listener(|this, _, _, cx| this.ask(cx)))
        });
        let dialog = self.confirming.then(|| {
            let weak = cx.entity().downgrade();
            let dismiss: Dismiss = Rc::new(move |_, cx| {
                let _ = weak.update(cx, |this, cx| this.close(cx));
            });
            let dialog = AlertDialog::new(format!("Delete {name}?"))
                .description(
                    "This deletes its key from this device. Then, whenever its community is \
                     open, it leaves every channel and is archived so it no longer appears in \
                     member lists or mention suggestions. It can't be undone.",
                )
                .children(
                    self.error
                        .clone()
                        .map(|error| div().text_body_sm().text_color(c.text_danger).child(error)),
                )
                .action(
                    Button::new("agent-delete-cancel", "Cancel")
                        .surface(Surface::Floating)
                        .disabled(self.removing)
                        .on_click(cx.listener(|this, _, _, cx| this.close(cx))),
                )
                .action(
                    Button::new("agent-delete-confirm", "Delete")
                        .destructive()
                        .loading(self.removing)
                        .on_click(cx.listener(|this, _, _, cx| this.remove(cx))),
                );
            overlay::modal(window, dialog, (!self.removing).then_some(dismiss))
        });
        div().children(button).children(dialog)
    }
}
