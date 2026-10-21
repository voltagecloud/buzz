//! Draft attachments and background sends (`attachment-draft.ts`,
//! `background-upload.ts`).
//!
//! Files start uploading, one at a time, as soon as they are added. Send
//! hands them, with the upload still running, to a background job of the
//! community's queue and clears the composer at once; the job waits for
//! them in order, then the composer's host publishes the message. A job that fails or is cancelled hands its files and the
//! sent text back to the draft. Drafts and jobs outlive their composer, so
//! files survive navigation (not a restart), as in buzz-app's tab.

use crate::community::Community;
use buzz_relay_client::messages::RunPick;
use buzz_relay_client::upload::{self, MAX_UPLOAD, Source, Stage, Upload, UploadError};
use buzz_relay_client::{Attachment, Media};
use futures::StreamExt;
use futures::channel::{mpsc, oneshot};
use futures::future::Shared;
use gpui_kit::*;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;
use std::sync::Arc;

/// At most this many files per message.
pub const MAX_FILES: usize = 10;
/// Bytes all drafts and sends of a community may hold (`MAX_RETAINED_BYTES`).
const MAX_RETAINED: u64 = 2 * MAX_UPLOAD;

/// A recorded voice note: it plays from memory under `key` until it is sent.
#[derive(Clone, Debug)]
pub struct Voice {
    pub key: String,
    pub duration: f64,
    pub waveform: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Queued,
    Preparing,
    Uploading,
    Ready,
    Error,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Queued => "Queued",
            Status::Preparing => "Preparing…",
            Status::Uploading => "Uploading…",
            Status::Ready => "Ready",
            Status::Error => "Upload failed",
        }
    }

    /// Preparing or transferring; queued files wait for Send.
    pub fn busy(self) -> bool {
        matches!(self, Status::Preparing | Status::Uploading)
    }
}

/// One file of a draft (`DraftAttachment`).
#[derive(Clone, Debug)]
pub struct DraftFile {
    pub id: String,
    pub name: String,
    /// The type the name or bytes suggest; the relay's wins once uploaded.
    pub mime: String,
    pub size: u64,
    pub source: Source,
    pub voice: Option<Voice>,
    pub status: Status,
    pub uploaded: Option<Attachment>,
    pub error: Option<SharedString>,
    /// Bytes handed to the connection, of the prepared total.
    pub transfer: Option<(u64, u64)>,
}

/// A file being added.
pub struct NewFile {
    pub name: String,
    pub mime: String,
    pub size: u64,
    pub source: Source,
    pub voice: Option<Voice>,
}

/// A failed send's text, handed back to whichever composer shows its
/// draft.
#[derive(Clone)]
pub struct Recovered {
    pub draft: String,
    /// The reason, for the composer's error line.
    pub error: Option<SharedString>,
    /// The model and effort picks of the send, which return with its text
    /// alone.
    pub picks: BTreeMap<String, RunPick>,
}

/// What a draft file previews as (`draftAttachmentMedia`): the relay's
/// type once uploaded, else the file's type or name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preview {
    Image,
    Video,
    Audio,
    File,
}

impl DraftFile {
    /// The type the relay or the file gives.
    pub fn kind(&self) -> &str {
        self.uploaded
            .as_ref()
            .map_or(self.mime.as_str(), |a| a.mime.as_str())
    }

    pub fn preview(&self) -> Preview {
        let kind = self.kind();
        let lower = self.name.to_ascii_lowercase();
        let named = |extensions: &[&str]| {
            self.uploaded.is_none() && extensions.iter().any(|ext| lower.ends_with(ext))
        };
        if matches!(
            kind,
            "image/png" | "image/jpeg" | "image/gif" | "image/webp"
        ) || named(&[".png", ".jpg", ".jpeg", ".gif", ".webp"])
        {
            Preview::Image
        } else if kind.starts_with("video/") || named(&[".mp4", ".mov", ".webm", ".m4v"]) {
            Preview::Video
        } else if kind.starts_with("audio/") || self.voice.is_some() {
            Preview::Audio
        } else {
            Preview::File
        }
    }
}

/// The files of one draft (`AttachmentDraft`).
///
/// Files upload eagerly, one at a time, from the moment they are added,
/// and stay previewable meanwhile. Send moves the draft's files with
/// their running upload to a background send without restarting it.
pub struct AttachmentDraft {
    media: Media,
    pub items: Vec<DraftFile>,
    /// Uploads running, by file ID.
    active: HashMap<String, Running>,
    /// No eager upload starts: nobody shows the draft, or its composer is
    /// disabled.
    paused: bool,
    /// Composers showing this draft.
    viewers: usize,
    /// Text a failed send handed back, until a composer takes it.
    pub recovered: Option<Recovered>,
}

