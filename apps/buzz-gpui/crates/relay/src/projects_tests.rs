use super::*;
use crate::testing::Signer;
use nostr::prelude::{Keys, SecretKey};
use std::sync::Mutex;

fn keys(byte: u8) -> Keys {
    Keys::new(SecretKey::from_slice(&[byte; 32]).unwrap())
}

struct World {
    signer: Signer,
    owner: Keys,
    outsider: Keys,
}

impl World {
    fn new() -> Self {
        Self {
            signer: Signer::new(),
            owner: keys(1),
            outsider: keys(2),
        }
    }

    fn owner_hex(&self) -> String {
        self.owner.public_key().to_hex()
    }

    fn address(&self) -> String {
        format!("30617:{}:repo", self.owner_hex())
    }

    fn event(&self, kind: u16, tags: &[&[&str]], content: &str, at: u64) -> Verified {
        self.signer.sign(&self.owner, kind, at, content, tags)
    }

    fn outsider(&self, kind: u16, tags: &[&[&str]], content: &str, at: u64) -> Verified {
        self.signer.sign(&self.outsider, kind, at, content, tags)
    }

    fn repo(&self) -> Verified {
        self.event(
            REPOSITORY,
            &[&["d", "repo"], &["name", "Actual repository"]],
            "",
            1,
        )
    }

    fn project(&self) -> Verified {
        let address = self.address();
        self.event(
            PROJECT,
            &[
                &["d", "project"],
                &["name", "Actual project"],
                &["a", &address],
                &["buzz-related-channel", "general"],
            ],
            "",
            1,
        )
    }

    fn issue(&self) -> Verified {
        let address = self.address();
        self.event(
            ISSUE,
            &[&["a", &address], &["subject", "Fix the reader"]],
            "Issue description",
            1,
        )
    }

    fn pr(&self) -> Verified {
        let address = self.address();
        let commit = "a".repeat(40);
        self.event(
            PULL_REQUEST,
            &[
                &["a", &address],
                &["subject", "Reader changes"],
                &["c", &commit],
            ],
            "Pull request description",
            1,
        )
    }

    fn route(&self, kind: RouteKind, dtag: &str) -> EntityRoute {
        EntityRoute {
            kind,
            owner: self.owner_hex(),
            dtag: dtag.into(),
            tab: None,
            commit: None,
            id: None,
        }
    }
}

/// Answers reads from a fixed event list, as the relay matches filters.
struct Fake {
    events: Vec<Verified>,
    reads: Mutex<usize>,
    offline: bool,
}

fn fake(events: Vec<Verified>) -> Destinations<Fake> {
    Destinations::new(Fake {
        events,
        reads: Mutex::new(0),
        offline: false,
    })
}

fn matches(event: &Verified, filter: &Filter) -> bool {
    filter.iter().all(|(key, value)| {
        let list = || value.as_array().cloned().unwrap_or_default();
        match key.as_str() {
            "kinds" => list()
                .iter()
                .any(|k| k.as_u64() == Some(event.kind.as_u16() as u64)),
            "authors" => list()
                .iter()
                .any(|a| a.as_str() == Some(&event.pubkey.to_hex())),
            "ids" => list()
                .iter()
                .any(|a| a.as_str() == Some(&event.id.to_hex())),
            "limit" => true,
            tag if tag.starts_with('#') => {
                let name = &tag[1..];
                list().iter().any(|wanted| {
                    event.tags.iter().any(|t| {
                        let t = t.as_slice();
                        t.first().is_some_and(|n| n == name)
                            && t.get(1).map(String::as_str) == wanted.as_str()
                    })
                })
            }
            _ => false,
        }
    })
}

impl ProjectReads for Fake {
    fn read(
        &self,
        filters: Vec<Filter>,
    ) -> impl Future<Output = Result<Vec<Verified>, ReadError>> + Send {
        *self.reads.lock().unwrap() += 1;
        let result = if self.offline {
            Err(ReadError {
                kind: ReadErrorKind::Unavailable,
                message: "offline".into(),
                status: None,
                retry_after: None,
            })
        } else {
            Ok(self
                .events
                .iter()
                .filter(|e| filters.iter().any(|f| matches(e, f)))
                .cloned()
                .collect())
        };
        std::future::ready(result)
    }
}

fn block_on<T>(future: impl Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(future)
}

// ── routes ─────────────────────────────────────────────────────────────────

