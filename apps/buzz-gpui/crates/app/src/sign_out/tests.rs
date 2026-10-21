//! buzz-app's sign-out tests, on this app's folders. Everything runs in a
//! fresh temporary folder; nothing real is read, locked or deleted.

use super::*;
use std::cell::Cell;

/// A temporary folder, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("buzz-sign-out-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// `root` stands for the user data folder; `cache/buzz-gpui` for the rest.
fn paths_for(root: &Path) -> Paths {
    Paths::under(root, vec![root.join("cache").join(IDENTIFIER)])
}

const CONTROLLER: &str = "buzz-gpui/host/agent-controller";
const AGENTS: &str = "buzz-gpui/host/agents";

fn fixture() -> (Scratch, Paths) {
    let dir = Scratch::new();
    let paths = paths_for(dir.path());
    let controller = dir.path().join(CONTROLLER);
    fs::create_dir_all(&controller).unwrap();
    fs::write(controller.join("agents.json"), "agents").unwrap();
    let agents = dir.path().join(AGENTS);
    fs::create_dir_all(&agents).unwrap();
    fs::write(agents.join("identities.json"), "[]").unwrap();
    fs::write(agents.join("hosts.lock"), "").unwrap();
    fs::create_dir_all(paths.app_data.join("agent-keys")).unwrap();
    fs::write(paths.app_data.join("agent-keys/agent-1"), "key").unwrap();
    fs::write(paths.app_data.join("buzz.sqlite3"), "store").unwrap();
    fs::create_dir_all(paths.app_data.join("archive")).unwrap();
    fs::write(paths.app_data.join("archive/events.sqlite3"), "archive").unwrap();
    fs::create_dir_all(&paths.others[0]).unwrap();
    fs::write(paths.others[0].join("thumbnails"), "cache").unwrap();
    (dir, paths)
}

/// What a launch runs once it owns the locks: the pending sign-out, if any.
fn finish_pending(
    paths: &Paths,
    remove_agent_keys: impl FnOnce(&Path) -> Result<(), String>,
    remove_key: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    let Some(choices) = pending(&paths.marker)? else {
        return Ok(());
    };
    finish(paths, choices, remove_agent_keys, remove_key).map_err(|_| FAILED.to_owned())
}

/// Lets go of a lock another Buzz holds. Unlocked rather than closed: a
/// process another test spawns meanwhile holds a copy of the descriptor
/// until it execs, and the lock lasts until its last copy closes.
fn release(file: File) {
    file.unlock().unwrap();
}

fn no_agents(_: &Path) -> Result<(), String> {
    panic!("agent keys must not be touched")
}

fn mark(paths: &Paths, wipe: bool, remove_agents: bool) {
    write_marker(
        &paths.marker,
        Choices {
            wipe,
            remove_agents,
        },
    )
    .unwrap();
}

/// Everything under `root` but the finished record and the lock files.
fn listing(root: &Path) -> Vec<String> {
    let mut found: Vec<String> = walk(root, root);
    found.retain(|path| !path.ends_with("sign-outs-finished") && !path.ends_with("instance.lock"));
    found.sort();
    found
}

fn walk(root: &Path, dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .unwrap()
        .flat_map(|entry| {
            let path = entry.unwrap().path();
            let name = path.strip_prefix(root).unwrap().display().to_string();
            let meta = fs::symlink_metadata(&path).unwrap();
            if meta.is_dir() {
                let mut nested = walk(root, &path);
                nested.push(format!("{name}/"));
                nested
            } else {
                vec![name]
            }
        })
        .collect()
}

/// What an erase that keeps agents leaves.
const KEPT_AGENTS: [&str; 9] = [
    "buzz-gpui/",
    "buzz-gpui/agent-keys/",
    "buzz-gpui/agent-keys/agent-1",
    "buzz-gpui/host/",
    "buzz-gpui/host/agent-controller/",
    "buzz-gpui/host/agent-controller/agents.json",
    "buzz-gpui/host/agents/",
    "buzz-gpui/host/agents/identities.json",
    "cache/",
];

