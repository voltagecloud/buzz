//! The session's channel kit and channel creation with setup
//! (`channel-templates/capability.ts` `createChannelKit`, `setup.ts`
//! `createChannelSetup`, `relay/session.ts` `channelCreation`).
//!
//! The kit is the typed view of the session's one recipe reader
//! ([`super::MeKit`]), which saves through the outbox with an expected
//! head, as buzz-app does: a save is refused when the record changed, when
//! an earlier save is unresolved, or within the same second as the head. Channel creation freezes its intent in a
//! device receipt before anything is sent, returns once the channel is
//! confirmed (admission), and finishes its setup on its own afterwards
//! (completion), leaving a notice when setup could not finish.

use super::util::{CLOSED, NOT_CONNECTED, with};
use super::{Actor, Session};
use crate::event::Verified;
use crate::kit::setup::{
    ChannelCreationInput, PendingCreation, Progress, Receipts, SETUP_NOT_CONTINUED, SetupNotice,
    TEMPLATE_NOT_CONTINUED, pending_creation,
};
use crate::kit::{self, KitEntry, KitRecord, KitValue};
use crate::lifecycle::{self, Action, CONFIRM_DELAYS};
use crate::outbox::{Delivery, Outbox, SendOptions};
use crate::reader::{ReadOptions, Reader};
use crate::socket::Filter;
use crate::store::{Partition, Store};
use nostr::prelude::EventId;
use std::sync::{Arc, Mutex};
use tokio::sync::{oneshot, watch};

const UNCONFIRMED: &str = "This may have taken effect, but it is not confirmed yet. Refresh and check before trying again.";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum KitStatus {
    #[default]
    Idle,
    Loading,
    Ready,
    Error,
    Unavailable,
}

/// The recipe catalog as last read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KitSnapshot {
    pub status: KitStatus,
    pub entries: Arc<[KitEntry]>,
    pub error: Option<String>,
}

/// Channel creation the dialog and sidebar show: a creation awaiting
/// "Retry channel", and setups that could not finish.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CreationSnapshot {
    pub pending: Option<ChannelCreationInput>,
    pub notices: Vec<SetupNotice>,
}

#[derive(Default)]
pub(super) struct KitShared {
    creation: watch::Sender<Arc<CreationSnapshot>>,
    /// The creation in this session whose Create is unconfirmed.
    running: Mutex<Option<Progress>>,
    /// The admission of the creation being admitted now, which a Create
    /// made meanwhile shares (`admitting`).
    admission: Mutex<Option<Shared>>,
}

/// One run's admission, once it settles.
type Admission = Option<Result<String, String>>;
type Shared = watch::Receiver<Admission>;

impl KitShared {
    fn set_creation(&self) {
        let pending = self
            .running
            .lock()
            .expect("kit lock")
            .as_ref()
            .map(|p| p.input.clone());
        self.creation.send_modify(|snapshot| {
            if snapshot.pending != pending {
                let mut next = (**snapshot).clone();
                next.pending = pending;
                *snapshot = Arc::new(next);
            }
        });
    }

    fn notice(&self, notice: SetupNotice) {
        self.creation.send_modify(|snapshot| {
            let mut next = (**snapshot).clone();
            next.notices.retain(|n| n.id != notice.id);
            next.notices.push(notice);
            *snapshot = Arc::new(next);
        });
    }
}

/// What kit operations need from the actor.
#[derive(Clone)]
struct Kit {
    reader: Reader,
    viewer: String,
    relay: String,
    outbox: Outbox,
    store: Store,
    partition: Partition,
    epoch: u64,
}

impl Actor {
    fn kit_context(&self) -> Result<Kit, String> {
        if self.cached {
            return Err(super::channels::RECONNECT_TO_UPDATE.into());
        }
        let relay = self.relay.as_ref().ok_or(NOT_CONNECTED)?.authority.clone();
        let viewer = self.identity.pubkey_hex();
        Ok(Kit {
            reader: self.reader.clone(),
            partition: Partition::new(&self.origin, &viewer),
            viewer,
            relay,
            outbox: self.outbox.clone().ok_or(NOT_CONNECTED)?,
            store: self.store.clone(),
            epoch: self.epoch,
        })
    }
}

impl Kit {
    async fn read(&self, filters: Vec<Filter>) -> Result<Vec<Verified>, String> {
        self.reader
            .read(
                filters,
                ReadOptions {
                    fresh: true,
                    ..ReadOptions::default()
                },
            )
            .await
            .map(|events| events.to_vec())
            .map_err(|error| error.message)
    }
}