fn all_routes() -> Vec<EntityRoute> {
    let owner = "ab".repeat(32);
    let id = "cd".repeat(32);
    let base = |kind, dtag: &str| EntityRoute {
        kind,
        owner: owner.clone(),
        dtag: dtag.into(),
        tab: None,
        commit: None,
        id: None,
    };
    let mut routes = vec![
        base(RouteKind::Repo, "repo"),
        base(RouteKind::Project, "project"),
        EntityRoute {
            id: Some(id.clone()),
            ..base(RouteKind::Pr, "repo")
        },
        EntityRoute {
            id: Some(id.clone()),
            ..base(RouteKind::Issue, "repo")
        },
    ];
    for tab in EntityTab::ALL {
        routes.push(base(RouteKind::Repo, "repo").with_tab(Some(tab)));
        routes.push(base(RouteKind::Project, "project").with_tab(Some(tab)));
    }
    for length in [40, 64] {
        routes.push(base(RouteKind::Repo, "repo").with_commit(&"a".repeat(length)));
    }
    routes
}

#[test]
fn routes_round_trip_through_links_and_params() {
    for route in all_routes() {
        let href = route.href();
        assert_eq!(
            EntityRoute::parse_link(&href),
            Some(route.clone()),
            "{href}"
        );
        assert_eq!(
            EntityRoute::from_params(&route.params()),
            Some(route.clone()),
            "{href}"
        );
    }
}

#[test]
fn links_normalize_hex_and_keep_repository_casing() {
    let owner = "ab".repeat(32);
    let id = "cd".repeat(32);
    let href = format!(
        "buzz://repo/?owner={}&d=My-Repo&tab=commits&commit={}",
        owner.to_uppercase(),
        "A".repeat(40)
    );
    assert_eq!(
        EntityRoute::parse_link(&href),
        Some(EntityRoute {
            kind: RouteKind::Repo,
            owner: owner.clone(),
            dtag: "My-Repo".into(),
            tab: Some(EntityTab::Commits),
            commit: Some("a".repeat(40)),
            id: None,
        })
    );
    let issue = EntityRoute::item(ItemKind::Issue, &owner, "repo", &id).unwrap();
    assert_eq!(
        issue.href(),
        format!("buzz://issue?id={id}&owner={owner}&d=repo")
    );
}

#[test]
fn links_ignore_parameters_outside_their_grammar() {
    let owner = "ab".repeat(32);
    let id = "cd".repeat(32);
    let commit = "a".repeat(40);
    for (canonical, extra) in [
        (
            format!("repo?owner={owner}&d=repo"),
            "relay=https://elsewhere.example".to_string(),
        ),
        (
            format!("repo?owner={owner}&d=repo&tab=commits&commit={commit}"),
            "foo=bar&foo=baz".to_string(),
        ),
        (
            format!("project?owner={owner}&d=project"),
            format!("commit={id}"),
        ),
        (format!("pr?id={id}&owner={owner}&d=repo"), "tab=prs".into()),
        (
            format!("issue?id={id}&owner={owner}&d=repo"),
            "foo=bar".into(),
        ),
    ] {
        let href = format!("buzz://{canonical}");
        let parsed = EntityRoute::parse_link(&href).expect(&href);
        for lax in [
            format!("{href}&{extra}"),
            format!(
                "buzz://{}",
                canonical.replacen('?', &format!("?{extra}&"), 1)
            ),
        ] {
            assert_eq!(
                EntityRoute::parse_link(&lax).as_ref(),
                Some(&parsed),
                "{lax}"
            );
        }
        assert_eq!(parsed.href(), href);
    }
    // A project's commits link drops the repository-only commit.
    let canonical = format!("buzz://project?owner={owner}&d=project&tab=commits");
    let parsed = EntityRoute::parse_link(&format!("{canonical}&commit={commit}")).unwrap();
    assert_eq!(parsed.commit, None);
    assert_eq!(parsed.href(), canonical);
}

