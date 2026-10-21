//! Custody for Agents2 agents, a port of buzz-app's `src-tauri/src/app_agents.rs`.
//!
//! The key and owner attestation of each agent stay on this host. The GUI
//! asks for one bounded event, read, upload or memory write at a time, and
//! the host signs it as the agent and sends it to the agent's community.
//! Identities live in their own file under the host's state directory, with
//! keys in the host's credential store. Identities earlier agent systems
//! saved on the host can be taken over with their keys.

mod hosting;
mod media;
mod pictures;

use crate::api::{
    AppAgentEvent, AppAgentRequest, AppAgentResponse, AppAgentSummary, Carry, HostProtocol,
    PortSource, PortableAgent, PortableAgents,
};
use crate::blocking;
use crate::sandbox::{AgentCustody, LaunchIdentity};
use base64::Engine;
use buzz_agent_controller::{
    AppAgent, AppAgents, CREDENTIALS_BUSY, Credentials, NewAppAgent, OldStores, Published, Secret,
};
use buzz_host_rpc::Peer;
use futures::future::BoxFuture;
use hosting::{Holder, Hosting};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

type Result<T> = std::result::Result<T, String>;

const BUSY_WAITS: u32 = 50;
const BUSY_WAIT: Duration = Duration::from_millis(100);
/// Creates prepared but not yet committed; older ones are dropped, with
/// their keys.
const PENDING_CREATES: usize = 8;
const STAGED_UPLOADS: usize = 4;
const STAGED_LIFETIME: Duration = Duration::from_secs(300);

/// Media an agent may upload, as the community accepts them.
const UPLOAD_TYPES: [&str; 5] = [
    "image/jpeg",
    "image/png",
    "image/gif",
    "image/webp",
    "video/mp4",
];
const UPLOAD_LIMIT: usize = 50 * 1024 * 1024;
/// The base64 length of the largest upload.
const UPLOAD_TEXT_LIMIT: usize = UPLOAD_LIMIT / 3 * 4 + 4;
const UPLOAD_REFUSED: &str = "Agents upload images or MP4 video up to 50 MB";

struct Creating {
    request_id: String,
    destination: String,
    owner: String,
    agent_type: String,
    name: String,
    prepared: NewAppAgent,
    /// What a brought-over agent kept, for its new type.
    carry: Carry,
}

struct Staged {
    id: String,
    data: String,
    started: Instant,
}

/// The saved identities, a key cache and the per-agent profile turns. Cheap
/// to clone; clones share everything.
#[derive(Clone)]
pub(crate) struct AppAgentHost {
    agents: AppAgents,
    /// Earlier agent systems' identities on this host, which it can take
    /// over.
    old: OldStores,
    credentials: Arc<dyn Credentials>,
    /// Each key after its first read, so only create and first use reach the
    /// credential store, whose lock refuses rather than waits.
    keys: Arc<Mutex<HashMap<String, Arc<Secret>>>>,
    /// One profile publication per agent at a time, so each is newer than
    /// the last. Delete takes it too, so no publication outlives the key.
    profiles: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    creating: Arc<Mutex<VecDeque<Creating>>>,
    staged: Arc<Mutex<Vec<Staged>>>,
    /// Which connection runs each agent, and the connections watched for
    /// closing.
    hosting: Arc<Hosting<Peer<HostProtocol>>>,
    watched: Arc<Mutex<Vec<Peer<HostProtocol>>>>,
}

impl Holder for Peer<HostProtocol> {
    fn same(&self, other: &Self) -> bool {
        self.same_connection(other)
    }

