//! End and Recover for a thread whose session would not start: the
//! desktop's `RemoteSessionRecoveryBanner`, as a thread banner. A start
//! that fails with a lifecycle code leaves a prompt; the banner offers what
//! that code asks for, and a start or recovery that works clears it.
//! Prompts are kept in memory only.

use super::{Sent, send};
use crate::agents2::Agents2;
use crate::ui::thread_banners::{self, Banner, BannerButton, BannerThread, ThreadBanner};
use buzz_host::api::SandboxScope;
use buzz_host::api::{EndState, RecoveryMode, SandboxRequest, SandboxResponse, SandboxSession};
use buzz_remote_sessions::{
    RecoveryKind, RecoveryPrompt, RecoveryPromptKey, RecoveryPrompts, SessionError, SessionScope,
    offers_checkpoint, recovery_kind,
};
use gpui_kit::*;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

const BANNER: &str = "voltage.remote-workspace/recovery";

/// A button the banner offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Recover(RecoveryMode),
    /// Sends End; its reply's state picks the next banner.
    End,
    /// Sends End again while the session is ending.
    Check,
    Dismiss,
}

impl Action {
    fn label(self) -> &'static str {
        match self {
            Self::Recover(RecoveryMode::Checkpoint) => "Recover from checkpoint",
            Self::Recover(RecoveryMode::Fresh) => "Start fresh",
            Self::End => "End session",
            Self::Check => "Check session",
            Self::Dismiss => "Dismiss",
        }
    }

    /// What the banner says while this action runs. A recovery can wait
    /// minutes for a machine, so it says the user has nothing left to do.
    fn progress(self) -> &'static str {
        match self {
            Self::Recover(RecoveryMode::Checkpoint) => {
                "Recovering from the checkpoint. Starting a machine can take a few minutes. \
                 Nothing else to do: the agent replies here when it is ready."
            }
            Self::Recover(RecoveryMode::Fresh) => {
                "Starting a fresh session. Starting a machine can take a few minutes. \
                 Nothing else to do: the agent replies here when it is ready."
            }
            // Dismiss is never in flight: it sends nothing.
            Self::End | Self::Check | Self::Dismiss => {
                "Ending the session and saving its checkpoint."
            }
        }
    }
}

/// The buttons for a prompt, as the desktop shows them. Without
/// `lifecycle`, the provider can neither end nor recover sessions.
pub fn banner_actions(kind: RecoveryKind, offers_checkpoint: bool, lifecycle: bool) -> Vec<Action> {
    let mut actions = Vec::new();
    if lifecycle {
        match kind {
            RecoveryKind::Recover => {
                if offers_checkpoint {
                    actions.push(Action::Recover(RecoveryMode::Checkpoint));
                }
                actions.push(Action::Recover(RecoveryMode::Fresh));
            }
            RecoveryKind::EndFirst => actions.push(Action::End),
            RecoveryKind::Ending => actions.push(Action::Check),
        }
    }
    actions.push(Action::Dismiss);
    actions
}

/// What a prompt's banner shows now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct View {
    pub kind: RecoveryKind,
    pub offers_checkpoint: bool,
    pub message: String,
}

impl View {
    fn of(error: &SessionError) -> Option<Self> {
        Some(Self {
            kind: recovery_kind(error)?,
            offers_checkpoint: offers_checkpoint(error),
            message: error.message.clone(),
        })
    }

    /// After an End, as the desktop's `END_OUTCOMES`.
    fn ended(state: EndState) -> Self {
        let (kind, offers_checkpoint, message) = match state {
            EndState::Ending => (
                RecoveryKind::Ending,
                true,
                "The session is ending. Recover it once it has saved its checkpoint.",
            ),
            EndState::Ended => (RecoveryKind::Recover, true, "The session has ended."),
            EndState::Absent => (
                RecoveryKind::Recover,
                false,
                "No session exists for this thread.",
            ),
        };
        Self {
            kind,
            offers_checkpoint,
            message: message.into(),
        }
    }

    /// After a failed action: a new lifecycle code retargets the buttons,
    /// and any other failure keeps them.
    fn failed(&self, error: &SessionError) -> Self {
        Self::of(error).unwrap_or_else(|| Self {
            message: error.message.clone(),
            ..self.clone()
        })
    }
}

/// One prompt and what its banner needs besides.
#[derive(Clone)]
struct Entry {
    /// The start that failed, sent again by its actions.
    session: SandboxSession,
    replay_floor: u64,
    view: View,
    /// The action in flight, if any.
    busy: Option<Action>,
    /// Which recorded failure this is. A later failed start replaces the
    /// entry with a newer generation, so an older action's answer cannot
    /// clear or change it.
    generation: u64,
}