const PLAIN: Choices = Choices {
    wipe: false,
    remove_agents: false,
};
const WIPE: Choices = Choices {
    wipe: true,
    remove_agents: false,
};

#[test]
fn no_marker_leaves_everything_alone() {
    let (dir, paths) = fixture();
    let before = listing(dir.path());
    assert_eq!(
        finish_pending(&paths, |_| Ok(()), || panic!("key must not be touched")),
        Ok(())
    );
    assert_eq!(listing(dir.path()), before);
}

#[test]
fn sign_out_without_wipe_removes_only_the_key_and_marker() {
    let (dir, paths) = fixture();
    let before = listing(dir.path());
    mark(&paths, false, false);
    let removed = Cell::new(false);
    assert_eq!(
        finish_pending(
            &paths,
            |_| Ok(()),
            || {
                removed.set(true);
                Ok(())
            }
        ),
        Ok(())
    );
    assert!(removed.get());
    assert_eq!(listing(dir.path()), before);
}

thread_local! {
    static FORGOTTEN: Cell<usize> = const { Cell::new(0) };
}

fn forget_hosts() -> Result<(), String> {
    FORGOTTEN.with(|count| count.set(count.get() + 1));
    Ok(())
}

#[test]
fn only_an_erase_forgets_remote_hosts_and_a_failure_keeps_the_marker() {
    for wipe in [false, true] {
        let (_dir, mut paths) = fixture();
        paths.forget_hosts = forget_hosts;
        FORGOTTEN.with(|count| count.set(0));
        mark(&paths, wipe, false);
        assert_eq!(finish_pending(&paths, no_agents, || Ok(())), Ok(()));
        assert_eq!(FORGOTTEN.with(Cell::get), usize::from(wipe));
    }
    let (_dir, mut paths) = fixture();
    paths.forget_hosts = || Err("keyring locked".into());
    mark(&paths, true, false);
    assert_eq!(
        finish_pending(&paths, no_agents, || Ok(())),
        Err(FAILED.to_owned())
    );
    assert!(paths.marker.exists());
}

#[test]
fn wipe_keeps_agents_unless_they_are_removed_too() {
    for remove_agents in [false, true] {
        let (dir, paths) = fixture();
        mark(&paths, true, remove_agents);
        let removed = Cell::new(false);
        assert_eq!(
            finish_pending(
                &paths,
                |host| {
                    assert!(host.join("agent-controller/agents.json").exists());
                    assert!(host.join("agents/identities.json").exists());
                    removed.set(true);
                    Ok(())
                },
                || Ok(())
            ),
            Ok(())
        );
        assert_eq!(removed.get(), remove_agents);
        let expected: &[&str] = if remove_agents {
            &["cache/"]
        } else {
            &KEPT_AGENTS
        };
        assert_eq!(listing(dir.path()), expected);
    }
}

#[test]
fn kept_agents_keep_only_their_list_settings_and_keys() {
    let (dir, paths) = fixture();
    let controller = dir.path().join(CONTROLLER);
    for file in [
        "defaults.json",
        "agents.previous.json",
        "buzz-agent/oauth/databricks/0123abcd.json",
        "logs/agent.log",
        "runs/agent-1/tmp/scratch",
        "something-new",
    ] {
        let path = controller.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "x").unwrap();
    }
    fs::create_dir_all(paths.ownership.clone()).unwrap();
    fs::write(paths.ownership.join("agent.lock"), "").unwrap();
    mark(&paths, true, false);
    assert_eq!(finish_pending(&paths, no_agents, || Ok(())), Ok(()));
    let mut expected: Vec<&str> = KEPT_AGENTS.to_vec();
    expected.push("buzz-gpui/host/agent-controller/defaults.json");
    expected.sort();
    assert_eq!(listing(dir.path()), expected);
    assert_eq!(fs::read(controller.join("agents.json")).unwrap(), b"agents");
}