    fn alive(&self) -> bool {
        !self.is_closed()
    }
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn summary(agent: AppAgent) -> AppAgentSummary {
    AppAgentSummary {
        pubkey: agent.pubkey,
        relay: agent.relay,
        owner: agent.owner,
        agent_type: agent.agent_type,
        name: agent.name,
        picture: Some(agent.picture.unwrap_or_default()),
        deleted: agent.deleted,
    }
}

fn source(source: PortSource) -> buzz_agent_controller::PortSource {
    match source {
        PortSource::Harness => buzz_agent_controller::PortSource::Harness,
        PortSource::Installed => buzz_agent_controller::PortSource::Installed,
        PortSource::Development => buzz_agent_controller::PortSource::Development,
    }
}

fn portable(agent: buzz_agent_controller::Portable) -> PortableAgent {
    use buzz_agent_controller::PortSource as Old;
    PortableAgent {
        source: match agent.source {
            Old::Harness => PortSource::Harness,
            Old::Installed => PortSource::Installed,
            Old::Development => PortSource::Development,
        },
        pubkey: agent.pubkey,
        name: agent.name,
        picture: agent.picture,
        relay: agent.relay,
    }
}

impl AppAgentHost {
    /// `path` is the identity file, e.g. `<state>/agents/identities.json`.
    /// Pictures saved beside it before identities held them move in.
    pub(crate) fn new(path: PathBuf, old: OldStores, credentials: Arc<dyn Credentials>) -> Self {
        let hosts = path.with_file_name("hosts");
        let agents = AppAgents::open(path.clone());
        pictures::migrate(&path.with_file_name("pictures.json"), &agents);
        Self {
            agents,
            old,
            credentials,
            keys: Arc::default(),
            profiles: Arc::default(),
            creating: Arc::default(),
            staged: Arc::default(),
            // Dev builds step aside for a packaged app.
            hosting: Arc::new(Hosting::new(hosts, !cfg!(debug_assertions))),
            watched: Arc::default(),
        }
    }

    /// [`AppAgentRequest::Claim`] and [`AppAgentRequest::Release`], which
    /// answer for the asking connection.
    pub(crate) async fn hold(
        &self,
        peer: Peer<HostProtocol>,
        request: AppAgentRequest,
    ) -> Result<AppAgentResponse> {
        match request {
            AppAgentRequest::Claim { pubkey } => {
                self.watch(&peer);
                let hosting = self.hosting.clone();
                blocking(move || hosting.claim(&pubkey, &peer))
                    .await
                    .map(AppAgentResponse::Claimed)
            }
            AppAgentRequest::Release { pubkey } => {
                self.hosting.release(&pubkey, &peer);
                Ok(AppAgentResponse::Done)
            }
            request => self.serve(request).await,
        }
    }

    /// Lets go of what a connection holds once it closes.
    fn watch(&self, peer: &Peer<HostProtocol>) {
        let mut watched = locked(&self.watched);
        if watched.iter().any(|known| known.same_connection(peer)) {
            return;
        }
        watched.push(peer.clone());
        let (peer, host) = (peer.clone(), self.clone());
        tokio::spawn(async move {
            peer.closed().await;
            host.hosting.release_all(&peer);
            locked(&host.watched).retain(|known| !known.same_connection(&peer));
        });
    }

    pub(crate) async fn serve(&self, request: AppAgentRequest) -> Result<AppAgentResponse> {
        use AppAgentRequest as R;
        use AppAgentResponse as A;
        match request {
            R::List => {
                let agents = self.agents.clone();
                blocking(move || Ok(agents.list()?.into_iter().map(summary).collect()))
                    .await
                    .map(A::Agents)
            }
            R::CreatePrepare {
                request_id,
                destination,
                owner,
                agent_type,
                name,
            } => self
                .prepare(request_id, destination, owner, agent_type, name)
                .map(|pubkey| A::Prepared { pubkey }),
            R::CreateCommit { request_id, auth } => {
                self.commit(request_id, auth).await.map(A::Created)
            }
            R::Rename { pubkey, name } => {
                let agents = self.agents.clone();
                blocking(move || agents.rename(&pubkey, &name))
                    .await
                    .map(|()| A::Done)
            }
            R::SetPicture { pubkey, picture } => {
                let agents = self.agents.clone();
                let picture = Some(picture.trim().to_owned()).filter(|url| !url.is_empty());
                blocking(move || agents.set_picture(&pubkey, picture))
                    .await
                    .map(|()| A::Done)
            }
            R::SetType { pubkey, agent_type } => {
                let agents = self.agents.clone();
                blocking(move || agents.set_type(&pubkey, &agent_type))
                    .await
                    .map(|()| A::Done)
            }
            R::Delete { pubkey } => self.delete(pubkey).await.map(|()| A::Done),
            R::Forget { pubkey } => {
                let agents = self.agents.clone();
                blocking(move || agents.forget(&pubkey))
                    .await
                    .map(|()| A::Done)
            }
            R::Publish { pubkey, event } => self.publish(pubkey, event).await.map(A::Event),
            R::PublishProfile { pubkey } => self.publish_profile(pubkey).await.map(|()| A::Done),
            R::Query { pubkey, filters } => self.query(pubkey, filters).await.map(A::Events),
            R::UploadPart { staged, data } => self.stage(staged, data).map(|()| A::Done),
            R::Upload {
                pubkey,
                data,
                mime,
                staged,
            } => self
                .upload(pubkey, data, mime, staged)
                .await
                .map(A::Uploaded),
            R::Remember {
                pubkey,
                slug,
                body,
                after,
            } => self.remember(pubkey, slug, body, after).await.map(A::Event),
            R::Portable => {
                let old = self.old.clone();
                blocking(move || {
                    let found = old.list();
                    Ok(PortableAgents {
                        agents: found.agents.into_iter().map(portable).collect(),
                        warnings: found.warnings,
                    })
                })
                .await
                .map(A::Portable)
            }
            R::PortPrepare {
                request_id,
                source,
                pubkey,
                destination,
                owner,
                agent_type,
            } => self
                .port(request_id, source, pubkey, destination, owner, agent_type)
                .await
                .map(|(pubkey, carry)| A::PortPrepared { pubkey, carry }),
            R::Media { pubkey, url } => {
                let (agent, key) = self.keyed(pubkey).await?;
                media::fetch(&agent, &key, &url).await.map(A::Media)
            }
            R::Claim { .. } | R::Release { .. } => {
                Err("Agents2 claims need a GUI connection".into())
            }
        }
    }

