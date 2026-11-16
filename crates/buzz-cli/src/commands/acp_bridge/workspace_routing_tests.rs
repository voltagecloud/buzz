//! Workspace routing (`PLANS/PASEO_WORKSPACES_DESIGN.md`, section 4.5): a
//! session's working directory selects its workspace, the workspace selects
//! the channel, `/fresh` routes again, and `session/list` places roots from
//! relay state alone, so a restarted bridge reports the same directories.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use nostr::Timestamp;

use super::super::bridge::MAX_SESSIONS;
use super::*;

const BUZZ: &str = "https://github.com/voltagecloud/buzz";

fn signed(author: &Keys, kind: u16, tags: Vec<Vec<String>>, at: u64) -> Event {
    EventBuilder::new(Kind::Custom(kind), "")
        .tags(tags.into_iter().map(|tag| Tag::parse(tag).unwrap()))
        // The fake relay signs rosters with the bridge signer, which a
        // roster also lists as the gateway.
        .allow_self_tagging()
        .custom_created_at(Timestamp::from(at))
        .sign_with_keys(author)
        .unwrap()
}

/// An owner project; `marked` adds the Paseo workspace marker.
fn project(
    owner: &Keys,
    slug: &str,
    channel: Uuid,
    sources: &[&str],
    at: u64,
    marked: bool,
) -> Event {
    let mut tags = vec![
        vec!["d".to_owned(), slug.to_owned()],
        vec!["buzz-channel".to_owned(), channel.to_string()],
    ];
    if marked {
        tags.push(vec!["buzz-paseo".to_owned(), "workspace".to_owned()]);
    }
    for source in sources {
        tags.push(vec!["buzz-source".to_owned(), (*source).to_owned()]);
    }
    signed(owner, 30621, tags, at)
}

fn workspace(owner: &Keys, slug: &str, channel: Uuid, at: u64) -> Event {
    project(owner, slug, channel, &[BUZZ], at, true)
}

/// Stream channel metadata, signed by the relay. The fake relay's identity
/// is the bridge signer (`FakeRelay::relay_identity`).
fn metadata(relay: &Keys, channel: Uuid) -> Event {
    let tags = vec![
        vec!["d".to_owned(), channel.to_string()],
        vec!["t".to_owned(), "stream".to_owned()],
    ];
    signed(relay, 39000, tags, 1)
}

/// A channel roster of posting members, signed by `signer`.
fn roster(signer: &Keys, channel: Uuid, members: &[PublicKey]) -> Event {
    let mut tags = vec![vec!["d".to_owned(), channel.to_string()]];
    for member in members {
        tags.push(vec![
            "p".into(),
            member.to_hex(),
            String::new(),
            "member".into(),
        ]);
    }
    signed(signer, 39002, tags, 1)
}

/// A stream channel whose relay-signed roster lists `members`.
fn channel(relay: &Keys, channel: Uuid, members: &[PublicKey]) -> [Event; 2] {
    [metadata(relay, channel), roster(relay, channel, members)]
}

fn channel_of(event: &Event) -> Uuid {
    Uuid::parse_str(&tag(event, "h")[0][1]).unwrap()
}

fn tag_value(event: &Event, name: &str) -> Option<String> {
    tag(event, name).first().map(|parts| parts[1].clone())
}

fn link(channel: Uuid, root: &Event) -> String {
    format!("buzz://message?channel={channel}&id={}", root.id.to_hex())
}

fn mkdirs(home: &Path, dirs: &[&str]) {
    for dir in dirs {
        std::fs::create_dir_all(home.join(dir)).unwrap();
    }
}

fn error_of(frame: &Value) -> String {
    frame["error"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("expected an error: {frame}"))
        .to_owned()
}

impl Harness {
    fn add(&self, events: impl IntoIterator<Item = Event>) {
        self.history.lock().unwrap().0.extend(events);
    }

    fn remove_project(&self, slug: &str) {
        self.history
            .lock()
            .unwrap()
            .0
            .retain(|event| event.kind.as_u16() != 30621 || tag(event, "d")[0][1] != slug);
    }