#[test]
fn links_reject_ambiguous_or_unsupported_forms() {
    let owner = "ab".repeat(32);
    let id = "cd".repeat(32);
    for part in [
        format!("repo?owner={owner}"),
        "repo?owner=no&d=repo".to_string(),
        format!("repo?owner={owner}&d=..repo"),
        format!("repo?owner={owner}&d=a..b"),
        format!("repo?owner={owner}&d=has%20space"),
        format!("repo?owner={owner}&d=repo&tab=overview"),
        format!("repo?owner={owner}&d=repo&tab=files&commit={id}"),
        format!("repo?owner={owner}&d=repo&tab=commits&commit=short"),
        format!("repo?owner={owner}&d=repo&owner={owner}"),
        format!("repo?owner={owner}&d=repo&tab=files&tab=commits"),
        format!("pr?id={id}&id={id}&owner={owner}&d=repo"),
        format!("issue?owner={owner}&d=repo"),
        format!("repo/path?owner={owner}&d=repo"),
        format!("repo?owner={owner}&d=repo#frag"),
        format!("user@repo?owner={owner}&d=repo"),
        format!("repo:1?owner={owner}&d=repo"),
        format!("commit?h={id}"),
    ] {
        assert_eq!(
            EntityRoute::parse_link(&format!("buzz://{part}")),
            None,
            "{part}"
        );
    }
    let owner = json!("ab".repeat(32));
    for params in [
        Value::Null,
        json!([]),
        json!({}),
        json!({"type": "["}),
        json!({"type": "repo", "owner": owner, "dtag": "repo", "extra": "x"}),
        json!({"type": "repo", "owner": owner, "dtag": "repo", "tab": null}),
        json!({"type": "repo", "owner": owner, "d": "repo"}),
    ] {
        assert_eq!(EntityRoute::from_params(&params), None, "{params}");
    }
}

#[test]
fn identifiers_follow_the_link_grammar() {
    for good in [
        "repo",
        "My-Repo",
        "_x",
        "-x",
        "a.b",
        "plugins.",
        &"n".repeat(64),
    ] {
        assert!(entity_dtag(good), "{good}");
    }
    for bad in [
        "",
        ".hidden",
        "a..b",
        "has space",
        "a/b",
        "nested:repo",
        &"n".repeat(65),
    ] {
        assert!(!entity_dtag(bad), "{bad}");
    }
}

#[test]
fn community_repositories_are_canonical_urls_on_this_origin() {
    let base = format!("https://relay.test/git/{}", "a".repeat(64));
    for name in [
        "plugins".to_string(),
        "plugins.git".into(),
        "plugins.".into(),
        "plugins..git".into(),
        "n".repeat(64),
        format!("{}.git", "n".repeat(64)),
    ] {
        let url = format!("{base}/{name}");
        assert_eq!(
            community_git_repository("https://relay.test", &url),
            Some(url.clone())
        );
    }
    for name in [
        ".hidden".to_string(),
        "a..b".into(),
        "a..b.git".into(),
        ".git".into(),
        "..git".into(),
        "n".repeat(65),
        format!("{}.git", "n".repeat(65)),
    ] {
        assert_eq!(
            community_git_repository("https://relay.test", &format!("{base}/{name}")),
            None,
            "{name}"
        );
    }
    assert_eq!(
        community_git_repository("https://other.test", &format!("{base}/x")),
        None
    );
    assert_eq!(
        community_git_repository("https://relay.test", &format!("{base}/x/info/refs")),
        None
    );
}

// ── git reads ──────────────────────────────────────────────────────────────

fn snapshot(head: Option<&str>) -> Value {
    json!({
        "head": head,
        "commits": head.map(|h| vec![json!({"hash": h, "author": "Ann", "date": 1, "subject": "init"})]).unwrap_or_default(),
        "files": head.map(|_| vec![json!({"path": "README.md", "hash": "b".repeat(40), "size": 5})]).unwrap_or_default(),
        "readme": head.map(|_| "hello"),
        "file": null,
        "diff": null,
    })
}