/// The draft a running upload reports to. Send points it at the
/// background draft that took the upload over.
type Owner = Rc<RefCell<WeakEntity<AttachmentDraft>>>;

/// One running upload.
struct Running {
    upload: String,
    owner: Owner,
    /// Its outcome, for a send that waits on it.
    result: Shared<oneshot::Receiver<Result<Attachment, UploadError>>>,
}

/// What [`AttachmentDraft::pump`] starts next: the first queued file,
/// unless the draft is paused or an upload already runs.
fn next_queued(items: &[DraftFile], paused: bool, running: bool) -> Option<String> {
    if paused || running {
        return None;
    }
    items
        .iter()
        .find(|item| item.status == Status::Queued)
        .map(|item| item.id.clone())
}

/// Stops unfinished files (`cancelActive`): they wait queued for the
/// draft to resume, or, when a retry is required, fail with "Upload
/// paused".
fn freeze(items: &mut [DraftFile], retry_required: bool) {
    for item in items {
        if item.status == Status::Queued || item.status.busy() {
            if retry_required {
                item.status = Status::Error;
                item.error = Some("Upload paused. Retry to continue.".into());
            } else {
                item.status = Status::Queued;
                item.error = None;
            }
            item.transfer = None;
        }
    }
}

impl AttachmentDraft {
    fn new(media: Media) -> Self {
        Self {
            media,
            items: Vec::new(),
            active: HashMap::new(),
            paused: false,
            viewers: 0,
            recovered: None,
        }
    }

    pub fn retained(&self) -> u64 {
        self.items.iter().map(|item| item.size).sum()
    }

    /// The community's media, for files already on the relay.
    pub fn media(&self) -> &Media {
        &self.media
    }

    /// Only a failed file blocks Send; unfinished uploads go on in the
    /// background send.
    pub fn blocked(&self) -> bool {
        self.items.iter().any(|item| item.status == Status::Error)
    }

    fn replace(&mut self, id: &str, cx: &mut Context<Self>, change: impl FnOnce(&mut DraftFile)) {
        if let Some(item) = self.items.iter_mut().find(|item| item.id == id) {
            change(item);
            cx.notify();
        }
    }

    /// Adds files after the checks of `add`; `retained` counts every other
    /// draft and send of the community. They start uploading at once.
    fn add(
        &mut self,
        files: Vec<NewFile>,
        retained: u64,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if self.items.len() + files.len() > MAX_FILES {
            return Err(format!("Attach at most {MAX_FILES} files per message."));
        }
        if files
            .iter()
            .any(|file| file.size == 0 || file.size > MAX_UPLOAD)
        {
            return Err(UploadError::Size.to_string());
        }
        let adding: u64 = files.iter().map(|file| file.size).sum();
        if retained + self.retained() + adding > MAX_RETAINED {
            return Err(
                "Attachment drafts are full (1,000 MiB). Send or remove some files first.".into(),
            );
        }
        self.items.extend(files.into_iter().map(|file| DraftFile {
            id: uuid::Uuid::new_v4().to_string(),
            name: file.name,
            mime: file.mime,
            size: file.size,
            source: file.source,
            voice: file.voice,
            status: Status::Queued,
            uploaded: None,
            error: None,
            transfer: None,
        }));
        cx.notify();
        self.paused = false;
        self.pump(cx);
        Ok(())
    }

    /// Moves files, with their results and errors, into an empty draft.
    fn adopt(&mut self, files: Vec<DraftFile>, cx: &mut Context<Self>) -> bool {
        if !self.items.is_empty() || files.is_empty() {
            return false;
        }
        self.items = files;
        cx.notify();
        true
    }

    /// Removes a file, stopping its upload.
    pub fn remove(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(running) = self.active.remove(id) {
            upload::cancel(&running.upload);
        }
        self.items.retain(|item| item.id != id);
        cx.notify();
        self.pump(cx);
    }

    /// Queues a failed file again, which uploads from scratch. Only an
    /// explicit retry clears a real failure.
    pub fn retry(&mut self, id: &str, cx: &mut Context<Self>) {
        if !self
            .items
            .iter()
            .any(|item| item.id == id && item.status == Status::Error)
        {
            return;
        }
        self.replace(id, cx, |item| {
            item.status = Status::Queued;
            item.error = None;
            item.transfer = None;
        });
        self.paused = false;
        self.pump(cx);
    }

