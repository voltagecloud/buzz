//! Settings cards contributed by bundled plugins (`features/settings/service.ts`,
//! `app/settings-sections.ts`), such as Templates and Membership.
//!
//! buzz-app's plugins register cards at run time; here each slice calls
//! [`register`] from its init. The settings sidebar places a card under the
//! selected community, in the community's Administration group, or in a
//! named account-level group, and lists it only while its visibility says
//! so.

use buzz_ui::IconName;
use gpui_kit::*;

/// Where the settings sidebar lists a card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CardPlacement {
    /// After Profile under the selected community's name (no `group` and
    /// no `section`). The icon defaults to a chat bubble.
    Community,
    /// The community's "Administration" group (`section: "administration"`),
    /// for permission-gated controls. The icon defaults to people.
    Administration,
    /// An account-level group with this label, such as "Integrations".
    Group(&'static str),
}

/// A card's verified visibility (`visibility.snapshot()`): only `Visible`
/// lists it, but an open card stays open while its check is pending.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CardVisibility {
    Visible,
    Hidden,
    Pending,
    Error,
}

/// Whether a card is listed now (`visibility.snapshot`).
pub type Visibility = std::rc::Rc<dyn Fn(&App) -> CardVisibility>;

/// The view a card shows; the page keeps it while its section is open.
pub type BuildCard = std::rc::Rc<dyn Fn(&mut Window, &mut App) -> AnyView>;

/// A card's [`BuildCard`] from a closure.
pub fn build(build: impl Fn(&mut Window, &mut App) -> AnyView + 'static) -> BuildCard {
    std::rc::Rc::new(build)
}

#[derive(Clone)]
pub struct SettingsCard {
    /// `plugin/card`, such as `buzz.moderation/membership`. The text before
    /// the slash is the plugin id the Plugins switches gate.
    pub key: &'static str,
    pub title: &'static str,
    pub icon: Option<IconName>,
    pub placement: CardPlacement,
    /// `None` is always visible.
    pub visibility: Option<Visibility>,
    pub build: BuildCard,
}

impl SettingsCard {
    /// Hidden while its plugin is switched off in Settings → Plugins.
    pub fn visibility(&self, cx: &App) -> CardVisibility {
        if !crate::plugins::enabled_key(self.key) {
            return CardVisibility::Hidden;
        }
        self.visibility
            .as_ref()
            .map_or(CardVisibility::Visible, |visibility| visibility(cx))
    }

    /// The sidebar icon: the card's own, else its placement's default.
    pub fn sidebar_icon(&self) -> Option<IconName> {
        self.icon.or(match self.placement {
            CardPlacement::Community => Some(IconName::ChatCircle),
            CardPlacement::Administration => Some(IconName::Users),
            CardPlacement::Group(_) => None,
        })
    }
}

/// Starts a card's visibility check and holds it until the subscription
/// drops (`visibility.ensure`).
pub type EnsureVisibility = fn(&mut App) -> Subscription;

#[derive(Default)]
struct SettingsCards(Vec<SettingsCard>, Vec<(&'static str, EnsureVisibility)>);

impl Global for SettingsCards {}

/// Adds a card, replacing one with the same key.
pub fn register(card: SettingsCard, cx: &mut App) {
    let cards = &mut cx.default_global::<SettingsCards>().0;
    cards.retain(|existing| existing.key != card.key);
    cards.push(card);
}

/// Removes the card `key`, as when the plugin that added it stops.
pub fn unregister(key: &str, cx: &mut App) {
    cx.default_global::<SettingsCards>()
        .0
        .retain(|existing| existing.key != key);
    changed(cx);
}

/// Gives the card `key` a visibility check that runs while the settings
/// page shows, such as reading a roster to learn the viewer's role. The
/// check calls [`changed`] when its answer may have moved.
pub fn register_check(key: &'static str, ensure: EnsureVisibility, cx: &mut App) {
    let checks = &mut cx.default_global::<SettingsCards>().1;
    checks.retain(|(existing, _)| *existing != key);
    checks.push((key, ensure));
}

/// Holds every card's visibility check while the settings navigation is
/// shown (`retainVisibility`).
pub fn retain_visibility(cx: &mut App) -> Vec<Subscription> {
    let checks = cx
        .try_global::<SettingsCards>()
        .map(|cards| cards.1.clone())
        .unwrap_or_default();
    checks.into_iter().map(|(_, ensure)| ensure(cx)).collect()
}

/// Tells the settings page that a card's visibility may have changed.
pub fn changed(cx: &mut App) {
    cx.default_global::<SettingsCards>();
    cx.update_global::<SettingsCards, _>(|_, _| {});
}

/// Calls `f` when a card is registered or its visibility may have changed.
pub fn observe<T: 'static>(
    window: &mut Window,
    cx: &mut Context<T>,
    f: impl FnMut(&mut T, &mut Window, &mut Context<T>) + 'static,
) -> Subscription {
    cx.default_global::<SettingsCards>();
    cx.observe_global_in::<SettingsCards>(window, f)
}

/// Every registered card, in registration order.
pub fn all(cx: &App) -> Vec<SettingsCard> {
    cx.try_global::<SettingsCards>()
        .map(|cards| cards.0.clone())
        .unwrap_or_default()
}

pub fn get(key: &str, cx: &App) -> Option<SettingsCard> {
    all(cx).into_iter().find(|card| card.key == key)
}

/// The cards the sidebar lists now: their plugin is on and their check
/// passes.
pub fn visible(cx: &App) -> Vec<SettingsCard> {
    all(cx)
        .into_iter()
        .filter(|card| card.visibility(cx) == CardVisibility::Visible)
        .collect()
}

/// The labels of the account-level groups, in first-registration order.
pub fn groups(cards: &[SettingsCard]) -> Vec<&'static str> {
    let mut labels = Vec::new();
    for card in cards {
        if let CardPlacement::Group(label) = card.placement
            && !labels.contains(&label)
        {
            labels.push(label);
        }
    }
    labels
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    fn card(key: &'static str, placement: CardPlacement) -> SettingsCard {
        SettingsCard {
            key,
            title: "Card",
            icon: None,
            placement,
            visibility: None,
            build: build(|_, _| unreachable!()),
        }
    }

    #[test]
    fn groups_keep_first_registration_order() {
        let cards = [
            card("a/one", CardPlacement::Group("Integrations")),
            card("b/two", CardPlacement::Community),
            card("c/three", CardPlacement::Group("Account")),
            card("d/four", CardPlacement::Group("Integrations")),
        ];
        assert_eq!(groups(&cards), ["Integrations", "Account"]);
    }

    #[test]
    fn placement_picks_the_default_icon() {
        let admin = card("buzz.moderation/membership", CardPlacement::Administration);
        assert_eq!(admin.sidebar_icon(), Some(IconName::Users));
        assert_eq!(
            card("x/y", CardPlacement::Group("Account")).sidebar_icon(),
            None
        );
    }
}