#[test]
fn git_reads_are_validated_and_bounded() {
    let read = GitRead {
        owner: "AB".repeat(32),
        dtag: "repo".into(),
        commit: None,
        path: None,
    }
    .parse()
    .unwrap();
    assert_eq!(read.owner, "ab".repeat(32));
    for bad in [
        Some("a//b"),
        Some("./a"),
        Some("a/../b"),
        Some("a\u{7f}"),
        Some("/abs"),
    ] {
        let read = GitRead {
            path: bad.map(str::to_string),
            ..read.clone()
        };
        assert!(read.parse().is_none(), "{bad:?}");
    }
    let head = "c".repeat(40);
    let ok = git_response(&read, 200, &snapshot(Some(&head)).to_string()).unwrap();
    assert_eq!(ok.readme.as_deref(), Some("hello"));
    let empty = git_response(&read, 200, &snapshot(None).to_string()).unwrap();
    assert_eq!(empty.head, None);
    assert_eq!(git_response(&read, 401, ""), Err(EntityFailure::Denied));
    assert_eq!(git_response(&read, 403, ""), Err(EntityFailure::Denied));
    assert_eq!(git_response(&read, 404, ""), Err(EntityFailure::NotFound));
    assert_eq!(
        git_response(&read, 503, ""),
        Err(EntityFailure::Unavailable)
    );
    assert_eq!(
        git_response(&read, 200, &" ".repeat(GIT_READ_BYTES + 1)),
        Err(EntityFailure::Unavailable)
    );
    // An empty repository has no content at all.
    let mut forged = snapshot(None);
    forged["readme"] = json!("x");
    assert_eq!(
        git_response(&read, 200, &forged.to_string()),
        Err(EntityFailure::Unavailable)
    );
    // A commit read must show that commit, with its changes.
    let commit_read = GitRead {
        commit: Some(head.clone()),
        ..read.clone()
    };
    assert_eq!(
        git_response(&commit_read, 200, &snapshot(Some(&head)).to_string()),
        Err(EntityFailure::Unavailable)
    );
    let mut shown = snapshot(Some(&head));
    shown["diff"] = json!("commit ...");
    assert!(git_response(&commit_read, 200, &shown.to_string()).is_ok());
    let other = GitRead {
        commit: Some("d".repeat(40)),
        ..read.clone()
    };
    assert_eq!(
        git_response(&other, 200, &shown.to_string()),
        Err(EntityFailure::Unavailable)
    );
    // A file read must return that file.
    let file_read = GitRead {
        path: Some("src/main.rs".into()),
        ..read.clone()
    };
    let mut file = snapshot(Some(&head));
    file["file"] = json!({"path": "README.md", "content": "hello", "size": 5});
    assert_eq!(
        git_response(&file_read, 200, &file.to_string()),
        Err(EntityFailure::Unavailable)
    );
    file["file"] = json!({"path": "src/main.rs", "content": null, "size": 2_000_000});
    assert!(git_response(&file_read, 200, &file.to_string()).is_ok());
}

// ── destinations ───────────────────────────────────────────────────────────

#[test]
fn resolves_repositories_projects_members_and_related_channels() {
    let world = World::new();
    let destinations = fake(vec![world.repo(), world.project()]);
    let repo = block_on(destinations.load(&world.route(RouteKind::Repo, "repo"))).unwrap();
    assert_eq!(repo.entity.name, "Actual repository");
    let project = block_on(destinations.load(&world.route(RouteKind::Project, "project"))).unwrap();
    assert_eq!(project.entity.name, "Actual project");
    assert_eq!(project.repositories.len(), 1);
    assert_eq!(project.repositories[0].address, world.address());
    assert_eq!(project.channels, ["general"]);
}

#[test]
fn resolves_an_item_with_only_authorized_status_and_its_discussion() {
    let world = World::new();
    for item in [world.issue(), world.pr()] {
        let id = item.id.to_hex();
        let good = world.event(STATUS_CLOSED, &[&["e", &id, "", "root"]], "", 2);
        let forged = world.outsider(STATUS_APPLIED, &[&["e", &id, "", "root"]], "", 3);
        let comment = world.outsider(COMMENT, &[&["E", &id]], "Review discussion", 4);
        let note = world.outsider(NOTE, &[&["e", &id]], "Earlier note", 2);
        let destinations = fake(vec![
            world.repo(),
            item.clone(),
            good,
            forged,
            comment.clone(),
            note.clone(),
        ]);
        let kind = if item.kind.as_u16() == PULL_REQUEST {
            RouteKind::Pr
        } else {
            RouteKind::Issue
        };
        let route = EntityRoute {
            id: Some(id),
            ..world.route(kind, "repo")
        };
        let result = block_on(destinations.load(&route)).unwrap();
        assert_eq!(result.item.as_ref(), Some(&item));
        assert_eq!(result.status, Some("Closed"));
        assert_eq!(result.activity, [note, comment]);
        assert_eq!(
            result.commit,
            (kind == RouteKind::Pr).then(|| "a".repeat(40))
        );
    }
}

