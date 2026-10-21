//! Completions plugins add to the composer (`conversation.registerCompletion`
//! in `features/conversation/contracts.ts`): typing a plugin's trigger
//! character, such as `/`, at the start of a word opens a popup of its
//! suggestions, and picking one replaces the trigger and query with its
//! text. A plugin takes `@`, `#` or `:` only by ranking before Buzz's own
//! completion on it, or while that plugin is off.
//!
//! A plugin answers later than the composer paints, so a query reads
//! `None` until its answer is in; [`generation`] changes when one arrives
//! and the composer asks again.

use gpui_kit::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// One suggestion.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub label: SharedString,
    pub detail: Option<SharedString>,
    /// What a pick puts in place of the trigger and query.
    pub text: String,
    /// People a pick mentions instead of `text`, as chips that make them
    /// recipients (`edit: { mentions }`).
    pub mentions: Vec<String>,
    /// Why it cannot be picked, shown in its row.
    pub disabled: Option<SharedString>,
    /// A line under it, such as what a command does.
    pub preview: Option<SharedString>,
    /// The exact match a space typed after the query picks (`spaceId`).
    pub exact: bool,
    /// A picture before the label: an https or `data:image/` URL.
    pub image: Option<String>,
    /// A person whose avatar shows before the label, by pubkey.
    pub avatar: Option<String>,
    /// The names `mentions` chips show, by pubkey, where the plugin gave
    /// them (`CompletionEdit.mention.name`).
    pub names: std::collections::HashMap<String, String>,
}

/// A completion's answer (`CompletionResult`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Answer {
    pub items: Vec<Item>,
    /// A line in place of the list or under it, such as "No matches".
    pub status: Option<SharedString>,
    /// Offers a row that asks again.
    pub retry: bool,
}

/// Where a query is typed (`CompletionContext`).
#[derive(Clone, Copy)]
pub struct Place<'a> {
    /// The composer's community, as its `wss://` relay.
    pub community: &'a str,
    pub channel: &'a str,
    /// The thread the composer replies in, as hex.
    pub thread: Option<&'a str>,
    /// The composer may offer library agents not yet in the conversation.
    pub invite_agents: bool,
    /// The whole draft and the query's character range in it
    /// (`ComposerObservation`).
    pub draft: &'a str,
    pub start: usize,
    pub end: usize,
}

/// The suggestions for a query; `None` while waiting.
pub type Query = Rc<dyn Fn(&str, Place, &App) -> Option<Answer>>;

pub struct PluginCompletion {
    pub key: &'static str,
    pub trigger: char,
    /// Of completions on one trigger, the lowest order, then the lowest
    /// key, answers it.
    pub order: i32,
    pub query: Query,
    /// Forgets its answers, so the next query asks again.
    pub retry: Rc<dyn Fn()>,
}

thread_local! {
    static REGISTRY: RefCell<Vec<Rc<PluginCompletion>>> = RefCell::default();
    static GENERATION: Cell<u64> = const { Cell::new(0) };
}

/// Adds a completion, replacing one with the same key.
pub fn register(completion: PluginCompletion) {
    REGISTRY.with_borrow_mut(|registry| {
        registry.retain(|existing| existing.key != completion.key);
        registry.push(Rc::new(completion));
        registry.sort_by(|a, b| a.order.cmp(&b.order).then_with(|| a.key.cmp(b.key)));
    });
    changed();
}

pub fn unregister(key: &str) {
    REGISTRY.with_borrow_mut(|registry| registry.retain(|existing| existing.key != key));
    changed();
}

/// Every plugin completion as `(key, trigger, order)`.
pub fn listed() -> Vec<(&'static str, char, i32)> {
    REGISTRY.with_borrow(|registry| {
        registry
            .iter()
            .map(|completion| (completion.key, completion.trigger, completion.order))
            .collect()
    })
}

/// Buzz's own completions on `@`, `#` and `:`, as `(order, key)`: a
/// plugin takes one of these triggers only by ranking before it, the
/// lower order and then the lower key (buzz-app's bundled mentions sit at
/// -10, channels and emoji at 0).
const BUNDLED: [(char, i32, &str); 3] = [
    ('@', -10, "buzz.mentions/typeahead"),
    ('#', 0, "buzz.channels/channel-typeahead"),
    (':', 0, "buzz.emoji/typeahead"),
];

/// The triggers and the completion each opens: the first by order, then
/// key, where it outranks Buzz's own.
pub fn triggers() -> Vec<(char, &'static str)> {
    REGISTRY.with_borrow(|registry| {
        let mut triggers: Vec<(char, &'static str)> = Vec::new();
        for completion in registry.iter() {
            // Only against Buzz's own that are on: off, they free the trigger.
            let outranked = BUNDLED.iter().any(|&(trigger, order, key)| {
                trigger == completion.trigger
                    && crate::plugins::enabled_key(key)
                    && (order, key) <= (completion.order, completion.key)
            });
            if !outranked
                && !triggers
                    .iter()
                    .any(|(trigger, _)| *trigger == completion.trigger)
            {
                triggers.push((completion.trigger, completion.key));
            }
        }
        triggers
    })
}

pub fn get(key: &str) -> Option<Rc<PluginCompletion>> {
    REGISTRY.with_borrow(|registry| {
        registry
            .iter()
            .find(|completion| completion.key == key)
            .cloned()
    })
}

/// Changes when a completion is added or removed, or an answer arrives.
pub fn generation() -> u64 {
    GENERATION.get()
}

pub fn changed() {
    GENERATION.set(GENERATION.get() + 1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn the_lowest_order_answers_a_trigger() {
        let add = |key: &'static str, order| {
            register(PluginCompletion {
                key,
                trigger: '/',
                order,
                query: Rc::new(|_, _, _| None),
                retry: Rc::new(|| {}),
            })
        };
        add("b.plugin/commands", 0);
        add("a.plugin/commands", 0);
        assert_eq!(triggers(), [('/', "a.plugin/commands")]);
        add("c.plugin/commands", -1);
        assert_eq!(triggers(), [('/', "c.plugin/commands")]);
        for key in [
            "a.plugin/commands",
            "b.plugin/commands",
            "c.plugin/commands",
        ] {
            unregister(key);
        }
        assert!(triggers().is_empty());
    }
}