    fn profile_turn(&self, pubkey: &str) -> Arc<tokio::sync::Mutex<()>> {
        locked(&self.profiles)
            .entry(pubkey.into())
            .or_default()
            .clone()
    }

    /// The agent's key; read from the credential store only on first use. A
    /// busy store is waited out for a few seconds, without holding up agents
    /// whose keys are already read.
    async fn key(&self, agent: AppAgent) -> Result<(AppAgent, Arc<Secret>)> {
        let host = self.clone();
        blocking(move || {
            let mut waits = 0;
            loop {
                let mut keys = locked(&host.keys);
                if let Some(key) = keys.get(&agent.pubkey) {
                    return Ok((agent, key.clone()));
                }
                match agent.read_key(host.credentials.as_ref()) {
                    Err(error) if error == CREDENTIALS_BUSY && waits < BUSY_WAITS => {
                        drop(keys);
                        waits += 1;
                        std::thread::sleep(BUSY_WAIT);
                    }
                    read => {
                        let key = Arc::new(read?);
                        keys.insert(agent.pubkey.clone(), key.clone());
                        return Ok((agent, key));
                    }
                }
            }
        })
        .await
    }

    /// The agent, if it still has its key. One deleted elsewhere loses its
    /// cached key.
    async fn agent(&self, pubkey: String) -> Result<AppAgent> {
        let host = self.clone();
        blocking(move || {
            let agent = host.agents.get(&pubkey);
            if agent.is_err() {
                locked(&host.keys).remove(&pubkey);
            }
            agent
        })
        .await
    }

    async fn keyed(&self, pubkey: String) -> Result<(AppAgent, Arc<Secret>)> {
        self.key(self.agent(pubkey).await?).await
    }

    /// As [`Self::keyed`], from the key cache only, so it never waits on
    /// the credential store.
    fn cached(&self, pubkey: &str) -> Result<(AppAgent, Arc<Secret>)> {
        // Held across the read, as delete holds it, so no deleted agent's
        // key is returned.
        let mut keys = locked(&self.keys);
        match self.agents.get(pubkey) {
            Ok(agent) => {
                let key = keys.get(pubkey).cloned();
                Ok((agent, key.ok_or("The agent's key has not been read yet")?))
            }
            Err(error) => {
                keys.remove(pubkey);
                Err(error)
            }
        }
    }