#[test]
fn missing_or_mismatched_destinations_are_not_found() {
    let world = World::new();
    let issue = world.issue();
    let other = world.event(REPOSITORY, &[&["d", "other"]], "", 1);
    let destinations = fake(vec![world.repo(), other, issue.clone()]);
    let id = issue.id.to_hex();
    for route in [
        world.route(RouteKind::Repo, "missing"),
        EntityRoute {
            id: Some(id.clone()),
            ..world.route(RouteKind::Pr, "repo")
        },
        EntityRoute {
            id: Some(id.clone()),
            ..world.route(RouteKind::Issue, "other")
        },
    ] {
        assert_eq!(
            block_on(destinations.load(&route)).unwrap_err(),
            EntityFailure::NotFound,
            "{route:?}"
        );
    }
}

#[test]
fn a_deleted_or_malformed_head_does_not_fall_back() {
    let world = World::new();
    let address = world.address();
    let latest = world.event(REPOSITORY, &[&["d", "repo"]], "new", 4);
    let removal = world.event(DELETION, &[&["a", &address]], "", 5);
    let foreign = world.outsider(DELETION, &[&["a", &address]], "", 6);
    let route = world.route(RouteKind::Repo, "repo");
    assert_eq!(
        block_on(fake(vec![world.repo(), latest.clone(), removal]).load(&route)).unwrap_err(),
        EntityFailure::NotFound
    );
    let kept = block_on(fake(vec![latest.clone(), foreign]).load(&route)).unwrap();
    assert_eq!(kept.entity.event, latest);
    let malformed = world.event(
        PROJECT,
        &[&["d", "project"], &["a", "not-a-coordinate"]],
        "",
        5,
    );
    assert_eq!(
        block_on(
            fake(vec![world.project(), malformed])
                .load(&world.route(RouteKind::Project, "project"))
        )
        .unwrap_err(),
        EntityFailure::NotFound
    );
}

#[test]
fn lists_a_repositorys_items_and_unavailable_members() {
    let world = World::new();
    let issue = world.issue();
    let removal = world.event(DELETION, &[&["e", &issue.id.to_hex()]], "", 3);
    let route = EntityRoute {
        id: Some(issue.id.to_hex()),
        ..world.route(RouteKind::Issue, "repo")
    };
    assert_eq!(
        block_on(fake(vec![world.repo(), issue.clone(), removal]).load(&route)).unwrap_err(),
        EntityFailure::NotFound
    );
    let pr = world.pr();
    let prs = block_on(
        fake(vec![world.repo(), issue, pr.clone()]).load(
            &world
                .route(RouteKind::Repo, "repo")
                .with_tab(Some(EntityTab::Prs)),
        ),
    )
    .unwrap();
    assert_eq!(prs.items, [pr]);
    assert!(!prs.truncated);
    let project =
        block_on(fake(vec![world.project()]).load(&world.route(RouteKind::Project, "project")))
            .unwrap();
    assert_eq!(project.unavailable_repositories, [world.address()]);
}

#[test]
fn the_directory_drops_deleted_entities_and_reports_failures() {
    let world = World::new();
    let address = world.address();
    let destinations = fake(vec![
        world.repo(),
        world.project(),
        world.event(DELETION, &[&["a", &address]], "", 3),
    ]);
    let list = block_on(destinations.list()).unwrap();
    assert_eq!(
        list.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
        ["Actual project"]
    );
    assert_eq!(*destinations.read.reads.lock().unwrap(), 2);
    let offline = Destinations::new(Fake {
        events: Vec::new(),
        reads: Mutex::new(0),
        offline: true,
    });
    assert_eq!(
        block_on(offline.list()).unwrap_err(),
        EntityFailure::Unavailable
    );
}

#[test]
fn unlisted_announcements_leave_the_directory() {
    let world = World::new();
    let unlisted = world.event(
        REPOSITORY,
        &[&["d", "repo"], &["buzz-visibility", "unlisted"]],
        "",
        2,
    );
    let list = block_on(fake(vec![world.repo(), unlisted]).list()).unwrap();
    assert!(list.is_empty());
}