    /// Pauses eager uploads (no composer shows the draft, or its composer
    /// is disabled): unfinished files wait queued and resume later without
    /// a retry.
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        self.cancel_active(false, cx);
    }

    /// Resumes eager uploads.
    pub fn resume(&mut self, cx: &mut Context<Self>) {
        self.paused = false;
        self.pump(cx);
    }

    /// Stops every running upload. With `retry_required`, unfinished files
    /// fail with "Upload paused" until a retry; otherwise they wait queued.
    fn cancel_active(&mut self, retry_required: bool, cx: &mut Context<Self>) {
        self.paused = true;
        for (_, running) in self.active.drain() {
            upload::cancel(&running.upload);
        }
        freeze(&mut self.items, retry_required);
        cx.notify();
    }

    /// Drops every file, stopping their uploads.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.cancel_active(true, cx);
        self.items.clear();
        cx.notify();
    }

    /// Starts the next queued file, one at a time.
    fn pump(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = next_queued(&self.items, self.paused, !self.active.is_empty()) {
            self.start_one(&id, cx);
        }
    }

    /// Hands the files, with the upload still running, to `target`, an
    /// empty draft (`transferAttachmentDraft`). This draft is left empty
    /// and paused.
    fn transfer(&mut self, target: &Entity<Self>, cx: &mut Context<Self>) {
        self.paused = true;
        let items = std::mem::take(&mut self.items);
        let active = std::mem::take(&mut self.active);
        cx.notify();
        target.update(cx, |target, cx| {
            let weak = cx.weak_entity();
            for running in active.values() {
                *running.owner.borrow_mut() = weak.clone();
            }
            target.paused = false;
            target.items = items;
            target.active = active;
            cx.notify();
        });
    }

    /// Uploads the files in order (`prepareForSend`), waiting on uploads
    /// already running. The first failure stops the rest: unfinished files
    /// fail with "Upload paused" so the draft they return to retries them.
    fn prepare_for_send(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<Attachment>, UploadError>> {
        self.paused = false;
        let order: Vec<String> = self.items.iter().map(|item| item.id.clone()).collect();
        cx.spawn(async move |this, cx| {
            let mut uploaded = Vec::new();
            for id in order {
                let started = this
                    .update(cx, |this, cx| this.start_one(&id, cx))
                    .map_err(|_| UploadError::Cancelled)?;
                let result = match started {
                    Start::Done(attachment) => Ok(attachment),
                    Start::Missing => Err(UploadError::Cancelled),
                    Start::Failed => Err(UploadError::Failed),
                    Start::Running(result) => result.await.unwrap_or(Err(UploadError::Cancelled)),
                };
                match result {
                    Ok(attachment) => uploaded.push(attachment),
                    Err(error) => {
                        let _ = this.update(cx, |this, cx| this.cancel_active(true, cx));
                        return Err(error);
                    }
                }
            }
            Ok(uploaded)
        })
    }

    /// The file's upload: done, already running, or started now. A failed
    /// file waits for an explicit retry.
    fn start_one(&mut self, id: &str, cx: &mut Context<Self>) -> Start {
        let Some(item) = self.items.iter_mut().find(|item| item.id == id) else {
            return Start::Missing;
        };
        if let Some(attachment) = &item.uploaded {
            return Start::Done(attachment.clone());
        }
        if item.status == Status::Error {
            return Start::Failed;
        }
        if let Some(running) = self.active.get(id) {
            return Start::Running(running.result.clone());
        }
        let job = Upload::new(item.name.clone(), item.mime.clone(), item.source.clone());
        item.status = Status::Preparing;
        item.error = None;
        item.transfer = None;
        let voice = item.voice.as_ref().map(|voice| voice.duration);
        let owner: Owner = Rc::new(RefCell::new(cx.weak_entity()));
        let (done, result) = oneshot::channel();
        let result = futures::FutureExt::shared(result);
        self.active.insert(
            id.to_string(),
            Running {
                upload: job.id.clone(),
                owner: owner.clone(),
                result: result.clone(),
            },
        );
        cx.notify();
        let (report, mut stages) = mpsc::unbounded();
        let media = self.media.clone();
        let (file, upload_id) = (id.to_string(), job.id.clone());
        let reporter = owner.clone();
        cx.spawn(async move |_, cx| {
            while let Some(stage) = stages.next().await {
                let owner = reporter.borrow().clone();
                let _ = owner.update(cx, |this, cx| {
                    // A removed, paused or replaced upload reports nothing.
                    if this.active.get(&file).is_none_or(|r| r.upload != upload_id) {
                        return;
                    }
                    this.replace(&file, cx, |item| match stage {
                        Stage::Preparing => item.status = Status::Preparing,
                        Stage::Uploading => item.status = Status::Uploading,
                        Stage::Sent { sent, total } if total > 0 && sent <= total => {
                            item.status = Status::Uploading;
                            item.transfer = Some((sent, total));
                        }
                        Stage::Sent { .. } => {}
                    })
                });
            }
        })
        .detach();
        let (file, upload_id) = (id.to_string(), job.id.clone());
        cx.spawn(async move |_, cx| {
            let result = upload::send(&media, job, move |stage| {
                let _ = report.unbounded_send(stage);
            })
            .await;
            let owner = owner.borrow().clone();
            let settled = owner
                .update(cx, |this, cx| {
                    this.settle_upload(&file, &upload_id, result, voice, cx)
                })
                .unwrap_or(Err(UploadError::Cancelled));
            let _ = done.send(settled);
        })
        .detach();
        Start::Running(result)
    }

    /// Records how an upload ended, then starts the next queued file.
    fn settle_upload(
        &mut self,
        file: &str,
        upload_id: &str,
        result: Result<Attachment, UploadError>,
        voice: Option<f64>,
        cx: &mut Context<Self>,
    ) -> Result<Attachment, UploadError> {
        // A removed or paused file's result is no longer wanted.
        let current = self
            .active
            .get(file)
            .is_some_and(|running| running.upload == upload_id);
        if current {
            self.active.remove(file);
        }
        let settled = match result {
            Ok(mut attachment) if current => {
                attachment.voice_duration = voice;
                self.replace(file, cx, |item| {
                    item.status = Status::Ready;
                    item.uploaded = Some(attachment.clone());
                    item.transfer = None;
                });
                Ok(attachment)
            }
            Ok(_) => Err(UploadError::Cancelled),
            Err(error) => {
                if current {
                    self.replace(file, cx, |item| {
                        item.status = Status::Error;
                        item.error = Some(error.to_string().into());
                        item.transfer = None;
                    });
                }
                Err(error)
            }
        };
        self.pump(cx);
        settled
    }
}

