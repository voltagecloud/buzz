//! What a plugin adds to the app's registries besides pages and panels:
//! settings cards, notification categories, message actions and markers,
//! link, message and attachment renderers, composer tools, page badges and
//! shortcuts (`docs/plugins.md`).
//!
//! The app draws renderers while it paints, without waiting. A plugin's
//! renderer is asked once per target in the background; until it answers,
//! the app draws what it would have anyway, and the answer is kept so the
//! next paint uses it. The answers are cleared when the plugin restarts.

use super::draw::{self, Draw};
use super::process::Requester;
use super::protocol::{self, Node};
use super::{ExternalPlugins, capabilities, intern};
use crate::community::AppState;
use crate::runtime::Runtime;
use crate::ui::composer::Composer;
use crate::ui::composer_completions::{self, Answer as Completed, Item, Place};
use crate::ui::composer_tools::ToolContext;
use crate::ui::markdown::Directory;
use crate::ui::renderers::{
    AttachmentRenderer, InMessage, LinkRenderer, MessageAction, MessageRenderer,
};
use buzz_relay_client::Media;
use buzz_relay_client::fold::{Attachment, AttachmentKind, MessageRow};
use buzz_ui::{
    ActiveTheme, Badge, BadgeTone, Icon, IconButton, IconButtonSize, IconName, TypeStyled,
};
use gpui_kit::*;
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::rc::Rc;
use std::sync::Arc;

/// Answers kept at once; the oldest go first.
const MAX_ANSWERS: usize = 2_000;

/// How long a renderer that failed to answer is left before it is
/// asked again.
const RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(30);

enum Answer {
    Pending,
    Ready(Option<Rc<Node>>),
    /// Shown, but the plugin asked to be asked again (`render/invalidate`).
    Stale(Option<Rc<Node>>),
    /// Asked again; the old answer shows until the new one comes.
    Asking(Option<Rc<Node>>),
    /// The request failed or timed out: nothing is drawn, and the plugin
    /// is asked again after [`RETRY_AFTER`].
    Failed(std::time::Instant),
}

/// What came back for one request.
enum Outcome {
    Answered(Option<Box<Node>>),
    Failed,
}

impl Answer {
    /// What draws now: `None` while the first answer is on its way.
    fn shown(&self) -> Option<Option<Rc<Node>>> {
        match self {
            Answer::Pending => None,
            Answer::Ready(tree) | Answer::Stale(tree) | Answer::Asking(tree) => Some(tree.clone()),
            Answer::Failed(_) => Some(None),
        }
    }

    /// Whether the plugin should be asked (again).
    fn due(&self) -> bool {
        match self {
            Answer::Stale(_) => true,
            Answer::Failed(at) => at.elapsed() >= RETRY_AFTER,
            Answer::Pending | Answer::Ready(_) | Answer::Asking(_) => false,
        }
    }

    /// The entry while a request is out.
    fn asking(&self) -> Answer {
        match self {
            Answer::Stale(tree) => Answer::Asking(tree.clone()),
            _ => Answer::Pending,
        }
    }
}

#[derive(Default)]
struct Answers {
    entries: HashMap<(String, String), Answer>,
    order: VecDeque<(String, String)>,
}

/// A renderer's answer for a target, on its way to the UI thread.
type Delivered = ((String, String), Outcome);

/// How a message accessory's place opens the thread of the message it
/// drew for.
/// Held weakly: a place that went (a page closed, a channel left) opens
/// nothing.
type ThreadOpener = (
    std::rc::Weak<dyn Fn(&Arc<MessageRow>, &mut Window, &mut App)>,
    Option<std::rc::Weak<dyn Fn(buzz_relay_client::EventId, &App) -> Option<Arc<MessageRow>>>>,
    Arc<MessageRow>,
);

thread_local! {
    static ANSWERS: RefCell<Answers> = RefCell::default();
    static INBOX: RefCell<Option<futures::channel::mpsc::UnboundedSender<Delivered>>> =
        RefCell::default();
    /// Message ids each action marks, by action key.
    static MARKS: RefCell<HashMap<String, HashSet<String>>> = RefCell::default();
    /// A page's badge text and tone, by page key.
    static BADGES: RefCell<HashMap<String, (String, BadgeTone)>> = RefCell::default();
    /// Composer ids handed to plugins, each for one plugin's tool or
    /// embedded composer: writes through it stop once that is gone
    /// (`ComposerToolProps` live only as long as the tool).
    static COMPOSERS: RefCell<HashMap<u64, Grant>> = RefCell::default();
    /// Captures a tool started, by the composer number it was handed: they
    /// end when the tool goes (`capture` lives as long as the tool).
    static CAPTURING: RefCell<HashMap<u64, (WeakEntity<Composer>, EntityId)>> = RefCell::default();
    /// Each plugin page's icon as declared, by page key.
    static PAGE_ICONS: RefCell<HashMap<String, String>> = RefCell::default();
    /// How each composer tool was last declared, by key.
    static DECLARED: RefCell<HashMap<&'static str, String>> = RefCell::default();
    /// Draft attachments a plugin's renderer was offered to remove, by url.
    static REMOVERS: RefCell<HashMap<String, String>> = RefCell::default();
    /// Settings cards a plugin hid or marked pending, by key.
    static CARD_STATES: RefCell<HashMap<String, crate::ui::settings::cards::CardVisibility>> =
        RefCell::default();
    /// Text renderer keys: their answers change how prose is prepared.
    static TEXT_KEYS: RefCell<HashSet<String>> = RefCell::default();
    /// Completion answers by completion key, channel and query; `None`
    /// while one is on its way.
    static SUGGESTIONS: RefCell<Suggestions> = RefCell::default();
    static SUGGESTION_INBOX: RefCell<Option<futures::channel::mpsc::UnboundedSender<Suggested>>> =
        RefCell::default();
    static NEXT_COMPOSER: Cell<u64> = const { Cell::new(0) };
    /// Each contribution's title and order, by `plugin/id`, for
    /// `contributions/list`.
    static TITLES: RefCell<HashMap<String, (String, Option<i32>)>> = RefCell::default();
    /// Each plugin's settings cards, for `settings/check`.
    /// How each message accessory's place opens its message's thread, by
    /// accessory key and message id, as last drawn.
    static THREAD_OPENERS: RefCell<HashMap<(String, String), ThreadOpener>> = RefCell::default();
    static CARD_OWNERS: RefCell<HashMap<String, Vec<String>>> = RefCell::default();
    /// How many holders keep Settings' card checks running now.
    static CARDS_HELD: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// The query each `completion/query` request answers, by its
    /// `request` id, for `completion/publish`.
    static QUERIES: RefCell<HashMap<u64, SuggestionKey>> = RefCell::default();
    static NEXT_QUERY: Cell<u64> = const { Cell::new(0) };
}

type SuggestionKey = (String, String, String);
/// A completion's answer, on its way to the UI thread.
type Suggested = (SuggestionKey, Completed);

#[derive(Default)]
struct Suggestions {
    entries: HashMap<SuggestionKey, Option<Completed>>,
    order: VecDeque<SuggestionKey>,
}

/// A completion's suggestions for `query` typed in `channel`, asking the
/// plugin the first time.
fn suggest(
    plugin: &str,
    key: &str,
    id: &str,
    query: &str,
    place: Place,
    cx: &App,
) -> Option<Completed> {
    let Place {
        community,
        channel,
        thread,
        invite_agents,
        draft,
        start,
        end,
    } = place;
    let entry = (
        key.to_owned(),
        format!("{community}/{channel}/{thread:?}/{invite_agents}"),
        query.to_owned(),
    );
    if let Some(known) =
        SUGGESTIONS.with_borrow(|suggestions| suggestions.entries.get(&entry).cloned())
    {
        return known;
    }
    let connection = ExternalPlugins::running_connection(plugin, cx)?;
    let inbox = SUGGESTION_INBOX.with_borrow(Clone::clone)?;
    let query_id = NEXT_QUERY.get() + 1;
    NEXT_QUERY.set(query_id);
    QUERIES.with_borrow_mut(|queries| {
        queries.insert(query_id, entry.clone());
        // Only queries still kept can be published to.
        if queries.len() > MAX_ANSWERS {
            queries.retain(|id, _| *id + MAX_ANSWERS as u64 > query_id);
        }
    });
    let request = connection.request(
        "completion/query",
        json!({
            "completion": id,
            "request": query_id,
            "query": query,
            "community": community,
            "channel": channel,
            "thread": thread,
            "inviteAgents": invite_agents,
            // The draft around the query (`ComposerObservation`), as it was
            // when this query was first typed.
            "observation": {
                "text": draft.chars().take(10_000).collect::<String>(),
                "start": start,
                "end": end,
            },
        }),
    );
    SUGGESTIONS.with_borrow_mut(|suggestions| {
        suggestions.entries.insert(entry.clone(), None);
        suggestions.order.push_back(entry.clone());
        while suggestions.order.len() > MAX_ANSWERS {
            if let Some(old) = suggestions.order.pop_front() {
                suggestions.entries.remove(&old);
            }
        }
    });
    Runtime::handle(cx).spawn(async move {
        let items = match request.await {
            Ok(Ok(value)) => answer(&value),
            _ => Completed {
                status: Some("Suggestions are not available.".into()),
                retry: true,
                ..Completed::default()
            },
        };
        let _ = inbox.unbounded_send((entry, items));
    });
    None
}

/// Settings is showing its sections (`retainVisibility`): each running
/// plugin with cards hears `settings/check` and checks them now, as a
/// buzz-app card's `visibility.ensure` runs.
fn check_cards(cx: &mut App) -> Subscription {
    let owners = CARD_OWNERS.with_borrow(Clone::clone);
    for (plugin, cards) in owners {
        ExternalPlugins::notify_running(&plugin, "settings/check", json!({ "cards": cards }), cx);
    }
    CARDS_HELD.set(CARDS_HELD.get() + 1);
    Subscription::new(|| CARDS_HELD.set(CARDS_HELD.get().saturating_sub(1)))
}