#[test]
fn keeps_metadata_coordinates_outside_the_link_grammar() {
    let world = World::new();
    let member = world.event(REPOSITORY, &[&["d", "nested:repository"]], "", 1);
    let coordinate = format!("30617:{}:nested:repository", world.owner_hex());
    let parent = world.event(PROJECT, &[&["d", "project"], &["a", &coordinate]], "", 1);
    let result =
        block_on(fake(vec![parent, member]).load(&world.route(RouteKind::Project, "project")))
            .unwrap();
    assert_eq!(result.repositories[0].dtag, "nested:repository");
    assert_eq!(result.repositories[0].route(), None);
}

#[test]
fn honors_the_newest_trusted_pr_revision_and_draft_label() {
    let world = World::new();
    let address = world.address();
    let a = "a".repeat(40);
    let root = world.event(
        PULL_REQUEST,
        &[&["a", &address], &["c", &a], &["t", "draft"]],
        "",
        1,
    );
    let id = root.id.to_hex();
    let author = root.pubkey.to_hex();
    let (b, c, d) = ("b".repeat(40), "c".repeat(40), "d".repeat(40));
    let update = world.event(
        PULL_REQUEST_UPDATE,
        &[
            &["a", &address],
            &["E", &id],
            &["P", &author],
            &["c", &b],
            &["clone", "https://git.example/repo.git"],
        ],
        "",
        2,
    );
    let forged = world.outsider(PULL_REQUEST_UPDATE, &[&["E", &id], &["c", &c]], "", 3);
    let misdirected = world.event(PULL_REQUEST_UPDATE, &[&["e", &id], &["c", &d]], "", 4);
    let result = block_on(
        fake(vec![world.repo(), root, update, forged, misdirected]).load(&EntityRoute {
            id: Some(id),
            ..world.route(RouteKind::Pr, "repo")
        }),
    )
    .unwrap();
    assert_eq!(result.status, Some("Draft"));
    assert_eq!(result.commit, Some(b));
}

#[test]
fn project_content_is_not_description_metadata() {
    let world = World::new();
    let project = world.event(PROJECT, &[&["d", "p"]], "body text", 1);
    assert_eq!(entity_from_event(&project).unwrap().description, "");
    let repo = world.event(REPOSITORY, &[&["d", "r"]], "repo body", 1);
    let entity = entity_from_event(&repo).unwrap();
    assert_eq!(entity.description, "repo body");
    assert_eq!(entity.name, "r");
}

#[test]
fn malformed_project_members_are_refused() {
    let world = World::new();
    let address = world.address();
    for tags in [
        vec![vec!["d", "p"], vec!["a", &address], vec!["a", &address]],
        vec![vec!["d", "p"], vec!["a", "30618:x:y"]],
        vec![vec!["d", "p"], vec!["a", &address, "hint", "extra"]],
        vec![vec!["d", "p"], vec!["name", "a"], vec!["name", "b"]],
        vec![vec!["d", "p"], vec!["d", "q"]],
    ] {
        let tags: Vec<&[&str]> = tags.iter().map(Vec::as_slice).collect();
        let event = world.event(PROJECT, &tags, "", 1);
        assert!(entity_from_event(&event).is_none(), "{tags:?}");
    }
}

// ── channel homes (authoritativeProjects) ──────────────────────────────────

#[test]
fn a_project_home_needs_a_repository_bound_to_the_channel() {
    let world = World::new();
    let address = world.address();
    let repo = |channel: &str| {
        world.event(
            REPOSITORY,
            &[&["d", "repo"], &["buzz-channel", channel]],
            "",
            1,
        )
    };
    let project = world.event(
        PROJECT,
        &[&["d", "project"], &["a", &address], &["buzz-channel", "c1"]],
        "",
        1,
    );
    // The repository must name this channel.
    assert!(authoritative_projects(std::slice::from_ref(&project), &[repo("c2")], "c1").is_empty());
    assert_eq!(
        authoritative_projects(std::slice::from_ref(&project), &[repo("c1")], "c1").len(),
        1
    );
    // A stranger's project cannot claim someone else's repository.
    let hostile = world.outsider(
        PROJECT,
        &[&["d", "project"], &["a", &address], &["buzz-channel", "c1"]],
        "",
        1,
    );
    assert!(authoritative_projects(std::slice::from_ref(&hostile), &[repo("c1")], "c1").is_empty());
    // A maintainer can.
    let outsider = world.outsider.public_key().to_hex();
    let maintained = world.event(
        REPOSITORY,
        &[
            &["d", "repo"],
            &["buzz-channel", "c1"],
            &["maintainers", &outsider],
        ],
        "",
        1,
    );
    assert_eq!(
        authoritative_projects(&[hostile], &[maintained], "c1").len(),
        1
    );
    // Unlisted projects are ignored.
    let unlisted = world.event(
        PROJECT,
        &[
            &["d", "project"],
            &["a", &address],
            &["buzz-channel", "c1"],
            &["buzz-visibility", "unlisted"],
        ],
        "",
        1,
    );
    assert!(authoritative_projects(&[unlisted], &[repo("c1")], "c1").is_empty());
}