/// Waits until the outbox delivered `id`: accepted or seen.
async fn delivered(outbox: &Outbox, id: EventId) -> Result<(), String> {
    let mut changes = outbox.changes();
    loop {
        match outbox.get(&id) {
            Some(op) => match op.delivery {
                Delivery::Accepted | Delivery::Seen => return Ok(()),
                Delivery::Failed => {
                    return Err(op
                        .error
                        .clone()
                        .unwrap_or_else(|| "The relay refused this change.".into()));
                }
                Delivery::Unknown => return Err(UNCONFIRMED.into()),
                Delivery::Sending => {}
            },
            None => return Err("The change was dismissed.".into()),
        }
        changes.changed().await.map_err(|_| CLOSED.to_owned())?;
    }
}

impl Session {
    async fn kit_context(&self) -> Result<Kit, String> {
        with(&self.commands, |actor| actor.kit_context()).await?
    }

    /// The recipe catalog as last read (`channelKit.snapshot`).
    pub fn kit(&self) -> watch::Receiver<Arc<KitSnapshot>> {
        self.me_kit.typed()
    }

    /// Reads the catalog once (`ensure`).
    pub fn kit_ensure(&self) {
        if self.me_kit.typed().borrow().status == KitStatus::Idle {
            drop(self.me_kit.refresh());
        }
    }

    /// Reads the viewer's recipes in this community again (`refresh`). A
    /// call during a refresh waits for it.
    pub async fn kit_refresh(&self) -> Result<(), String> {
        self.me_kit.refresh().await;
        let snapshot = self.me_kit.typed().borrow().clone();
        match snapshot.status {
            KitStatus::Ready => Ok(()),
            _ => Err(snapshot
                .error
                .clone()
                .unwrap_or_else(|| NOT_CONNECTED.into())),
        }
    }

    /// Saves a recipe over `expected`, the head it was edited from (`None`
    /// for a new one), and returns the confirmed event (`save`). Deleting
    /// saves the record with `deleted: true`.
    pub async fn kit_save(
        &self,
        value: KitValue,
        expected: Option<EventId>,
        deleted: bool,
    ) -> Result<EventId, String> {
        save(&self.me_kit, &self.origin.http(), value, expected, deleted).await
    }

    /// The selected Canvas of a channel, if any (`canvas.read`, not
    /// strongly consistent): "Save as template" copies it.
    pub async fn kit_canvas(&self, channel: &str) -> Result<Option<String>, String> {
        let (commands, channel) = (self.commands.clone(), channel.to_owned());
        let canvas = self.canvas();
        self.runtime
            .spawn(async move {
                with(&commands, |actor| actor.kit_context()).await??;
                Ok(canvas
                    .head(&channel, false)
                    .await
                    .map_err(|e| e.to_string())?
                    .map(|event| event.content.clone()))
            })
            .await
            .unwrap_or_else(|_| Err(CLOSED.into()))
    }

    /// Channel creation state: the creation awaiting "Retry channel" in
    /// this session, and setup notices (`channelCreation`).
    pub fn creation(&self) -> watch::Receiver<Arc<CreationSnapshot>> {
        self.kit.creation.subscribe()
    }

    /// Hides a setup notice; the channel and its receipts are unchanged.
    pub fn dismiss_setup_notice(&self, id: &str) {
        self.kit.creation.send_modify(|snapshot| {
            if snapshot.notices.iter().any(|n| n.id == id) {
                let mut next = (**snapshot).clone();
                next.notices.retain(|n| n.id != id);
                *snapshot = Arc::new(next);
            }
        });
    }

    /// The creation a "Retry channel" would check: this session's
    /// unconfirmed one, else one the outbox kept from before a restart
    /// (`channelCreation.snapshot`).
    pub async fn pending_creation(&self) -> Option<ChannelCreationInput> {
        if let Some(running) = self.kit.running.lock().expect("kit lock").as_ref() {
            return Some(running.input.clone());
        }
        let running = None;
        with(&self.commands, move |actor| restored(actor, running))
            .await
            .ok()
            .flatten()
            .map(|pending| pending.input)
    }

