//! The profile panel's agent identity (`ProfileAgentIdentity.tsx`): who
//! manages the agent, by its verified NIP-OA owner. Profile metadata never
//! grants control.

use super::ProfilePanel;
use buzz_ui::{Button, ButtonSize, ButtonVariant, TypeStyled, tokens::space};
use gpui_kit::*;

impl ProfilePanel {
    /// "Managed by", after the public key (`ProfileAgentIdentity`).
    pub(super) fn agent_identity_items(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut items = Vec::new();
        if self.known_agent(cx)
            && let Some(owner) = self.verified_owner(cx)
        {
            items.push(self.managed_by(owner, cx));
        }
        items
    }

    /// "Managed by" and the owner's name, which opens their profile.
    fn managed_by(&self, owner: String, cx: &mut Context<Self>) -> AnyElement {
        let viewer = self.community.read(cx).session.viewer().to_string();
        let shown = self
            .community
            .read(cx)
            .snapshot
            .profiles
            .get(&owner)
            .map(|profile| profile.name.clone())
            .unwrap_or_else(|| super::short_key(&owner));
        let name = if owner == viewer {
            format!("{shown} (you)")
        } else {
            shown
        };
        self.session(cx).want_profiles(vec![owner.clone()]);
        div()
            .flex()
            .flex_col()
            .items_start()
            .gap(space::S2)
            .child(div().text_body().child("Managed by"))
            .child(
                Button::new("profile-owner", name)
                    .size(ButtonSize::Sm)
                    .variant(ButtonVariant::Ghost)
                    .on_click(move |event, window, cx| {
                        crate::ui::navigation::show_profile(
                            owner.clone(),
                            event.position(),
                            window,
                            cx,
                        )
                    }),
            )
            .into_any_element()
    }
}