#[test]
fn key_removal_failure_restores_data_keeps_marker_and_the_retry_succeeds() {
    for remove_agents in [false, true] {
        let (dir, paths) = fixture();
        mark(&paths, true, remove_agents);
        let before = listing(dir.path());
        assert_eq!(
            finish_pending(&paths, |_| Ok(()), || Err("keyring locked".into())),
            Err(FAILED.into())
        );
        // Everything, including the marker, is exactly where it was.
        assert_eq!(listing(dir.path()), before);
        assert!(paths.marker.exists());
        assert_eq!(finish_pending(&paths, |_| Ok(()), || Ok(())), Ok(()));
        assert!(
            !listing(dir.path())
                .iter()
                .any(|path| path.contains("sqlite"))
        );
        assert!(!paths.marker.exists());
    }
}

#[test]
fn a_wipe_interrupted_part_way_resumes_on_the_next_launch() {
    let (dir, paths) = fixture();
    mark(&paths, true, false);
    // Stopped after moving app data aside, before returning what's kept.
    fs::rename(&paths.app_data, trash(&paths.app_data)).unwrap();
    assert_eq!(finish_pending(&paths, |_| Ok(()), || Ok(())), Ok(()));
    assert_eq!(listing(dir.path()), KEPT_AGENTS);
}

#[test]
fn an_interrupted_wipe_rolls_back_fully_when_the_key_cannot_be_removed() {
    let (dir, paths) = fixture();
    mark(&paths, true, false);
    let before = listing(dir.path());
    fs::rename(&paths.app_data, trash(&paths.app_data)).unwrap();
    assert!(finish_pending(&paths, |_| Ok(()), || Err("locked".into())).is_err());
    assert_eq!(listing(dir.path()), before);
}

#[test]
fn an_unreadable_marker_blocks_without_touching_the_key_or_data() {
    let (dir, paths) = fixture();
    fs::write(&paths.marker, "{\"wipe\":true}").unwrap();
    let before = listing(dir.path());
    assert!(finish_pending(&paths, |_| Ok(()), || panic!("key must not be touched")).is_err());
    assert_eq!(listing(dir.path()), before);
    // Boot refuses the same way, taking every lock first.
    assert_eq!(
        boot_with(&paths, no_agents, || panic!("key must not be touched")).err(),
        Some(FAILED.into())
    );
}

#[test]
fn a_retry_clears_storage_recreated_after_the_key_was_removed() {
    let (dir, paths) = fixture();
    mark(&paths, true, false);
    // Trash cleanup fails once (a file where its folder should be).
    fs::write(trash(&paths.others[0]), "stuck").unwrap();
    assert!(finish_pending(&paths, |_| Ok(()), || Ok(())).is_err());
    assert!(paths.marker.exists());
    // Something recreates storage before the next launch.
    fs::create_dir_all(&paths.others[0]).unwrap();
    fs::write(paths.others[0].join("recreated"), "new").unwrap();
    fs::write(paths.app_data.join("recreated.sqlite3"), "new").unwrap();
    fs::remove_file(trash(&paths.others[0])).unwrap();
    // The key is already gone; the retry must still succeed and leave nothing.
    assert_eq!(finish_pending(&paths, |_| Ok(()), || Ok(())), Ok(()));
    assert_eq!(listing(dir.path()), KEPT_AGENTS);
}

#[test]
fn instances_share_the_lock_and_sign_out_needs_it_alone() {
    let (_dir, paths) = fixture();
    let first = boot_with(&paths, no_agents, || panic!("no marker")).unwrap();
    let second = boot_with(&paths, no_agents, || panic!("no marker")).unwrap();
    assert_eq!(first.begin(WIPE), Err(refuse(OTHERS)));
    assert_eq!(first.begin(PLAIN), Err(refuse(OTHERS)));
    drop(second);
    assert_eq!(first.begin(WIPE), Ok(()));
    // While one instance signs out, a new launch neither runs nor waits
    // forever.
    assert_eq!(
        boot_with(&paths, no_agents, || panic!("no marker")).err(),
        Some(SIGNING_OUT.into())
    );
    assert_eq!(first.abort(WIPE, refuse("withdrawn")), refuse("withdrawn"));
    assert!(boot_with(&paths, no_agents, || panic!("no marker")).is_ok());
}