/// `message/openThread`: opens the thread of `message`, as the place its
/// accessory `accessory` was drawn does (`openThread`). `false` where
/// that place cannot.
pub fn open_accessory_thread(
    plugin: &str,
    params: &Value,
    cx: &mut App,
) -> Result<Value, capabilities::Failure> {
    let field = |key: &str| {
        params
            .get(key)
            .and_then(Value::as_str)
            .ok_or((protocol::INVALID_PARAMS, format!("{key} is required")))
    };
    let key = format!("{plugin}/{}", field("accessory")?);
    let message = field("message")?.to_owned();
    let entry = (key, message);
    let opener = THREAD_OPENERS.with_borrow(|openers| openers.get(&entry).cloned());
    // Its place still shows the message, not deleted since.
    let current = opener.and_then(|(open, find, row)| {
        let open = open.upgrade()?;
        let row = match find {
            Some(find) => find.upgrade()?(row.id, cx)?,
            None => row,
        };
        Some((open, row))
    });
    let Some((open, row)) = current else {
        // Its place is gone (`false` after it retires).
        THREAD_OPENERS.with_borrow_mut(|openers| openers.remove(&entry));
        return Ok(json!(false));
    };
    let window =
        crate::ui::navigation::main_window(cx).ok_or((1, "The window is not open.".to_owned()))?;
    window
        .update(cx, |_, window, cx| open(&row, window, cx))
        .map_err(|_| (1, "The window is not open.".to_owned()))?;
    Ok(json!(true))
}

/// A contribution's title and order, by `plugin/id` key.
pub fn titled(key: &str) -> Option<(String, Option<i32>)> {
    TITLES.with_borrow(|titles| titles.get(key).cloned())
}

/// `completion/publish`: new suggestions for an earlier query, by its
/// `request` id (`ComposerCompletionProps.publish`), such as results
/// after a "Searching…" first answer. `false` once the query is gone.
pub fn publish(plugin: &str, params: &Value) -> Result<Value, capabilities::Failure> {
    let request = params
        .get("request")
        .and_then(Value::as_u64)
        .ok_or((protocol::INVALID_PARAMS, "request is required".to_owned()))?;
    let Some(entry) = QUERIES.with_borrow(|queries| queries.get(&request).cloned()) else {
        return Ok(json!(false));
    };
    if !entry.0.starts_with(&format!("{plugin}/")) {
        return Err((protocol::INVALID_PARAMS, "not your query".to_owned()));
    }
    let published =
        SUGGESTIONS.with_borrow_mut(|suggestions| match suggestions.entries.get_mut(&entry) {
            Some(slot) => {
                // `withdraw` takes back what was published, as buzz-app's
                // `publish` disposer does: nothing shows.
                *slot = Some(
                    if params.get("withdraw").and_then(Value::as_bool) == Some(true) {
                        answer(&json!({ "items": [] }))
                    } else {
                        answer(params)
                    },
                );
                true
            }
            None => false,
        });
    if published {
        composer_completions::changed();
    }
    Ok(json!(published))
}

/// `completion/invalidate`: asks again for completion `completion`'s
/// queries the next time they are typed.
pub fn invalidate_completion(plugin: &str, params: &Value) -> Result<Value, capabilities::Failure> {
    let id = params.get("completion").and_then(Value::as_str).ok_or((
        protocol::INVALID_PARAMS,
        "completion is required".to_owned(),
    ))?;
    forget_suggestions(&format!("{plugin}/{id}"));
    Ok(Value::Null)
}

/// Forgets completion `key`'s answers.
fn forget_suggestions(key: &str) {
    SUGGESTIONS.with_borrow_mut(|suggestions| {
        suggestions.entries.retain(|(entry, _, _), _| entry != key);
        suggestions.order.retain(|(entry, _, _)| entry != key);
    });
    composer_completions::changed();
}

/// `completion/query`'s answer: `{"items", "status", "retry"}`.
fn answer(value: &Value) -> Completed {
    let status = value
        .get("status")
        .and_then(Value::as_str)
        .map(|status| status.chars().take(200).collect::<String>().into());
    Completed {
        items: items(value),
        status,
        retry: value.get("retry").and_then(Value::as_bool) == Some(true),
    }
}

