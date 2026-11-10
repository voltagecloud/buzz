//! Owner-approved snapshot transfer and ordinary diff-event patch return.

use super::*;
use buzz_sdk::sandbox_attach::snapshot as wire_snapshot;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

/// Read a sandbox snapshot or return a patch through its thread.
#[derive(Subcommand)]
pub enum SnapshotCmd {
    /// Request a separately approved snapshot and restore a read-only local copy
    Pull {
        /// Channel containing the sandbox thread
        #[arg(long)]
        channel: Uuid,
        /// Thread root or a message inside the sandbox thread
        #[arg(long)]
        thread: EventId,
        /// Sandbox agent public key; otherwise infer from the thread
        #[arg(long)]
        agent: Option<String>,
        /// Human owner; hosted gateways must specify their owner
        #[arg(long, env = "BUZZ_PASEO_OWNER")]
        owner: Option<String>,
        /// New local destination; its parent must already exist
        #[arg(long)]
        into: PathBuf,
        /// Dedicated snapshot store; defaults to the destination's parent
        #[arg(long)]
        store_root: Option<PathBuf>,
        /// Local temporary-work budget, below the filesystem's actual quota
        #[arg(long, default_value_t = 67_108_864, value_parser = clap::value_parser!(u64).range(1_048_576..=2_147_483_648))]
        work_bytes: u64,
        /// Total retained snapshot-store budget, including existing snapshots
        #[arg(long, default_value_t = 134_217_728, value_parser = clap::value_parser!(u64).range(1_048_576..=2_147_483_648))]
        store_bytes: u64,
        /// Approval lifetime in minutes, maximum 60
        #[arg(long, default_value_t = 15, value_parser = clap::value_parser!(u64).range(1..=60))]
        minutes: u64,
    },
    /// Approve one snapshot request with the sandbox owner's primary key
    Authorize {
        /// Expected human owner; this must match the signing key
        #[arg(long, env = "BUZZ_PASEO_OWNER")]
        owner: String,
        /// Pending snapshot request event ID
        #[arg(long)]
        request: EventId,
        /// Lifetime in minutes, capped by the pending request
        #[arg(long, default_value_t = 15, value_parser = clap::value_parser!(u64).range(1..=60))]
        minutes: u64,
    },
    /// Post a patch as a kind 40008 reply for the agent to review and apply
    PushPatch {
        /// Channel containing the sandbox thread
        #[arg(long)]
        channel: Uuid,
        /// Thread root or a message inside the sandbox thread
        #[arg(long)]
        thread: EventId,
        /// Sandbox agent public key; otherwise infer from the thread
        #[arg(long)]
        agent: Option<String>,
        /// Repository HTTP(S) URL, without embedded credentials
        #[arg(long)]
        repo: String,
        /// Full SHA-1 or SHA-256 snapshot base commit
        #[arg(long)]
        commit: String,
        /// UTF-8 patch file, at most 60 KiB
        file: PathBuf,
    },
}

async fn current_pending(
    client: &BuzzClient,
    pending: &wire_snapshot::Pending,
) -> Result<(), CliError> {
    let event = one(client,json!({"kinds":[30624],"authors":[pending.requester()],"#d":[format!("snapshot:{}:request",pending.session())],"limit":1})).await?;
    if event.id != pending.id() || wire_snapshot::Pending::parse(&event, now()).is_err() {
        return Err(failure(
            "snapshot request was replaced, withdrawn or expired",
        ));
    }
    Ok(())
}

async fn authorized(
    client: &BuzzClient,
    owner: PublicKey,
    relay: PublicKey,
    pending: &wire_snapshot::Pending,
    grant: &wire_snapshot::Grant,
) -> Result<(), CliError> {
    timeout(Duration::from_secs(5), async {
        let (head,roster,delegations,profiles,_) = tokio::try_join!(
            one(client,json!({"kinds":[30624],"authors":[owner],"#d":[format!("snapshot:{}:grant",grant.session())],"limit":1})),
            one(client,json!({"kinds":[39002],"authors":[relay],"#d":[grant.scope().channel],"limit":1})),
            query(client,json!({"kinds":[30181],"authors":[owner],"limit":32})),
            query(client,json!({"kinds":[0],"authors":[grant.requester()],"limit":1})),
            current_pending(client,pending),
        )?;
        if head.id != grant.id() || !grant.authorized(pending,Authority{owner,relay,roster:&roster,delegations:&delegations,profiles:&profiles},now()) {
            return Err(CliError::Auth("snapshot approval, membership or gateway authority ended".into()));
        }
        Ok(())
    }).await.map_err(|_|failure("snapshot authorization refresh timed out"))?
}