    async fn new_session(&mut self, id: u64, cwd: &str) -> Value {
        self.client
            .request(id, "session/new", json!({"cwd": cwd, "mcpServers": []}))
            .await
            .0
    }

    /// Create a session in `cwd` and return the thread root its first
    /// prompt posts.
    async fn post_root(&mut self, id: u64, cwd: &str, text: &str) -> (String, Event) {
        let created = self.new_session(id, cwd).await;
        let session = created["result"]["sessionId"]
            .as_str()
            .unwrap_or_else(|| panic!("{cwd}: {created}"))
            .to_owned();
        self.client.send(prompt(id + 1, &session, text)).await;
        let root = recv(&mut self.submitted).await;
        assert!(tag(&root, "e").is_empty(), "the first prompt posts a root");
        (session, root)
    }

    async fn sessions(&mut self, id: u64) -> HashMap<String, PathBuf> {
        let (listed, _) = self.client.request(id, "session/list", json!({})).await;
        listed["result"]["sessions"]
            .as_array()
            .unwrap_or_else(|| panic!("{listed}"))
            .iter()
            .map(|session| {
                (
                    session["sessionId"].as_str().unwrap().to_owned(),
                    PathBuf::from(session["cwd"].as_str().unwrap()),
                )
            })
            .collect()
    }
}