/// `completion/query`'s items: `[{"label", "detail", "text", "mentions",
/// "disabled", "preview", "exact"}]`.
fn items(value: &Value) -> Vec<Item> {
    let text = |item: &Value, key: &str| {
        item.get(key)
            .and_then(Value::as_str)
            .map(|text| text.chars().take(2_000).collect::<String>())
    };
    value
        .get("items")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .take(50)
                .filter_map(|item| {
                    let label = text(item, "label").filter(|label| !label.is_empty())?;
                    // Each a pubkey, or `{"pubkey","name"}` to name its chip.
                    let mut names = HashMap::new();
                    // All or nothing: an item naming a bad key, or more than
                    // 32, is dropped, as buzz-app drops it.
                    let mentions: Vec<String> = item
                        .get("mentions")
                        .and_then(Value::as_array)
                        .map(|keys| -> Option<Vec<String>> {
                            if keys.len() > 32 {
                                return None;
                            }
                            keys.iter()
                                .map(|mention| {
                                    let key = mention
                                        .as_str()
                                        .or_else(|| mention.get("pubkey").and_then(Value::as_str))
                                        .filter(|key| {
                                            key.len() == 64
                                                && key.bytes().all(|byte| byte.is_ascii_hexdigit())
                                        })?
                                        .to_ascii_lowercase();
                                    if let Some(name) = mention.get("name").and_then(Value::as_str)
                                    {
                                        names.insert(key.clone(), name.chars().take(64).collect());
                                    }
                                    Some(key)
                                })
                                .collect()
                        })
                        .unwrap_or(Some(Vec::new()))?;
                    Some(Item {
                        names,
                        text: text(item, "text").unwrap_or_else(|| label.clone()),
                        detail: text(item, "detail").map(Into::into),
                        label: label.into(),
                        mentions,
                        disabled: text(item, "disabled").map(Into::into),
                        preview: text(item, "preview").map(Into::into),
                        exact: item.get("exact").and_then(Value::as_bool) == Some(true),
                        image: item
                            .get("image")
                            .and_then(Value::as_str)
                            .filter(|url| {
                                (url.starts_with("https://") && url.len() <= 2_000)
                                    || (url.starts_with("data:image/") && url.len() <= 64 * 1024)
                            })
                            .map(str::to_owned),
                        avatar: item
                            .get("avatar")
                            .and_then(Value::as_str)
                            .filter(|key| {
                                key.len() == 64 && key.bytes().all(|byte| byte.is_ascii_hexdigit())
                            })
                            .map(str::to_ascii_lowercase),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Starts taking renderer answers.
pub fn init(cx: &mut App) {
    super::naming::init(cx);
    // Held from the start, so a Settings page or visit begun before any
    // plugin card arrives still checks the cards that come.
    crate::ui::settings::cards::register_check("buzz.plugins/cards", check_cards, cx);
    let (sender, mut suggestions) = futures::channel::mpsc::unbounded();
    SUGGESTION_INBOX.with_borrow_mut(|inbox| *inbox = Some(sender));
    cx.spawn(async move |cx| {
        use futures::StreamExt;
        while let Some((key, items)) = suggestions.next().await {
            SUGGESTIONS.with_borrow_mut(|suggestions| {
                if let Some(entry) = suggestions.entries.get_mut(&key) {
                    *entry = Some(items);
                }
            });
            composer_completions::changed();
            cx.update(|cx| cx.refresh_windows());
        }
    })
    .detach();
    let (sender, mut answers) = futures::channel::mpsc::unbounded();
    INBOX.with_borrow_mut(|inbox| *inbox = Some(sender));
    cx.spawn(async move |cx| {
        use futures::StreamExt;
        while let Some((key, outcome)) = answers.next().await {
            let drawn = matches!(outcome, Outcome::Answered(Some(_)));
            ANSWERS.with_borrow_mut(|answers| {
                if let Some(answer) = answers.entries.get_mut(&key) {
                    *answer = match (outcome, &*answer) {
                        (Outcome::Answered(tree), _) => {
                            Answer::Ready(tree.map(|tree| Rc::new(*tree)))
                        }
                        // A refresh that failed keeps what showed.
                        (Outcome::Failed, Answer::Asking(tree)) => Answer::Ready(tree.clone()),
                        (Outcome::Failed, _) => Answer::Failed(std::time::Instant::now()),
                    };
                }
            });
            if drawn && TEXT_KEYS.with_borrow(|keys| keys.contains(&key.0)) {
                crate::ui::renderers::text_changed();
            }
            cx.update(|cx| cx.refresh_windows());
        }
    })
    .detach();
}

/// Forgets what `plugin` answered and marked, when it restarts or stops.
/// Whether the draft at `url` can be removed, noting that `plugin`
/// was offered it.
fn removable(plugin: &str, url: &str) -> bool {
    if !crate::ui::renderers::removal_offered() {
        return false;
    }
    let removable = crate::ui::renderers::is_removable(url);
    REMOVERS.with_borrow_mut(|removers| {
        removers.retain(|url, _| crate::ui::renderers::is_removable(url));
        if removable {
            removers.insert(url.to_owned(), plugin.to_owned());
        }
    });
    removable
}

/// The composers `plugin` was handed through its tools: a restart remakes
/// those tools, which ends their captures.
pub fn tool_composers(plugin: &str) -> std::collections::HashSet<u64> {
    COMPOSERS.with_borrow(|composers| {
        composers
            .iter()
            .filter(|(_, grant)| grant.plugin == plugin && grant.tool.is_some())
            .map(|(id, _)| *id)
            .collect()
    })
}

pub fn forget(plugin: &str) {
    crate::shortcuts::forget_external(plugin);
    let prefix = format!("{plugin}/");
    // Its tools' states were the old run's (a new one starts enabled).
    TOOL_STATES.with_borrow_mut(|states| states.retain(|(key, _), _| !key.starts_with(&prefix)));
    // Its tool's popover goes with the tool, which a new run makes anew.
    TOOL_POPOVER.with_borrow_mut(|open| {
        if open
            .as_ref()
            .is_some_and(|(_, key, _)| key.starts_with(&format!("{plugin}/")))
        {
            *open = None;
        }
    });
    COMPOSERS.with_borrow_mut(|composers| composers.retain(|_, grant| grant.plugin != plugin));
    // A new run builds its tools anew, with grants of its own.
    DECLARED.with_borrow_mut(|declared| declared.retain(|key, _| !key.starts_with(&prefix)));
    REMOVERS.with_borrow_mut(|removers| removers.retain(|_, owner| owner != plugin));
    ANSWERS.with_borrow_mut(|answers| {
        answers
            .entries
            .retain(|(key, _), _| !key.starts_with(&prefix));
        answers.order.retain(|(key, _)| !key.starts_with(&prefix));
    });
    MARKS.with_borrow_mut(|marks| marks.retain(|key, _| !key.starts_with(&prefix)));
    CARD_STATES.with_borrow_mut(|states| states.retain(|key, _| !key.starts_with(&prefix)));
    CARD_OWNERS.with_borrow_mut(|owners| owners.remove(plugin));
    THREAD_OPENERS.with_borrow_mut(|openers| {
        openers.retain(|(key, _), _| !key.starts_with(&format!("{plugin}/")))
    });
    super::naming::forget(plugin);
    // Prose drew this plugin's old answers.
    crate::ui::renderers::text_changed();
    SUGGESTIONS.with_borrow_mut(|suggestions| {
        suggestions
            .entries
            .retain(|(key, _, _), _| !key.starts_with(&prefix));
        suggestions
            .order
            .retain(|(key, _, _)| !key.starts_with(&prefix));
    });
    BADGES.with_borrow_mut(|badges| badges.retain(|key, _| !key.starts_with(&prefix)));
}

/// The plugin's answer for `target` of renderer `key`, asking for it the
/// first time.
fn ask(
    plugin: &str,
    key: &str,
    target: &str,
    method: &str,
    params: impl FnOnce() -> Value,
    cx: &App,
) -> Option<Rc<Node>> {
    let entry = (key.to_owned(), target.to_owned());
    let (shown, due) = ANSWERS.with_borrow(|answers| match answers.entries.get(&entry) {
        Some(answer) => (answer.shown(), answer.due()),
        None => (None, true),
    });
    if !due {
        return shown.flatten();
    }
    let connection = ExternalPlugins::running_connection(plugin, cx)?;
    let request = connection.request(method, params());
    begin(&entry);
    let inbox = INBOX.with_borrow(Clone::clone)?;
    Runtime::handle(cx).spawn(async move {
        let outcome = match request.await {
            Ok(Ok(Value::Null)) => Outcome::Answered(None),
            Ok(Ok(value)) => {
                let tree = value.get("tree").cloned().unwrap_or(value);
                Outcome::Answered(
                    Node::parse(tree)
                        .inspect_err(|error| tracing::warn!(%error, "plugin renderer answer"))
                        .ok()
                        .map(Box::new),
                )
            }
            _ => Outcome::Failed,
        };
        let _ = inbox.unbounded_send((entry, outcome));
    });
    shown.flatten()
}

/// Marks `entry` as asked, keeping what it showed if it was stale.
fn begin(entry: &(String, String)) {
    ANSWERS.with_borrow_mut(|answers| {
        let next = answers
            .entries
            .get(entry)
            .map_or(Answer::Pending, Answer::asking);
        if answers.entries.insert(entry.clone(), next).is_none() {
            answers.order.push_back(entry.clone());
        }
        while answers.order.len() > MAX_ANSWERS {
            if let Some(old) = answers.order.pop_front() {
                answers.entries.remove(&old);
            }
        }
    });
}

/// `render/invalidate`: the plugin's answers for renderer `key` (all, one
/// `target`, or those about `message`) are asked for again the next time
/// they draw; what they show stays until the new answer comes.
pub fn invalidate(
    plugin: &str,
    params: &Value,
    cx: &mut App,
) -> Result<Value, capabilities::Failure> {
    let renderer = params
        .get("renderer")
        .and_then(Value::as_str)
        .ok_or((protocol::INVALID_PARAMS, "renderer is required".to_owned()))?;
    let key = format!("{plugin}/{renderer}");
    let target = params.get("target").and_then(Value::as_str);
    let message = params.get("message").and_then(Value::as_str);
    let matches = |entry_key: &str, entry_target: &str| {
        (entry_key == key || entry_key.starts_with(&format!("{key}#")))
            && target.is_none_or(|target| {
                // An attachment's answers are kept per removability.
                entry_target == target
                    || entry_target
                        .strip_prefix(target)
                        .is_some_and(|rest| rest.starts_with("#removable="))
            })
            && message.is_none_or(|message| entry_target.starts_with(message))
    };
    let mut count = 0;
    ANSWERS.with_borrow_mut(|answers| {
        answers.entries.retain(|(entry_key, entry_target), answer| {
            if !matches(entry_key, entry_target) {
                return true;
            }
            count += 1;
            match answer {
                Answer::Ready(tree) => {
                    *answer = Answer::Stale(tree.clone());
                    true
                }
                Answer::Failed(_) => false,
                _ => true,
            }
        });
        let entries = &answers.entries;
        answers.order.retain(|entry| entries.contains_key(entry));
    });
    crate::ui::renderers::text_changed();
    cx.refresh_windows();
    Ok(json!(count))
}

/// What a renderer answered for `target`: `None` until it has, then its
/// tree or `None` if it declined.
pub(super) fn answered(key: &str, target: &str) -> Option<Option<Rc<Node>>> {
    ANSWERS.with_borrow(|answers| {
        answers
            .entries
            .get(&(key.to_owned(), target.to_owned()))
            .and_then(Answer::shown)
    })
}

/// Whether [`ask_with`] should ask about `target` now: never asked, made
/// stale, or failed a while ago.
/// Asked, with no answer yet to show.
fn waiting(key: &str, target: &str) -> bool {
    ANSWERS.with_borrow(|answers| {
        answers
            .entries
            .get(&(key.to_owned(), target.to_owned()))
            .is_some_and(|answer| answer.shown().is_none())
    })
}

pub(super) fn due(key: &str, target: &str) -> bool {
    ANSWERS.with_borrow(|answers| {
        answers
            .entries
            .get(&(key.to_owned(), target.to_owned()))
            .is_none_or(Answer::due)
    })
}

/// A renderer's answer: `{"tree"}`, a bare tree, or `null`.
pub(super) fn tree_of(value: Value) -> Option<Node> {
    if value.is_null() {
        return None;
    }
    Node::parse(value.get("tree").cloned().unwrap_or(value)).ok()
}

/// Asks plugin renderer `key` about `target` through `requester`, once;
/// `parse` makes the answer a tree.
pub(super) fn ask_with(
    requester: &Requester,
    key: &str,
    target: &str,
    method: &str,
    params: Value,
    parse: fn(Value) -> Option<Node>,
) {
    let entry = (key.to_owned(), target.to_owned());
    if !due(key, target) {
        return;
    }
    begin(&entry);
    let Some(inbox) = INBOX.with_borrow(Clone::clone) else {
        return;
    };
    let request = requester.request(method, params, std::time::Duration::from_secs(10));
    requester.runtime().spawn(async move {
        let outcome = match request.await {
            Ok(Ok(value)) => Outcome::Answered(parse(value).map(Box::new)),
            Ok(Err(_)) | Err(_) => Outcome::Failed,
        };
        let _ = inbox.unbounded_send((entry, outcome));
    });
}

/// Draws message text a plugin's pattern matches.
struct Inline {
    key: &'static str,
    plugin: String,
    id: String,
    pattern: regex::Regex,
    requester: Requester,
}

impl crate::ui::renderers::TextRenderer for Inline {
    fn key(&self) -> &str {
        self.key
    }

    fn find(&self, text: &str, message: InMessage) -> Vec<std::ops::Range<usize>> {
        let id = match message.reaction {
            Some(_) => format!("{}/reaction", message.row.id.to_hex()),
            None => message.row.id.to_hex(),
        };
        let reaction = message.reaction.map(|group| {
            // `MessageReaction`, as `ReactionLabel` passes a quick one.
            json!({
                "content": group.content,
                "emoji": group.emoji.as_ref().map(|emoji| json!({
                    "shortcode": emoji.shortcode,
                    "url": emoji.url,
                })),
                "events": group.events.iter().map(|reaction| json!({
                    "authorId": reaction.author.to_hex(),
                })).collect::<Vec<_>>(),
            })
        });
        let mut found = Vec::new();
        for matched in self.pattern.find_iter(text).take(32) {
            let piece = matched.as_str();
            if piece.is_empty() || piece.len() > 200 {
                continue;
            }
            // Answers are the plugin's for this piece in this message.
            let target = format!("{id}/{piece}");
            if let Some(Some(_)) = answered(self.key, &target) {
                found.push(matched.range());
            }
            if due(self.key, &target) {
                ask_with(
                    &self.requester,
                    self.key,
                    &target,
                    "inline/render",
                    json!({
                        "renderer": self.id,
                        "text": piece,
                        // The plain-text segment it was found in
                        // (`content.text`), and where in it, in characters.
                        "segment": text,
                        "start": text[..matched.start()].chars().count(),
                        "message": capabilities::message(message.row, message.profiles),
                        "reaction": reaction,
                    }),
                    tree_of,
                );
            }
        }
        found
    }

    fn render(&self, matched: &str, message: &str, cx: &App) -> Option<AnyElement> {
        // Its events name it as `render/invalidate` takes it.
        let target = format!("{message}/{matched}");
        let tree = answered(self.key, &target)??;
        Some(drawn(&self.plugin, &self.id, self.key, &target, &tree, cx))
    }
}

fn hashed(text: &str) -> String {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Draws a renderer's answer; its buttons send `render/event`.
pub(super) fn drawn(
    plugin: &str,
    id: &str,
    key: &str,
    target: &str,
    tree: &Node,
    cx: &App,
) -> AnyElement {
    drawn_with(plugin, id, key, target, tree, None, cx)
}

/// [`drawn`], running `used` before each event it sends, as when an
/// accessory's place becomes the one the plugin acts for.
fn drawn_with(
    plugin: &str,
    id: &str,
    key: &str,
    target: &str,
    tree: &Node,
    used: Option<Rc<dyn Fn()>>,
    cx: &App,
) -> AnyElement {
    let (plugin, renderer, place) = (plugin.to_owned(), id.to_owned(), target.to_owned());
    // A message's target is `<id>:<revision>`, an inline piece's
    // `<id>/<text>`; the message is the id.
    let message = target
        .split([':', '/'])
        .next()
        .filter(|id| id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .map(str::to_owned);
    let fields = HashMap::new();
    let embeds = HashMap::new();
    let draw = Draw {
        send: Rc::new(move |element, event, value, cx| {
            if let Some(used) = &used {
                used();
            }
            ExternalPlugins::notify_running(
                &plugin,
                "render/event",
                json!({
                    "renderer": renderer,
                    "target": place,
                    "message": message,
                    "id": element,
                    "event": event,
                    "value": value,
                }),
                cx,
            );
        }),
        fields: &fields,
        embeds: &embeds,
        community: AppState::current(cx),
        place: key,
        window: None,
    };
    draw::node(tree, &hashed(target), &draw, cx)
}

fn message_json(row: &MessageRow, cx: &App) -> Value {
    let profiles = community_of(row, cx)
        .map(|community| community.read(cx).snapshot.profiles.clone())
        .unwrap_or_default();
    capabilities::message(row, &profiles)
}

/// The community a message is in: the one listing its channel (channel
/// ids are unique), else the selected one.
fn community_of(row: &MessageRow, cx: &App) -> Option<Entity<crate::community::Community>> {
    cx.global::<AppState>()
        .communities
        .iter()
        .find(|community| {
            community
                .read(cx)
                .snapshot
                .channels
                .iter()
                .any(|channel| *channel.id == *row.channel)
        })
        .cloned()
        .or_else(|| AppState::current(cx))
}

/// The viewer, for actions on the user's own messages.
struct Link {
    key: &'static str,
    plugin: String,
    id: String,
    prefixes: Vec<String>,
    order: i32,
}

impl LinkRenderer for Link {
    fn key(&self) -> &str {
        self.key
    }

    fn order(&self, _: &str) -> i32 {
        self.order
    }

    fn matches(&self, url: &str) -> bool {
        self.prefixes
            .iter()
            .any(|prefix| url.starts_with(prefix.as_str()))
    }

    fn render(&self, url: &str, cx: &App) -> Option<AnyElement> {
        // Asked with the URL alone, as buzz-app's renderer gets only
        // `{url}`: the answer is kept per URL.
        let tree = ask(
            &self.plugin,
            self.key,
            url,
            "link/render",
            || json!({ "renderer": self.id, "url": url }),
            cx,
        )?;
        Some(drawn(&self.plugin, &self.id, self.key, url, &tree, cx))
    }
}

struct Message {
    key: &'static str,
    plugin: String,
    id: String,
    kinds: Vec<u16>,
    prefix: Option<String>,
    order: i32,
}

impl MessageRenderer for Message {
    fn key(&self) -> &str {
        self.key
    }

    fn order(&self, _: &MessageRow) -> i32 {
        self.order
    }

    fn matches(&self, row: &MessageRow) -> bool {
        (self.kinds.is_empty() && row.kind == 9 || self.kinds.contains(&row.kind))
            && self
                .prefix
                .as_ref()
                .is_none_or(|prefix| row.content.starts_with(prefix.as_str()))
    }

    fn waiting(&self, row: &MessageRow) -> bool {
        waiting(
            self.key,
            &format!("{}:{}", row.id.to_hex(), hashed(&row.content)),
        )
    }

    fn render(&self, row: &Arc<MessageRow>, _: &Directory, cx: &App) -> Option<AnyElement> {
        // An edit is a new target.
        let target = format!("{}:{}", row.id.to_hex(), hashed(&row.content));
        let tree = ask(
            &self.plugin,
            self.key,
            &target,
            "message/render",
            || json!({ "renderer": self.id, "message": message_json(row, cx) }),
            cx,
        )?;
        Some(drawn(&self.plugin, &self.id, self.key, &target, &tree, cx))
    }
}

/// A plugin accessory beside messages' reactions.
struct MessageAccessoryView {
    key: &'static str,
    plugin: String,
    id: String,
    order: i32,
    kinds: Vec<u16>,
}

impl crate::ui::renderers::MessageAccessory for MessageAccessoryView {
    fn key(&self) -> &str {
        self.key
    }

    fn order(&self) -> i32 {
        self.order
    }

    fn render(
        &self,
        row: &Arc<MessageRow>,
        open_thread: Option<&crate::ui::message::RowHandler>,
        find: Option<&crate::ui::message::FindRow>,
        cx: &App,
    ) -> Option<AnyElement> {
        let kinds_ok = if self.kinds.is_empty() {
            row.kind == 9
        } else {
            self.kinds.contains(&row.kind)
        };
        if !kinds_ok {
            return None;
        }
        // The message as it is now: an edit or a new reply asks again.
        let target = format!(
            "{}:{}:{}:{}",
            row.id.to_hex(),
            hashed(&row.content),
            row.reply_count,
            open_thread.is_some()
        );
        // What `message/openThread` runs: the place the user last acted
        // in for this accessory, or nothing where that place cannot.
        let used: Rc<dyn Fn()> = {
            let entry = (self.key.to_owned(), row.id.to_hex());
            let opener =
                open_thread.map(|open| (Rc::downgrade(open), find.map(Rc::downgrade), row.clone()));
            Rc::new(move || {
                THREAD_OPENERS.with_borrow_mut(|openers| {
                    if openers.len() >= 4096 {
                        openers.clear();
                    }
                    match &opener {
                        Some(opener) => openers.insert(entry.clone(), opener.clone()),
                        None => openers.remove(&entry),
                    };
                })
            })
        };
        let community = community_of(row, cx)
            .map(|community| community.read(cx).origin.websocket().to_string());
        let tree = ask(
            &self.plugin,
            self.key,
            &target,
            "message/accessory",
            || {
                json!({
                    "accessory": self.id,
                    "message": message_json(row, cx),
                    "openThread": open_thread.is_some(),
                    "community": community,
                })
            },
            cx,
        )?;
        Some(drawn_with(
            &self.plugin,
            &self.id,
            self.key,
            &target,
            &tree,
            Some(used),
            cx,
        ))
    }
}

struct AttachmentView {
    key: &'static str,
    plugin: String,
    id: String,
    mime: Vec<String>,
}

impl AttachmentRenderer for AttachmentView {
    fn key(&self) -> &str {
        self.key
    }

    fn waiting(&self, attachment: &Attachment) -> bool {
        let removable = removable(&self.plugin, &attachment.url);
        waiting(
            self.key,
            &format!("{}#removable={removable}", attachment.url),
        )
    }

    fn matches(&self, attachment: &Attachment) -> bool {
        let mime = attachment.mime.as_deref().unwrap_or_default();
        self.mime
            .iter()
            .any(|prefix| mime.starts_with(prefix.as_str()))
    }

    fn render(
        &self,
        attachment: &Attachment,
        _: SharedString,
        _: &Media,
        cx: &App,
    ) -> Option<AnyElement> {
        // Asked again when it stops (or starts) being removable.
        let removable = removable(&self.plugin, &attachment.url);
        let target = format!("{}#removable={removable}", attachment.url);
        let tree = ask(
            &self.plugin,
            self.key,
            &target,
            "attachment/render",
            || {
                json!({
                    "renderer": self.id,
                    "attachment": {
                        "url": attachment.url,
                        "kind": match attachment.kind {
                            AttachmentKind::Image => "image",
                            AttachmentKind::Video => "video",
                            AttachmentKind::Audio => "audio",
                            AttachmentKind::File => "file",
                        },
                        "mime": attachment.mime,
                        "name": attachment.name,
                        "size": attachment.size,
                        "duration": attachment.duration,
                        "waveform": attachment.waveform,
                        "voiceNote": attachment.voice_note,
                        "dimensions": attachment.dimensions.map(|size| json!({
                            "width": size.width,
                            "height": size.height,
                        })),
                        "blurhash": attachment.blurhash,
                        "previewUrl": attachment.preview_url,
                    },
                    "removable": removable,
                })
            },
            cx,
        )?;
        // Its events name the attachment by its URL.
        Some(drawn(
            &self.plugin,
            &self.id,
            self.key,
            &attachment.url,
            &tree,
            cx,
        ))
    }
}

struct Action {
    key: &'static str,
    plugin: String,
    id: String,
    title: String,
    icon: Option<IconName>,
    kinds: Vec<u16>,
    /// Only the viewer's own messages.
    mine: bool,
    /// The rest of the declared filters (`matches(message)`).
    attachments: Option<bool>,
    replies: Option<bool>,
    pattern: Option<regex::Regex>,
    channels: Vec<String>,
    /// Lowercase pubkeys.
    authors: Vec<String>,
    reactions: Option<bool>,
}

impl MessageAction for Action {
    fn key(&self) -> &str {
        self.key
    }

    fn title(&self) -> &str {
        &self.title
    }

    fn icon(&self) -> Option<IconName> {
        self.icon
    }

    fn matches(&self, row: &MessageRow, viewer: &str) -> bool {
        (self.kinds.is_empty() && row.kind == 9 || self.kinds.contains(&row.kind))
            && (!self.mine || (!viewer.is_empty() && row.author.to_hex() == viewer))
            && self
                .attachments
                .is_none_or(|wanted| wanted == !row.attachments.is_empty())
            && self
                .replies
                .is_none_or(|wanted| wanted == row.thread_root_id.is_some())
            && self
                .pattern
                .as_ref()
                .is_none_or(|pattern| pattern.is_match(&row.content))
            && (self.channels.is_empty()
                || self
                    .channels
                    .iter()
                    .any(|channel| **channel == *row.channel))
            && (self.authors.is_empty() || self.authors.contains(&row.author.to_hex()))
            && self
                .reactions
                .is_none_or(|wanted| wanted == !row.reactions.is_empty())
    }

    fn run(&self, row: &Arc<MessageRow>, _: &mut Window, cx: &mut App) {
        let message = message_json(row, cx);
        // The message's own community, which an embedded message may not
        // share with the selected one.
        let community = community_of(row, cx)
            .map(|community| community.read(cx).origin.websocket().to_string());
        ExternalPlugins::notify_running(
            &self.plugin,
            "message/action",
            json!({ "action": self.id, "message": message, "community": community }),
            cx,
        );
    }

    fn marker(&self, row: &MessageRow, _: &App) -> Option<AnyElement> {
        let marked = MARKS.with_borrow(|marks| {
            marks
                .get(self.key)
                .is_some_and(|ids| ids.contains(&row.id.to_hex()))
        });
        marked.then(|| {
            Icon::new(self.icon.unwrap_or(IconName::Flag))
                .size(14.)
                .into_any_element()
        })
    }
}

/// `message/markers`: the messages action `id` marks now.
pub fn set_marks(plugin: &str, params: &Value, cx: &mut App) {
    let Some(action) = params.get("action").and_then(Value::as_str) else {
        return;
    };
    let ids: HashSet<String> = params
        .get("messages")
        .and_then(Value::as_array)
        .map(|ids| {
            ids.iter()
                .filter_map(Value::as_str)
                .take(10_000)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    MARKS.with_borrow_mut(|marks| marks.insert(format!("{plugin}/{action}"), ids));
    cx.refresh_windows();
}

/// `settings/visibility`: whether one of the plugin's Settings sections
/// is listed (`visible`), left out (`hidden`) or still being decided
/// (`pending`).
pub fn set_card_visibility(plugin: &str, params: &Value, cx: &mut App) {
    use crate::ui::settings::cards::{self, CardVisibility};
    let Some(card) = params.get("card").and_then(Value::as_str) else {
        return;
    };
    let state = match params.get("visibility").and_then(Value::as_str) {
        Some("hidden") => CardVisibility::Hidden,
        Some("pending") => CardVisibility::Pending,
        Some("error") => CardVisibility::Error,
        _ => CardVisibility::Visible,
    };
    CARD_STATES.with_borrow_mut(|states| states.insert(format!("{plugin}/{card}"), state));
    cards::changed(cx);
    cx.refresh_windows();
}

/// A settings card hidden or whose check failed is checked again
/// (`ensure`): it is pending until its plugin answers
/// `settings/visibility`.
pub fn recheck_card(key: &str, cx: &mut App) -> bool {
    use crate::ui::settings::cards::{self, CardVisibility};
    let plugin = crate::plugins::plugin_of(key);
    let checked = CARD_STATES.with_borrow_mut(|states| match states.get_mut(key) {
        Some(state) if *state == CardVisibility::Error => {
            *state = CardVisibility::Pending;
            true
        }
        _ => false,
    });
    // A hidden card is asked about again but stays hidden: the visit
    // fails now unless the plugin answers otherwise (`ensure()`).
    if !checked
        && CARD_STATES.with_borrow(|states| states.get(key) == Some(&CardVisibility::Hidden))
    {
        let card = key.rsplit('/').next().unwrap_or(key);
        ExternalPlugins::notify_running(plugin, "settings/check", json!({ "cards": [card] }), cx);
    }
    if checked {
        let card = key.rsplit('/').next().unwrap_or(key);
        ExternalPlugins::notify_running(plugin, "settings/check", json!({ "cards": [card] }), cx);
        cards::changed(cx);
    }
    checked
}

/// `page/badge`: the text on a page's row, or none.
pub fn set_badge(plugin: &str, params: &Value, cx: &mut App) {
    let Some(page) = params.get("page").and_then(Value::as_str) else {
        return;
    };
    let key = format!("{plugin}/{page}");
    let text = params
        .get("text")
        .and_then(Value::as_str)
        .map(|text| text.chars().take(8).collect::<String>())
        .filter(|text| !text.is_empty());
    let tone = match params.get("tone").and_then(Value::as_str) {
        Some("accent") => BadgeTone::Accent,
        Some("success") => BadgeTone::Success,
        Some("warning") => BadgeTone::Warning,
        Some("danger") => BadgeTone::Danger,
        Some("prominent") => BadgeTone::Prominent,
        _ => BadgeTone::Neutral,
    };
    BADGES.with_borrow_mut(|badges| match text {
        Some(text) => badges.insert(key, (text, tone)),
        None => badges.remove(&key),
    });
    cx.refresh_windows();
}

/// A page row's badge for `key`.
pub fn badge(key: &'static str) -> crate::ui::pages::Badge {
    Rc::new(move |_| {
        BADGES.with_borrow(|badges| {
            badges
                .get(key)
                .map(|(text, tone)| Badge::new(text.clone()).tone(*tone).into_any_element())
        })
    })
}

/// What a plugin says about its tool's button (`composer/toolState`).
#[derive(Clone, Default)]
struct ToolState {
    pressed: bool,
    badge: Option<SharedString>,
    /// Why it cannot be used now.
    disabled: Option<SharedString>,
}

thread_local! {
    /// Tool states by `plugin/tool`, for every composer (`None`) or one.
    static TOOL_STATES: RefCell<HashMap<(String, Option<u64>), ToolState>> = RefCell::default();
    /// The open tool popover: its composer, `plugin/tool` and view.
    static TOOL_POPOVER: RefCell<Option<(u64, String, AnyView)>> = const { RefCell::new(None) };
}

/// `composer/toolState`: a tool's pressed state, badge and why it is off,
/// for one `composer` or all of them.
pub fn tool_state(
    plugin: &str,
    params: &Value,
    cx: &mut App,
) -> Result<Value, capabilities::Failure> {
    let tool = params
        .get("tool")
        .and_then(Value::as_str)
        .ok_or((protocol::INVALID_PARAMS, "tool is required".to_owned()))?;
    let text = |key: &str| {
        params
            .get(key)
            .and_then(Value::as_str)
            .map(|text| SharedString::from(text.chars().take(64).collect::<String>()))
    };
    let state = ToolState {
        pressed: params.get("pressed").and_then(Value::as_bool) == Some(true),
        badge: text("badge"),
        disabled: text("disabled"),
    };
    let composer = params.get("composer").and_then(Value::as_u64);
    TOOL_STATES
        .with_borrow_mut(|states| states.insert((format!("{plugin}/{tool}"), composer), state));
    cx.refresh_windows();
    Ok(Value::Null)
}

/// `composer/openPopover`: a view of kind `toolPopover` under one of the
/// plugin's tools in `composer`, until `ui/closeView`, a click outside or
/// another popover.
pub fn open_popover(
    plugin: &str,
    params: &Value,
    cx: &mut App,
) -> Result<Value, capabilities::Failure> {
    let tool = params
        .get("tool")
        .and_then(Value::as_str)
        .ok_or((protocol::INVALID_PARAMS, "tool is required".to_owned()))?;
    let composer = params
        .get("composer")
        .and_then(Value::as_u64)
        .ok_or((protocol::INVALID_PARAMS, "composer is required".to_owned()))?;
    if granted(plugin, composer).is_none() {
        return Err((1, "That composer is closed.".to_owned()));
    }
    let close: crate::ui::account_actions::Close = Rc::new(|_, cx| close_popover(cx));
    let context = params.get("context").cloned().unwrap_or(Value::Null);
    let view = super::view::ExternalView::tool_popover(
        plugin,
        tool,
        close,
        json!({ "composer": composer, "context": context }),
        cx,
    );
    let id = view.read(cx).view_id().to_owned();
    TOOL_POPOVER
        .with_borrow_mut(|open| *open = Some((composer, format!("{plugin}/{tool}"), view.into())));
    cx.refresh_windows();
    Ok(json!({ "view": id }))
}

fn close_popover(cx: &mut App) {
    TOOL_POPOVER.with_borrow_mut(|open| *open = None);
    cx.refresh_windows();
}

/// A composer tool's toolbar button.
struct Tool {
    plugin: String,
    id: String,
    title: SharedString,
    icon: IconName,
    context: ToolContext,
    composer: u64,
}

impl Render for Tool {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (plugin, id, channel, composer, thread) = (
            self.plugin.clone(),
            self.id.clone(),
            self.context.channel.clone(),
            self.composer,
            self.context.thread.clone(),
        );
        // The composer's community (`ComposerToolProps.scope`).
        let community = self
            .context
            .community
            .read(cx)
            .origin
            .websocket()
            .to_string();
        let (disabled, invite_agents) = self
            .context
            .composer
            .upgrade()
            .map(|composer| {
                let composer = composer.read(cx);
                (composer.disabled(cx), composer.invites_agents(cx))
            })
            .unwrap_or((true, false));
        let key = format!("{}/{}", self.plugin, self.id);
        let state = TOOL_STATES.with_borrow(|states| {
            states
                .get(&(key.clone(), Some(self.composer)))
                .or_else(|| states.get(&(key.clone(), None)))
                .cloned()
                .unwrap_or_default()
        });
        let popover = TOOL_POPOVER.with_borrow(|open| {
            open.as_ref()
                .filter(|(composer, tool, _)| *composer == self.composer && *tool == key)
                .map(|(_, _, view)| view.clone())
        });
        let tooltip = match &state.disabled {
            Some(reason) => SharedString::from(format!("{} — {reason}", self.title)),
            None => self.title.clone(),
        };
        let c = *cx.colors();
        let button = IconButton::new(
            SharedString::from(format!("plugin-tool-{}-{}", self.plugin, self.id)),
            self.icon,
        )
        .size(IconButtonSize::Sm)
        .tooltip(tooltip)
        .selected(state.pressed || popover.is_some())
        .disabled(disabled || state.disabled.is_some())
        .on_click(move |_, _, cx| {
            ExternalPlugins::notify_running(
                &plugin,
                "composer/tool",
                json!({
                    "tool": id,
                    "community": community,
                    "channel": channel,
                    "thread": thread,
                    "composer": composer,
                    "inviteAgents": invite_agents,
                }),
                cx,
            );
        });
        div()
            .relative()
            .child(button)
            .children(state.badge.map(|badge| {
                div()
                    .absolute()
                    .top(px(-4.))
                    .right(px(-4.))
                    .px(px(4.))
                    .rounded_full()
                    .bg(c.affordance_accent_prominent)
                    .text_color(c.text_on_accent)
                    .text_caption()
                    .child(badge)
            }))
            .children(popover.map(|view| {
                // Above the button: the composer sits at the bottom.
                div().absolute().left_0().top_0().child(
                    deferred(
                        anchored()
                            .anchor(Anchor::BottomLeft)
                            .offset(point(px(0.), px(-4.)))
                            .snap_to_window_with_margin(px(8.))
                            .child(
                                div()
                                    .occlude()
                                    .w(px(320.))
                                    .max_h(px(400.))
                                    .overflow_hidden()
                                    .p(px(8.))
                                    .rounded(px(12.))
                                    .bg(c.surface_popover)
                                    .border_1()
                                    .border_color(c.border_standard)
                                    .shadow_lg()
                                    .on_mouse_down_out(|_, _, cx| close_popover(cx))
                                    .child(view),
                            ),
                    )
                    .with_priority(1),
                )
            }))
    }
}

/// `composer/insert`: puts text at a composer's caret.
pub fn insert(plugin: &str, params: &Value, cx: &mut App) -> Result<Value, capabilities::Failure> {
    let id = params
        .get("composer")
        .and_then(Value::as_u64)
        .ok_or((protocol::INVALID_PARAMS, "composer is required".to_owned()))?;
    let text = params
        .get("text")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .ok_or((protocol::INVALID_PARAMS, "text is required".to_owned()))?
        .to_owned();
    let composer = granted(plugin, id).ok_or((1, "That composer is closed.".to_owned()))?;
    if composer.read(cx).disabled(cx) {
        return Err((1, "That composer cannot be written in now.".to_owned()));
    }
    let window =
        crate::ui::navigation::main_window(cx).ok_or((1, "The window is not open.".to_owned()))?;
    let input = composer.read(cx).input();
    // Within the draft's limit, as buzz-app's `insertText` refuses more.
    let (draft, selected) = {
        let state = input.read(cx);
        (state.value(), state.selected_range())
    };
    // What stays once the selection is replaced, plus the text.
    let replaced = draft
        .get(selected)
        .map_or(0, |replaced| replaced.chars().count());
    if draft.chars().count() - replaced + text.chars().count() > 16_000 {
        return Err((3, "The draft would be over 16,000 characters.".to_owned()));
    }
    window
        .update(cx, |_, window, cx| {
            input.update(cx, |state, cx| state.insert(text, window, cx));
        })
        .map_err(|_| (1, "The window is not open.".to_owned()))?;
    Ok(Value::Null)
}

/// The open composer `params.composer` names, and the window it is in.
/// A composer id `composer/insert` and the like take, for `composer`.
pub(super) fn composer_id(plugin: &str, composer: &Entity<Composer>) -> u64 {
    grant(plugin, composer.downgrade(), None)
}

struct Grant {
    composer: WeakEntity<Composer>,
    plugin: String,
    /// The tool it was handed to, when it was.
    tool: Option<gpui_kit::AnyWeakEntity>,
}

impl Grant {
    fn live(&self) -> Option<Entity<Composer>> {
        if self
            .tool
            .as_ref()
            .is_some_and(|tool| tool.upgrade().is_none())
        {
            return None;
        }
        self.composer.upgrade()
    }
}

fn grant(
    plugin: &str,
    composer: WeakEntity<Composer>,
    tool: Option<gpui_kit::AnyWeakEntity>,
) -> u64 {
    let id = NEXT_COMPOSER.with(|next| {
        next.set(next.get() + 1);
        next.get()
    });
    COMPOSERS.with_borrow_mut(|composers| {
        composers.retain(|_, grant| grant.live().is_some());
        composers.insert(
            id,
            Grant {
                composer,
                plugin: plugin.to_owned(),
                tool,
            },
        );
    });
    id
}

/// The live composer behind `id`, if it was handed to `plugin`.
fn granted(plugin: &str, id: u64) -> Option<Entity<Composer>> {
    COMPOSERS.with_borrow(|composers| {
        composers
            .get(&id)
            .filter(|grant| grant.plugin == plugin)
            .and_then(Grant::live)
    })
}

/// Whether `plugin`'s renderer was offered to remove the draft at `url`.
pub fn may_remove(plugin: &str, url: &str) -> bool {
    REMOVERS.with_borrow(|removers| removers.get(url).is_some_and(|owner| owner == plugin))
}

fn composer_of(
    plugin: &str,
    params: &Value,
    cx: &App,
) -> Result<(Entity<Composer>, AnyWindowHandle), capabilities::Failure> {
    let id = params
        .get("composer")
        .and_then(Value::as_u64)
        .ok_or((protocol::INVALID_PARAMS, "composer is required".to_owned()))?;
    let composer = granted(plugin, id).ok_or((1, "That composer is closed.".to_owned()))?;
    let window =
        crate::ui::navigation::main_window(cx).ok_or((1, "The window is not open.".to_owned()))?;
    Ok((composer, window))
}

/// [`composer_of`] for a change to the draft: refused while nothing can
/// be typed there (`ComposerToolProps.disabled`).
fn writable(
    plugin: &str,
    params: &Value,
    cx: &App,
) -> Result<(Entity<Composer>, AnyWindowHandle), capabilities::Failure> {
    let (composer, window) = composer_of(plugin, params, cx)?;
    if composer.read(cx).disabled(cx) {
        return Err((1, "That composer cannot be written in now.".to_owned()));
    }
    Ok((composer, window))
}

/// `composer/insertMentions`: mention chips for `pubkeys` (or `mentions`,
/// each `{"pubkey","name"}` to show its own name) at the caret, which also
/// make them recipients (`insertMention`, `insertMentions`).
pub fn insert_mentions(
    plugin: &str,
    params: &Value,
    cx: &mut App,
) -> Result<Value, capabilities::Failure> {
    let (composer, window) = writable(plugin, params, cx)?;
    let hex = |key: &str| key.len() == 64 && key.bytes().all(|byte| byte.is_ascii_hexdigit());
    // All or nothing (`insertMentions`): one bad entry refuses the lot.
    let bad = |message: &str| (protocol::INVALID_PARAMS, message.to_owned());
    let named: Vec<(String, Option<String>)> = match params.get("mentions") {
        Some(mentions) => mentions
            .as_array()
            .ok_or_else(|| bad("mentions must be a list"))?
            .iter()
            .map(|mention| {
                let key = mention
                    .get("pubkey")
                    .and_then(Value::as_str)
                    .filter(|key| hex(key))
                    .ok_or_else(|| bad("each mention needs a pubkey"))?;
                let name = mention
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned);
                Ok((key.to_ascii_lowercase(), name))
            })
            .collect::<Result<_, capabilities::Failure>>()?,
        None => params
            .get("pubkeys")
            .and_then(Value::as_array)
            .ok_or_else(|| bad("pubkeys must be a list"))?
            .iter()
            .map(|key| {
                key.as_str()
                    .filter(|key| hex(key))
                    .map(|key| (key.to_ascii_lowercase(), None))
                    .ok_or_else(|| bad("pubkeys must be pubkeys"))
            })
            .collect::<Result<_, capabilities::Failure>>()?,
    };
    let mut seen = std::collections::HashSet::new();
    let named: Vec<_> = named
        .into_iter()
        .filter(|(key, _)| seen.insert(key.clone()))
        .collect();
    if named.len() > 32 {
        return Err((3, "Choose at most 32 recipients".to_owned()));
    }
    if named.is_empty() {
        return Err((protocol::INVALID_PARAMS, "pubkeys names no one".to_owned()));
    }
    // Names from the composer's own community, which may not be selected.
    let community = composer.read(cx).community().clone();
    let profiles = community.read(cx).snapshot.profiles.clone();
    let recipients: Vec<(String, SharedString, Option<String>)> = named
        .into_iter()
        .map(|(key, name)| {
            let name = name.unwrap_or_else(|| {
                profiles
                    .get(&key)
                    .map(|profile| profile.name.clone())
                    .unwrap_or_else(|| key.chars().take(10).collect())
            });
            (key, name.into(), None)
        })
        .collect();
    let inserted = window
        .update(cx, |_, window, cx| {
            composer.update(cx, |composer, cx| {
                composer.insert_team(&recipients, None, window, cx)
            })
        })
        .map_err(|_| (1, "The window is not open.".to_owned()))?;
    if inserted {
        Ok(Value::Null)
    } else {
        // The composer's own reason: someone not mentionable, or too many.
        let reason = composer
            .read(cx)
            .error_text()
            .unwrap_or_else(|| "Choose at most 32 recipients".to_owned());
        Err((3, reason))
    }
}

/// `composer/insertResource`: a link to `uri`, labelled `label`, as one
/// chip at the caret (`insertResource`).
pub fn insert_resource(
    plugin: &str,
    params: &Value,
    cx: &mut App,
) -> Result<Value, capabilities::Failure> {
    let (composer, window) = writable(plugin, params, cx)?;
    let text = |key: &str| {
        params
            .get(key)
            .and_then(Value::as_str)
            .ok_or((protocol::INVALID_PARAMS, format!("{key} is required")))
    };
    let (uri, label) = (text("uri")?.to_owned(), text("label")?.to_owned());
    window
        .update(cx, |_, window, cx| {
            crate::ui::composer_tools::insert_resource(&composer, &uri, &label, window, cx)
        })
        .map_err(|_| (1, "The window is not open.".to_owned()))?
        .map(|()| Value::Null)
        .map_err(|reason| (3, reason.to_owned()))
}

/// `composer/capture`: the plugin's view of kind `capture` in place of
/// the composer's editor, as voice notes record (`capture`), until
/// `composer/accept` or the user discards it.
pub fn capture(plugin: &str, params: &Value, cx: &mut App) -> Result<Value, capabilities::Failure> {
    let (composer, window) = writable(plugin, params, cx)?;
    let id = params
        .get("composer")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let weak = composer.downgrade();
    // Its accept ends only this capture, not one begun after it.
    let made = Rc::new(std::cell::Cell::new(None));
    let accept: super::view::Accept = Rc::new({
        let made = made.clone();
        move |file, window, cx| {
            made.get().is_some_and(|capture| {
                weak.update(cx, |composer, cx| {
                    composer.accept_plugin_capture(capture, file, window, cx)
                })
                .unwrap_or(false)
            })
        }
    });
    // Refused before the view opens, so a plugin never starts recording
    // for a capture that cannot be.
    if let Some(refusal) = composer.update(cx, |composer, cx| composer.capture_refusal(cx)) {
        return Err((3, refusal));
    }
    let (view, view_id) = super::view::ExternalView::capture(plugin, id, accept, cx);
    let capture = view.entity_id();
    made.set(Some(capture));
    window
        .update(cx, |_, window, cx| {
            composer.update(cx, |composer, cx| {
                composer.start_plugin_capture(view.into(), window, cx)
            })
        })
        .map_err(|_| (1, "The window is not open.".to_owned()))?
        .map_err(|reason| (3, reason))?;
    CAPTURING.with_borrow_mut(|capturing| {
        capturing.retain(|_, (composer, _)| composer.upgrade().is_some());
        capturing.insert(id, (composer.downgrade(), capture));
    });
    Ok(json!({ "view": view_id }))
}

/// `composer/accept`: ends a capture, attaching the file in `data`
/// (base64, with `name` and `mime`) if given.
pub fn accept(
    params: &Value,
    accept: super::view::Accept,
    cx: &mut App,
) -> Result<Value, capabilities::Failure> {
    use base64::Engine;
    use buzz_relay_client::upload::Source;
    let invalid = |text: &str| (protocol::INVALID_PARAMS, text.to_owned());
    // A file on disk, or a small one inline.
    let given = match (
        params.get("path").and_then(Value::as_str),
        params.get("data").and_then(Value::as_str),
    ) {
        (Some(path), _) => {
            let path = std::path::PathBuf::from(path);
            let size = std::fs::metadata(&path)
                .ok()
                .filter(|meta| meta.is_file())
                .map(|meta| meta.len())
                .ok_or_else(|| invalid("path is not a readable file"))?;
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned());
            let head = {
                use std::io::Read;
                let mut head = vec![0; 512];
                let read = std::fs::File::open(&path)
                    .and_then(|mut file| file.read(&mut head))
                    .unwrap_or(0);
                head.truncate(read);
                head
            };
            Some((Source::Path(path), size, name, head))
        }
        (None, Some(data)) => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data)
                .map_err(|_| invalid("data is not base64"))?;
            if bytes.len() > 3 * 1024 * 1024 {
                return Err(invalid(
                    "Inline data is at most 3 MiB; pass a path instead.",
                ));
            }
            let head = bytes[..bytes.len().min(512)].to_vec();
            let size = bytes.len() as u64;
            Some((Source::Bytes(bytes.into()), size, None, head))
        }
        (None, None) => None,
    };
    let file = match given {
        None => None,
        Some((source, size, file_name, head)) => {
            if size == 0 {
                return Err(invalid("The file is empty."));
            }
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.trim().is_empty())
                .map(str::to_owned)
                .or(file_name)
                .unwrap_or_else(|| "capture".to_owned())
                .chars()
                .take(200)
                .collect::<String>();
            let mime = params
                .get("mime")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| buzz_relay_client::upload::mime_type(&name, &head));
            if size > buzz_relay_client::messages::media_byte_limit(&mime) {
                return Err(invalid(
                    "The file is larger than the community takes for its type.",
                ));
            }
            let voice = match params.get("voice").filter(|voice| !voice.is_null()) {
                None => None,
                Some(voice) => {
                    let duration = voice
                        .get("duration")
                        .and_then(Value::as_f64)
                        .filter(|duration| duration.is_finite() && *duration > 0.0)
                        .ok_or((
                            protocol::INVALID_PARAMS,
                            "voice.duration must be seconds".to_owned(),
                        ))?;
                    let waveform = voice
                        .get("waveform")
                        .and_then(Value::as_array)
                        .map(|samples| {
                            samples
                                .iter()
                                .take(256)
                                .filter_map(Value::as_u64)
                                .map(|sample| sample.min(255) as u8)
                                .collect()
                        })
                        .unwrap_or_default();
                    Some((duration, waveform))
                }
            };
            Some(crate::ui::composer::Captured {
                name,
                mime,
                source,
                size,
                voice,
            })
        }
    };
    let window =
        crate::ui::navigation::main_window(cx).ok_or((1, "The window is not open.".to_owned()))?;
    let accepted = window
        .update(cx, |_, window, cx| accept(file, window, cx))
        .map_err(|_| (1, "The window is not open.".to_owned()))?;
    if accepted {
        Ok(Value::Null)
    } else {
        Err((3, "The capture has ended.".to_owned()))
    }
}

/// `composer/focus`: puts the caret back in the composer (`focus`).
pub fn focus(plugin: &str, params: &Value, cx: &mut App) -> Result<Value, capabilities::Failure> {
    let (composer, window) = composer_of(plugin, params, cx)?;
    // Nothing to focus while it cannot be used, as buzz-app's `focus`.
    if composer.read(cx).disabled(cx) {
        return Ok(Value::Null);
    }
    let input = composer.read(cx).input();
    window
        .update(cx, |_, window, cx| {
            input.update(cx, |state, cx| state.focus(window, cx))
        })
        .map_err(|_| (1, "The window is not open.".to_owned()))?;
    Ok(Value::Null)
}

/// What one plugin added to these registries, to take away later.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Added {
    Card(&'static str),
    Category(&'static str),
    Renderer(&'static str),
    Tool(&'static str),
    Shortcut(&'static str),
    Completion(&'static str),
    AccountAction(&'static str),
    ReactionPicker(&'static str),
    NamingPolicy(&'static str),
}

pub fn take_away(added: Added, cx: &mut App) {
    match added {
        Added::Card(key) => crate::ui::settings::cards::unregister(key, cx),
        Added::Category(key) => crate::notifications::categories::unregister(key, cx),
        Added::Renderer(key) => {
            crate::ui::renderers::unregister(key);
            // Its thread openers go with it.
            THREAD_OPENERS.with_borrow_mut(|openers| openers.retain(|(at, _), _| at != key));
        }
        Added::Tool(key) => crate::ui::composer_tools::unregister(key, cx),
        Added::Shortcut(key) => crate::shortcuts::unregister_external(key),
        Added::Completion(key) => composer_completions::unregister(key),
        Added::AccountAction(key) => {
            crate::ui::account_actions::unregister(key, cx);
            crate::ui::navigation::with_workspace(cx, move |workspace, _, cx| {
                workspace.withdraw_action(key, cx)
            });
        }
        Added::ReactionPicker(key) => crate::ui::emoji_picker::unregister_reaction_picker(key, cx),
        Added::NamingPolicy(key) => crate::ui::identity::unregister_policy(key),
    }
}

/// Notes plugin page `key`'s declared icon, for `contributions/list`.
pub fn set_page_icon(key: &str, icon: Option<&str>) {
    PAGE_ICONS.with_borrow_mut(|icons| match icon {
        Some(icon) => icons.insert(key.to_owned(), icon.to_owned()),
        None => icons.remove(key),
    });
}

/// Plugin page `key`'s declared icon, if it has one.
pub fn page_icon(key: &str) -> Option<String> {
    PAGE_ICONS.with_borrow(|icons| icons.get(key).cloned())
}

/// Whether composer tool `key` is registered as `declared` already, so
/// registering it again would only rebuild its views. Notes `declared`.
fn unchanged(key: &'static str, declared: &str, cx: &App) -> bool {
    let registered = crate::ui::composer_tools::listed(cx)
        .iter()
        .any(|(listed, _, _)| *listed == key);
    DECLARED.with_borrow_mut(|known| {
        let same = registered && known.get(key).is_some_and(|known| known == declared);
        known.insert(key, declared.to_owned());
        same
    })
}

/// Registers everything in `contributions` but pages and panels.
pub fn register(
    plugin: &str,
    name: &str,
    contributions: &protocol::Contributions,
    requester: Option<Requester>,
    cx: &mut App,
) -> Vec<Added> {
    use crate::notifications::categories::{self, CategorySpec};
    use crate::ui::settings::cards::{self, CardPlacement, SettingsCard};
    let mut added = Vec::new();
    let key = |id: &str| intern(format!("{plugin}/{id}"));
    let had: Vec<String> = CARD_OWNERS.with_borrow_mut(|owners| {
        let cards: Vec<String> = contributions
            .settings_cards
            .iter()
            .map(|card| card.id.clone())
            .collect();
        if cards.is_empty() {
            owners.remove(plugin)
        } else {
            owners.insert(plugin.to_owned(), cards)
        }
        .unwrap_or_default()
    });
    if !contributions.settings_cards.is_empty() {
        cards::register_check("buzz.plugins/cards", check_cards, cx);
        // Cards that come while Settings shows are checked at once, as
        // `retainVisibility` ensures each card that becomes active.
        // Only the new ones: a card already held stays checked.
        let cards: Vec<&str> = contributions
            .settings_cards
            .iter()
            .map(|card| card.id.as_str())
            .filter(|id| !had.iter().any(|old| old == id))
            .collect();
        if CARDS_HELD.get() > 0 && !cards.is_empty() {
            ExternalPlugins::notify_running(
                plugin,
                "settings/check",
                json!({ "cards": cards }),
                cx,
            );
        }
    }
    // Titles and orders the contribution readers report.
    TITLES.with_borrow_mut(|titles| {
        titles.retain(|key: &String, _| !key.starts_with(&format!("{plugin}/")));
        let mut add = |id: &str, title: Option<&str>, order: Option<i32>| {
            titles.insert(
                format!("{plugin}/{id}"),
                (title.unwrap_or(id).to_owned(), order),
            );
        };
        for tool in &contributions.composer_tools {
            add(&tool.id, Some(&tool.title), Some(tool.order));
        }
        for completion in &contributions.completions {
            add(
                &completion.id,
                Some(&completion.title),
                Some(completion.order),
            );
        }
        for accessory in &contributions.composer_accessories {
            add(&accessory.id, Some(&accessory.title), Some(accessory.order));
        }
        for renderer in &contributions.inline_renderers {
            add(&renderer.id, renderer.title.as_deref(), None);
        }
        for renderer in &contributions.link_renderers {
            add(
                &renderer.id,
                renderer.title.as_deref(),
                Some(renderer.order),
            );
        }
        for renderer in &contributions.message_renderers {
            add(
                &renderer.id,
                renderer.title.as_deref(),
                Some(renderer.order),
            );
        }
        for renderer in &contributions.attachment_renderers {
            add(&renderer.id, renderer.title.as_deref(), None);
        }
    });
    for card in &contributions.settings_cards {
        let card_key = key(&card.id);
        let (owner, id) = (plugin.to_owned(), card.id.clone());
        cards::register(
            SettingsCard {
                key: card_key,
                title: intern(card.title.clone()),
                icon: card.icon.as_deref().map(|name| super::icon(Some(name))),
                placement: match card.placement.to_ascii_lowercase().as_str() {
                    "community" => CardPlacement::Community,
                    "administration" => CardPlacement::Administration,
                    _ => CardPlacement::Group(intern(card.placement.clone())),
                },
                visibility: Some(Rc::new(move |_| {
                    CARD_STATES.with_borrow(|states| {
                        states
                            .get(card_key)
                            .copied()
                            .unwrap_or(cards::CardVisibility::Visible)
                    })
                })),
                build: cards::build(move |window, cx| {
                    super::ExternalView::settings(&owner, &id, window, cx)
                }),
            },
            cx,
        );
        cards::changed(cx);
        added.push(Added::Card(card_key));
    }
    for category in &contributions.notifications {
        let category_key = key(&category.id);
        categories::register(
            CategorySpec {
                key: category_key,
                label: intern(category.label.clone()),
            },
            cx,
        );
        added.push(Added::Category(category_key));
    }
    for action in &contributions.message_actions {
        let action_key = key(&action.id);
        // Declared again by an update, it keeps its place in registration
        // order.
        crate::ui::renderers::register_action(Rc::new(Action {
            key: action_key,
            plugin: plugin.to_owned(),
            id: action.id.clone(),
            title: action.title.clone(),
            icon: action.icon.as_deref().map(|name| super::icon(Some(name))),
            kinds: action.kinds.clone(),
            mine: action.mine,
            attachments: action.attachments,
            replies: action.replies,
            pattern: action
                .pattern
                .as_deref()
                .and_then(|pattern| regex::Regex::new(pattern).ok()),
            channels: action.channels.clone(),
            authors: action
                .authors
                .iter()
                .map(|author| author.to_ascii_lowercase())
                .collect(),
            reactions: action.reactions,
        }));
        added.push(Added::Renderer(action_key));
    }
    for renderer in &contributions.link_renderers {
        let renderer_key = key(&renderer.id);
        // A restart registers it again under the same key.
        crate::ui::renderers::unregister(renderer_key);
        crate::ui::renderers::register_link(Rc::new(Link {
            key: renderer_key,
            plugin: plugin.to_owned(),
            id: renderer.id.clone(),
            prefixes: renderer.matches.clone(),
            order: renderer.order,
        }));
        added.push(Added::Renderer(renderer_key));
    }
    for renderer in &contributions.message_renderers {
        let renderer_key = key(&renderer.id);
        // A restart registers it again under the same key.
        crate::ui::renderers::unregister(renderer_key);
        crate::ui::renderers::register_message(Rc::new(Message {
            key: renderer_key,
            plugin: plugin.to_owned(),
            id: renderer.id.clone(),
            kinds: renderer.kinds.clone(),
            prefix: renderer.prefix.clone(),
            order: renderer.order,
        }));
        added.push(Added::Renderer(renderer_key));
    }
    for renderer in &contributions.attachment_renderers {
        let renderer_key = key(&renderer.id);
        // Declared again, it keeps its place in registration order.
        crate::ui::renderers::register_attachment(Rc::new(AttachmentView {
            key: renderer_key,
            plugin: plugin.to_owned(),
            id: renderer.id.clone(),
            mime: renderer.mime.clone(),
        }));
        added.push(Added::Renderer(renderer_key));
    }
    for tool in &contributions.composer_tools {
        let tool_key = key(&tool.id);
        // Unchanged and still registered: it stays, so its composer views
        // (and what they were handed) do too.
        if unchanged(tool_key, &format!("{tool:?}"), cx) {
            added.push(Added::Tool(tool_key));
            if tool.reaction_picker {
                added.push(Added::ReactionPicker(tool_key));
            }
            continue;
        }
        let (owner, id, title, icon) = (
            plugin.to_owned(),
            tool.id.clone(),
            SharedString::from(tool.title.clone()),
            super::icon(tool.icon.as_deref()),
        );
        crate::ui::composer_tools::register(
            crate::ui::composer_tools::ComposerTool {
                key: tool_key,
                order: tool.order,
                accessory: false,
                build: Rc::new(move |context, _, cx| {
                    let (plugin, id, title) = (owner.clone(), id.clone(), title.clone());
                    // Its id lives as long as this tool does.
                    let tool = cx.new(|cx| {
                        let composer = grant(
                            &plugin,
                            context.composer.clone(),
                            Some(cx.entity().downgrade().into()),
                        );
                        // A capture this tool started ends with it.
                        // Views stay while their registration does, so a
                        // released tool went or was declared anew.
                        cx.on_release(move |_: &mut Tool, cx| {
                            let capturing =
                                CAPTURING.with_borrow_mut(|capturing| capturing.remove(&composer));
                            // After the composer that dropped the tool is done.
                            if let Some((capturing, capture)) = capturing {
                                cx.defer(move |cx| {
                                    let _ = capturing.update(cx, |composer, cx| {
                                        composer.cancel_plugin_capture(capture, cx)
                                    });
                                });
                            }
                        })
                        .detach();
                        Tool {
                            plugin,
                            id,
                            title,
                            icon,
                            context,
                            composer,
                        }
                    });
                    tool.into()
                }),
            },
            cx,
        );
        added.push(Added::Tool(tool_key));
        if tool.reaction_picker {
            let (owner, id) = (plugin.to_owned(), tool.id.clone());
            crate::ui::emoji_picker::register_reaction_picker(
                crate::ui::emoji_picker::ReactionPicker {
                    key: tool_key,
                    order: tool.order,
                    build: Rc::new(move |select, community, window, cx| {
                        super::view::ExternalView::reaction_picker(
                            &owner, &id, select, community, window, cx,
                        )
                    }),
                },
                cx,
            );
            added.push(Added::ReactionPicker(tool_key));
        }
    }
    for accessory in &contributions.composer_accessories {
        let accessory_key = key(&accessory.id);
        if accessory.placement == protocol::AccessoryPlacement::Message {
            crate::ui::renderers::unregister(accessory_key);
            crate::ui::renderers::register_message_accessory(Rc::new(MessageAccessoryView {
                key: accessory_key,
                plugin: plugin.to_owned(),
                id: accessory.id.clone(),
                order: accessory.order,
                kinds: accessory.kinds.clone(),
            }));
            added.push(Added::Renderer(accessory_key));
            continue;
        }
        if unchanged(accessory_key, &format!("{accessory:?}"), cx) {
            added.push(Added::Tool(accessory_key));
            continue;
        }
        let (owner, id) = (plugin.to_owned(), accessory.id.clone());
        crate::ui::composer_tools::register(
            crate::ui::composer_tools::ComposerTool {
                key: accessory_key,
                order: accessory.order,
                accessory: true,
                build: Rc::new(move |context, window, cx| {
                    super::view::ExternalView::accessory(
                        &owner,
                        &id,
                        &context.community,
                        &context.channel,
                        context.thread.as_deref(),
                        window,
                        cx,
                    )
                }),
            },
            cx,
        );
        added.push(Added::Tool(accessory_key));
    }
    for action in &contributions.account_actions {
        let action_key = key(&action.id);
        let (owner, id, title) = (
            plugin.to_owned(),
            action.id.clone(),
            SharedString::from(action.title.clone()),
        );
        crate::ui::account_actions::register(
            crate::ui::account_actions::AccountAction {
                key: action_key,
                title: title.clone(),
                icon: super::icon(action.icon.as_deref()),
                build: Rc::new(move |close, window, cx| {
                    super::view::ExternalView::dialog(&owner, &id, title.clone(), close, window, cx)
                }),
            },
            cx,
        );
        added.push(Added::AccountAction(action_key));
    }
    for completion in &contributions.completions {
        let completion_key = key(&completion.id);
        let Some(trigger) = completion.trigger.chars().next() else {
            continue;
        };
        let (plugin, id) = (plugin.to_owned(), completion.id.clone());
        composer_completions::register(composer_completions::PluginCompletion {
            key: completion_key,
            trigger,
            order: completion.order,
            query: Rc::new(move |query, place, cx| {
                suggest(&plugin, completion_key, &id, query, place, cx)
            }),
            retry: Rc::new(move || forget_suggestions(completion_key)),
        });
        added.push(Added::Completion(completion_key));
    }
    if let Some(requester) = &requester {
        for policy in &contributions.naming_policies {
            added.push(Added::NamingPolicy(super::naming::register(
                plugin,
                &policy.id,
                requester.clone(),
            )));
        }
    }
    if let Some(requester) = requester {
        for renderer in &contributions.inline_renderers {
            let Ok(pattern) = renderer.regex() else {
                continue;
            };
            let renderer_key = key(&renderer.id);
            // Declared again, it keeps its place in registration order.
            TEXT_KEYS.with_borrow_mut(|keys| keys.insert(renderer_key.to_owned()));
            crate::ui::renderers::register_text(Rc::new(Inline {
                key: renderer_key,
                plugin: plugin.to_owned(),
                id: renderer.id.clone(),
                pattern,
                requester: requester.clone(),
            }));
            added.push(Added::Renderer(renderer_key));
        }
    }
    let owner = intern(name.to_owned());
    for shortcut in &contributions.shortcuts {
        let shortcut_key = key(&shortcut.id);
        let defaults: Vec<crate::shortcuts::Binding> = shortcut
            .keys
            .iter()
            .filter_map(|keys| protocol::Chord::parse(keys).ok())
            .map(|chord| {
                crate::shortcuts::binding(&chord.key, chord.primary, chord.shift, chord.alt)
            })
            .collect();
        let (plugin, id) = (plugin.to_owned(), shortcut.id.clone());
        crate::shortcuts::register_external(crate::shortcuts::ExternalShortcut {
            key: shortcut_key,
            plugin: owner,
            title: intern(shortcut.title.clone()),
            defaults: Box::leak(defaults.into_boxed_slice()),
            order: shortcut.order,
            in_editable: shortcut.allow_in_editable,
            in_modal: shortcut.allow_in_modal,
            repeat: shortcut.repeat,
            run: Rc::new(move |_, cx| {
                ExternalPlugins::notify_running(&plugin, "shortcut", json!({ "shortcut": id }), cx);
            }),
        });
        added.push(Added::Shortcut(shortcut_key));
    }
    added
}