    /// Creates a channel with its setup (`channelCreation.create`) and
    /// returns its id once the viewer is confirmed in it. Group, Canvas and
    /// agents then continue on their own; a failure there becomes a
    /// notice. With an unconfirmed creation pending, this retries only that
    /// creation, whatever `input` says.
    pub async fn create_channel_with(
        self: &Arc<Self>,
        input: ChannelCreationInput,
    ) -> Result<String, String> {
        let mut input = input;
        input.name = lifecycle::canonical_name(&input.name);
        let session = self.clone();
        let (sender, admission) = oneshot::channel();
        self.runtime.spawn(async move {
            let shared = session.kit.clone();
            let name = input.name.clone();
            let mut admitted = Admitted {
                sender: Some(sender),
                opened: None,
                sharing: None,
            };
            if let Err(error) = run(&session, input, &mut admitted).await {
                if admitted.sender.is_some() {
                    admitted.settle(Err(error));
                } else if let Some(id) = admitted.opened.take() {
                    // Admitted: the form is gone, so setup reports here,
                    // and the receipt goes so it cannot fill the budget.
                    if let Ok(ctx) = session.kit_context().await {
                        let _ = Receipts::remove(&ctx.store, &ctx.partition, &id);
                    }
                    shared.notice(SetupNotice { id, name, error });
                }
            }
        });
        admission.await.unwrap_or_else(|_| Err(CLOSED.into()))
    }
}

/// The two outcomes of one run: the form's admission, then completion.
struct Admitted {
    sender: Option<oneshot::Sender<Result<String, String>>>,
    /// The channel, once admitted.
    opened: Option<String>,
    /// Hands this run's admission to Creates made while it runs.
    sharing: Option<(Arc<KitShared>, watch::Sender<Admission>)>,
}

impl Admitted {
    /// Answers the form, and every Create that shared it; another
    /// creation may start from here (`admitting = undefined`).
    fn settle(&mut self, result: Result<String, String>) {
        if let Some((shared, sharing)) = self.sharing.take() {
            *shared.admission.lock().expect("kit lock") = None;
            sharing.send_replace(Some(result.clone()));
        }
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(result);
        }
    }
}

impl Drop for Admitted {
    fn drop(&mut self) {
        // A run that ended without settling never blocks the next one.
        if self.sharing.is_some() {
            self.settle(Err(CLOSED.into()));
        }
    }
}

fn admit(admitted: &mut Admitted, id: &str) {
    admitted.opened = Some(id.to_owned());
    admitted.settle(Ok(id.to_owned()));
}

/// `restoredChannelCreation`: the newest 9007 the outbox still holds for a
/// channel this device did not admit and the viewer is not in.
fn restored(actor: &Actor, running: Option<String>) -> Option<PendingCreation> {
    let outbox = actor.outbox.as_ref()?;
    let viewer = actor.identity.pubkey_hex();
    let receipts =
        Receipts::load(&actor.store, &Partition::new(&actor.origin, &viewer)).unwrap_or_default();
    outbox
        .visible()
        .iter()
        .rev()
        .filter_map(|op| pending_creation(op).map(|pending| (op.delivery, pending)))
        .find(|(delivery, pending)| {
            let unsettled = matches!(
                delivery,
                Delivery::Sending | Delivery::Unknown | Delivery::Failed
            ) || receipts.0.get(&pending.id).is_some_and(|p| !p.created);
            unsettled
                && running.as_deref() != Some(pending.id.as_str())
                && !receipts.admitted(&pending.id)
                && !actor
                    .discovery
                    .as_ref()
                    .is_some_and(|discovery| discovery.can_access(&pending.id))
        })
        .map(|(_, pending)| pending)
}

/// Saves a typed recipe through the session's one recipe writer.
async fn save(
    kit: &super::MeKit,
    community: &str,
    value: KitValue,
    expected: Option<EventId>,
    deleted: bool,
) -> Result<EventId, String> {
    let record = kit::parse_kit_record(
        &KitRecord::new(value, community, deleted).to_json(),
        community,
    )?;
    let id = kit
        .save(
            record.to_json()["value"].take(),
            expected.map(|id| id.to_hex()),
            deleted,
        )
        .await?;
    EventId::from_hex(&id).map_err(|error| error.to_string())
}

/// Seeds a new channel's starting Canvas through the outbox with
/// `expected-revision: none`, and returns the operation.
fn seed_canvas(ctx: &Kit, channel: &str, content: &str) -> Result<EventId, String> {
    let draft = crate::canvas::save_draft(channel, content, None).map_err(|e| e.to_string())?;
    ctx.outbox
        .send(draft, SendOptions::default())
        .map_err(|e| e.to_string())
}