impl Entry {
    fn request(&self, action: Action) -> Option<SandboxRequest> {
        if self.busy.is_some() {
            return None;
        }
        let session = self.session.clone();
        match action {
            Action::Recover(mode) => Some(SandboxRequest::Recover {
                session,
                mode,
                replay_floor: Some(self.replay_floor),
            }),
            Action::End | Action::Check => Some(SandboxRequest::End { session }),
            Action::Dismiss => None,
        }
    }
}

/// Failed starts awaiting a choice, and which agents' providers can end
/// and recover sessions.
#[derive(Default)]
pub struct Prompts {
    prompts: RecoveryPrompts,
    entries: HashMap<RecoveryPromptKey, Entry>,
    /// By agent; absent until its host answers.
    lifecycle: HashMap<String, bool>,
    checking: HashSet<String>,
    /// The last [`Entry::generation`] given out.
    generation: u64,
}

impl Global for Prompts {}

fn key_of(session: &SandboxSession) -> Option<(RecoveryPromptKey, SessionScope)> {
    let scope = match &session.scope {
        SandboxScope::Thread {
            channel_id,
            thread_root,
        } => SessionScope::new(channel_id.parse().ok()?, thread_root.clone()).ok()?,
        SandboxScope::DirectMessage { dm_channel_id } => SessionScope::DirectMessage {
            dm_channel_id: dm_channel_id.parse().ok()?,
        },
    };
    let key = RecoveryPromptKey::new(
        &session.tenant.relay,
        &session.tenant.signer,
        &session.pubkey,
        &scope,
    );
    Some((key, scope))
}

impl Prompts {
    /// Records a start that failed with a lifecycle code, and clears the
    /// thread's prompt once one works. Returns whether a prompt was
    /// recorded.
    pub fn started(&mut self, sent: &Sent, name: &str) -> bool {
        let Sent {
            session,
            result,
            replay_floor,
        } = sent;
        let Some((key, scope)) = key_of(session) else {
            return false;
        };
        let error = match result {
            Ok(()) => {
                self.clear(&key);
                return false;
            }
            Err(error) => error,
        };
        let Some(view) = View::of(error) else {
            // A failure without a lifecycle code asks for nothing new, but it
            // is newer than any action in flight here: fence that action's
            // answer off so it cannot hide this failure.
            self.supersede(&key);
            return false;
        };
        self.prompts.record(RecoveryPrompt {
            agent_pubkey: session.pubkey.clone(),
            agent_name: name.into(),
            session: scope,
            expected_relay_url: session.tenant.relay.clone(),
            expected_signer_pubkey: session.tenant.signer.clone(),
            error: error.clone(),
        });
        self.generation += 1;
        let entry = Entry {
            session: session.clone(),
            replay_floor: *replay_floor,
            view,
            busy: None,
            generation: self.generation,
        };
        self.entries.insert(key, entry);
        let kept: HashSet<RecoveryPromptKey> = self.prompts.iter().map(|p| p.key()).collect();
        self.entries.retain(|key, _| kept.contains(key));
        true
    }

    pub fn clear(&mut self, key: &RecoveryPromptKey) {
        self.prompts.clear(key);
        self.entries.remove(key);
    }

    /// Moves `key`'s prompt to a new generation and ends its action in
    /// flight, so that action's answer is dropped. The prompt and its
    /// buttons stay, so the owner can act again.
    fn supersede(&mut self, key: &RecoveryPromptKey) {
        if let Some(entry) = self.entries.get_mut(key) {
            self.generation += 1;
            entry.generation = self.generation;
            entry.busy = None;
        }
    }

    /// Whether `key`'s prompt is still the failure an action of
    /// `generation` was sent for.
    fn current(&self, key: &RecoveryPromptKey, generation: u64) -> bool {
        self.entries
            .get(key)
            .is_some_and(|entry| entry.generation == generation)
    }

    /// An action's answer: a recovery clears the prompt; an End or a
    /// failure picks the next banner. An answer for an older generation
    /// changes nothing: a newer failed start replaced its prompt.
    pub fn answered(
        &mut self,
        key: &RecoveryPromptKey,
        generation: u64,
        answer: &Result<SandboxResponse, SessionError>,
    ) {
        let Some(entry) = self
            .entries
            .get_mut(key)
            .filter(|entry| entry.generation == generation)
        else {
            return;
        };
        entry.busy = None;
        match answer {
            Ok(SandboxResponse::Ended { state }) => entry.view = View::ended(*state),
            Ok(_) => self.clear(key),
            Err(error) => entry.view = entry.view.failed(error),
        }
    }