async fn authorize(
    client: &BuzzClient,
    owner: PublicKey,
    request: EventId,
    minutes: u64,
) -> Result<Event, CliError> {
    if client.keys().public_key() != owner {
        return Err(CliError::Auth(
            "snapshot approval requires the expected owner's primary key".into(),
        ));
    }
    let event = one(client, json!({"ids":[request],"kinds":[30624],"limit":1})).await?;
    if event.id != request {
        return Err(failure("relay returned another snapshot request"));
    }
    let pending = wire_snapshot::Pending::parse(&event, now()).map_err(failure)?;
    let approval = wire_snapshot::approve(
        client.keys(),
        &pending,
        pending.expires_at().min(now().saturating_add(minutes * 60)),
    )
    .map_err(failure)?;
    let grant = wire_snapshot::Grant::parse(&approval, pending.scope(), now()).map_err(failure)?;
    let relay = relay_key(client).await?;
    let (roster, delegations, profiles, _) = tokio::try_join!(
        one(
            client,
            json!({"kinds":[39002],"authors":[relay],"#d":[pending.scope().channel],"limit":1})
        ),
        query(
            client,
            json!({"kinds":[30181],"authors":[owner],"limit":32})
        ),
        query(
            client,
            json!({"kinds":[0],"authors":[pending.requester()],"limit":1})
        ),
        current_pending(client, &pending),
    )?;
    if !grant.authorized(
        &pending,
        Authority {
            owner,
            relay,
            roster: &roster,
            delegations: &delegations,
            profiles: &profiles,
        },
        now(),
    ) {
        return Err(CliError::Auth(
            "snapshot requester is not an admitted owner/live gateway".into(),
        ));
    }
    publish(client, approval.clone()).await?;
    authorized(client, owner, relay, &pending, &grant).await?;
    Ok(approval)
}

fn destination(into: &Path, store_root: Option<&Path>) -> Result<(PathBuf, PathBuf), CliError> {
    let name = into
        .file_name()
        .ok_or_else(|| CliError::Usage("snapshot destination must name a new directory".into()))?;
    let parent = into
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = parent
        .canonicalize()
        .map_err(|_| CliError::Usage("snapshot destination parent must already exist".into()))?;
    let into = parent.join(name);
    if into.try_exists().map_err(failure)? || into.symlink_metadata().is_ok() {
        return Err(CliError::Usage(
            "snapshot destination already exists".into(),
        ));
    }
    let store = store_root
        .unwrap_or(&parent)
        .canonicalize()
        .map_err(failure)?;
    if !store.is_dir() || !into.starts_with(&store) {
        return Err(CliError::Usage(
            "snapshot destination must be inside its store root".into(),
        ));
    }
    Ok((into, store))
}

async fn wait_grant(
    client: &BuzzClient,
    owner: PublicKey,
    relay: PublicKey,
    pending: &wire_snapshot::Pending,
    cancel: &CancellationToken,
) -> Result<wire_snapshot::Grant, CliError> {
    loop {
        if cancel.is_cancelled() {
            return Err(failure("snapshot pull cancelled"));
        }
        if now() >= pending.expires_at() {
            return Err(failure(
                "snapshot request expired while awaiting owner approval",
            ));
        }
        current_pending(client, pending).await?;
        let events=query(client,json!({"kinds":[30624],"authors":[owner],"#d":[format!("snapshot:{}:grant",pending.session())],"limit":1})).await?;
        if let Some(event) = events.first() {
            let grant =
                wire_snapshot::Grant::parse(event, pending.scope(), now()).map_err(failure)?;
            authorized(client, owner, relay, pending, &grant).await?;
            return Ok(grant);
        }
        tokio::select! { _=cancel.cancelled()=>return Err(failure("snapshot pull cancelled")), _=sleep(Duration::from_secs(2))=>{} }
    }
}