/// Answers like [`Fake`], except that each queued gate holds one read
/// until the test releases it.
struct Gated {
    events: Vec<Verified>,
    gates: Mutex<std::collections::VecDeque<futures::channel::oneshot::Receiver<Gate>>>,
}

type Gate = Result<Vec<Verified>, ReadError>;

impl Gated {
    fn hold(&self) -> futures::channel::oneshot::Sender<Gate> {
        let (send, receive) = futures::channel::oneshot::channel();
        self.gates.lock().unwrap().push_back(receive);
        send
    }
}

impl ProjectReads for Gated {
    fn read(
        &self,
        filters: Vec<Filter>,
    ) -> impl Future<Output = Result<Vec<Verified>, ReadError>> + Send {
        let gate = self.gates.lock().unwrap().pop_front();
        let answer: Vec<Verified> = self
            .events
            .iter()
            .filter(|e| filters.iter().any(|f| matches(e, f)))
            .cloned()
            .collect();
        async move {
            match gate {
                Some(gate) => gate.await.unwrap_or(Ok(Vec::new())),
                None => Ok(answer),
            }
        }
    }
}

fn gated(events: Vec<Verified>) -> Destinations<Gated> {
    Destinations::new(Gated {
        events,
        gates: Mutex::new(Default::default()),
    })
}

fn offline() -> ReadError {
    ReadError {
        kind: ReadErrorKind::Unavailable,
        message: "offline".into(),
        status: None,
        retry_after: None,
    }
}

fn status(home: &Option<Home>) -> &'static str {
    match home {
        None => "unknown",
        Some(Home::None) => "none",
        Some(Home::Ambiguous) => "ambiguous",
        Some(Home::Project(_)) => "home",
    }
}

/// The channel repository and its projects, by `d`.
fn channel_homes(world: &World, projects: &[&str]) -> Vec<Verified> {
    let address = world.address();
    let mut events = vec![world.event(
        REPOSITORY,
        &[&["d", "repo"], &["buzz-channel", "c1"]],
        "",
        1,
    )];
    for d in projects {
        events.push(world.event(
            PROJECT,
            &[&["d", d], &["a", &address], &["buzz-channel", "c1"]],
            "",
            1,
        ));
    }
    events
}

#[test]
fn homes_are_kept_until_the_next_check_completes() {
    use futures::FutureExt;
    let world = World::new();
    for (projects, expected) in [
        (vec![], "none"),
        (vec!["one"], "home"),
        (vec!["one", "two"], "ambiguous"),
    ] {
        let events = if projects.is_empty() {
            Vec::new()
        } else {
            channel_homes(&world, &projects)
        };
        let destinations = gated(events.clone());
        let first = block_on(destinations.home("c1")).ok();
        assert_eq!(status(&first), expected);
        assert_eq!(status(&destinations.peek_home("c1")), expected);
        // Another session keeps its own.
        assert_eq!(status(&gated(events).peek_home("c1")), "unknown");
        let release = destinations.read.hold();
        let mut next = Box::pin(destinations.home("c1"));
        assert!(next.as_mut().now_or_never().is_none());
        assert_eq!(status(&destinations.peek_home("c1")), expected);
        release.send(Ok(Vec::new())).unwrap();
        assert!(matches!(block_on(next), Ok(Home::None)));
        assert_eq!(status(&destinations.peek_home("c1")), "none");
    }
}