enum Start {
    Done(Attachment),
    Missing,
    /// The file failed; only a retry uploads it again.
    Failed,
    Running(Shared<oneshot::Receiver<Result<Attachment, UploadError>>>),
}

/// The drafts and send queues of every community, keyed by its partition.
#[derive(Default)]
struct Registry {
    drafts: HashMap<(String, String), Entity<AttachmentDraft>>,
    queues: HashMap<String, Entity<Queue>>,
}

impl Global for Registry {}

fn partition(community: &Entity<Community>, cx: &App) -> String {
    let community = community.read(cx);
    format!("{}|{}", community.origin.http(), community.session.viewer())
}

/// The draft for `key` (`draft:<channel>` or its thread's), created when
/// first asked for. A composer showing it calls [`view`] and [`leave`].
pub fn draft(community: &Entity<Community>, key: &str, cx: &mut App) -> Entity<AttachmentDraft> {
    let scope = (partition(community, cx), key.to_string());
    if let Some(draft) = cx.default_global::<Registry>().drafts.get(&scope) {
        return draft.clone();
    }
    let media = community.read(cx).session.media().clone();
    let draft = cx.new(|_| AttachmentDraft::new(media));
    cx.default_global::<Registry>()
        .drafts
        .insert(scope, draft.clone());
    draft
}

/// A composer started showing `draft`; the first one resumes its uploads.
pub fn view(draft: &Entity<AttachmentDraft>, cx: &mut App) {
    draft.update(cx, |draft, cx| {
        draft.viewers += 1;
        if draft.viewers == 1 {
            draft.resume(cx);
        }
    });
}

/// A composer stopped showing `draft`. With nobody left, uploads pause
/// (their files wait queued for a return) and an empty draft is forgotten.
pub fn leave(
    community: &Entity<Community>,
    key: &str,
    draft: &Entity<AttachmentDraft>,
    cx: &mut App,
) {
    let empty = draft.update(cx, |draft, cx| {
        draft.viewers = draft.viewers.saturating_sub(1);
        if draft.viewers > 0 {
            return false;
        }
        draft.cancel(cx);
        draft.items.is_empty() && draft.recovered.is_none()
    });
    if empty {
        let scope = (partition(community, cx), key.to_string());
        let registry = cx.default_global::<Registry>();
        if registry.drafts.get(&scope) == Some(draft) {
            registry.drafts.remove(&scope);
        }
    }
}