    /// Generates the key. A retry with the same request ID and inputs gets
    /// the same key back.
    fn prepare(
        &self,
        request_id: String,
        destination: String,
        owner: String,
        agent_type: String,
        name: String,
    ) -> Result<String> {
        let mut creating = locked(&self.creating);
        if let Some(pending) = creating.iter().find(|c| c.request_id == request_id) {
            if (
                &pending.destination,
                &pending.owner,
                &pending.agent_type,
                &pending.name,
            ) != (&destination, &owner, &agent_type, &name)
            {
                return Err("Create destination or owner changed".into());
            }
            return Ok(pending.prepared.pubkey().to_owned());
        }
        let prepared = AppAgents::prepare(&destination, &owner, &agent_type, &name)?;
        let pubkey = prepared.pubkey().to_owned();
        if creating.len() >= PENDING_CREATES {
            creating.pop_front();
        }
        creating.push_back(Creating {
            request_id,
            destination,
            owner,
            agent_type,
            name,
            prepared,
            carry: Carry::default(),
        });
        Ok(pubkey)
    }

    /// Takes over an identity an earlier agent system saved, with its key,
    /// name, picture and description, as [`Self::prepare`] makes a new one.
    /// A retry with the same request ID and inputs gets the same key back.
    async fn port(
        &self,
        request_id: String,
        from: PortSource,
        pubkey: String,
        destination: String,
        owner: String,
        agent_type: String,
    ) -> Result<(String, Carry)> {
        if let Some(pending) = locked(&self.creating)
            .iter()
            .find(|c| c.request_id == request_id)
        {
            if (
                pending.prepared.pubkey(),
                &pending.destination,
                &pending.owner,
                &pending.agent_type,
            ) != (pubkey.as_str(), &destination, &owner, &agent_type)
            {
                return Err("Create destination or owner changed".into());
            }
            return Ok((pubkey, pending.carry.clone()));
        }
        let (old, credentials) = (self.old.clone(), self.credentials.clone());
        let taken = pubkey.clone();
        let (found, key) = blocking(move || {
            credentials.retry();
            let mut waits = 0;
            loop {
                match old.take(source(from), &taken, credentials.as_ref()) {
                    Err(error) if error == CREDENTIALS_BUSY && waits < BUSY_WAITS => {
                        waits += 1;
                        std::thread::sleep(BUSY_WAIT);
                    }
                    taken => return taken,
                }
            }
        })
        .await?;
        let name = found.name.clone();
        let carry = Carry {
            instructions: found.carry.instructions.clone(),
            workspace: found.carry.workspace.clone(),
        };
        let prepared = AppAgents::adopt(
            &destination,
            &owner,
            &agent_type,
            key,
            &found.name,
            found.picture,
            found.about,
        )?;
        let mut creating = locked(&self.creating);
        if creating.len() >= PENDING_CREATES {
            creating.pop_front();
        }
        creating.push_back(Creating {
            request_id,
            destination,
            owner,
            agent_type,
            name,
            prepared,
            carry: carry.clone(),
        });
        Ok((pubkey, carry))
    }

    /// Saves the prepared identity with the owner's attestation. Nothing is
    /// saved unless the attestation is for this key and owner and the key
    /// is stored.
    async fn commit(&self, request_id: String, auth: String) -> Result<AppAgentSummary> {
        let tag: Vec<String> =
            serde_json::from_str(&auth).map_err(|_| "Invalid owner authorization")?;
        let pending = {
            let mut creating = locked(&self.creating);
            let index = creating
                .iter()
                .position(|c| c.request_id == request_id)
                .ok_or("Create request expired; start again")?;
            creating.remove(index).expect("the index was just found")
        };
        let host = self.clone();
        blocking(move || {
            // Held so no first key read meets this write in the credential store.
            let mut keys = locked(&host.keys);
            host.credentials.retry();
            let (agent, key) =
                host.agents
                    .commit(pending.prepared, &tag, host.credentials.as_ref())?;
            keys.insert(agent.pubkey.clone(), Arc::new(key));
            Ok(summary(agent))
        })
        .await
    }

    /// Deletes the key once any profile publication in flight has settled,
    /// so the community's cleanup sees every profile it will ever have. The
    /// row stays, marked deleted, until the GUI has cleaned up the community
    /// and forgets it.
    async fn delete(&self, pubkey: String) -> Result<()> {
        let turn = self.profile_turn(&pubkey);
        let _turn = turn.lock().await;
        let host = self.clone();
        blocking(move || {
            // Held across the deletion, so no publication re-reads the key meanwhile.
            let mut keys = locked(&host.keys);
            keys.remove(&pubkey);
            host.hosting.forget(&pubkey);
            host.agents.remove(&pubkey, host.credentials.as_ref())
        })
        .await
    }