async fn wait_receipt(
    client: &BuzzClient,
    owner: PublicKey,
    relay: PublicKey,
    pending: &wire_snapshot::Pending,
    grant: &wire_snapshot::Grant,
    cancel: &CancellationToken,
) -> Result<wire_snapshot::Receipt, CliError> {
    loop {
        if cancel.is_cancelled() {
            return Err(failure("snapshot pull cancelled"));
        }
        authorized(client, owner, relay, pending, grant).await?;
        let ready=query(client,json!({"kinds":[30624],"authors":[grant.scope().agent],"#d":[format!("snapshot:{}:ready",grant.session())],"limit":1})).await?;
        if let Some(event) = ready.first() {
            return wire_snapshot::receipt(client.keys(), grant, event, now()).map_err(failure);
        }
        let ended=query(client,json!({"kinds":[30624],"authors":[grant.scope().agent],"#d":[format!("snapshot:{}:ended",grant.session())],"limit":1})).await?;
        if let Some(event) = ended.first() {
            let end = wire_snapshot::End::parse(event, grant).map_err(failure)?;
            if let Some(reason) = end.reason() {
                return Err(failure(format!(
                    "sandbox refused snapshot creation ({reason:?}); inspect its metadata audit"
                )));
            }
        }
        tokio::select! { _=cancel.cancelled()=>return Err(failure("snapshot pull cancelled")), _=sleep(Duration::from_secs(2))=>{} }
    }
}

async fn download(
    client: &BuzzClient,
    owner: PublicKey,
    relay: PublicKey,
    pending: &wire_snapshot::Pending,
    grant: &wire_snapshot::Grant,
    receipt: &wire_snapshot::Receipt,
    cancel: &CancellationToken,
) -> Result<Vec<u8>, CliError> {
    let transfer = client.download_snapshot_blob(receipt.hash(), receipt.size());
    tokio::pin!(transfer);
    let mut refresh = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(5),
        Duration::from_secs(5),
    );
    refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let bytes = loop {
        tokio::select! {
            _=cancel.cancelled()=>return Err(failure("snapshot pull cancelled")),
            result=&mut transfer=>break result?,
            _=refresh.tick()=>authorized(client,owner,relay,pending,grant).await?,
        }
    };
    authorized(client, owner, relay, pending, grant).await?;
    receipt.open(&bytes, now()).map_err(failure)
}

async fn withdraw(client: &BuzzClient, pending: &wire_snapshot::Pending) -> Result<(), CliError> {
    let deletion = client.sign_event(
        nostr::EventBuilder::new(nostr::Kind::EventDeletion, "snapshot request withdrawn").tags([
            nostr::Tag::parse(["e", &pending.id().to_hex()]).map_err(failure)?,
            nostr::Tag::parse(["h", &pending.scope().channel.to_string()]).map_err(failure)?,
        ]),
    )?;
    publish(client, deletion).await
}

struct Pull {
    owner: PublicKey,
    scope: Scope,
    into: PathBuf,
    store: PathBuf,
    budget: crate::sandbox_archive::RestoreBudget,
    minutes: u64,
}

async fn pull(
    client: &BuzzClient,
    options: Pull,
    cancel: CancellationToken,
) -> Result<(), CliError> {
    if cancel.is_cancelled() {
        return Err(failure("snapshot pull cancelled"));
    }
    let relay = relay_key(client).await?;
    let request = wire_snapshot::request(
        client.keys(),
        options.scope,
        Uuid::new_v4(),
        now() + options.minutes * 60,
    )
    .map_err(failure)?;
    let pending = wire_snapshot::Pending::parse(&request, now()).map_err(failure)?;
    publish(client, request).await?;
    let result=async {
        if options.owner==client.keys().public_key() {
            authorize(client,options.owner,pending.id(),options.minutes).await?;
        } else {
            eprintln!("Owner approval required for snapshot channel {} thread {} agent {} requester {}.\nOn the owner's machine, using the same relay, run:\nbuzz sandbox snapshot authorize --owner {} --request {}",options.scope.channel,options.scope.thread,options.scope.agent,client.keys().public_key(),options.owner,pending.id());
        }
        let grant=wait_grant(client,options.owner,relay,&pending,&cancel).await?;
        let receipt=wait_receipt(client,options.owner,relay,&pending,&grant,&cancel).await?;
        if receipt.size().saturating_mul(2)>options.budget.work_bytes { return Err(failure("snapshot ciphertext and plaintext exceed local work budget")); }
        let archive=download(client,options.owner,relay,&pending,&grant,&receipt,&cancel).await?;
        if cancel.is_cancelled() { return Err(failure("snapshot pull cancelled")); }
        let restored=crate::sandbox_archive::extract(archive,&options.into,&options.store,options.budget,cancel.clone()).await.map_err(failure)?;
        println!("{}",json!({"directory":restored.directory,"repositories":restored.repositories,"grant":grant.id()}));
        Ok(())
    }.await;
    match result {
        Ok(()) => Ok(()),
        Err(error) => {
            withdraw(client,&pending).await.map_err(|_|failure(format!("{error}; snapshot request withdrawal failed, so remote work may continue until its deadline")))?;
            Err(error)
        }
    }
}