/// Reads the roster until it lists the viewer in `channel`, then feeds
/// the events to discovery.
async fn confirm_created(
    session: &Session,
    ctx: &Kit,
    channel: &str,
    member: &str,
) -> Result<(), String> {
    let filters = lifecycle::created_filters(&ctx.relay, member, channel);
    let mut found = None;
    for delay in CONFIRM_DELAYS {
        tokio::time::sleep(delay).await;
        let events = ctx.read(filters.clone()).await?;
        if lifecycle::confirmed(Action::Join, &events, &ctx.relay, member, channel) {
            found = Some(events);
            break;
        }
    }
    let events = found.ok_or(UNCONFIRMED)?;
    let epoch = ctx.epoch;
    with(&session.commands, move |actor| {
        if actor.epoch == epoch {
            actor.accept(&events);
        }
    })
    .await
}

/// Waits for a 9007 and the viewer's membership; an uncertain delivery
/// still counts when the relay holds the exact command.
async fn admitted_creation(
    session: &Session,
    ctx: &Kit,
    op: EventId,
    channel: &str,
) -> Result<(), String> {
    if let Err(error) = delivered(&ctx.outbox, op).await {
        let receipt = ctx
            .read(vec![lifecycle::creation_receipt_filter(&op, &ctx.viewer)])
            .await?;
        if !receipt.iter().any(|event| event.id == op) {
            return Err(error);
        }
    }
    confirm_created(session, ctx, channel, &ctx.viewer).await
}

/// One creation from start to finish (`run`).
async fn run(
    session: &Arc<Session>,
    input: ChannelCreationInput,
    admitted: &mut Admitted,
) -> Result<(), String> {
    let shared = session.kit.clone();
    let ctx = session.kit_context().await?;
    ctx.outbox.ready().await.map_err(|e| e.to_string())?;
    let running = shared.running.lock().expect("kit lock").is_some();
    let running_id = None;
    let restored = if running {
        None
    } else {
        with(&session.commands, move |actor| restored(actor, running_id)).await?
    };
    if let Some(pending) = restored {
        // Only the same channel retries it; anything else waits until it
        // is retried (`channelCreation.create`).
        if input.setup.is_some() {
            return Err("Template setup is unavailable on this host".into());
        }
        let mut labels: Vec<String> = Vec::new();
        for label in &input.labels {
            if !labels.contains(label) {
                labels.push(label.clone());
            }
        }
        let normalized = ChannelCreationInput {
            description: input
                .description
                .as_deref()
                .map(str::trim)
                .filter(|description| !description.is_empty())
                .map(str::to_owned),
            labels,
            setup: None,
            ..input
        };
        if normalized != pending.input {
            return Err(
                "Another channel is still awaiting confirmation. Retry it before changing the details."
                    .into(),
            );
        }
        return retry_restored(session, &ctx, pending, admitted).await;
    }
    // A Create while another is being admitted gets that one's admission,
    // whatever its input (`if (admitting) return admitting`).
    let joined = {
        let mut slot = shared.admission.lock().expect("kit lock");
        match slot.as_ref() {
            Some(admission) => Some(admission.clone()),
            None => {
                let (sharing, admission) = watch::channel(None);
                *slot = Some(admission);
                admitted.sharing = Some((shared.clone(), sharing));
                None
            }
        }
    };
    if let Some(mut admission) = joined {
        let id = admission
            .wait_for(Option::is_some)
            .await
            .map_err(|_| CLOSED.to_owned())?
            .clone()
            .unwrap_or_else(|| Err(CLOSED.into()))?;
        admitted.settle(Ok(id));
        return Ok(());
    }
    let running = shared.running.lock().expect("kit lock").clone();
    if let Some(checking) = running {
        return retry_running(session, &ctx, checking, admitted).await;
    }
    // Admitted or failed, the admission settles: another creation may
    // start while setup runs.
    let admission = start(session, &ctx, input, admitted).await;
    complete(session, &ctx, admission?).await
}

