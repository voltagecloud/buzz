//! Account menu actions (`accountActions.register`): rows a plugin adds
//! to the account menu, each opening a dialog, such as "Send feedback".

use buzz_ui::IconName;
use gpui_kit::*;
use std::rc::Rc;

/// Closes the dialog an action opened.
pub type Close = Rc<dyn Fn(&mut Window, &mut App)>;
/// Makes an action's dialog, given how to close it.
pub type Build = Rc<dyn Fn(Close, &mut Window, &mut App) -> AnyView>;

#[derive(Clone)]
pub struct AccountAction {
    /// `plugin/action`, unique.
    pub key: &'static str,
    pub title: SharedString,
    pub icon: IconName,
    pub build: Build,
}

#[derive(Default)]
struct Actions(Vec<AccountAction>);

impl Global for Actions {}

/// Adds an action, replacing one with the same key. Actions are listed
/// by key.
pub fn register(action: AccountAction, cx: &mut App) {
    let actions = &mut cx.default_global::<Actions>().0;
    actions.retain(|existing| existing.key != action.key);
    actions.push(action);
    actions.sort_by_key(|action| action.key);
}

pub fn unregister(key: &str, cx: &mut App) {
    cx.default_global::<Actions>()
        .0
        .retain(|existing| existing.key != key);
}

/// The actions whose plugin is on.
pub fn all(cx: &App) -> Vec<AccountAction> {
    cx.try_global::<Actions>()
        .map(|actions| actions.0.clone())
        .unwrap_or_default()
        .into_iter()
        .filter(|action| crate::plugins::enabled_key(action.key))
        .collect()
}