/// Session creation routes by the working directory, after lexical `..`
/// traversal. A workspace directory holds no sources, and a shared clone that
/// a workspace names as a source is in no workspace. Projects without the
/// marker or from another key are no workspaces, and a replacement or a
/// deletion routes the next session even while listing reuses a lookup.
#[tokio::test]
async fn new_sessions_post_in_their_workspace_channel() {
    let ids = Ids::new();
    let home = ids.home.path().to_owned();
    let mut h = start(&ids, Timing::default());
    h.relay_tx.send(RelayInbound::Live).await.unwrap();
    let relay = ids.signer.clone();
    let members = [ids.signer.public_key(), ids.agent.public_key()];
    let (alpha, beta, plain, foreign, moved) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    let stranger = Keys::generate();
    h.add([
        workspace(&ids.signer, "alpha", alpha, 10),
        workspace(&ids.signer, "beta", beta, 11),
        project(&ids.signer, "plain", plain, &[], 12, false),
        workspace(&stranger, "foreign", foreign, 13),
    ]);
    for id in [alpha, beta, plain, foreign, moved] {
        h.add(channel(&relay, id, &members));
    }
    mkdirs(
        &home,
        &[
            "code/voltagecloud/buzz/src",
            "workspaces/alpha/notes",
            "workspaces/beta",
            "workspaces/plain",
            "workspaces/foreign",
        ],
    );
    let at = |path: &str| format!("{}/{path}", home.display());

    // Two workspaces on one repository post in two channels.
    for (step, (cwd, channel, workspace, project)) in [
        ("workspaces/alpha/notes", alpha, Some("alpha"), None),
        // `..` resolves lexically.
        (
            "workspaces/alpha/notes/../../beta",
            beta,
            Some("beta"),
            None,
        ),
        // Both workspaces name this shared clone as a source, but it lies in
        // neither: the legacy channel.
        (
            "code/voltagecloud/buzz/src",
            ids.channel,
            None,
            Some("voltagecloud/buzz"),
        ),
        (
            "workspaces/alpha/../../code/voltagecloud/buzz",
            ids.channel,
            None,
            Some("voltagecloud/buzz"),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let (_, root) = h.post_root(10 * step as u64 + 10, &at(cwd), "hello").await;
        assert_eq!(channel_of(&root), channel, "{cwd}");
        assert_eq!(
            tag_value(&root, "paseo-workspace").as_deref(),
            workspace,
            "{cwd}"
        );
        assert_eq!(
            tag_value(&root, "paseo-project").as_deref(),
            project,
            "{cwd}"
        );
    }

    // An unmarked project, another key's workspace, and an unknown directory
    // refuse the session and never fall back to the legacy channel.
    for (step, cwd) in [
        "workspaces/plain",
        "workspaces/foreign",
        "workspaces/unknown/src",
    ]
    .into_iter()
    .enumerate()
    {
        let refused = h.new_session(100 + step as u64, &at(cwd)).await;
        let message = error_of(&refused);
        assert!(
            message.contains("not an active Paseo workspace"),
            "{message}"
        );
        assert!(message.contains("Nothing was posted"), "{message}");
    }

    // The lookups above warmed the cache, which listing would reuse. A
    // new session still reads the replacement that moves alpha.
    h.add([workspace(&ids.signer, "alpha", moved, 20)]);
    let (_, root) = h.post_root(210, &at("workspaces/alpha"), "moved").await;
    assert_eq!(channel_of(&root), moved);
    // After a deletion, the directory refuses new sessions.
    h.remove_project("alpha");
    let refused = h.new_session(220, &at("workspaces/alpha")).await;
    assert!(error_of(&refused).contains("not an active Paseo workspace"));
    assert!(h.submitted.try_recv().is_err(), "a refusal posts nothing");
}

/// Without a legacy channel, or without a workspace lookup, nothing posts.
/// A lookup runs beside other sessions, so a slow one does not hold their
/// prompts.
#[tokio::test]
async fn sessions_outside_a_workspace_or_without_a_lookup_post_nothing() {
    let ids = Ids::new();
    let home = ids.home.path().to_owned();
    let mut scope = ids.scope();
    scope.channel = None;
    let mut h = start_scope(scope, Timing::default());
    h.relay_tx.send(RelayInbound::Live).await.unwrap();
    let relay = ids.signer.clone();
    let alpha = Uuid::new_v4();
    h.add([workspace(&ids.signer, "alpha", alpha, 10)]);
    h.add(channel(
        &relay,
        alpha,
        &[ids.signer.public_key(), ids.agent.public_key()],
    ));
    mkdirs(&home, &["code/voltagecloud/buzz", "workspaces/alpha"]);
    let at = |path: &str| format!("{}/{path}", home.display());
    let alpha_dir = at("workspaces/alpha");

    // Outside every workspace: the error names the command, and for a
    // shared clone, the workspaces whose sources include it.
    let elsewhere = error_of(&h.new_session(1, "/tmp").await);
    assert!(
        elsewhere.contains("buzz paseo workspace create"),
        "{elsewhere}"
    );
    assert!(!elsewhere.contains(&alpha_dir), "{elsewhere}");
    let clone = error_of(&h.new_session(2, &at("code/voltagecloud/buzz")).await);
    assert!(clone.contains("buzz paseo workspace create"), "{clone}");
    assert!(clone.contains(&alpha_dir), "{clone}");

    // A workspace session needs no legacy channel.
    let (_, root) = h.post_root(3, &alpha_dir, "in alpha").await;
    assert_eq!(channel_of(&root), alpha);
    let idle = h.new_session(8, &alpha_dir).await["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_owned();

    // A failed lookup refuses the session; it never falls back.
    h.history.lock().unwrap().1 = true;
    let failed = error_of(&h.new_session(5, &alpha_dir).await);
    assert!(failed.contains("workspace lookup failed"), "{failed}");
    h.history.lock().unwrap().1 = false;

    // A slow lookup: while every relay read waits, a prompt in another
    // session still posts.
    let held = Arc::clone(&h.query_gate)
        .acquire_many_owned(4)
        .await
        .unwrap();
    h.client
        .send(json!({"jsonrpc": "2.0", "id": 6, "method": "session/new", "params": {"cwd": alpha_dir, "mcpServers": []}}))
        .await;
    h.client.send(prompt(7, &idle, "while it waits")).await;
    let posted = recv(&mut h.submitted).await;
    assert_eq!(posted.content, "while it waits");
    assert_eq!(channel_of(&posted), alpha);
    drop(held);
    let (created, _) = h.client.response(6).await;
    assert!(created["result"]["sessionId"].is_string(), "{created}");
    assert!(h.submitted.try_recv().is_err(), "refusals posted nothing");
}

/// A workspace session needs the owner, the gateway, and the agent to be
/// posting members of the workspace channel, by the relay-signed roster.
#[tokio::test]
async fn workspace_sessions_need_every_member_in_the_channel() {
    let ids = Ids::new();
    let home = ids.home.path().to_owned();
    let owner = Keys::generate();
    let mut scope = ids.scope();
    scope.owner = owner.public_key();
    let mut h = start_scope(scope, Timing::default());
    h.relay_tx.send(RelayInbound::Live).await.unwrap();
    let relay = ids.signer.clone();
    let (owner_key, gateway, agent) = (
        owner.public_key(),
        ids.signer.public_key(),
        ids.agent.public_key(),
    );
    let stranger = Keys::generate();
    let cases: [(&str, &Keys, Vec<PublicKey>, Option<&str>); 5] = [
        (
            "left",
            &relay,
            vec![gateway, agent],
            Some("you (the owner)"),
        ),
        (
            "ungated",
            &relay,
            vec![owner_key, agent],
            Some("the Paseo gateway"),
        ),
        (
            "agentless",
            &relay,
            vec![owner_key, gateway],
            Some("the agent"),
        ),
        // A roster the relay did not sign admits no one.
        (
            "forged",
            &stranger,
            vec![owner_key, gateway, agent],
            Some("you (the owner)"),
        ),
        ("complete", &relay, vec![owner_key, gateway, agent], None),
    ];
    for (step, (slug, signer, members, refusal)) in cases.into_iter().enumerate() {
        let id = Uuid::new_v4();
        h.add([
            workspace(&owner, slug, id, 10 + step as u64),
            metadata(&relay, id),
            roster(signer, id, &members),
        ]);
        mkdirs(&home, &[&format!("workspaces/{slug}")]);
        let cwd = format!("{}/workspaces/{slug}", home.display());
        match refusal {
            Some(missing) => {
                let message = error_of(&h.new_session(10 * step as u64, &cwd).await);
                assert!(message.contains(missing), "{slug}: {message}");
                assert!(message.contains("Nothing was posted"), "{slug}: {message}");
            }
            None => {
                let (_, root) = h.post_root(10 * step as u64, &cwd, "hello").await;
                assert_eq!(channel_of(&root), id);
            }
        }
    }
    assert!(h.submitted.try_recv().is_err(), "refusals posted nothing");
}

/// `/fresh` routes the new root again from the session's working
/// directory: it follows a moved workspace, links the old root in the old
/// root's channel, and on a failed lookup posts nothing and stays ended.
#[tokio::test]
async fn fresh_routes_again_and_links_the_old_root_in_its_channel() {
    let ids = Ids::new();
    let home = ids.home.path().to_owned();
    let mut h = start(&ids, Timing::default());
    h.relay_tx.send(RelayInbound::Live).await.unwrap();
    let relay = ids.signer.clone();
    let (first, second) = (Uuid::new_v4(), Uuid::new_v4());
    let members = [ids.signer.public_key(), ids.agent.public_key()];
    h.add([workspace(&ids.signer, "alpha", first, 10)]);
    h.add(channel(&relay, first, &members));
    h.add(channel(&relay, second, &members));
    mkdirs(&home, &["workspaces/alpha"]);
    let cwd = format!("{}/workspaces/alpha", home.display());

    let (session, mut root) = h.post_root(1, &cwd, "first").await;
    let mut request = 2;
    // Each step: the thread's session ends (the notice arrives in the
    // root's own channel), then `/fresh` posts a new root.
    for (step, (moved_to, expected)) in [(None, first), (Some(second), second)]
        .into_iter()
        .enumerate()
    {
        h.inject(thread_message(
            &ids.agent,
            channel_of(&root),
            &root.id.to_hex(),
            ENDED_NOTICE,
        ))
        .await;
        let (ended, _) = h.client.response(request).await;
        assert_eq!(ended["result"]["stopReason"], "end_turn", "step {step}");
        if let Some(channel) = moved_to {
            h.add([workspace(&ids.signer, "alpha", channel, 20)]);
        }
        request += 1;
        h.client.send(prompt(request, &session, "/fresh")).await;
        let fresh = recv(&mut h.submitted).await;
        assert!(tag(&fresh, "e").is_empty(), "/fresh posts a root");
        assert_eq!(channel_of(&fresh), expected, "step {step}");
        assert_eq!(
            tag_value(&fresh, "paseo-workspace").as_deref(),
            Some("alpha")
        );
        assert!(
            fresh.content.contains(&link(channel_of(&root), &root)),
            "{}",
            fresh.content
        );
        root = fresh;
    }

    // The latest root ends. A `/fresh` cancelled during its lookup, or whose
    // lookup fails, posts nothing and leaves the session ended, so a retry
    // works.
    h.inject(thread_message(
        &ids.agent,
        second,
        &root.id.to_hex(),
        ENDED_NOTICE,
    ))
    .await;
    let (ended, _) = h.client.response(request).await;
    assert_eq!(ended["result"]["stopReason"], "end_turn");
    let held = Arc::clone(&h.query_gate)
        .acquire_many_owned(4)
        .await
        .unwrap();
    h.client
        .send(prompt(request + 3, &session, "/fresh never posted"))
        .await;
    h.client
        .send(
            json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": session}}),
        )
        .await;
    let (cancelled, _) = h.client.response(request + 3).await;
    assert_eq!(cancelled["result"]["stopReason"], "cancelled");
    // The lookup completes after the cancel; its result is dropped.
    drop(held);
    h.history.lock().unwrap().1 = true;
    let (failed, _) = h
        .client
        .request(
            request + 1,
            "session/prompt",
            prompt(0, &session, "/fresh")["params"].clone(),
        )
        .await;
    assert!(error_of(&failed).contains("workspace lookup failed"));
    assert!(
        h.submitted.try_recv().is_err(),
        "a failed lookup posts nothing"
    );
    h.history.lock().unwrap().1 = false;
    h.client.send(prompt(request + 2, &session, "/fresh")).await;
    let retried = recv(&mut h.submitted).await;
    assert_eq!(channel_of(&retried), second);
    assert!(!retried.content.contains("never posted"));
    assert!(
        retried.content.contains(&link(second, &root)),
        "{}",
        retried.content
    );
}

/// Workspace lookups that complete together never take the bridge past
/// [`MAX_SESSIONS`]: each completion checks the cap again.
#[tokio::test]
async fn completing_lookups_keep_the_session_cap() {
    let ids = Ids::new();
    let home = ids.home.path().to_owned();
    // The relay is never live, so every prompt keeps running.
    let mut h = start(&ids, Timing::default());
    let relay = ids.signer.clone();
    let alpha = Uuid::new_v4();
    h.add([workspace(&ids.signer, "alpha", alpha, 10)]);
    h.add(channel(
        &relay,
        alpha,
        &[ids.signer.public_key(), ids.agent.public_key()],
    ));
    mkdirs(&home, &["workspaces/alpha"]);
    let cwd = format!("{}/workspaces/alpha", home.display());
    for i in 0..MAX_SESSIONS as u64 - 1 {
        let created = h.new_session(1000 + i, "/tmp").await;
        let session = created["result"]["sessionId"].as_str().unwrap().to_owned();
        h.client.send(prompt(2000 + i, &session, "running")).await;
    }

    let held = Arc::clone(&h.query_gate)
        .acquire_many_owned(4)
        .await
        .unwrap();
    for id in 1..=4 {
        h.client
            .send(json!({"jsonrpc": "2.0", "id": id, "method": "session/new", "params": {"cwd": cwd, "mcpServers": []}}))
            .await;
    }
    drop(held);
    let mut created = Vec::new();
    for id in 1..=4 {
        let (frame, _) = h.client.response(id).await;
        created.push(frame["result"]["sessionId"].as_str().unwrap().to_owned());
    }
    // Only one slot was free: each completion evicted the previous idle
    // session, so exactly one of the four remains.
    for (i, session) in created.iter().enumerate() {
        h.client.send(prompt(10 + i as u64, session, "hello")).await;
    }
    let (_, notes) = h.client.request(99, "authenticate", json!({})).await;
    let unknown = notes
        .iter()
        .filter(|frame| {
            frame["error"]["message"]
                .as_str()
                .is_some_and(|message| message.starts_with("unknown session"))
        })
        .count();
    assert_eq!(unknown, 3, "{notes:?}");
}

/// At [`MAX_SESSIONS`], a refused `session/new` evicts nothing: only a
/// created session takes the least recently used idle session's place.
#[tokio::test]
async fn refused_sessions_at_the_cap_keep_idle_sessions() {
    let ids = Ids::new();
    let home = ids.home.path().to_owned();
    let mut h = start(&ids, Timing::default());
    h.relay_tx.send(RelayInbound::Live).await.unwrap();
    mkdirs(&home, &["workspaces/gone"]);
    let gone = format!("{}/workspaces/gone", home.display());
    let mut sessions = Vec::new();
    for i in 0..MAX_SESSIONS as u64 {
        let created = h.new_session(1000 + i, "/tmp").await;
        sessions.push(created["result"]["sessionId"].as_str().unwrap().to_owned());
    }

    let deleted = error_of(&h.new_session(1, &gone).await);
    assert!(
        deleted.contains("not an active Paseo workspace"),
        "{deleted}"
    );
    h.history.lock().unwrap().1 = true;
    let failed = error_of(&h.new_session(2, &gone).await);
    assert!(failed.contains("workspace lookup failed"), "{failed}");
    h.history.lock().unwrap().1 = false;

    // The two least recently used sessions are still there.
    for (id, session) in [(3, &sessions[0]), (4, &sessions[1])] {
        h.client.send(prompt(id, session, "still here")).await;
        let posted = recv(&mut h.submitted).await;
        assert_eq!(posted.content, "still here");
    }
}

/// Whether restore's `importCwd` accepts `cwd`: an existing directory
/// strictly under `home`, outside hidden directories, reached without links.
fn importable(home: &Path, cwd: &Path) -> bool {
    let relative = cwd.strip_prefix(home).ok();
    relative.is_some_and(|relative| {
        !relative.as_os_str().is_empty()
            && relative
                .components()
                .all(|part| !part.as_os_str().to_string_lossy().starts_with('.'))
    }) && cwd.canonicalize().ok().as_deref() == Some(cwd)
        && cwd.is_dir()
}

/// `session/list` places every root by the section 4.5 order from relay
/// state alone, so a restarted bridge, and the restore run that lists
/// through one, report the same directories across each transition.
#[tokio::test]
async fn placement_holds_across_bridge_restarts_and_transitions() {
    let ids = Ids::new();
    let home = ids.home.path().canonicalize().unwrap();
    let legacy = ids.channel;
    let (alpha, elsewhere) = (Uuid::new_v4(), Uuid::new_v4());
    let agent = ids.agent.public_key().to_hex();
    let outsider = Keys::generate();
    mkdirs(
        &home,
        &[
            "code/voltagecloud/buzz",
            "workspaces/alpha",
            "workspaces/older",
            "workspaces/newer",
            "workspaces/retired",
        ],
    );
    let root = |author: &Keys, mentions: &[&str], extra: &[[&str; 2]], text: &str| {
        let mut builder =
            buzz_sdk::build_message(legacy, text, None, mentions, false, &[], &[]).unwrap();
        let tags: Vec<Tag> = extra.iter().map(|tag| Tag::parse(*tag).unwrap()).collect();
        builder = builder.tags(tags);
        builder.sign_with_keys(author).unwrap()
    };
    let tagged = root(
        &ids.signer,
        &[&agent],
        &[["paseo-workspace", "alpha"]],
        "tagged",
    );
    let retained = root(
        &ids.signer,
        &[&agent],
        &[["paseo-workspace", "retired"]],
        "retained",
    );
    let missing = root(
        &ids.signer,
        &[&agent],
        &[
            ["paseo-workspace", "gone"],
            ["paseo-project", "voltagecloud/buzz"],
        ],
        "missing",
    );
    let project_root = root(
        &ids.signer,
        &[&agent],
        &[["paseo-project", "voltagecloud/buzz"]],
        "project",
    );
    let untagged = root(&ids.signer, &[&agent], &[], "untagged");
    // A teammate's root names alpha, which it cannot choose; the agent
    // joined its thread.
    let joined = root(&outsider, &[], &[["paseo-workspace", "alpha"]], "joined");
    let joined_reply = thread_message(&ids.agent, legacy, &joined.id.to_hex(), "joining");
    let relay = Keys::generate();
    let history = vec![
        metadata(&relay, legacy),
        metadata(&relay, alpha),
        metadata(&relay, elsewhere),
        workspace(&ids.signer, "alpha", alpha, 10),
        workspace(&ids.signer, "older", legacy, 11),
        workspace(&ids.signer, "newer", legacy, 12),
        tagged.clone(),
        retained.clone(),
        missing.clone(),
        project_root.clone(),
        untagged.clone(),
        joined.clone(),
        joined_reply,
    ];
    let dir = |path: &str| home.join(path);
    let fixed = [
        // Step 1, live and retained; a recorded workspace without a
        // directory goes to `~/buzz`, before step 2 is tried.
        (&tagged, dir("workspaces/alpha")),
        (&retained, dir("workspaces/retired")),
        (&missing, dir("buzz")),
        // Step 2.
        (&project_root, dir("code/voltagecloud/buzz")),
    ];
    // Step 3 picks the newest workspace on the root's channel. Moving it
    // away leaves the older one; deleting that leaves step 4.
    let moved = workspace(&ids.signer, "newer", elsewhere, 20);
    let is_older = |event: &Event| event.kind.as_u16() == 30621 && tag(event, "d")[0][1] == "older";
    let phases = [
        (history.clone(), dir("workspaces/newer")),
        (
            history
                .iter()
                .cloned()
                .chain([moved.clone()])
                .collect::<Vec<_>>(),
            dir("workspaces/older"),
        ),
        (
            history
                .iter()
                .filter(|event| !is_older(event))
                .cloned()
                .chain([moved])
                .collect(),
            dir("buzz"),
        ),
    ];
    for (phase, (events, bound)) in phases.into_iter().enumerate() {
        let expected: Vec<(&Event, PathBuf)> = fixed
            .iter()
            .cloned()
            .chain([(&untagged, bound.clone()), (&joined, bound.clone())])
            .collect();
        // Two bridge processes over the same relay state: the second one is
        // the restart, or the bridge a restore run lists through.
        for run in 0..2 {
            let mut scope = ids.scope();
            scope.home = home.clone();
            let mut h = start_scope(scope, Timing::default());
            h.history.lock().unwrap().0 = events.clone();
            let listed = h.sessions(1).await;
            assert_eq!(
                listed.len(),
                expected.len(),
                "phase {phase} run {run}: {listed:?}"
            );
            for (root, cwd) in &expected {
                let got = &listed[&root.id.to_hex()];
                assert_eq!(got, cwd, "phase {phase} run {run}: {}", root.content);
                assert!(importable(&home, got), "{}", got.display());
            }
        }
    }
}

/// The fake relay narrows a multi-value `#h` before its limit, as the
/// relay's SQL channel predicate does.
#[test]
fn fake_relay_applies_a_multi_value_channel_filter_before_its_limit() {
    let author = Keys::generate();
    let (first, second, other) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let message = |channel: Uuid, at: u64| {
        buzz_sdk::build_message(channel, "m", None, &[], false, &[], &[])
            .unwrap()
            .custom_created_at(Timestamp::from(at))
            .sign_with_keys(&author)
            .unwrap()
    };
    let events = [message(first, 1), message(second, 2), message(other, 3)];
    let page = fake_query_page(
        &events,
        &json!({"kinds": [9], "#h": [first.to_string(), second.to_string()], "limit": 1}),
    );
    assert_eq!(page.iter().map(channel_of).collect::<Vec<_>>(), [second]);
}
