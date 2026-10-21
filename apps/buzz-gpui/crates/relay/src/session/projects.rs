//! Projects for one session (`session.projects`, `session.projectGit`):
//! discovery and entity reads through the session's verified reader, and
//! repository content through buzz-desktop's `project_git`, which buzz-app
//! calls as `relay_project_git`.

use super::Session;
use crate::event::Verified;
use crate::origin::RelayOrigin;
use crate::projects::{
    Destinations, Entity, EntityDetail, EntityFailure, EntityRoute, GitRead, GitSnapshot, Home,
    Homes, ProjectReads, community_git_repository, git_response,
};
use crate::reader::{ReadError, ReadOptions, Reader};
use crate::socket::Filter;
use base64::Engine;
use buzz_desktop::relay::project_git;
use buzz_desktop::{EventTemplate, Signer};
use buzz_identity::Identity;
use std::future::Future;
use std::sync::Arc;

impl ProjectReads for Reader {
    fn read(
        &self,
        filters: Vec<Filter>,
    ) -> impl Future<Output = Result<Vec<Verified>, ReadError>> + Send {
        let reader = self.clone();
        async move { Ok(reader.read(filters, ReadOptions::default()).await?.to_vec()) }
    }
}

impl Session {
    /// The session's Projects reads.
    pub fn projects(&self) -> Projects {
        Projects {
            reader: self.direct.reader.clone(),
            identity: self.direct.identity.clone(),
            origin: self.origin.clone(),
            runtime: self.runtime.clone(),
            homes: self.homes.clone(),
        }
    }
}

/// Projects reads bound to one session's viewer and community.
#[derive(Clone)]
pub struct Projects {
    reader: Reader,
    identity: Arc<Identity>,
    origin: RelayOrigin,
    runtime: tokio::runtime::Handle,
    homes: Homes,
}

/// A task on the I/O runtime that stops when its awaiting future is
/// dropped, as an aborted signal stops buzz-app's reads.
struct Spawned<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for Spawned<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Projects {
    async fn run<T: Send + 'static>(
        &self,
        work: impl Future<Output = Result<T, EntityFailure>> + Send + 'static,
    ) -> Result<T, EntityFailure> {
        let mut task = Spawned(self.runtime.spawn(work));
        (&mut task.0)
            .await
            .unwrap_or(Err(EntityFailure::Unavailable))
    }

    fn destinations(&self) -> Destinations<Reader> {
        Destinations::with_homes(self.reader.clone(), self.homes.clone())
    }

    /// The recent directory of repositories and projects.
    pub async fn list(&self) -> Result<Vec<Entity>, EntityFailure> {
        let destinations = self.destinations();
        self.run(async move { destinations.list().await }).await
    }

    /// Everything `route` shows, except repository content.
    pub async fn load(&self, route: EntityRoute) -> Result<EntityDetail, EntityFailure> {
        let destinations = self.destinations();
        self.run(async move { destinations.load(&route).await })
            .await
    }

    /// The last completed answer of [`Self::home`] for `channel` in this
    /// session, so a new picker can keep its button steady while it
    /// checks again.
    pub fn peek_home(&self, channel: &str) -> Option<Home> {
        self.homes.peek(channel)
    }

    /// The channel's project, for the composer's resource picker.
    pub async fn home(&self, channel: String) -> Result<Home, EntityFailure> {
        let destinations = self.destinations();
        self.run(async move { destinations.home(&channel).await })
            .await
    }

    /// The community's HTTPS origin with its root path, as buzz-desktop
    /// takes it.
    fn community(&self) -> String {
        format!("{}/", self.origin.http())
    }

    /// Reads repository content with system Git into a throwaway bare
    /// repository. Only HTTPS communities serve Git. Dropping the future
    /// cancels the read; Git is still stopped and reaped in the background.
    pub async fn read_git(&self, read: GitRead) -> Result<GitSnapshot, EntityFailure> {
        let read = read.parse().ok_or(EntityFailure::Unavailable)?;
        let signer = GitSigner {
            identity: self.identity.clone(),
            origin: self.origin.http(),
        };
        let community = self.community();
        let id = uuid::Uuid::new_v4().to_string();
        let mut cancel = CancelGit(Some(id.clone()));
        let request = project_git::GitRead {
            owner: read.owner.clone(),
            dtag: read.dtag.clone(),
            commit: read.commit.clone(),
            path: read.path.clone(),
        };
        let response = self
            .runtime
            .spawn(async move { project_git::read(&signer, &community, &id, request).await })
            .await;
        cancel.0 = None;
        match response {
            Ok(Ok(response)) => git_response(&read, response.status, &response.body),
            Ok(Err(error)) => {
                tracing::debug!(%error, "repository read refused");
                Err(EntityFailure::Unavailable)
            }
            Err(_) => Err(EntityFailure::Unavailable),
        }
    }

    /// `authorizeGit`: the repository's canonical URL and its token
    /// ([`Self::git_authorization`]), `None` for a repository this
    /// community does not serve, or an error when signing fails.
    pub fn authorize_git(&self, repository: &str) -> Result<Option<(String, String)>, String> {
        let Some(canonical) = community_git_repository(&self.origin.http(), repository) else {
            return Ok(None);
        };
        let token = self.git_authorization(&canonical)?;
        Ok(Some((canonical, token)))
    }

    /// `relay_git_authorization`: the base64 NIP-98 token, after
    /// `Authorization: Nostr `, for one repository of this community. The
    /// relay accepts it for 60 seconds on every Git route of that
    /// repository, so one clone reuses it.
    pub fn git_authorization(&self, repository: &str) -> Result<String, String> {
        let canonical = community_git_repository(&self.origin.http(), repository)
            .filter(|url| url == repository)
            .ok_or("Not a repository in this community")?;
        let event = self
            .identity
            .git_auth(&canonical)
            .map_err(|error| error.to_string())?;
        let json = serde_json::to_vec(&event)
            .map_err(|_| "Could not encode repository authorization".to_string())?;
        Ok(base64::engine::general_purpose::STANDARD.encode(json))
    }
}

