//! End and Recover for a thread whose session would not start: the
//! desktop's `RemoteSessionRecoveryBanner`, as a thread banner. A start
//! that fails with a lifecycle code leaves a prompt; the banner offers what
//! that code asks for, and a start or recovery that works clears it.
//! Prompts are kept in memory only.

use super::{Sent, send};
use crate::agents2::Agents2;
use crate::ui::thread_banners::{self, Banner, BannerButton, BannerThread, ThreadBanner};
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
    view: View,
    busy: bool,
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
}

impl Global for Prompts {}

fn key_of(session: &SandboxSession) -> Option<(RecoveryPromptKey, SessionScope)> {
    let scope = SessionScope::new(
        session.scope.channel_id.parse().ok()?,
        session.scope.thread_root.clone(),
    )
    .ok()?;
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
    pub fn started(&mut self, (session, result): &Sent, name: &str) -> bool {
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
        let entry = Entry {
            session: session.clone(),
            view,
            busy: false,
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

    /// An action's answer: a recovery clears the prompt; an End or a
    /// failure picks the next banner.
    pub fn answered(
        &mut self,
        key: &RecoveryPromptKey,
        answer: &Result<SandboxResponse, SessionError>,
    ) {
        let Some(entry) = self.entries.get_mut(key) else {
            return;
        };
        entry.busy = false;
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
        let Ok(scope) = SessionScope::new(channel, thread.root.clone()) else {
            return Vec::new();
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
    if !cx.has_global::<Prompts>() {
        return;
    }
    let recorded = cx.update_global(|prompts: &mut Prompts, _| prompts.started(sent, name));
    let pubkey = sent.0.pubkey.clone();
    let prompts = cx.global::<Prompts>();
    if recorded && !prompts.lifecycle.contains_key(&pubkey) && !prompts.checking.contains(&pubkey) {
        check_lifecycle(&sent.0, cx);
    }
    thread_banners::refresh(cx);
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
            let buttons = banner_actions(entry.view.kind, entry.view.offers_checkpoint, lifecycle)
                .into_iter()
                .map(|action| {
                    let key = key.clone();
                    BannerButton {
                        label: action.label().into(),
                        loading: entry.busy && action != Action::Dismiss,
                        action: Rc::new(move |_, cx| act(&key, action, cx)),
                    }
                })
                .collect();
            Banner {
                text: format!("{}: {}", prompt.agent_name, entry.view.message).into(),
                buttons,
            }
        })
        .collect()
}

fn act(key: &RecoveryPromptKey, action: Action, cx: &mut App) {
    let Some(entry) = cx.global::<Prompts>().entries.get(key).cloned() else {
        return;
    };
    let (pubkey, session) = (entry.session.pubkey.clone(), entry.session);
    let request = match action {
        _ if entry.busy => return,
        Action::Dismiss => {
            cx.global_mut::<Prompts>().clear(key);
            thread_banners::refresh(cx);
            return;
        }
        Action::Recover(mode) => SandboxRequest::Recover { session, mode },
        Action::End | Action::Check => SandboxRequest::End { session },
    };
    let Some(link) = link_of(&pubkey, cx) else {
        return;
    };
    if let Some(entry) = cx.global_mut::<Prompts>().entries.get_mut(key) {
        entry.busy = true;
    }
    thread_banners::refresh(cx);
    let answer =
        crate::runtime::Runtime::handle(cx).spawn(async move { send(&link, request).await });
    let key = key.clone();
    cx.spawn(async move |cx| {
        let answer = answer
            .await
            .unwrap_or_else(|_| Err("The request was cancelled".into()));
        cx.update(|cx| {
            cx.global_mut::<Prompts>().answered(&key, &answer);
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
            scope: SandboxScope {
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

    #[test]
    fn a_coded_failure_records_a_prompt_and_a_success_clears_it() {
        let mut prompts = Prompts::default();
        assert!(prompts.started(&(session('c'), Err(coded("session_ended"))), "Ada"));
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
        assert!(!prompts.started(&(session('d'), Err(transient)), "Ada"));
        assert!(prompts.for_thread(&thread('d')).is_empty());

        assert!(!prompts.started(&(session('c'), Ok(())), "Ada"));
        assert!(prompts.for_thread(&thread('c')).is_empty());
    }

    #[test]
    fn an_end_reply_picks_the_next_banner() {
        let mut prompts = Prompts::default();
        prompts.started(&(session('c'), Err(coded("session_active"))), "Ada");
        let key = key_of(&session('c')).unwrap().0;
        let view = |prompts: &Prompts| prompts.entries[&key].view.clone();
        assert_eq!(view(&prompts).kind, RecoveryKind::EndFirst);
        let ending = Ok(SandboxResponse::Ended {
            state: EndState::Ending,
        });
        prompts.answered(&key, &ending);
        assert_eq!(view(&prompts).kind, RecoveryKind::Ending);
        let absent = Ok(SandboxResponse::Ended {
            state: EndState::Absent,
        });
        prompts.answered(&key, &absent);
        assert_eq!(view(&prompts).kind, RecoveryKind::Recover);
        assert!(!view(&prompts).offers_checkpoint);
        // A failure without a code keeps the buttons.
        prompts.answered(&key, &Err("Timed out".into()));
        assert_eq!(view(&prompts).kind, RecoveryKind::Recover);
        assert_eq!(view(&prompts).message, "Timed out");
        prompts.answered(&key, &Ok(SandboxResponse::Started));
        assert!(prompts.for_thread(&thread('c')).is_empty());
    }
}