    /// The prompts in `thread` and their views, oldest first.
    fn for_thread(&self, thread: &BannerThread) -> Vec<(RecoveryPrompt, Entry)> {
        let Ok(channel) = thread.channel.parse() else {
            return Vec::new();
        };
        let scope = if thread.root.is_empty() {
            SessionScope::DirectMessage {
                dm_channel_id: channel,
            }
        } else {
            let Ok(scope) = SessionScope::new(channel, thread.root.clone()) else {
                return Vec::new();
            };
            scope
        };
        let relay = |url: &str| url.trim().trim_end_matches('/').to_owned();
        self.prompts
            .for_thread(&scope)
            .filter(|prompt| relay(&prompt.expected_relay_url) == relay(&thread.relay))
            .filter_map(|prompt| Some((prompt.clone(), self.entries.get(&prompt.key())?.clone())))
            .collect()
    }
}

/// Registers the banner. Call from the type's init.
pub fn init(cx: &mut App) {
    cx.set_global(Prompts::default());
    thread_banners::register(
        ThreadBanner {
            key: BANNER,
            order: 0,
            banners: Rc::new(banners),
        },
        cx,
    );
}

/// After a start: records or clears its prompt, and asks once whether the
/// agent's provider can end and recover sessions.
pub fn started(sent: &Sent, name: &str, cx: &mut App) {
    if sent.result.is_ok() {
        resolve(&sent.session, cx);
    }
    if !cx.has_global::<Prompts>() {
        return;
    }
    let recorded = cx.update_global(|prompts: &mut Prompts, _| prompts.started(sent, name));
    let pubkey = sent.session.pubkey.clone();
    let prompts = cx.global::<Prompts>();
    if recorded && !prompts.lifecycle.contains_key(&pubkey) && !prompts.checking.contains(&pubkey) {
        check_lifecycle(&sent.session, cx);
    }
    thread_banners::refresh(cx);
}

/// A session that works in a thread replaces the attempts that failed
/// there, so their "Could not finish" no longer hides its activity. Only
/// the session's own community changes: the scope is the one the agent's
/// local work records (`agents2::service::work_scope`).
fn resolve(session: &SandboxSession, cx: &mut App) {
    let Some(scope) = work_scope(session) else {
        return;
    };
    let (channel, root) = match &session.scope {
        SandboxScope::Thread {
            channel_id,
            thread_root,
        } => (channel_id, Some(thread_root.as_str())),
        // One session serves the whole conversation.
        SandboxScope::DirectMessage { dm_channel_id } => (dm_channel_id, None),
    };
    cx.default_global::<crate::agent_activity::local::LocalActivity>()
        .resolve_thread(&scope, &session.pubkey, channel, root);
}

/// The local work scope of the session's community and owner.
fn work_scope(session: &SandboxSession) -> Option<String> {
    let origin = buzz_relay_client::RelayOrigin::parse(&session.tenant.relay).ok()?;
    Some(crate::agent_activity::local::scope(
        &origin.http(),
        &session.tenant.signer,
    ))
}

fn check_lifecycle(session: &SandboxSession, cx: &mut App) {
    let Some(link) = link_of(&session.pubkey, cx) else {
        return;
    };
    let pubkey = session.pubkey.clone();
    cx.global_mut::<Prompts>().checking.insert(pubkey.clone());
    let request = SandboxRequest::Lifecycle {
        pubkey: pubkey.clone(),
        spec: session.spec.clone(),
    };
    let answer =
        crate::runtime::Runtime::handle(cx).spawn(async move { send(&link, request).await });
    cx.spawn(async move |cx| {
        let answer = answer.await;
        cx.update(|cx| {
            let prompts = cx.global_mut::<Prompts>();
            prompts.checking.remove(&pubkey);
            // A failed check is asked again after the next failed start.
            if let Ok(Ok(SandboxResponse::Lifecycle(supported))) = answer {
                prompts.lifecycle.insert(pubkey, supported);
            }
            thread_banners::refresh(cx);
        });
    })
    .detach();
}

fn link_of(pubkey: &str, cx: &App) -> Option<crate::agents2::runtime::HostLink> {
    let agents = Agents2::global(cx)?;
    Some(agents.read(cx).handle_of(pubkey, cx)?.host)
}