async fn interruptible_pull(client: &BuzzClient, options: Pull) -> Result<(), CliError> {
    let cancel = CancellationToken::new();
    let work = pull(client, options, cancel.clone());
    tokio::pin!(work);
    tokio::select! {
        result=&mut work=>result,
        signal=interrupted()=>{
            cancel.cancel();
            let result=work.await;
            match (signal,result) {
                (Ok(()),result)=>result,
                (Err(signal),Ok(()))=>Err(signal),
                (Err(signal),Err(error))=>Err(failure(format!("{signal}; snapshot cancellation: {error}"))),
            }
        }
    }
}

async fn interrupted() -> Result<(), CliError> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).map_err(failure)?;
        let mut hup = signal(SignalKind::hangup()).map_err(failure)?;
        tokio::select! {
            result=tokio::signal::ctrl_c()=>result.map_err(failure),
            _=term.recv()=>Ok(()),
            _=hup.recv()=>Ok(()),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.map_err(failure)
    }
}

fn read_patch(file: &Path, repo: &str, commit: &str) -> Result<String, CliError> {
    let url = url::Url::parse(repo)
        .map_err(|_| CliError::Usage("patch repository must be an HTTP(S) URL".into()))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(CliError::Usage(
            "patch repository URL must not contain credentials, query or fragment".into(),
        ));
    }
    if !matches!(commit.len(), 40 | 64) || !commit.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err(CliError::Usage(
            "patch base commit must be a full SHA-1 or SHA-256 hash".into(),
        ));
    }
    let mut open = std::fs::OpenOptions::new();
    open.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open.custom_flags(nix::fcntl::OFlag::O_NONBLOCK.bits());
    }
    let stream = open.open(file).map_err(failure)?;
    if !stream.metadata().map_err(failure)?.is_file() {
        return Err(CliError::Usage("patch must be a regular file".into()));
    }
    let mut bytes = Vec::new();
    stream
        .take(60 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(failure)?;
    if bytes.is_empty() || bytes.len() > 60 * 1024 {
        return Err(CliError::Usage(
            "patch must contain 1 through 60 KiB of UTF-8 text".into(),
        ));
    }
    String::from_utf8(bytes).map_err(|_| CliError::Usage("patch must be UTF-8".into()))
}

/// Execute snapshot operations after the parent validates WSS and caller identity.
pub async fn dispatch(command: SnapshotCmd, client: &BuzzClient) -> Result<(), CliError> {
    match command {
        SnapshotCmd::Authorize {
            owner,
            request,
            minutes,
        } => {
            let event = authorize(client, key(&owner)?, request, minutes).await?;
            println!("{}", json!({"event_id":event.id,"accepted":true}));
            Ok(())
        }
        SnapshotCmd::Pull {
            channel,
            thread,
            agent,
            owner,
            into,
            store_root,
            work_bytes,
            store_bytes,
            minutes,
        } => {
            let (into, store) = destination(&into, store_root.as_deref())?;
            crate::sandbox_archive::check_git().await.map_err(failure)?;
            let owner = owner
                .as_deref()
                .map(key)
                .transpose()?
                .unwrap_or(client.keys().public_key());
            let scope =
                select_target(client, Some(channel), Some(thread), agent.as_deref()).await?;
            interruptible_pull(
                client,
                Pull {
                    owner,
                    scope,
                    into,
                    store,
                    budget: crate::sandbox_archive::RestoreBudget {
                        work_bytes,
                        store_bytes,
                    },
                    minutes,
                },
            )
            .await
        }
        SnapshotCmd::PushPatch {
            channel,
            thread,
            agent,
            repo,
            commit,
            file,
        } => {
            let patch = read_patch(&file, &repo, &commit)?;
            let scope =
                select_target(client, Some(channel), Some(thread), agent.as_deref()).await?;
            let meta = buzz_sdk::DiffMeta {
                repo_url: repo,
                commit_sha: commit,
                file_path: None,
                parent_commit: None,
                branch: None,
                pr_number: None,
                language: None,
                description: Some(
                    "Patch returned from a local snapshot; review before applying".into(),
                ),
                truncated: false,
                alt_text: Some("Sandbox patch".into()),
            };
            let builder = buzz_sdk::build_diff_message(
                channel,
                &patch,
                &meta,
                Some(&buzz_sdk::ThreadRef {
                    root_event_id: scope.thread,
                    parent_event_id: scope.thread,
                }),
            )
            .map_err(failure)?
            .tag(nostr::Tag::public_key(scope.agent));
            let event = client.sign_event(builder)?;
            publish(client, event.clone()).await?;
            println!("{}", json!({"event_id":event.id,"accepted":true}));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests;