/// Adds files to `draft`, within the community's retained-bytes budget.
pub fn add(
    community: &Entity<Community>,
    draft: &Entity<AttachmentDraft>,
    files: Vec<NewFile>,
    cx: &mut App,
) -> Result<(), String> {
    let prefix = format!("{}|", partition(community, cx));
    let retained: u64 = cx
        .default_global::<Registry>()
        .drafts
        .iter()
        .filter(|((scope, _), entity)| scope.starts_with(&prefix) && *entity != draft)
        .map(|(_, entity)| entity.clone())
        .collect::<Vec<_>>()
        .into_iter()
        .map(|entity| entity.read(cx).retained())
        .sum();
    draft.update(cx, |draft, cx| draft.add(files, retained, cx))
}

// ---------------------------------------------------------------------------
// Background sends

/// A notice of the queue: a failure, or a send whose files could not be
/// handed back.
#[derive(Clone)]
pub struct Notice {
    pub id: String,
    pub message: SharedString,
    /// A failed send kept in the queue, which "Retry failed send recovery"
    /// hands back.
    pub recovery: Option<String>,
}

/// What the pill shows (`BackgroundUploads`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Progress {
    pub phase: &'static str,
    /// Fraction of bytes sent, or `None` while there are no byte counts.
    pub fraction: Option<f64>,
}

/// One send's files and how it ends.
struct Job {
    id: String,
    owner: String,
    store: Entity<AttachmentDraft>,
    cancelled: bool,
    /// The draft text as sent, handed back on failure.
    draft: String,
    /// The draft the composer was left with, which a recovery may replace.
    followup: String,
    /// Every model and effort pick at Send, handed back with the text.
    picks: BTreeMap<String, RunPick>,
    /// For the host, once the files are uploaded.
    ready: Option<oneshot::Sender<Vec<Attachment>>>,
}

/// A failed send waiting to be handed back.
struct Held {
    owner: String,
    files: Vec<DraftFile>,
    draft: String,
    followup: String,
    picks: BTreeMap<String, RunPick>,
    error: Option<SharedString>,
}

/// The community's background sends (`queueFor`).
pub struct Queue {
    community: WeakEntity<Community>,
    jobs: Vec<Job>,
    held: HashMap<String, Held>,
    pub notices: Vec<Notice>,
    /// Composers that can show notices; the first one does.
    hosts: Vec<EntityId>,
}

pub fn queue(community: &Entity<Community>, cx: &mut App) -> Entity<Queue> {
    let scope = partition(community, cx);
    if let Some(queue) = cx.default_global::<Registry>().queues.get(&scope) {
        return queue.clone();
    }
    let weak = community.downgrade();
    let queue = cx.new(|_| Queue {
        community: weak,
        jobs: Vec::new(),
        held: HashMap::new(),
        notices: Vec::new(),
        hosts: Vec::new(),
    });
    cx.default_global::<Registry>()
        .queues
        .insert(scope, queue.clone());
    queue
}

/// The host's handle on a background send: the uploaded files, then how
/// publishing went.
#[derive(Clone)]
pub struct BackgroundSend {
    queue: WeakEntity<Queue>,
    job: String,
    uploaded: futures::future::Shared<oneshot::Receiver<Vec<Attachment>>>,
}

impl BackgroundSend {
    /// The uploaded files, or `None` when the send failed or was cancelled;
    /// its files and text are back in the draft then.
    pub async fn uploaded(&self) -> Option<Vec<Attachment>> {
        self.uploaded.clone().await.ok()
    }

    /// Reports how publishing went. A failure hands the files back with
    /// "Message failed to send: …".
    pub fn settle(&self, result: &Result<(), String>, cx: &mut App) {
        if let Some(queue) = self.queue.upgrade() {
            queue.update(cx, |queue, cx| queue.settle(&self.job, result, cx));
        }
    }
}

impl std::fmt::Debug for BackgroundSend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackgroundSend")
            .field("job", &self.job)
            .finish()
    }
}

impl Queue {
    pub fn uploading(&self) -> bool {
        !self.jobs.is_empty()
    }

    /// A conversation cannot send again until its send settles.
    pub fn pending(&self, owner: &str) -> bool {
        self.jobs.iter().any(|job| job.owner == owner)
    }