fn banners(thread: &BannerThread, cx: &App) -> Vec<Banner> {
    let Some(prompts) = cx.try_global::<Prompts>() else {
        return Vec::new();
    };
    prompts
        .for_thread(thread)
        .into_iter()
        .map(|(prompt, entry)| {
            let lifecycle = prompts.lifecycle.get(&prompt.agent_pubkey) == Some(&true);
            let key = prompt.key();
            let (message, actions) = shown(&entry, lifecycle);
            let buttons = actions
                .into_iter()
                .map(|action| {
                    let key = key.clone();
                    BannerButton {
                        label: action.label().into(),
                        loading: entry.busy == Some(action),
                        action: Rc::new(move |_, cx| act(&key, action, cx)),
                    }
                })
                .collect();
            Banner {
                text: format!("{}: {}", prompt.agent_name, message).into(),
                buttons,
            }
        })
        .collect()
}

/// The banner's message and buttons. While an action runs, only that
/// action shows, with its progress: the other buttons cannot act then.
fn shown(entry: &Entry, lifecycle: bool) -> (&str, Vec<Action>) {
    match entry.busy {
        Some(action) => (action.progress(), vec![action]),
        None => (
            entry.view.message.as_str(),
            banner_actions(entry.view.kind, entry.view.offers_checkpoint, lifecycle),
        ),
    }
}