/// `preflight` and the first steps up to admission.
async fn start(
    session: &Arc<Session>,
    ctx: &Kit,
    input: ChannelCreationInput,
    admitted: &mut Admitted,
) -> Result<Progress, String> {
    input.check()?;
    // A relay without NIP-CL drops label tags: refuse before any write.
    if !input.labels.is_empty() && !session.channel_labels() {
        return Err(crate::channel_labels::UNSUPPORTED.into());
    }
    if let Some(setup) = &input.setup {
        if !setup.canvas.is_empty() && crate::canvas::too_large(&setup.canvas) {
            return Err("Canvas writing is unavailable".into());
        }
        if setup.agents.iter().any(|key| !crate::members::is_key(key)) {
            return Err("Agent membership is unavailable".into());
        }
        let grouped = || {
            let snapshot = session.snapshot().borrow().clone();
            snapshot.sidebar.data.as_ref().is_some_and(|data| {
                data.sections
                    .iter()
                    .any(|section| section.id == setup.group_id)
            })
        };
        if !setup.group_id.is_empty() && !grouped() {
            return Err(
                "The destination group is unavailable; refresh your sidebar before creating the channel"
                    .into(),
            );
        }
    }
    let mut p = Progress::new(input);
    // Reserve the frozen intent before anything is sent.
    Receipts::write(&ctx.store, &ctx.partition, &p)?;
    *session.kit.running.lock().expect("kit lock") = Some(p.clone());
    session.kit.set_creation();
    let shared = session.kit.clone();
    let outcome = async {
        let draft = lifecycle::create(&p.input.channel(), &p.id).map_err(|e| e.to_string())?;
        let op = ctx
            .outbox
            .send(draft, SendOptions::default())
            .map_err(|e| e.to_string())?;
        p.operations.insert("create".into(), op.to_hex());
        Receipts::write(&ctx.store, &ctx.partition, &p)?;
        admitted_creation(session, ctx, op, &p.id).await?;
        Ok::<EventId, String>(op)
    }
    .await;
    match outcome {
        Ok(_) => {}
        Err(error) => {
            let create = p
                .operations
                .get("create")
                .and_then(|id| EventId::from_hex(id).ok());
            let failed = create
                .and_then(|id| ctx.outbox.get(&id))
                .is_some_and(|op| op.delivery == Delivery::Failed);
            if create.is_none() || failed {
                // Never left behind for "Retry channel", even if retiring
                // fails.
                *shared.running.lock().expect("kit lock") = None;
                shared.set_creation();
                retire(ctx, &p).await?;
            } else {
                // Uncertain: "Retry channel" checks this exact Create.
                *shared.running.lock().expect("kit lock") = Some(p.clone());
                shared.set_creation();
            }
            return Err(error);
        }
    }
    p.created = true;
    *shared.running.lock().expect("kit lock") = None;
    shared.set_creation();
    admit(admitted, &p.id);
    Receipts::write(&ctx.store, &ctx.partition, &p)?;
    Ok(p)
}

/// Group, Canvas and agents after admission; then the receipt and its
/// delivery evidence are retired.
async fn complete(session: &Arc<Session>, ctx: &Kit, mut p: Progress) -> Result<(), String> {
    let persist = |p: &Progress| Receipts::write(&ctx.store, &ctx.partition, p);
    let Some(setup) = p.input.setup.clone() else {
        return retire(ctx, &p).await;
    };
    let mut placement_error = None;
    if !setup.group_id.is_empty() {
        let placed = session
            .sidebar_move(
                &p.id,
                crate::sidebar_store::Destination::Section(Some(setup.group_id.clone())),
            )
            .await;
        if let Err(error) = placed {
            placement_error = Some(error);
        }
    }
    if placement_error.is_none() {
        p.group_done = true;
        persist(&p)?;
    }
    let canvas_api = session.canvas();
    let rest = async {
        if !setup.canvas.is_empty() && !p.canvas_done {
            let seeded = p.operations.get("canvas").cloned();
            let head = canvas_api.head(&p.id, true).await.map_err(|e| e.to_string())?.map(|e| e.id.to_hex());
            if head.is_some() && head != seeded {
                return Err("Canvas has a different saved document; no agents were added by this attempt.".to_owned());
            }
            let canvas = match seeded.and_then(|id| EventId::from_hex(&id).ok()) {
                Some(id) => id,
                None => {
                    let id = seed_canvas(ctx, &p.id, &setup.canvas)?;
                    p.operations.insert("canvas".into(), id.to_hex());
                    persist(&p)?;
                    id
                }
            };
            // A refused seed is dismissed, so it cannot fence a later
            // manual Canvas save.
            match canvas_api.confirm(&ctx.outbox, canvas).await {
                Ok(()) => {}
                Err(error) if error.is_conflict() => {
                    return Err("Canvas changed before the seed was saved; no agents were added by this attempt.".into());
                }
                Err(error) => return Err(error.to_string()),
            }
            if canvas_api.head(&p.id, true).await.map_err(|e| e.to_string())?.map(|e| e.id) != Some(canvas) {
                return Err("The seed Canvas is not the selected document; no agents were added by this attempt.".into());
            }
            p.canvas_done = true;
            persist(&p)?;
        }
        for agent in &setup.agents {
            if *agent == ctx.viewer || p.added.contains(agent) {
                continue;
            }
            session.add_member_plain(&p.id, agent).await?;
            if !p.accepted.contains(agent) {
                p.accepted.push(agent.clone());
            }
            p.added.push(agent.clone());
            persist(&p)?;
        }
        Ok(())
    }
    .await;
    match (placement_error, rest) {
        (Some(placement), Err(error)) => Err(format!("{placement}; {error}")),
        (Some(placement), Ok(())) => Err(placement),
        (None, Err(error)) => Err(error),
        (None, Ok(())) => retire(ctx, &p).await,
    }
}

