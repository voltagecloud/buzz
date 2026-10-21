//! Projects discovery and entity reads against the local test relay, with
//! throwaway NIP-34 events. Skipped unless `BUZZ_TEST_RELAY` points at
//! `.test-relay`.

use buzz_identity::Identity;
use buzz_relay_client::projects::{
    EntityFailure, EntityKind, EntityRoute, GitRead, Home, ItemKind,
};
use buzz_relay_client::*;
use nostr::prelude::{Event, EventBuilder, FinalizeEvent, Keys, Kind, Tag, Timestamp};
use serde_json::Value;
use std::sync::Arc;

struct Fixture {
    origin: RelayOrigin,
    identities: Value,
    seed: Value,
}

fn fixture() -> Option<Fixture> {
    let dir = std::env::var("BUZZ_TEST_RELAY").ok()?;
    let read = |name: &str| -> Value {
        serde_json::from_str(&std::fs::read_to_string(format!("{dir}/{name}")).unwrap()).unwrap()
    };
    let identities = read("identities.json");
    Some(Fixture {
        origin: RelayOrigin::parse(identities["relay_ws_url"].as_str().unwrap()).unwrap(),
        seed: read("seed.json"),
        identities,
    })
}

impl Fixture {
    fn nsec(&self, name: &str) -> &str {
        self.identities["identities"][name]["nsec"]
            .as_str()
            .unwrap()
    }

    fn keys(&self, name: &str) -> Keys {
        Keys::parse(self.nsec(name)).unwrap()
    }

    fn identity(&self, name: &str) -> Arc<Identity> {
        Arc::new(Identity::from_nsec(self.nsec(name)).unwrap())
    }

    fn hex(&self, name: &str) -> String {
        self.keys(name).public_key().to_hex()
    }

    /// Signs and publishes as `name`.
    async fn publish(&self, name: &str, kind: u16, content: &str, tags: &[&[&str]]) -> Event {
        let tags = tags
            .iter()
            .map(|tag| Tag::parse(tag.iter().map(|s| s.to_string())).unwrap());
        let event = EventBuilder::new(Kind::from_u16(kind), content)
            .tags(tags)
            .custom_created_at(Timestamp::now())
            .finalize(&self.keys(name))
            .unwrap();
        let reply = RelayHttp::new(self.origin.clone(), self.identity(name))
            .post("/events", serde_json::to_string(&event).unwrap())
            .await
            .unwrap_or_else(|error| panic!("kind {kind} was refused: {error:?}"));
        let reply: Value = serde_json::from_slice(&reply).unwrap();
        assert_eq!(reply["accepted"], true, "kind {kind}: {reply}");
        event
    }
}

fn unique(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{prefix}-{nanos}")
}