    /// Signs one event as the agent and posts it to the agent's community.
    async fn publish(&self, pubkey: String, event: AppAgentEvent) -> Result<String> {
        let (agent, key) = self.keyed(pubkey).await?;
        let signed = agent.sign(&key, event.kind, event.content, event.tags)?;
        let signed = post(&agent, &key, signed).await?;
        serde_json::to_string(&signed).map_err(|_| "Invalid agent event".into())
    }

    /// Publishes changed public metadata: its name, picture and
    /// description. Safe to call any time; one publication runs per agent
    /// at a time and records only what its community accepted.
    async fn publish_profile(&self, pubkey: String) -> Result<()> {
        let turn = self.profile_turn(&pubkey);
        let _turn = turn.lock().await;
        let agent = self.agent(pubkey).await?;
        if agent.profile_is_current() {
            return Ok(());
        }
        let (agent, key) = self.key(agent).await?;
        let event = agent.profile_event(&key)?;
        let signed = post(&agent, &key, event).await?;
        let created_at = signed
            .get("created_at")
            .and_then(Value::as_u64)
            .ok_or("Invalid agent event")?;
        let agents = self.agents.clone();
        blocking(move || {
            agents.published(
                &agent.pubkey,
                Published {
                    name: agent.name,
                    picture: agent.picture,
                    about: agent.about,
                    created_at,
                },
            )
        })
        .await
    }

    /// Reads from the agent's community as the agent, so it sees what the
    /// agent may see rather than what the owner may.
    async fn query(&self, pubkey: String, filters: String) -> Result<String> {
        let filters: Vec<Value> =
            serde_json::from_str(&filters).map_err(|_| "Agents read with one to eight filters")?;
        if filters.is_empty() || filters.len() > 8 {
            return Err("Agents read with one to eight filters".into());
        }
        let (agent, key) = self.keyed(pubkey).await?;
        let bytes = serde_json::to_vec(&filters).map_err(|_| "Could not encode agent read")?;
        let response = client()?
            .post(agent.query_url())
            .timeout(Duration::from_secs(20))
            .header("Content-Type", "application/json")
            .header(
                "Authorization",
                authorization(agent.http_auth(&key, &agent.query_url(), &bytes)?)?,
            )
            .header("x-auth-tag", &agent.auth)
            .body(bytes)
            .send()
            .await
            .map_err(|_| "Agent read failed")?;
        let status = response.status();
        let body = bounded(response, 8 * 1024 * 1024).await?;
        if !status.is_success() {
            return Err(format!(
                "The community refused the agent read: {}",
                reason(status, &body)
            ));
        }
        let events: Value = serde_json::from_slice(&body).map_err(|_| "Invalid agent read")?;
        serde_json::to_string(&events).map_err(|_| "Invalid agent read".into())
    }

    /// Appends one part to a staged upload, starting it on its first part.
    fn stage(&self, id: String, data: String) -> Result<()> {
        let mut staged = locked(&self.staged);
        staged.retain(|upload| upload.started.elapsed() < STAGED_LIFETIME);
        match staged.iter_mut().find(|upload| upload.id == id) {
            Some(upload) => {
                if upload.data.len() + data.len() > UPLOAD_TEXT_LIMIT {
                    staged.retain(|upload| upload.id != id);
                    return Err(UPLOAD_REFUSED.into());
                }
                upload.data.push_str(&data);
            }
            None => {
                if data.len() > UPLOAD_TEXT_LIMIT {
                    return Err(UPLOAD_REFUSED.into());
                }
                if staged.len() >= STAGED_UPLOADS {
                    staged.remove(0);
                }
                staged.push(Staged {
                    id,
                    data,
                    started: Instant::now(),
                });
            }
        }
        Ok(())
    }