#[test]
fn a_running_copy_with_the_same_key_refuses_a_plain_sign_out() {
    let (_dir, paths) = fixture();
    // buzz-app holds the shared key lock too, under the same name.
    let other = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&paths.key_lock)
        .unwrap();
    other.lock_shared().unwrap();
    let instance = boot_with(&paths, no_agents, || panic!("no marker")).unwrap();
    assert_eq!(instance.begin(PLAIN), Err(refuse(OTHERS)));
    // A pending plain sign-out waits for that copy too.
    drop(instance);
    mark(&paths, false, false);
    assert_eq!(
        boot_with(&paths, no_agents, || panic!("the key stays")).err(),
        Some(BUSY.into())
    );
    release(other);
    assert!(boot_with(&paths, no_agents, || Ok(())).is_ok());
    assert!(!paths.marker.exists());
}

#[test]
fn an_erase_is_refused_while_any_copy_runs() {
    let (_dir, paths) = fixture();
    // Another Buzz with a different key holds only the all-Buzz lock.
    let other = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&paths.lock)
        .unwrap();
    other.lock_shared().unwrap();
    let instance = boot_with(&paths, no_agents, || panic!("no marker")).unwrap();
    assert_eq!(instance.begin(WIPE), Err(refuse(OTHERS_ALL)));
    // A plain sign-out only needs copies using this key closed.
    assert_eq!(instance.begin(PLAIN), Ok(()));
    drop(instance);
    mark(&paths, true, false);
    assert_eq!(
        boot_with(&paths, no_agents, || panic!("the key stays")).err(),
        Some(BUSY_ALL.into())
    );
    release(other);
}

#[test]
fn a_second_sign_out_in_the_same_instance_is_refused() {
    let (_dir, paths) = fixture();
    let instance = boot_with(&paths, no_agents, || panic!("no marker")).unwrap();
    instance.begin(WIPE).unwrap();
    assert_eq!(instance.begin(WIPE), Err(refuse(ALREADY)));
}

#[test]
fn a_pending_sign_out_waits_for_the_restarting_instance_to_exit() {
    let (dir, paths) = fixture();
    let exiting = boot_with(&paths, no_agents, || panic!("no marker")).unwrap();
    exiting.begin(WIPE).unwrap();
    mark(&paths, false, false);
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(30));
        drop(exiting);
    });
    let before = listing(dir.path());
    let removed = Cell::new(false);
    assert!(
        boot_with(&paths, no_agents, || {
            removed.set(true);
            Ok(())
        })
        .is_ok()
    );
    release.join().unwrap();
    assert!(removed.get());
    assert!(!paths.marker.exists());
    assert_eq!(
        listing(dir.path()),
        before
            .into_iter()
            .filter(|path| !path.ends_with("sign-out-pending"))
            .collect::<Vec<_>>()
    );
}

#[test]
fn a_first_launch_creates_a_missing_user_data_folder() {
    let dir = Scratch::new();
    let paths = paths_for(&dir.path().join("missing/share"));
    assert!(boot_with(&paths, no_agents, || panic!("no marker")).is_ok());
    assert!(paths.lock.is_file() && paths.key_lock.is_file());
}

#[test]
fn locks_and_records_are_named_like_buzz_app() {
    let dir = Scratch::new();
    let paths = paths_for(dir.path());
    let name = |path: &Path| path.file_name().unwrap().to_str().unwrap().to_owned();
    assert_eq!(
        name(&paths.lock),
        ".dev.local.buzz.foundation.instance.lock"
    );
    assert_eq!(
        name(&paths.key_lock),
        ".dev.local.buzz.foundation.identity.instance.lock"
    );
    assert_eq!(
        name(&paths.finished),
        ".dev.local.buzz.foundation.identity.sign-outs-finished"
    );
    // The marker is this app's own, outside its data folder.
    assert_eq!(
        name(&paths.marker),
        ".buzz-gpui--dev.local.buzz.foundation.identity.sign-out-pending"
    );
    assert_eq!(paths.marker.parent(), Some(dir.path()));
}