#[test]
fn a_failed_cleared_or_cancelled_check_keeps_no_invalid_answer() {
    use futures::FutureExt;
    let world = World::new();
    for outcome in ["failure", "cancel", "clear"] {
        let destinations = gated(channel_homes(&world, &["one"]));
        block_on(destinations.home("c1")).unwrap();
        let release = destinations.read.hold();
        let mut next = Box::pin(destinations.home("c1"));
        assert!(next.as_mut().now_or_never().is_none());
        match outcome {
            "cancel" => drop(next),
            "clear" => {
                destinations.homes.clear();
                release.send(Ok(Vec::new())).unwrap();
                assert!(block_on(next).is_err());
            }
            _ => {
                release.send(Err(offline())).unwrap();
                assert!(block_on(next).is_err());
            }
        }
        let kept = if outcome == "cancel" {
            "home"
        } else {
            "unknown"
        };
        assert_eq!(status(&destinations.peek_home("c1")), kept, "{outcome}");
    }
}

#[test]
fn an_older_check_cannot_replace_or_erase_a_newer_answer() {
    use futures::FutureExt;
    let world = World::new();
    for outcome in ["success", "failure", "cancel"] {
        let destinations = gated(channel_homes(&world, &["one"]));
        let release = destinations.read.hold();
        let mut older = Box::pin(destinations.home("c1"));
        assert!(older.as_mut().now_or_never().is_none());
        block_on(destinations.home("c1")).unwrap();
        match outcome {
            "success" => {
                release.send(Ok(Vec::new())).unwrap();
                assert!(matches!(block_on(older), Ok(Home::None)));
            }
            "failure" => {
                release.send(Err(offline())).unwrap();
                assert!(block_on(older).is_err());
            }
            _ => drop(older),
        }
        assert_eq!(status(&destinations.peek_home("c1")), "home", "{outcome}");
    }
}

#[test]
fn kept_homes_are_bounded_by_count_and_bytes() {
    let destinations = gated(Vec::new());
    for channel in 0..65 {
        block_on(destinations.home(&channel.to_string())).unwrap();
    }
    assert_eq!(status(&destinations.peek_home("0")), "unknown");
    assert_eq!(status(&destinations.peek_home("64")), "none");
    let world = World::new();
    let mut events = channel_homes(&world, &[]);
    events.push(world.event(
        PROJECT,
        &[
            &["d", "large"],
            &["a", &world.address()],
            &["buzz-channel", "c1"],
        ],
        &"x".repeat(1024 * 1024),
        1,
    ));
    let oversized = gated(events);
    assert!(matches!(
        block_on(oversized.home("c1")),
        Ok(Home::Project(_))
    ));
    assert_eq!(status(&oversized.peek_home("c1")), "unknown");
}

#[test]
fn a_cancelled_newer_check_leaves_the_surviving_one_its_answer() {
    use futures::FutureExt;
    for after in [false, true] {
        let destinations = gated(Vec::new());
        let release_older = destinations.read.hold();
        let mut older = Box::pin(destinations.home("c1"));
        assert!(older.as_mut().now_or_never().is_none());
        let _release_newer = destinations.read.hold();
        let mut newer = Box::pin(destinations.home("c1"));
        assert!(newer.as_mut().now_or_never().is_none());
        if after {
            release_older.send(Ok(Vec::new())).unwrap();
            block_on(older).unwrap();
            drop(newer);
        } else {
            drop(newer);
            release_older.send(Ok(Vec::new())).unwrap();
            block_on(older).unwrap();
        }
        assert_eq!(status(&destinations.peek_home("c1")), "none");
    }
}

#[test]
fn homes_resolve_one_surviving_project_and_fail_closed() {
    let world = World::new();
    let address = world.address();
    let repo = world.event(
        REPOSITORY,
        &[&["d", "repo"], &["buzz-channel", "c1"]],
        "",
        1,
    );
    let project = |d: &str| {
        world.event(
            PROJECT,
            &[&["d", d], &["a", &address], &["buzz-channel", "c1"]],
            "",
            1,
        )
    };
    let home = block_on(fake(vec![repo.clone(), project("one")]).home("c1")).unwrap();
    assert!(matches!(home, Home::Project(ref p) if p.dtag == "one"));
    assert!(matches!(
        block_on(fake(vec![repo.clone()]).home("c1")).unwrap(),
        Home::None
    ));
    assert!(matches!(
        block_on(fake(vec![repo.clone(), project("one"), project("two")]).home("c1")).unwrap(),
        Home::Ambiguous
    ));
    let gone = format!("30621:{}:two", world.owner_hex());
    let removal = world.event(DELETION, &[&["a", &gone]], "", 2);
    assert!(matches!(
        block_on(fake(vec![repo, project("one"), project("two"), removal]).home("c1")).unwrap(),
        Home::Project(_)
    ));
}
