//! Banners contributed to a thread: a note and its buttons among the
//! thread panel's status rows, such as an offer to recover an agent's
//! ended session there. Each slice calls [`register`] from its init, as
//! panels do.

// The slice API: banners compiled in elsewhere use what the app does not.
#![allow(dead_code)]

use gpui_kit::*;
use std::rc::Rc;

/// The thread a banner is asked about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BannerThread {
    /// The community's `wss://` origin.
    pub relay: String,
    pub channel: String,
    /// The thread's root event id, as hex.
    pub root: String,
}

/// What a banner's button does.
pub type Action = Rc<dyn Fn(&mut Window, &mut App)>;

#[derive(Clone)]
pub struct BannerButton {
    pub label: SharedString,
    /// Shows the button working, as while its request runs.
    pub loading: bool,
    pub action: Action,
}

#[derive(Clone)]
pub struct Banner {
    pub text: SharedString,
    pub buttons: Vec<BannerButton>,
}

/// A slice's banners for a thread, usually none. Asked on every draw, so
/// it only reads.
pub type Banners<C = App> = Rc<dyn Fn(&BannerThread, &C) -> Vec<Banner>>;

pub struct ThreadBanner<C = App> {
    /// `plugin/banner`, unique.
    pub key: &'static str,
    /// Lower shows first, then the lower key.
    pub order: i32,
    pub banners: Banners<C>,
}

/// Generic over what banners read, so tests need no app.
struct Registry<C = App>(Vec<ThreadBanner<C>>);

impl<C> Default for Registry<C> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl Global for Registry {}

impl<C> Registry<C> {
    fn add(&mut self, banner: ThreadBanner<C>) {
        self.0.retain(|existing| existing.key != banner.key);
        self.0.push(banner);
        self.0
            .sort_by(|a, b| a.order.cmp(&b.order).then_with(|| a.key.cmp(b.key)));
    }

    fn remove(&mut self, key: &str) {
        self.0.retain(|existing| existing.key != key);
    }

    /// Those whose plugin is on, in order.
    fn for_thread(&self, thread: &BannerThread, cx: &C) -> Vec<Banner> {
        self.0
            .iter()
            .filter(|banner| crate::plugins::enabled_key(banner.key))
            .flat_map(|banner| (banner.banners)(thread, cx))
            .collect()
    }
}

/// Adds banners, replacing those with the same key.
pub fn register(banner: ThreadBanner, cx: &mut App) {
    cx.default_global::<Registry>().add(banner);
}

/// Removes the banners `key`, as when the plugin that added them stops.
pub fn unregister(key: &str, cx: &mut App) {
    cx.default_global::<Registry>().remove(key);
}

/// Redraws open threads' banners, once what a slice shows has changed.
pub fn refresh(cx: &mut App) {
    cx.default_global::<Registry>();
}

/// Redraws `view` when banners change.
pub fn observe<V: 'static>(cx: &mut Context<V>) -> Subscription {
    cx.observe_global::<Registry>(|_, cx| cx.notify())
}

pub fn for_thread(thread: &BannerThread, cx: &App) -> Vec<Banner> {
    cx.try_global::<Registry>()
        .map(|registry| registry.for_thread(thread, cx))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;
    use std::cell::RefCell;

    fn banner(text: &str) -> Banner {
        Banner {
            text: text.to_owned().into(),
            buttons: Vec::new(),
        }
    }

    #[test]
    fn a_registered_banner_gets_the_thread() {
        let asked = Rc::new(RefCell::new(Vec::new()));
        let mut registry = Registry::<()>::default();
        registry.add(ThreadBanner {
            key: "test.workspace/recover",
            order: 0,
            banners: Rc::new({
                let asked = asked.clone();
                move |thread: &BannerThread, _: &()| {
                    asked.borrow_mut().push(thread.clone());
                    vec![banner("Session ended")]
                }
            }),
        });
        let thread = BannerThread {
            relay: "wss://relay.example".into(),
            channel: "6f1c3c7e-8f0e-4b8e-9d55-1a3c1f5b2e10".into(),
            root: "c".repeat(64),
        };
        let shown = registry.for_thread(&thread, &());
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].text, "Session ended");
        assert_eq!(*asked.borrow(), vec![thread]);
    }

    #[test]
    fn banners_show_in_order_and_replace_by_key() {
        let mut registry = Registry::<()>::default();
        let add = |registry: &mut Registry<()>, key, order, text: &'static str| {
            registry.add(ThreadBanner {
                key,
                order,
                banners: Rc::new(move |_: &BannerThread, _: &()| vec![banner(text)]),
            })
        };
        add(&mut registry, "b/late", 10, "late");
        add(&mut registry, "b/early", 0, "first");
        add(&mut registry, "a/early", 0, "a first");
        add(&mut registry, "b/early", 0, "early");
        let thread = BannerThread {
            relay: String::new(),
            channel: String::new(),
            root: String::new(),
        };
        let texts = |registry: &Registry<()>| {
            registry
                .for_thread(&thread, &())
                .into_iter()
                .map(|banner| banner.text.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(texts(&registry), ["a first", "early", "late"]);
        registry.remove("a/early");
        assert_eq!(texts(&registry), ["early", "late"]);
    }
}