    pub fn progress(&self, cx: &App) -> Progress {
        let files: Vec<DraftFile> = self
            .jobs
            .iter()
            .flat_map(|job| job.store.read(cx).items.clone())
            .collect();
        let known = files
            .iter()
            .all(|item| item.status == Status::Ready || item.transfer.is_some());
        let total: u64 = files
            .iter()
            .map(|item| {
                item.uploaded
                    .as_ref()
                    .map(|a| a.size)
                    .or(item.transfer.map(|(_, total)| total))
                    .unwrap_or(0)
            })
            .sum();
        let sent: u64 = files
            .iter()
            .map(|item| match item.status {
                Status::Ready => item.uploaded.as_ref().map_or(0, |a| a.size),
                _ => item.transfer.map_or(0, |(sent, _)| sent),
            })
            .sum();
        let phase = if files.iter().any(|item| item.status == Status::Uploading) {
            "Uploading"
        } else if files.iter().all(|item| item.status == Status::Ready) {
            "Finishing"
        } else {
            "Preparing"
        };
        Progress {
            phase,
            fraction: (known && total > 0).then(|| sent as f64 / total as f64),
        }
    }

    /// Whether `host` shows the notices: only the first composer does.
    pub fn hosts_notices(&self, host: EntityId) -> bool {
        self.hosts.first() == Some(&host)
    }

    pub fn add_host(&mut self, host: EntityId, cx: &mut Context<Self>) {
        self.hosts.push(host);
        cx.notify();
    }

    pub fn remove_host(&mut self, host: EntityId, cx: &mut Context<Self>) {
        self.hosts.retain(|other| *other != host);
        cx.notify();
    }

    pub fn dismiss(&mut self, id: &str, cx: &mut Context<Self>) {
        self.notices.retain(|notice| notice.id != id);
        cx.notify();
    }

    /// Takes the files of `source` after Send, with their upload still
    /// running, and waits for them in order. `draft` is the text as sent,
    /// `followup` what the composer was left with and `picks` the model and
    /// effort picks at Send, for handing back.
    pub fn start(
        &mut self,
        owner: String,
        source: &Entity<AttachmentDraft>,
        draft: String,
        followup: String,
        picks: BTreeMap<String, RunPick>,
        cx: &mut Context<Self>,
    ) -> Option<BackgroundSend> {
        let community = self.community.upgrade()?;
        let id = format!("background:{}", uuid::Uuid::new_v4());
        let store = self::draft(&community, &id, cx);
        if source
            .read(cx)
            .items
            .iter()
            .all(|item| item.uploaded.is_some())
        {
            let files = source.read(cx).items.clone();
            store.update(cx, |store, cx| store.adopt(files, cx));
            source.update(cx, |source, cx| source.clear(cx));
        } else {
            source.update(cx, |source, cx| source.transfer(&store, cx));
        }
        cx.observe(&store, |_, _, cx| cx.notify()).detach();
        let (ready, uploaded) = oneshot::channel();
        let prepared = store.update(cx, |store, cx| store.prepare_for_send(cx));
        let job_id = id.clone();
        cx.spawn(async move |this, cx| {
            let result = prepared.await;
            let _ = this.update(cx, |queue, cx| queue.uploaded(&job_id, result, cx));
        })
        .detach();
        self.jobs.push(Job {
            id: id.clone(),
            owner,
            store,
            cancelled: false,
            draft,
            followup,
            picks,
            ready: Some(ready),
        });
        cx.notify();
        Some(BackgroundSend {
            queue: cx.entity().downgrade(),
            job: id,
            uploaded: futures::FutureExt::shared(uploaded),
        })
    }

    /// Cancel stops the newest send and hands it back like a failure.
    pub fn cancel_newest(&mut self, cx: &mut Context<Self>) {
        let Some(job) = self.jobs.iter_mut().rev().find(|job| !job.cancelled) else {
            return;
        };
        job.cancelled = true;
        let store = job.store.clone();
        store.update(cx, |store, cx| store.cancel_active(true, cx));
    }

    fn uploaded(
        &mut self,
        id: &str,
        result: Result<Vec<Attachment>, UploadError>,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.jobs.iter().position(|job| job.id == id) else {
            return;
        };
        match result {
            Ok(files) if !self.jobs[index].cancelled => {
                if let Some(ready) = self.jobs[index].ready.take()
                    && ready.send(files).is_ok()
                {
                    return;
                }
                // Nobody publishes it any more.
                self.finish(index, None, None, cx);
            }
            Ok(_) => self.finish(index, None, None, cx),
            Err(error) => {
                let job = &self.jobs[index];
                let notice = (!job.cancelled && error != UploadError::Cancelled)
                    .then(|| format!("Upload failed: {error}"));
                self.finish(index, notice, None, cx);
            }
        }
    }