fn act(key: &RecoveryPromptKey, action: Action, cx: &mut App) {
    let Some(entry) = cx.global::<Prompts>().entries.get(key).cloned() else {
        return;
    };
    if action == Action::Dismiss && entry.busy.is_none() {
        cx.global_mut::<Prompts>().clear(key);
        thread_banners::refresh(cx);
        return;
    }
    let Some(request) = entry.request(action) else {
        return;
    };
    let pubkey = entry.session.pubkey.clone();
    let Some(link) = link_of(&pubkey, cx) else {
        return;
    };
    if let Some(entry) = cx.global_mut::<Prompts>().entries.get_mut(key) {
        entry.busy = Some(action);
    }
    thread_banners::refresh(cx);
    let answer =
        crate::runtime::Runtime::handle(cx).spawn(async move { send(&link, request).await });
    let key = key.clone();
    let session = entry.session.clone();
    let generation = entry.generation;
    cx.spawn(async move |cx| {
        let answer = answer
            .await
            .unwrap_or_else(|_| Err("The request was cancelled".into()));
        cx.update(|cx| {
            // A newer failed start may have replaced this prompt while the
            // request ran; its failure must stay shown.
            if !cx.global::<Prompts>().current(&key, generation) {
                return;
            }
            if matches!(action, Action::Recover(_))
                && matches!(&answer, Ok(SandboxResponse::Started))
            {
                resolve(&session, cx);
            }
            cx.global_mut::<Prompts>()
                .answered(&key, generation, &answer);
            thread_banners::refresh(cx);
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use buzz_host::api::{SandboxScope, SandboxTenant};
    use core::prelude::v1::test;

    fn session(root: char) -> SandboxSession {
        SandboxSession {
            pubkey: "b".repeat(64),
            tenant: SandboxTenant {
                relay: "wss://relay.example".into(),
                signer: "a".repeat(64),
            },
            scope: SandboxScope::Thread {
                channel_id: "f9ee507e-d03c-4570-baa6-02951c38e1cd".into(),
                thread_root: root.to_string().repeat(64),
            },
            spec: Default::default(),
        }
    }

    fn coded(code: &str) -> SessionError {
        SessionError {
            message: format!("Refused: {code}"),
            code: Some(code.into()),
        }
    }

    fn thread(root: char) -> BannerThread {
        BannerThread {
            relay: "wss://relay.example/".into(),
            channel: "f9ee507e-d03c-4570-baa6-02951c38e1cd".into(),
            root: root.to_string().repeat(64),
        }
    }

    #[test]
    fn actions_follow_the_desktop() {
        use Action::*;
        let recover = |offers| banner_actions(RecoveryKind::Recover, offers, true);
        assert_eq!(
            recover(true),
            [
                Recover(RecoveryMode::Checkpoint),
                Recover(RecoveryMode::Fresh),
                Dismiss
            ]
        );
        let no_checkpoint = offers_checkpoint(&coded("no_checkpoint"));
        assert_eq!(
            recover(no_checkpoint),
            [Recover(RecoveryMode::Fresh), Dismiss]
        );
        assert_eq!(
            banner_actions(RecoveryKind::EndFirst, true, true),
            [End, Dismiss]
        );
        assert_eq!(
            banner_actions(RecoveryKind::Ending, true, true),
            [Check, Dismiss]
        );
        assert_eq!(
            banner_actions(RecoveryKind::Recover, true, false),
            [Dismiss]
        );
    }

    fn sent(root: char, result: Result<(), SessionError>) -> Sent {
        Sent {
            session: session(root),
            result,
            replay_floor: 1_700_000_123,
        }
    }

    #[test]
    fn recovery_buttons_replay_the_failed_mention_for_both_modes() {
        let mut prompts = Prompts::default();
        assert!(prompts.started(&sent('c', Err(coded("session_ended"))), "Ada"));
        let shown = prompts.for_thread(&thread('c'));
        let entry = &shown[0].1;
        for mode in [RecoveryMode::Checkpoint, RecoveryMode::Fresh] {
            let Some(SandboxRequest::Recover {
                session,
                mode: requested,
                replay_floor,
            }) = entry.request(Action::Recover(mode))
            else {
                panic!("expected recovery request")
            };
            assert_eq!(requested, mode);
            assert_eq!(session.scope, self::session('c').scope);
            assert_eq!(replay_floor, Some(1_700_000_123));
        }
    }

    #[test]
    fn a_coded_failure_records_a_prompt_and_a_success_clears_it() {
        let mut prompts = Prompts::default();
        assert!(prompts.started(&sent('c', Err(coded("session_ended"))), "Ada"));
        let shown = prompts.for_thread(&thread('c'));
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].0.agent_name, "Ada");
        assert_eq!(shown[0].1.view.kind, RecoveryKind::Recover);
        assert!(
            prompts.for_thread(&thread('d')).is_empty(),
            "another thread"
        );

        // A failure without a lifecycle code asks for nothing.
        let transient = SessionError::from("The cluster is unreachable");
        assert!(!prompts.started(&sent('d', Err(transient)), "Ada"));
        assert!(prompts.for_thread(&thread('d')).is_empty());

        assert!(!prompts.started(&sent('c', Ok(())), "Ada"));
        assert!(prompts.for_thread(&thread('c')).is_empty());
    }

    #[test]
    fn a_dm_recovery_is_visible_in_its_conversation_and_replays_the_request() {
        let mut sent = sent('c', Err(coded("session_ended")));
        sent.session.scope = SandboxScope::DirectMessage {
            dm_channel_id: thread('c').channel,
        };
        let mut prompts = Prompts::default();
        assert!(prompts.started(&sent, "Ada"));
        let mut conversation = thread('c');
        conversation.root.clear();
        let shown = prompts.for_thread(&conversation);
        assert_eq!(shown.len(), 1);
        let Some(SandboxRequest::Recover {
            session,
            replay_floor,
            ..
        }) = shown[0]
            .1
            .request(Action::Recover(RecoveryMode::Checkpoint))
        else {
            panic!("expected recover");
        };
        assert_eq!(session.scope, sent.session.scope);
        assert_eq!(replay_floor, Some(sent.replay_floor));
        assert!(prompts.for_thread(&thread('c')).is_empty());
        conversation.channel = uuid::Uuid::new_v4().to_string();
        assert!(prompts.for_thread(&conversation).is_empty());
    }

    #[test]
    fn an_end_reply_picks_the_next_banner() {
        let mut prompts = Prompts::default();
        prompts.started(&sent('c', Err(coded("session_active"))), "Ada");
        let key = key_of(&session('c')).unwrap().0;
        let generation = prompts.entries[&key].generation;
        let view = |prompts: &Prompts| prompts.entries[&key].view.clone();
        assert_eq!(view(&prompts).kind, RecoveryKind::EndFirst);
        let ending = Ok(SandboxResponse::Ended {
            state: EndState::Ending,
        });
        prompts.answered(&key, generation, &ending);
        assert_eq!(view(&prompts).kind, RecoveryKind::Ending);
        let absent = Ok(SandboxResponse::Ended {
            state: EndState::Absent,
        });
        prompts.answered(&key, generation, &absent);
        assert_eq!(view(&prompts).kind, RecoveryKind::Recover);
        assert!(!view(&prompts).offers_checkpoint);
        // A failure without a code keeps the buttons.
        prompts.answered(&key, generation, &Err("Timed out".into()));
        assert_eq!(view(&prompts).kind, RecoveryKind::Recover);
        assert_eq!(view(&prompts).message, "Timed out");
        prompts.answered(&key, generation, &Ok(SandboxResponse::Started));
        assert!(prompts.for_thread(&thread('c')).is_empty());
    }

    #[test]
    fn a_running_action_shows_only_its_progress() {
        let mut prompts = Prompts::default();
        prompts.started(&sent('c', Err(coded("session_ended"))), "Ada");
        let key = key_of(&session('c')).unwrap().0;
        let idle = prompts.entries[&key].clone();
        let (message, actions) = shown(&idle, true);
        assert_eq!(message, idle.view.message);
        assert!(actions.contains(&Action::Recover(RecoveryMode::Fresh)));
        assert!(actions.contains(&Action::Dismiss));
        for action in [
            Action::Recover(RecoveryMode::Fresh),
            Action::Recover(RecoveryMode::Checkpoint),
            Action::End,
        ] {
            let mut running = idle.clone();
            running.busy = Some(action);
            let (message, actions) = shown(&running, true);
            assert_eq!(actions, vec![action], "{action:?}");
            assert_eq!(message, action.progress());
            assert!(running.request(action).is_none(), "no second send");
        }
        let generation = prompts.entries[&key].generation;
        prompts.entries.get_mut(&key).unwrap().busy = Some(Action::End);
        prompts.answered(&key, generation, &Err("Timed out".into()));
        let (message, actions) = shown(&prompts.entries[&key], true);
        assert_eq!(message, "Timed out");
        assert!(actions.len() > 1, "the choices return after an answer");
    }

    #[test]
    fn an_older_answer_leaves_a_newer_failure_alone() {
        let mut prompts = Prompts::default();
        prompts.started(&sent('c', Err(coded("session_ended"))), "Ada");
        let key = key_of(&session('c')).unwrap().0;
        let older = prompts.entries[&key].generation;
        prompts.entries.get_mut(&key).unwrap().busy = Some(Action::Recover(RecoveryMode::Fresh));
        // While Recover runs, a later mention fails to start too.
        prompts.started(&sent('c', Err(coded("config_differs"))), "Ada");
        let newer = prompts.entries[&key].generation;
        assert_ne!(older, newer);
        assert!(!prompts.current(&key, older));
        assert!(prompts.current(&key, newer));

        // The first Recover's answers arrive late: nothing changes.
        let before = prompts.entries[&key].view.clone();
        prompts.answered(&key, older, &Ok(SandboxResponse::Started));
        assert_eq!(prompts.for_thread(&thread('c')).len(), 1);
        assert_eq!(prompts.entries[&key].view, before);
        prompts.answered(&key, older, &Err("Timed out".into()));
        assert_eq!(prompts.entries[&key].view, before);

        // The newer prompt's own answer still applies.
        prompts.answered(&key, newer, &Ok(SandboxResponse::Started));
        assert!(prompts.for_thread(&thread('c')).is_empty());
    }

    #[test]
    fn an_uncoded_failure_fences_off_the_action_in_flight() {
        let mut prompts = Prompts::default();
        prompts.started(&sent('c', Err(coded("session_ended"))), "Ada");
        let key = key_of(&session('c')).unwrap().0;
        let older = prompts.entries[&key].generation;
        prompts.entries.get_mut(&key).unwrap().busy = Some(Action::Recover(RecoveryMode::Fresh));
        // While Recover runs, a later mention fails without a lifecycle code.
        let transient = SessionError::from("The cluster is unreachable");
        assert!(!prompts.started(&sent('c', Err(transient)), "Ada"));
        assert!(!prompts.current(&key, older));
        assert_eq!(prompts.entries[&key].busy, None, "the buttons return");

        // The first Recover's Started answer arrives late: the prompt stays.
        prompts.answered(&key, older, &Ok(SandboxResponse::Started));
        assert_eq!(prompts.for_thread(&thread('c')).len(), 1);

        // Without a prompt, an uncoded failure records nothing.
        let mut empty = Prompts::default();
        let transient = SessionError::from("The cluster is unreachable");
        assert!(!empty.started(&sent('c', Err(transient)), "Ada"));
        assert!(empty.entries.is_empty());
    }

    #[test]
    fn resolve_uses_the_scope_local_work_records() {
        let expected =
            crate::agent_activity::local::scope("https://relay.example", &"a".repeat(64));
        assert_eq!(work_scope(&session('c')), Some(expected));
        let mut unparsable = session('c');
        unparsable.tenant.relay = "not a relay".into();
        assert_eq!(work_scope(&unparsable), None);
    }
}