#[test]
fn a_finished_record_from_another_app_fences_a_refused_sign_out() {
    let (_dir, paths) = fixture();
    let instance = boot_with(&paths, no_agents, || panic!("no marker")).unwrap();
    // buzz-app finished a sign-out of the shared key while this instance's
    // locks were let go.
    fs::write(&paths.finished, ".").unwrap();
    let other = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&paths.key_lock)
        .unwrap();
    other.lock_shared().unwrap();
    assert_eq!(instance.begin(PLAIN), Err(fenced()));
}

#[test]
fn refusals_match_the_build() {
    assert_eq!(refusal(true, false, false), None);
    assert_eq!(refusal(true, true, false), Some(DEV_WIPE));
    assert_eq!(refusal(true, true, true), Some(DEV_WIPE));
    assert_eq!(refusal(false, true, true), None);
    assert_eq!(refusal(false, true, false), None);
    assert!(
        refusal(false, false, true)
            .unwrap()
            .contains("part of erasing")
    );
}

#[test]
fn a_pending_erase_waits_for_every_agent_supervisor_to_let_go() {
    let (dir, paths) = fixture();
    mark(&paths, true, true);
    fs::create_dir_all(&paths.ownership).unwrap();
    let held = File::create(paths.ownership.join("agent-community.lock")).unwrap();
    held.lock().unwrap();
    let before = listing(dir.path());
    let refused = boot_with(
        &paths,
        |_| panic!("agent keys must stay"),
        || panic!("the key must stay"),
    );
    assert_eq!(refused.err(), Some(AGENT_STOPPING.to_owned()));
    assert_eq!(listing(dir.path()), before);
    release(held);
    let removed = Cell::new(0);
    let finished = boot_with(
        &paths,
        |_| {
            removed.set(removed.get() + 1);
            Ok(())
        },
        || {
            removed.set(removed.get() + 1);
            Ok(())
        },
    );
    assert!(finished.is_ok());
    assert_eq!(removed.get(), 2);
    assert!(!paths.marker.exists());
    assert_eq!(listing(dir.path()), ["cache/"]);
}

#[test]
fn a_plain_sign_out_does_not_wait_for_agent_supervisors() {
    let (_dir, paths) = fixture();
    mark(&paths, false, false);
    fs::create_dir_all(&paths.ownership).unwrap();
    let held = File::create(paths.ownership.join("agent-community.lock")).unwrap();
    held.lock().unwrap();
    assert!(boot_with(&paths, no_agents, || Ok(())).is_ok());
    assert!(!paths.marker.exists());
}

#[test]
fn agent_keys_go_first_and_an_interruption_retries_without_moving_anything() {
    let (dir, paths) = fixture();
    mark(&paths, true, true);
    let before = listing(dir.path());
    assert_eq!(
        finish_pending(
            &paths,
            |_| Err("keyring interrupted".into()),
            || panic!("the human key waits for the agent keys")
        ),
        Err(FAILED.into())
    );
    assert_eq!(listing(dir.path()), before);
}

#[test]
fn committing_writes_the_marker_or_withdraws() {
    let (dir, paths) = fixture();
    let instance = boot_with(&paths, no_agents, || panic!("no marker")).unwrap();
    assert_eq!(commit(&instance, &paths, WIPE), Ok(()));
    assert_eq!(pending(&paths.marker), Ok(Some(WIPE)));
    assert_eq!(commit(&instance, &paths, WIPE), Err(refuse(ALREADY)));
    drop(instance);
    // A marker that can't be written withdraws and gives the locks back.
    let (_other, mut broken) = fixture();
    broken.marker = dir.path().join("missing-folder/marker");
    let instance = boot_with(&broken, no_agents, || panic!("no marker")).unwrap();
    assert_eq!(commit(&instance, &broken, PLAIN), Err(refuse(NOT_PREPARED)));
    assert_eq!(commit(&instance, &broken, PLAIN), Err(refuse(NOT_PREPARED)));
}