/// Cancels a Git read whose caller went away before it finished.
struct CancelGit(Option<String>);

impl Drop for CancelGit {
    fn drop(&mut self) {
        if let Some(id) = self.0.take() {
            let _ = project_git::cancel(&id);
        }
    }
}

/// The signer buzz-desktop's Git read asks for its NIP-98 header. It signs
/// only a `GET` of a repository on this community, through the identity's
/// purpose-bound Git authorization.
pub struct GitSigner {
    identity: Arc<Identity>,
    /// The community's HTTPS origin.
    origin: String,
}

impl GitSigner {
    fn check(&self, event: &EventTemplate) -> Result<serde_json::Value, String> {
        let refused = || "Only repository reads can be signed here".to_string();
        let [u, method] = event.tags.as_slice() else {
            return Err(refused());
        };
        let url = match u.as_slice() {
            [name, url] if name == "u" => url,
            _ => return Err(refused()),
        };
        if event.kind != 27235
            || !event.content.is_empty()
            || method.as_slice() != ["method", "GET"]
            || community_git_repository(&self.origin, url).as_ref() != Some(url)
        {
            return Err(refused());
        }
        let signed = self
            .identity
            .git_auth(url)
            .map_err(|error| error.to_string())?;
        serde_json::to_value(&signed).map_err(|_| "Could not encode relay authentication".into())
    }
}

impl Signer for GitSigner {
    fn sign(
        &self,
        event: EventTemplate,
    ) -> impl Future<Output = Result<serde_json::Value, String>> + Send {
        std::future::ready(self.check(&event))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn template(kind: u16, tags: &[&[&str]], content: &str) -> EventTemplate {
        EventTemplate {
            created_at: 1,
            kind,
            tags: tags
                .iter()
                .map(|t| t.iter().map(|s| s.to_string()).collect())
                .collect(),
            content: content.into(),
        }
    }

    #[test]
    fn the_git_signer_signs_only_this_communitys_repository_reads() {
        let signer = GitSigner {
            identity: Arc::new(Identity::ephemeral()),
            origin: "https://relay.example.com".into(),
        };
        let owner = "b".repeat(64);
        let url = format!("https://relay.example.com/git/{owner}/demo.git");
        let signed = signer
            .check(&template(27235, &[&["u", &url], &["method", "GET"]], ""))
            .unwrap();
        assert_eq!(signed["kind"], 27235);
        assert_eq!(signed["tags"][0][1], url.as_str());
        let other = format!("https://other.example.com/git/{owner}/demo.git");
        for bad in [
            template(27235, &[&["u", &other], &["method", "GET"]], ""),
            template(27235, &[&["u", &url], &["method", "POST"]], ""),
            template(27235, &[&["u", &url], &["method", "GET"]], "x"),
            template(27235, &[&["u", &url]], ""),
            template(
                27235,
                &[&["u", &url], &["method", "GET"], &["payload", "x"]],
                "",
            ),
            template(9, &[&["u", &url], &["method", "GET"]], ""),
            template(
                27235,
                &[
                    &["u", "https://relay.example.com/query"],
                    &["method", "GET"],
                ],
                "",
            ),
        ] {
            assert!(signer.check(&bad).is_err());
        }
    }
}