/// Removes the receipt and the delivery evidence of its operations.
async fn retire(ctx: &Kit, p: &Progress) -> Result<(), String> {
    for id in p.operations.values() {
        let Ok(id) = EventId::from_hex(id) else {
            continue;
        };
        ctx.outbox.dismiss(&id).await.map_err(|e| e.to_string())?;
        if ctx.outbox.get(&id).is_some() {
            return Err("Channel setup delivery evidence could not be retired".into());
        }
    }
    Receipts::remove(&ctx.store, &ctx.partition, &p.id)
}

/// "Retry channel" in this session: only the saved Create is checked or
/// resent, never a new channel or the remaining setup.
async fn retry_running(
    session: &Arc<Session>,
    ctx: &Kit,
    mut checking: Progress,
    admitted: &mut Admitted,
) -> Result<(), String> {
    let shared = session.kit.clone();
    let op = checking
        .operations
        .get("create")
        .and_then(|id| EventId::from_hex(id).ok())
        .ok_or("Channel creation was not saved. Reconnect before checking it.")?;
    if let Some(saved) = ctx.outbox.get(&op)
        && matches!(saved.delivery, Delivery::Unknown)
    {
        ctx.outbox.retry(&op, None).map_err(|e| e.to_string())?;
    }
    if let Err(error) = admitted_creation(session, ctx, op, &checking.id).await {
        if ctx
            .outbox
            .get(&op)
            .is_some_and(|saved| saved.delivery == Delivery::Failed)
        {
            *shared.running.lock().expect("kit lock") = None;
            shared.set_creation();
            retire(ctx, &checking).await?;
        }
        return Err(error);
    }
    checking.created = true;
    *shared.running.lock().expect("kit lock") = None;
    shared.set_creation();
    admit(admitted, &checking.id);
    Receipts::write(&ctx.store, &ctx.partition, &checking)?;
    if checking.input.setup.is_some() {
        return Err(TEMPLATE_NOT_CONTINUED.into());
    }
    retire(ctx, &checking).await
}

/// "Retry channel" after a restart: the outbox's own 9007, confirmed and
/// then dismissed. A saved setup is reported, never continued.
async fn retry_restored(
    session: &Arc<Session>,
    ctx: &Kit,
    pending: PendingCreation,
    admitted: &mut Admitted,
) -> Result<(), String> {
    if let Some(saved) = ctx.outbox.get(&pending.operation)
        && saved.delivery == Delivery::Unknown
    {
        ctx.outbox
            .retry(&pending.operation, None)
            .map_err(|e| e.to_string())?;
    }
    if let Err(error) = admitted_creation(session, ctx, pending.operation, &pending.id).await {
        if ctx
            .outbox
            .get(&pending.operation)
            .is_some_and(|saved| saved.delivery == Delivery::Failed)
        {
            let _ = ctx.outbox.dismiss(&pending.operation).await;
        }
        return Err(error);
    }
    let _ = ctx.outbox.dismiss(&pending.operation).await;
    admit(admitted, &pending.id);
    let receipts = Receipts::load(&ctx.store, &ctx.partition).unwrap_or_default();
    if receipts.0.contains_key(&pending.id) {
        return Err(SETUP_NOT_CONTINUED.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn snapshots_start_idle_and_empty() {
        let shared = KitShared::default();
        assert!(shared.creation.borrow().pending.is_none());
        shared.notice(SetupNotice {
            id: "a".into(),
            name: "x".into(),
            error: "e".into(),
        });
        shared.notice(SetupNotice {
            id: "a".into(),
            name: "x".into(),
            error: "f".into(),
        });
        assert_eq!(shared.creation.borrow().notices.len(), 1, "one per channel");
    }
}