#[cfg(unix)]
mod links {
    use super::*;
    use std::os::unix::fs::symlink;

    /// An outside folder holding `files`, for links to point at.
    fn outside(dir: &Path, files: &[&str]) -> PathBuf {
        let outside = dir.join("outside");
        for file in files {
            let path = outside.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, file.as_bytes()).unwrap();
        }
        outside
    }

    fn unchanged(outside: &Path, files: &[&str]) {
        for file in files {
            assert_eq!(fs::read(outside.join(file)).unwrap(), file.as_bytes());
        }
    }

    #[test]
    fn a_linked_kept_registry_stops_the_wipe_and_deletes_nothing_through_it() {
        let (dir, paths) = fixture();
        let controller = dir.path().join(CONTROLLER);
        let files = ["agents.json", "defaults.json", "sentinel"];
        let outside = outside(dir.path(), &files);
        fs::remove_dir_all(&controller).unwrap();
        symlink(&outside, &controller).unwrap();
        mark(&paths, true, false);
        let result = finish_pending(&paths, no_agents, || panic!("the human key stays"));
        assert_eq!(result, Err(FAILED.to_owned()));
        assert!(paths.marker.exists());
        unchanged(&outside, &files);
        // A real list in its place lets the retry finish.
        fs::remove_file(&controller).unwrap();
        fs::create_dir(&controller).unwrap();
        fs::write(controller.join("agents.json"), "agents").unwrap();
        assert_eq!(finish_pending(&paths, no_agents, || Ok(())), Ok(()));
        assert!(!paths.marker.exists());
        assert_eq!(fs::read(controller.join("agents.json")).unwrap(), b"agents");
        unchanged(&outside, &files);
    }

    #[test]
    fn a_linked_host_folder_stops_removing_agents() {
        let (dir, paths) = fixture();
        let files = ["agent-controller/agents.json", "sentinel"];
        let outside = outside(dir.path(), &files);
        let host = paths.app_data.join("host");
        fs::remove_dir_all(&host).unwrap();
        symlink(&outside, &host).unwrap();
        mark(&paths, true, true);
        let result = finish_pending(&paths, no_agents, || panic!("the human key stays"));
        assert_eq!(result, Err(FAILED.to_owned()));
        unchanged(&outside, &files);
    }

    #[test]
    fn a_linked_wipe_folder_or_trash_stops_the_wipe() {
        for linked in [
            "cache/buzz-gpui",
            "cache/buzz-gpui.sign-out-trash",
            "buzz-gpui.sign-out-trash",
            "buzz-gpui",
        ] {
            let (dir, paths) = fixture();
            let outside = outside(dir.path(), &["sentinel"]);
            let link = dir.path().join(linked);
            let _ = fs::remove_dir_all(&link);
            symlink(&outside, &link).unwrap();
            mark(&paths, true, false);
            let result = finish_pending(&paths, no_agents, || panic!("the human key stays"));
            assert_eq!(result, Err(FAILED.to_owned()), "{linked}");
            assert!(paths.marker.exists());
            unchanged(&outside, &["sentinel"]);
        }
    }

    #[test]
    fn a_linked_wipe_folder_refuses_before_any_marker() {
        let (dir, paths) = fixture();
        let outside = outside(dir.path(), &["sentinel"]);
        fs::remove_dir_all(&paths.others[0]).unwrap();
        symlink(&outside, &paths.others[0]).unwrap();
        let instance = boot_with(&paths, no_agents, || panic!("no marker")).unwrap();
        assert_eq!(commit(&instance, &paths, WIPE), Err(refuse(LINKED)));
        assert!(!paths.marker.exists());
        // Withdrawn: the plain sign-out is still admitted.
        assert_eq!(commit(&instance, &paths, PLAIN), Ok(()));
        unchanged(&outside, &["sentinel"]);
    }

    #[test]
    fn links_inside_erased_folders_are_removed_not_followed() {
        let (dir, paths) = fixture();
        let files = ["sentinel", "deep/sentinel"];
        let outside = outside(dir.path(), &files);
        symlink(&outside, paths.app_data.join("linked-folder")).unwrap();
        symlink(outside.join("sentinel"), paths.app_data.join("linked-file")).unwrap();
        symlink(&outside, paths.others[0].join("linked")).unwrap();
        // Inside a kept folder, a link that is not itself kept goes too.
        symlink(&outside, dir.path().join(CONTROLLER).join("linked")).unwrap();
        mark(&paths, true, false);
        assert_eq!(finish_pending(&paths, no_agents, || Ok(())), Ok(()));
        unchanged(&outside, &files);
        let left = listing(dir.path());
        assert!(!left.iter().any(|path| path.contains("linked")), "{left:?}");
    }

    #[test]
    fn a_linked_storage_parent_is_trusted_and_only_the_buzz_folder_under_it_goes() {
        let (dir, mut paths) = fixture();
        let files = ["sentinel", "buzz-gpui/thumbnails"];
        let outside = outside(dir.path(), &files);
        let parent = dir.path().join("linked-cache");
        symlink(&outside, &parent).unwrap();
        paths.others = vec![parent.join(IDENTIFIER)];
        mark(&paths, true, false);
        assert_eq!(finish_pending(&paths, no_agents, || Ok(())), Ok(()));
        assert!(fs::symlink_metadata(&parent).unwrap().is_symlink());
        assert!(!outside.join(IDENTIFIER).exists());
        unchanged(&outside, &["sentinel"]);
    }

    #[test]
    fn rollback_never_moves_through_an_app_data_swapped_for_a_link() {
        let (dir, paths) = fixture();
        let files = ["host/agent-controller/sentinel"];
        let outside = outside(dir.path(), &files);
        mark(&paths, true, false);
        let result = finish_pending(&paths, no_agents, || {
            fs::rename(&paths.app_data, dir.path().join("displaced")).unwrap();
            symlink(&outside, &paths.app_data).unwrap();
            Err("keyring busy".into())
        });
        assert_eq!(result, Err(FAILED.to_owned()));
        assert!(paths.marker.exists());
        unchanged(&outside, &files);
    }

    #[test]
    fn an_unreadable_data_folder_moves_nothing_and_the_retry_finishes() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, paths) = fixture();
        mark(&paths, true, true);
        let before = listing(dir.path());
        let mode =
            |mode| fs::set_permissions(&paths.app_data, fs::Permissions::from_mode(mode)).unwrap();
        mode(0o000);
        let result = finish_pending(&paths, no_agents, || panic!("the human key stays"));
        mode(0o755);
        // Root reads anything, so only an ordinary user sees the refusal.
        if result.is_err() {
            assert_eq!(listing(dir.path()), before);
        }
        let removed = Cell::new(false);
        let _ = finish_pending(
            &paths,
            |_| {
                removed.set(true);
                Ok(())
            },
            || Ok(()),
        );
        assert!(!paths.marker.exists());
        assert_eq!(listing(dir.path()), ["cache/"]);
    }
}

#[test]
fn launch_moves_the_agents2_folder_unless_another_copy_runs() {
    let (_dir, paths) = fixture();
    let host = paths.app_data.join("host");
    fs::rename(host.join("agents"), host.join("agents2")).unwrap();
    // A running copy may still write the old folder: nothing moves.
    let other = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&paths.lock)
        .unwrap();
    other.lock_shared().unwrap();
    assert_eq!(
        boot_with(&paths, no_agents, || panic!("no marker")).err(),
        Some(
            "Close other Buzz copies, then reopen Buzz to migrate agent storage. Nothing was \
             moved."
                .into()
        )
    );
    assert!(host.join("agents2/identities.json").exists());
    release(other);
    let instance = boot_with(&paths, no_agents, || panic!("no marker")).unwrap();
    assert!(host.join("agents/identities.json").exists());
    assert!(!host.join("agents2").exists());
    // The instance keeps only its shared lock, so another copy still starts.
    assert!(boot_with(&paths, no_agents, || panic!("no marker")).is_ok());
    drop(instance);
}