    /// Uploads one file (base64 `data` of type `mime`, after any staged
    /// parts) to the agent's community as the agent, and returns the
    /// community's description of it.
    async fn upload(
        &self,
        pubkey: String,
        data: String,
        mime: String,
        staged: Option<String>,
    ) -> Result<String> {
        use sha2::{Digest, Sha256};
        let data = match staged {
            None => data,
            Some(id) => {
                let mut staged = locked(&self.staged);
                let index = staged
                    .iter()
                    .position(|upload| upload.id == id)
                    .ok_or("Upload expired; try again")?;
                let mut upload = staged.remove(index);
                upload.data.push_str(&data);
                upload.data
            }
        };
        if !UPLOAD_TYPES.contains(&mime.as_str()) || data.len() > UPLOAD_TEXT_LIMIT {
            return Err(UPLOAD_REFUSED.into());
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|_| "Invalid upload data")?;
        let (agent, key) = self.keyed(pubkey).await?;
        let sha256 = format!("{:x}", Sha256::digest(&bytes));
        let auth = serde_json::to_vec(&agent.upload_auth(&key, &sha256)?)
            .map_err(|_| "Could not authorize upload")?;
        let response = client()?
            .put(agent.upload_url())
            .timeout(Duration::from_secs(120))
            .header("Content-Type", &mime)
            .header("X-SHA-256", &sha256)
            .header(
                "Authorization",
                format!(
                    "Nostr {}",
                    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(auth)
                ),
            )
            .header("x-auth-tag", &agent.auth)
            .body(bytes)
            .send()
            .await
            .map_err(|_| "Agent upload unconfirmed")?;
        let status = response.status();
        let body = bounded(response, 64 * 1024).await?;
        if !status.is_success() {
            return Err(format!(
                "The community refused the upload: {}",
                reason(status, &body)
            ));
        }
        let receipt: Value = serde_json::from_slice(&body).map_err(|_| "Invalid upload receipt")?;
        serde_json::to_string(&receipt).map_err(|_| "Invalid upload receipt".into())
    }

    /// Writes one memory entry, encrypted to the agent's owner. `after` is
    /// the `created_at` of the entry it replaces, or 0.
    async fn remember(
        &self,
        pubkey: String,
        slug: String,
        body: String,
        after: u64,
    ) -> Result<String> {
        let (agent, key) = self.keyed(pubkey).await?;
        let signed = agent.memory(&key, &slug, &body, after)?;
        let signed = post(&agent, &key, signed).await?;
        serde_json::to_string(&signed).map_err(|_| "Invalid agent event".into())
    }
}

impl AgentCustody for AppAgentHost {
    fn launch_identity(&self, pubkey: &str) -> BoxFuture<'static, Result<LaunchIdentity>> {
        let (host, pubkey) = (self.clone(), pubkey.to_owned());
        Box::pin(async move {
            let (agent, key) = host.keyed(pubkey).await?;
            launch_identity(agent, &key)
        })
    }

    fn cached_launch_identity(&self, pubkey: &str) -> Result<LaunchIdentity> {
        let (agent, key) = self.cached(pubkey)?;
        launch_identity(agent, &key)
    }
}

fn launch_identity(agent: AppAgent, key: &Secret) -> Result<LaunchIdentity> {
    Ok(LaunchIdentity {
        nsec: nsec(key)?,
        pubkey: agent.pubkey,
        relay: agent.relay,
        owner: agent.owner,
        auth: agent.auth,
    })
}

/// The key as bech32 `nsec1…`, which providers take.
fn nsec(key: &Secret) -> Result<Zeroizing<String>> {
    let hex = key.hex();
    let mut bytes = Zeroizing::new([0u8; 32]);
    for (byte, pair) in bytes.iter_mut().zip(hex.as_bytes().chunks(2)) {
        let digit = |c: u8| (c as char).to_digit(16).unwrap_or(0) as u8;
        *byte = (digit(pair[0]) << 4) | digit(pair[1]);
    }
    // Sized for the whole key, so it never reallocates and leaves a copy.
    let mut nsec = Zeroizing::new(String::with_capacity(63));
    bech32::encode_to_fmt::<bech32::Bech32, _>(
        &mut *nsec,
        bech32::Hrp::parse_unchecked("nsec"),
        &*bytes,
    )
    .map_err(|_| "Could not encode the agent's key")?;
    Ok(nsec)
}

/// A NIP-98 `Authorization` header for `event`.
fn authorization(event: Value) -> Result<String> {
    let json = serde_json::to_vec(&event).map_err(|_| "Could not authorize profile request")?;
    Ok(format!(
        "Nostr {}",
        base64::engine::general_purpose::STANDARD.encode(json)
    ))
}