#[tokio::test(flavor = "multi_thread")]
async fn discovers_and_loads_repositories_projects_and_items() {
    let Some(fixture) = fixture() else {
        eprintln!("BUZZ_TEST_RELAY not set; skipping");
        return;
    };
    let general = fixture.seed["channels"]["general"]
        .as_str()
        .unwrap()
        .to_string();
    let alice = fixture.hex("alice");
    let repo_d = unique("gpui-test-repo");
    let project_d = unique("gpui-test-project");
    let address = format!("30617:{alice}:{repo_d}");
    fixture
        .publish(
            "alice",
            30617,
            "",
            &[
                &["d", &repo_d],
                &["name", "Throwaway repository"],
                &["description", "Seeded by projects_relay.rs"],
                &["buzz-channel", &general],
            ],
        )
        .await;
    fixture
        .publish(
            "alice",
            30621,
            "",
            &[
                &["d", &project_d],
                &["name", "Throwaway project"],
                &["a", &address],
                &["buzz-channel", &general],
            ],
        )
        .await;
    let issue = fixture
        .publish(
            "bob",
            1621,
            "The body",
            &[
                &["a", &address],
                &["p", &alice],
                &["subject", "A throwaway issue"],
            ],
        )
        .await;
    let commit = "1".repeat(40);
    let pr = fixture
        .publish(
            "bob",
            1618,
            "PR body",
            &[
                &["a", &address],
                &["p", &alice],
                &["subject", "A throwaway PR"],
                &["c", &commit],
                &["clone", "https://example.invalid/repo.git"],
            ],
        )
        .await;
    let issue_id = issue.id.to_hex();
    let comment = fixture
        .publish(
            "carol",
            1,
            "A comment",
            &[&["e", &issue_id], &["p", &fixture.hex("bob")]],
        )
        .await;
    fixture
        .publish(
            "alice",
            1632,
            "",
            &[&["e", &issue_id, "", "root"], &["a", &address]],
        )
        .await;

    let session = Session::open(
        &tokio::runtime::Handle::current(),
        fixture.origin.clone(),
        fixture.identity("alice"),
        Store::in_memory().unwrap(),
    );
    let projects = session.projects();

    let list = projects.list().await.unwrap();
    let names: Vec<&str> = list.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"Throwaway repository"), "{names:?}");
    assert!(names.contains(&"Throwaway project"), "{names:?}");

    let repo_route = EntityRoute::entity(EntityKind::Repo, &alice, &repo_d).unwrap();
    let repo = projects.load(repo_route.clone()).await.unwrap();
    assert_eq!(repo.entity.description, "Seeded by projects_relay.rs");
    assert_eq!(repo.channels, std::slice::from_ref(&general));

    let project_route = EntityRoute::entity(EntityKind::Project, &alice, &project_d).unwrap();
    let project = projects.load(project_route.clone()).await.unwrap();
    assert_eq!(project.repositories.len(), 1);
    assert_eq!(project.repositories[0].address, address);
    assert!(project.unavailable_repositories.is_empty());

    let issues = projects
        .load(
            project_route
                .clone()
                .with_tab(Some(projects::EntityTab::Issues)),
        )
        .await
        .unwrap();
    assert_eq!(issues.items.len(), 1);
    assert_eq!(issues.items[0].id, issue.id);
    let prs = projects
        .load(repo_route.clone().with_tab(Some(projects::EntityTab::Prs)))
        .await
        .unwrap();
    assert_eq!(prs.items.len(), 1);
    assert_eq!(prs.items[0].id, pr.id);

    let item = projects
        .load(EntityRoute::item(ItemKind::Issue, &alice, &repo_d, &issue_id).unwrap())
        .await
        .unwrap();
    assert_eq!(item.status, Some("Closed"));
    assert_eq!(item.activity.len(), 1);
    assert_eq!(item.activity[0].id, comment.id);
    let pr_detail = projects
        .load(EntityRoute::item(ItemKind::Pr, &alice, &repo_d, &pr.id.to_hex()).unwrap())
        .await
        .unwrap();
    assert_eq!(pr_detail.commit.as_deref(), Some(commit.as_str()));
    assert_eq!(pr_detail.status, Some("Open"));
    // An issue is not a pull request.
    assert_eq!(
        projects
            .load(EntityRoute::item(ItemKind::Pr, &alice, &repo_d, &issue_id).unwrap())
            .await
            .unwrap_err(),
        EntityFailure::NotFound
    );

    // A channel of its own has exactly this project as its home.
    let home_channel = uuid_like();
    let home_repo = unique("gpui-test-home");
    let home_address = format!("30617:{alice}:{home_repo}");
    fixture
        .publish(
            "alice",
            30617,
            "",
            &[&["d", &home_repo], &["buzz-channel", &home_channel]],
        )
        .await;
    assert!(matches!(
        projects.home(home_channel.clone()).await.unwrap(),
        Home::None
    ));
    let home_project = unique("gpui-test-home-project");
    fixture
        .publish(
            "alice",
            30621,
            "",
            &[
                &["d", &home_project],
                &["a", &home_address],
                &["buzz-channel", &home_channel],
            ],
        )
        .await;
    match projects.home(home_channel).await.unwrap() {
        Home::Project(project) => assert_eq!(project.dtag, home_project),
        other => panic!("expected a home, got {other:?}"),
    }

    // The owner's deletion removes the project.
    fixture
        .publish(
            "alice",
            5,
            "",
            &[&["a", &format!("30621:{alice}:{project_d}")]],
        )
        .await;
    assert_eq!(
        projects.load(project_route).await.unwrap_err(),
        EntityFailure::NotFound
    );
    let list = projects.list().await.unwrap();
    assert!(!list.iter().any(|e| e.dtag == project_d));

    // Git is served over HTTPS only; this relay is plain HTTP.
    assert_eq!(
        projects
            .read_git(GitRead {
                owner: alice.clone(),
                dtag: repo_d.clone(),
                commit: None,
                path: None,
            })
            .await
            .unwrap_err(),
        EntityFailure::Unavailable
    );
    assert!(
        projects
            .git_authorization(&format!("http://127.0.0.1:53000/git/{alice}/{repo_d}"))
            .is_err()
    );

    // Leave nothing announced behind.
    let leftovers = [
        address.clone(),
        home_address.clone(),
        format!("30621:{alice}:{home_project}"),
    ];
    // The relay takes one deletion target per event.
    for address in &leftovers {
        fixture.publish("alice", 5, "", &[&["a", address]]).await;
    }
    let list = projects.list().await.unwrap();
    assert!(!list.iter().any(|e| e.dtag == repo_d || e.dtag == home_repo));
}

/// A UUID-shaped channel id no channel has.
fn uuid_like() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let hex = format!("{nanos:032x}");
    format!(
        "{}-{}-4{}-8{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[13..16],
        &hex[17..20],
        &hex[20..32]
    )
}