    fn settle(&mut self, id: &str, result: &Result<(), String>, cx: &mut Context<Self>) {
        let Some(index) = self.jobs.iter().position(|job| job.id == id) else {
            return;
        };
        match result {
            Ok(()) => {
                let job = self.jobs.remove(index);
                let voices: Vec<String> = job
                    .store
                    .read(cx)
                    .items
                    .iter()
                    .filter_map(|item| item.voice.as_ref().map(|voice| voice.key.clone()))
                    .collect();
                for key in voices {
                    crate::ui::media::forget_local(&key, cx);
                }
                forget(&job.store, &job.id, self.community.upgrade(), cx);
                cx.notify();
            }
            Err(error) => self.finish(
                index,
                Some(format!("Message failed to send: {error}")),
                Some(error.clone()),
                cx,
            ),
        }
    }

    /// Ends a send that did not publish: its files and text go back to its
    /// draft, or stay here with a notice when that draft moved on. A
    /// publishing `error` shows in the composer too.
    fn finish(
        &mut self,
        index: usize,
        notice: Option<String>,
        error: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let job = self.jobs.remove(index);
        let files = job.store.read(cx).items.clone();
        forget(&job.store, &job.id, self.community.upgrade(), cx);
        let held = Held {
            owner: job.owner,
            files,
            draft: job.draft,
            followup: job.followup,
            picks: job.picks,
            error: error.map(SharedString::from),
        };
        if !self.restore(&held, cx) {
            self.notices.push(Notice {
                id: job.id.clone(),
                message: RECOVERY_CONFLICT.into(),
                recovery: Some(job.id.clone()),
            });
            self.held.insert(job.id.clone(), held);
        }
        if let Some(message) = notice {
            self.notices.push(Notice {
                id: format!("{}:failure", job.id),
                message: message.into(),
                recovery: None,
            });
        }
        cx.notify();
    }

    /// "Retry failed send recovery".
    pub fn retry_recovery(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(held) = self.held.remove(id) else {
            return;
        };
        if self.restore(&held, cx) {
            self.notices.retain(|notice| notice.id != id);
        } else {
            self.held.insert(id.to_string(), held);
        }
        cx.notify();
    }

    /// Hands `held` back to its draft when nothing replaced it: the saved
    /// text is still what was sent, the follow-up, or empty, and its draft
    /// has no files.
    fn restore(&self, held: &Held, cx: &mut App) -> bool {
        let Some(community) = self.community.upgrade() else {
            return false;
        };
        let target = draft(&community, &held.owner, cx);
        if !target.read(cx).items.is_empty() {
            return false;
        }
        let key = held.owner.clone();
        let partition = community.read(cx).partition();
        let store = &cx.global::<crate::community::AppState>().store;
        let saved = store.view_state(&partition, &key).ok().flatten();
        let untouched = saved.as_ref().is_none_or(|saved| {
            *saved == held.draft || *saved == held.followup || super::draft_is_empty(saved)
        });
        if !untouched {
            return false;
        }
        if store
            .set_view_state(&partition, &key, Some(&held.draft))
            .is_err()
        {
            return false;
        }
        let files = held.files.clone();
        let error = held.error.clone();
        let draft_text = held.draft.clone();
        let picks = held.picks.clone();
        target.update(cx, |target, cx| {
            target.adopt(files, cx);
            target.recovered = Some(Recovered {
                draft: draft_text,
                error,
                picks,
            });
            cx.notify();
        });
        true
    }
}

const RECOVERY_CONFLICT: &str = "Failed send was kept because this draft changed. Clear the draft, then retry recovery before leaving this session.";

fn forget(
    store: &Entity<AttachmentDraft>,
    key: &str,
    community: Option<Entity<Community>>,
    cx: &mut App,
) {
    store.update(cx, |store, cx| {
        store.active.clear();
        store.items.clear();
        cx.notify();
    });
    if let Some(community) = community {
        let scope = (partition(&community, cx), key.to_string());
        cx.default_global::<Registry>().drafts.remove(&scope);
    }
}

/// Files read from paths: their size and type, without reading them.
pub fn files_from_paths(paths: Vec<std::path::PathBuf>) -> Vec<Result<NewFile, (String, String)>> {
    paths
        .into_iter()
        .filter(|path| path.is_file())
        .map(|path| {
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "File".into());
            let source = Source::Path(path);
            let size = source
                .len()
                .map_err(|_| (name.clone(), UploadError::Failed.to_string()))?;
            let header = source.header(64).unwrap_or_default();
            Ok(NewFile {
                mime: upload::mime_type(&name, &header),
                name,
                size,
                source,
                voice: None,
            })
        })
        .collect()
}

/// A file from memory, such as a pasted image.
pub fn file_from_bytes(name: String, bytes: Vec<u8>) -> NewFile {
    let mime = upload::mime_type(&name, &bytes);
    NewFile {
        name,
        mime,
        size: bytes.len() as u64,
        source: Source::Bytes(Arc::from(bytes)),
        voice: None,
    }
}