/// One client for every agent request, so frequent small posts such as
/// typing pulses reuse a connection instead of opening a new one each
/// time. Each request sets its own timeout.
fn client() -> Result<&'static reqwest::Client> {
    static CLIENT: std::sync::OnceLock<Option<reqwest::Client>> = std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(10))
                // A connection idle across sleep may be dead; open a new one.
                .pool_idle_timeout(Duration::from_secs(30))
                .build()
                .ok()
        })
        .as_ref()
        .ok_or_else(|| "Agent client unavailable".into())
}

async fn bounded(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| "Agent request failed")? {
        if body.len() + chunk.len() > limit {
            return Err("Agent response is too large".into());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Why the community refused, from its JSON body when it gives one.
fn reason(status: reqwest::StatusCode, body: &[u8]) -> String {
    let receipt: Value =
        serde_json::from_slice(&body[..body.len().min(16 * 1024)]).unwrap_or(Value::Null);
    receipt
        .get("message")
        .or_else(|| receipt.get("error"))
        .and_then(Value::as_str)
        .map(|text| text.chars().take(300).collect::<String>())
        .unwrap_or_else(|| format!("status {}", status.as_u16()))
}

async fn post(agent: &AppAgent, key: &Secret, signed: Value) -> Result<Value> {
    let event_id = signed
        .get("id")
        .and_then(Value::as_str)
        .ok_or("Invalid agent event")?
        .to_owned();
    let bytes = serde_json::to_vec(&signed).map_err(|_| "Could not encode agent event")?;
    let response = client()?
        .post(agent.events_url())
        .timeout(Duration::from_secs(20))
        .header("Content-Type", "application/json")
        .header(
            "Authorization",
            authorization(agent.http_auth(key, &agent.events_url(), &bytes)?)?,
        )
        .header("x-auth-tag", &agent.auth)
        .body(bytes)
        .send()
        .await
        .map_err(|_| "Agent event unconfirmed; it may or may not have been accepted")?;
    let status = response.status();
    let body = response.bytes().await.unwrap_or_default();
    let receipt: Value =
        serde_json::from_slice(&body[..body.len().min(16 * 1024)]).unwrap_or(Value::Null);
    if !status.is_success()
        || receipt.get("accepted").and_then(Value::as_bool) != Some(true)
        || receipt.get("event_id").and_then(Value::as_str) != Some(event_id.as_str())
    {
        return Err(format!(
            "The community refused the agent event: {}",
            reason(status, &body)
        ));
    }
    Ok(signed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// One saved key, behind a store that is busy for the first few reads.
    struct Busy(String, AtomicU32);
    impl Credentials for Busy {
        fn read_legacy(
            &self,
            _: buzz_agent_controller::LegacySource,
            _: &str,
        ) -> std::result::Result<Secret, String> {
            unreachable!()
        }
        fn read(&self, _: &str, pubkey: &str) -> std::result::Result<Option<Secret>, String> {
            if self.1.load(Ordering::SeqCst) > 0 {
                self.1.fetch_sub(1, Ordering::SeqCst);
                return Err(CREDENTIALS_BUSY.into());
            }
            Secret::parse(&self.0, pubkey).map(Some)
        }
        fn add(&self, _: &str, _: &Secret) -> std::result::Result<(), String> {
            unreachable!()
        }
        fn delete(&self, _: &str, _: &str) -> std::result::Result<(), String> {
            unreachable!()
        }
    }

    /// Earlier stores that do not exist.
    fn old(dir: &std::path::Path) -> OldStores {
        OldStores {
            harness: dir.join("agent-controller/agents.json"),
            data_dir: dir.join("data"),
        }
    }

    #[tokio::test]
    async fn a_first_key_read_waits_out_a_busy_store() {
        let key = Secret::generate().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let host = AppAgentHost::new(
            dir.path().join("identities.json"),
            old(dir.path()),
            Arc::new(Busy(key.hex().to_string(), AtomicU32::new(3))),
        );
        let agent = AppAgent {
            pubkey: key.pubkey().into(),
            relay: "wss://relay.example".into(),
            owner: "a".repeat(64),
            auth: "[]".into(),
            agent_type: "p/t".into(),
            name: "Ada".into(),
            picture: None,
            about: None,
            profile: None,
            deleted: false,
        };
        let (_, read) = host.key(agent).await.unwrap();
        assert_eq!(read.pubkey(), key.pubkey());
    }

    #[tokio::test]
    async fn delete_waits_for_a_profile_publication_in_flight() {
        let dir = tempfile::tempdir().unwrap();
        let credentials = crate::credentials::FileCredentials::new(dir.path().join("keys"));
        let host = AppAgentHost::new(
            dir.path().join("identities.json"),
            old(dir.path()),
            Arc::new(credentials.unwrap()),
        );
        let pubkey = "c".repeat(64);
        let turn = host.profile_turn(&pubkey);
        let publishing = turn.lock().await;
        let mut deleting = tokio::spawn({
            let host = host.clone();
            let pubkey = pubkey.clone();
            async move { host.delete(pubkey).await }
        });
        // Unblocked, deleting an absent agent takes well under this.
        let waited = Duration::from_millis(300);
        assert!(tokio::time::timeout(waited, &mut deleting).await.is_err());
        drop(publishing);
        deleting.await.unwrap().unwrap();
    }

    /// A host with one saved agent, and the agent's owner.
    async fn saved(dir: &std::path::Path) -> (AppAgentHost, String, String) {
        let credentials = crate::credentials::FileCredentials::new(dir.join("keys")).unwrap();
        let host = AppAgentHost::new(dir.join("identities.json"), old(dir), Arc::new(credentials));
        let owner = buzz_identity::Identity::ephemeral();
        let pubkey = host
            .prepare(
                "create".into(),
                "wss://relay.example".into(),
                owner.pubkey_hex(),
                "p/t".into(),
                "Ada".into(),
            )
            .unwrap();
        let auth = owner.attest_agent(&pubkey).unwrap();
        host.commit("create".into(), auth).await.unwrap();
        (host, pubkey, owner.pubkey_hex())
    }

    #[tokio::test]
    async fn custody_gives_the_agents_key_as_an_nsec() {
        let dir = tempfile::tempdir().unwrap();
        let (_, pubkey, owner) = saved(dir.path()).await;
        // A host that has not read the key yet, as after a restart.
        let credentials = crate::credentials::FileCredentials::new(dir.path().join("keys"));
        let host = AppAgentHost::new(
            dir.path().join("identities.json"),
            old(dir.path()),
            Arc::new(credentials.unwrap()),
        );
        assert!(host.cached_launch_identity(&pubkey).is_err());
        let identity = host.launch_identity(&pubkey).await.unwrap();
        assert!(identity.nsec.starts_with("nsec1"));
        assert_eq!(
            Secret::parse(&identity.nsec, &pubkey).unwrap().pubkey(),
            pubkey
        );
        assert_eq!(identity.relay, "wss://relay.example");
        assert_eq!(identity.owner, owner);
        let tag: Vec<String> = serde_json::from_str(&identity.auth).unwrap();
        assert_eq!(tag.first().map(String::as_str), Some("auth"));
        let cached = host.cached_launch_identity(&pubkey).unwrap();
        assert_eq!(*cached.nsec, *identity.nsec, "the read key is cached");

        let shown = format!("{identity:?}");
        assert!(shown.contains(&pubkey));
        assert!(!shown.contains("nsec1"), "{shown}");
    }

    #[tokio::test]
    async fn custody_refuses_a_deleted_agent() {
        let dir = tempfile::tempdir().unwrap();
        let (host, pubkey, _) = saved(dir.path()).await;
        host.launch_identity(&pubkey).await.unwrap();
        host.delete(pubkey.clone()).await.unwrap();
        assert!(host.launch_identity(&pubkey).await.is_err());
        assert!(host.cached_launch_identity(&pubkey).is_err());
        assert!(host.launch_identity(&"d".repeat(64)).await.is_err());
    }

    #[test]
    fn staged_uploads_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let credentials = crate::credentials::FileCredentials::new(dir.path().join("keys"));
        let host = AppAgentHost::new(
            dir.path().join("identities.json"),
            old(dir.path()),
            Arc::new(credentials.unwrap()),
        );
        host.stage("a".into(), "QUJD".into()).unwrap();
        host.stage("a".into(), "REVG".into()).unwrap();
        assert_eq!(locked(&host.staged)[0].data, "QUJDREVG");
        let large = "A".repeat(UPLOAD_TEXT_LIMIT);
        assert_eq!(
            host.stage("a".into(), large).unwrap_err(),
            UPLOAD_REFUSED,
            "a part past the limit drops the upload"
        );
        assert!(locked(&host.staged).is_empty());
    }
}
