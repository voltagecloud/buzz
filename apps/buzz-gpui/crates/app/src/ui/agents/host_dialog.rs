//! The Add host dialog: connects to a remote `buzzd`.

use super::form::field;
use crate::hosts::Hosts;
use buzz_ui::{ActiveTheme, Button, Dialog, Surface, TextField, TypeStyled};
use gpui_kit::component::input::InputState;
use gpui_kit::*;

/// The dialog closed, saved or not.
pub struct Dismissed;

pub struct AddHost {
    name: Entity<InputState>,
    address: Entity<InputState>,
    token: Entity<InputState>,
    error: Option<SharedString>,
}

impl EventEmitter<Dismissed> for AddHost {}

impl AddHost {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("devserver"));
        let address = cx.new(|cx| {
            InputState::new(window, cx).placeholder("tcp://devserver.tailnet.ts.net:7420")
        });
        let token = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder("Paste the token")
        });
        name.update(cx, |state, cx| state.focus(window, cx));
        Self {
            name,
            address,
            token,
            error: None,
        }
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let name = self.name.read(cx).value();
        let address = self.address.read(cx).value();
        let token = self.token.read(cx).value();
        match Hosts::add(&name, &address, &token, cx) {
            Ok(_) => cx.emit(Dismissed),
            Err(error) => {
                self.error = Some(error.into());
                cx.notify();
            }
        }
    }
}

impl Render for AddHost {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        Dialog::new("add-host", "Add host")
            .description(
                "Connect to buzzd on another machine to run agents there. Run `buzzd token` \
                 on that machine for its token.",
            )
            .on_close(cx.listener(|_, _, _, cx| cx.emit(Dismissed)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(rems(1.))
                    .child(field(
                        "Name",
                        None,
                        TextField::new(&self.name).surface(Surface::Floating),
                        &c,
                    ))
                    .child(field(
                        "Address",
                        Some("tcp://host:port, or unix:///path/to/buzzd.sock".into()),
                        TextField::new(&self.address).surface(Surface::Floating),
                        &c,
                    ))
                    .child(field(
                        "Token",
                        Some(
                            "Saved in this computer's keyring. Without a keyring it is \
                             saved in config.toml instead."
                                .into(),
                        ),
                        TextField::new(&self.token).surface(Surface::Floating),
                        &c,
                    ))
                    .children(
                        self.error.clone().map(|error| {
                            div().text_body_sm().text_color(c.text_danger).child(error)
                        }),
                    ),
            )
            .footer(
                Button::new("cancel-host", "Cancel")
                    .surface(Surface::Floating)
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(Dismissed))),
            )
            .footer(
                Button::new("save-host", "Add host")
                    .prominent()
                    .on_click(cx.listener(|this, _, _, cx| this.submit(cx))),
            )
    }
}