/// `formatSize` in `ComposerAttachments`: "N KB" (at least 1) under a MiB,
/// else "N.N MB".
pub fn file_size(bytes: u64) -> String {
    const MIB: f64 = 1024. * 1024.;
    if (bytes as f64) < MIB {
        format!("{} KB", ((bytes as f64 / 1024.).round() as u64).max(1))
    } else {
        format!("{:.1} MB", bytes as f64 / MIB)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn sizes_read_like_the_reference() {
        assert_eq!(file_size(1), "1 KB");
        assert_eq!(file_size(1536), "2 KB");
        assert_eq!(file_size(1024 * 1023), "1023 KB");
        assert_eq!(file_size(1024 * 1024), "1.0 MB");
        assert_eq!(file_size(1_258_291), "1.2 MB");
        assert_eq!(file_size(500 * 1024 * 1024), "500.0 MB");
    }

    #[test]
    fn statuses_label_like_the_reference() {
        assert_eq!(Status::Queued.label(), "Queued");
        assert_eq!(Status::Preparing.label(), "Preparing…");
        assert_eq!(Status::Uploading.label(), "Uploading…");
        assert_eq!(Status::Ready.label(), "Ready");
        assert_eq!(Status::Error.label(), "Upload failed");
        assert!(!Status::Queued.busy() && Status::Uploading.busy());
    }

    fn file(id: &str, name: &str, mime: &str, status: Status) -> DraftFile {
        DraftFile {
            id: id.into(),
            name: name.into(),
            mime: mime.into(),
            size: 1,
            source: Source::Bytes(Arc::from(vec![0u8])),
            voice: None,
            status,
            uploaded: None,
            error: None,
            transfer: None,
        }
    }

    #[test]
    fn eager_uploads_start_one_queued_file_at_a_time() {
        let items = vec![
            file("a", "a.png", "image/png", Status::Ready),
            file("b", "b.png", "image/png", Status::Error),
            file("c", "c.png", "image/png", Status::Queued),
            file("d", "d.png", "image/png", Status::Queued),
        ];
        // A failed file waits for Retry; the first queued one starts.
        assert_eq!(next_queued(&items, false, false).as_deref(), Some("c"));
        // Not while another runs, nor while paused.
        assert_eq!(next_queued(&items, false, true), None);
        assert_eq!(next_queued(&items, true, false), None);
    }

    #[test]
    fn pausing_requeues_unfinished_files_and_a_failed_send_requires_retry() {
        let fresh = || {
            vec![
                file("a", "a.png", "image/png", Status::Ready),
                file("b", "b.png", "image/png", Status::Uploading),
                file("c", "c.png", "image/png", Status::Queued),
                file("d", "d.png", "image/png", Status::Error),
            ]
        };
        let mut paused = fresh();
        paused[1].transfer = Some((1, 2));
        freeze(&mut paused, false);
        let statuses: Vec<Status> = paused.iter().map(|f| f.status).collect();
        assert_eq!(
            statuses,
            [Status::Ready, Status::Queued, Status::Queued, Status::Error]
        );
        assert!(paused[1].transfer.is_none() && paused[1].error.is_none());
        let mut frozen = fresh();
        freeze(&mut frozen, true);
        let statuses: Vec<Status> = frozen.iter().map(|f| f.status).collect();
        assert_eq!(
            statuses,
            [Status::Ready, Status::Error, Status::Error, Status::Error]
        );
        assert_eq!(
            frozen[2].error.as_deref(),
            Some("Upload paused. Retry to continue.")
        );
    }

    #[test]
    fn previews_follow_the_relay_type_once_uploaded() {
        assert_eq!(
            file("a", "a.PNG", "", Status::Queued).preview(),
            Preview::Image
        );
        assert_eq!(
            file("a", "clip.mov", "", Status::Queued).preview(),
            Preview::Video
        );
        assert_eq!(
            file("a", "a.wav", "audio/wav", Status::Queued).preview(),
            Preview::Audio
        );
        assert_eq!(
            file("a", "notes.txt", "text/plain", Status::Queued).preview(),
            Preview::File
        );
        let mut uploaded = file("a", "a.png", "image/png", Status::Ready);
        uploaded.uploaded = Some(Attachment {
            name: "a.png".into(),
            url: "https://example.com/a".into(),
            mime: "application/octet-stream".into(),
            size: 1,
            sha256: String::new(),
            voice_duration: None,
        });
        assert_eq!(uploaded.preview(), Preview::File);
    }
}
