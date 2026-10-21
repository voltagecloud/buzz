//! Custody for Agents2 agents (buzz-app's `app_agent_*` commands). The host
//! keeps each agent's key and owner attestation; the GUI asks for one bounded
//! event, read, upload or memory write at a time, which the host signs as the
//! agent and sends to the agent's community. The saved identities are the
//! agent list. JSON payloads travel as JSON text.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AppAgentRequest {
    /// Every saved identity, deleted ones included.
    List,
    /// Generates the new agent's key. The GUI signs the owner attestation
    /// for the returned pubkey and sends it in [`Self::CreateCommit`].
    /// Retrying with the same `request_id` returns the same key.
    CreatePrepare {
        request_id: String,
        /// The community, as a relay URL; saved in canonical `wss://` form.
        destination: String,
        owner: String,
        /// The agent type's key, set at create and changed by
        /// [`Self::SetType`].
        agent_type: String,
        name: String,
    },
    /// Saves the prepared identity and its key. `auth` is the NIP-OA `auth`
    /// tag, JSON encoded.
    CreateCommit {
        request_id: String,
        auth: String,
    },
    Rename {
        pubkey: String,
        name: String,
    },
    /// Sets the picture its profile shows, an `https` URL without
    /// credentials; empty removes it. [`Self::PublishProfile`] then
    /// publishes it. Hosts from before pictures answer that they do not
    /// support this request, so a GUI never believes a picture was saved
    /// that was not.
    SetPicture {
        pubkey: String,
        picture: String,
    },
    /// Makes it an agent of another type (`app_agent_set_type`): its key,
    /// name, picture and communities stay. Answers
    /// [`AppAgentResponse::Done`]. Hosts from before this answer that they
    /// do not support it, so a GUI never believes the type changed when it
    /// did not.
    SetType {
        pubkey: String,
        agent_type: String,
    },
    /// Deletes the key and marks the identity deleted. The row stays until
    /// [`Self::Forget`], once its community is cleaned up.
    Delete {
        pubkey: String,
    },
    /// Drops a deleted identity.
    Forget {
        pubkey: String,
    },
    /// Signs one event as the agent (deletions, reactions, messages, edits,
    /// canvases and DM opens only) and posts it to its community.
    Publish {
        pubkey: String,
        event: AppAgentEvent,
    },
    /// Publishes the agent's name, picture and description as its profile
    /// unless its community already has them.
    PublishProfile {
        pubkey: String,
    },
    /// Reads the agent's community as the agent: a JSON array of one to
    /// eight filters.
    Query {
        pubkey: String,
        filters: String,
    },
    /// Uploads base64 `data` (an image or MP4) as the agent. A file larger
    /// than one frame arrives first in [`Self::UploadPart`]s named by
    /// `staged`, and `data` is its last part.
    Upload {
        pubkey: String,
        data: String,
        mime: String,
        #[serde(default)]
        staged: Option<String>,
    },
    /// One base64 part of a staged upload, appended in order.
    UploadPart {
        staged: String,
        data: String,
    },
    /// Writes memory entry `slug`, encrypted to the owner and newer than the
    /// entry it replaces (`after`, its `created_at`, or 0).
    Remember {
        pubkey: String,
        slug: String,
        body: String,
        after: u64,
    },
    /// Downloads one of the agent's community's media blobs, by its URL or
    /// `sha256[.ext]`, as the agent, so the agent can see a Buzz image.
    /// Answers [`AppAgentResponse::Media`]. Hosts from before this answer
    /// that they do not support it.
    Media {
        pubkey: String,
        url: String,
    },
    /// Whether the asking GUI runs the agent now. Every GUI connected to
    /// this host, and every copy of the app sharing its state directory,
    /// hears the same events; one runs each agent. Asked before each run,
    /// so another takes over within a tick of the holder letting go or
    /// disconnecting. Answers [`AppAgentResponse::Claimed`]. Hosts from
    /// before this answer an error, and the GUI runs the agent anyway.
    Claim {
        pubkey: String,
    },
    /// Lets another GUI or copy of the app run the agent, as when this GUI
    /// cannot.
    Release {
        pubkey: String,
    },
    /// Identities earlier agent systems saved on this host, which Agents can
    /// take over (`app_agent_portable`). Answers
    /// [`AppAgentResponse::Portable`]. Hosts from before this answer that
    /// they do not support it.
    Portable,
    /// Takes over one of them as an agent of `agent_type` in `destination`,
    /// keeping its key, name, picture and description (`app_agent_port`).
    /// Answers [`AppAgentResponse::PortPrepared`]; as with
    /// [`Self::CreatePrepare`], the GUI then signs the owner attestation for
    /// its pubkey and sends it in [`Self::CreateCommit`]. The old store
    /// keeps the identity.
    PortPrepare {
        request_id: String,
        source: PortSource,
        pubkey: String,
        /// The community, as a relay URL.
        destination: String,
        owner: String,
        agent_type: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AppAgentResponse {
    Agents(Vec<AppAgentSummary>),
    Prepared {
        pubkey: String,
    },
    Created(AppAgentSummary),
    Done,
    /// The signed event the community accepted, as JSON.
    Event(String),
    /// The events a query returned, as a JSON array.
    Events(String),
    /// The community's description of an uploaded blob, as JSON.
    Uploaded(String),
    /// Whether the asking GUI runs the agent: see [`AppAgentRequest::Claim`].
    Claimed(bool),
    Portable(PortableAgents),
    /// The key [`AppAgentRequest::PortPrepare`] took over, and what the
    /// agent kept there that its new type may adopt.
    PortPrepared {
        pubkey: String,
        #[serde(default)]
        carry: Carry,
    },
    /// A media blob, base64: see [`AppAgentRequest::Media`].
    Media(String),
}

/// An Agents2 agent's identity; never its key.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AppAgentSummary {
    pub pubkey: String,
    /// The canonical `wss://` community origin it was created for.
    pub relay: String,
    pub owner: String,
    /// The agent type's key, set at create and changed by
    /// [`AppAgentRequest::SetType`].
    pub agent_type: String,
    pub name: String,
    /// The picture its profile shows; empty for none. `None` from hosts
    /// from before pictures, which cannot set one.
    pub picture: Option<String>,
    /// Its key is gone. Its community still has to drop it from its
    /// channels and archive it, after which it is forgotten.
    pub deleted: bool,
}

/// Where an earlier agent system saved an identity on the host: its v1
/// harness agents, or the installed or development build of old Buzz.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PortSource {
    Harness,
    Installed,
    Development,
}

/// An identity an earlier agent system saved, which Agents can take over.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PortableAgent {
    pub source: PortSource,
    pub pubkey: String,
    pub name: String,
    #[serde(default)]
    pub picture: Option<String>,
    /// The community it was saved for there, if valid.
    #[serde(default)]
    pub relay: String,
}

/// What an agent kept in an earlier system that a new type may give it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Carry {
    pub instructions: Option<String>,
    /// Its working directory, absolute or `~/…`.
    pub workspace: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PortableAgents {
    pub agents: Vec<PortableAgent>,
    /// A store that could not be read; the others are still listed.
    pub warnings: Vec<String>,
}

/// An event for an agent to sign.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppAgentEvent {
    pub kind: u16,
    pub content: String,
    pub tags: Vec<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire<T: Serialize>(value: &T) -> Vec<u8> {
        rmp_serde::to_vec_named(value).unwrap()
    }

    /// The summary and requests as hosts and GUIs from before pictures
    /// have them.
    mod old {
        use serde::{Deserialize, Serialize};

        #[derive(Debug, Serialize, Deserialize)]
        pub enum AppAgentRequest {
            List,
            Rename { pubkey: String, name: String },
            PublishProfile { pubkey: String },
        }

        #[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
        #[serde(rename_all = "camelCase", default)]
        pub struct AppAgentSummary {
            pub pubkey: String,
            pub relay: String,
            pub owner: String,
            pub agent_type: String,
            pub name: String,
            pub deleted: bool,
        }
    }

    #[test]
    fn summaries_carry_the_picture_both_ways() {
        let summary = AppAgentSummary {
            pubkey: "a".repeat(64),
            name: "Ada".into(),
            picture: Some("https://relay.example/media/a.png".into()),
            ..Default::default()
        };
        let read: AppAgentSummary = rmp_serde::from_slice(&wire(&summary)).unwrap();
        assert_eq!(read, summary);
        let none = AppAgentSummary {
            picture: Some(String::new()),
            ..summary.clone()
        };
        let read: AppAgentSummary = rmp_serde::from_slice(&wire(&none)).unwrap();
        assert_eq!(read.picture.as_deref(), Some(""), "none is not unknown");

        let old: old::AppAgentSummary = rmp_serde::from_slice(&wire(&summary)).unwrap();
        assert_eq!(old.name, "Ada", "an older GUI ignores the picture");
        let from_old: AppAgentSummary = rmp_serde::from_slice(&wire(&old::AppAgentSummary {
            pubkey: "a".repeat(64),
            name: "Ada".into(),
            ..Default::default()
        }))
        .unwrap();
        assert_eq!(from_old.picture, None, "an older host cannot set pictures");
    }

    #[test]
    fn older_hosts_refuse_set_picture_and_newer_ones_take_old_requests() {
        let set = AppAgentRequest::SetPicture {
            pubkey: "a".repeat(64),
            picture: "https://relay.example/media/a.png".into(),
        };
        // An older host cannot decode it, so it answers that it does not
        // support the request rather than dropping the picture.
        assert!(rmp_serde::from_slice::<old::AppAgentRequest>(&wire(&set)).is_err());
        match rmp_serde::from_slice(&wire(&set)).unwrap() {
            AppAgentRequest::SetPicture { picture, .. } => {
                assert_eq!(picture, "https://relay.example/media/a.png")
            }
            other => panic!("expected SetPicture, got {other:?}"),
        }
        let rename = old::AppAgentRequest::Rename {
            pubkey: "a".repeat(64),
            name: "Ada".into(),
        };
        assert!(matches!(
            rmp_serde::from_slice(&wire(&rename)).unwrap(),
            AppAgentRequest::Rename { name, .. } if name == "Ada"
        ));
        let publish = old::AppAgentRequest::PublishProfile {
            pubkey: "a".repeat(64),
        };
        assert!(matches!(
            rmp_serde::from_slice(&wire(&publish)).unwrap(),
            AppAgentRequest::PublishProfile { .. }
        ));
        assert!(matches!(
            rmp_serde::from_slice(&wire(&old::AppAgentRequest::List)).unwrap(),
            AppAgentRequest::List
        ));
    }

    #[test]
    fn older_hosts_refuse_set_type_and_newer_ones_read_it() {
        let set = AppAgentRequest::SetType {
            pubkey: "a".repeat(64),
            agent_type: "codex/codex".into(),
        };
        // An older host cannot decode it, so it answers that it does not
        // support the request and the GUI keeps the agent's old type.
        assert!(rmp_serde::from_slice::<old::AppAgentRequest>(&wire(&set)).is_err());
        assert!(matches!(
            rmp_serde::from_slice(&wire(&set)).unwrap(),
            AppAgentRequest::SetType { agent_type, .. } if agent_type == "codex/codex"
        ));
    }

    #[test]
    fn older_hosts_refuse_media_and_media_answers_round_trip() {
        let media = AppAgentRequest::Media {
            pubkey: "a".repeat(64),
            url: "https://relay.example/media/a.png".into(),
        };
        assert!(rmp_serde::from_slice::<old::AppAgentRequest>(&wire(&media)).is_err());
        assert!(matches!(
            rmp_serde::from_slice(&wire(&media)).unwrap(),
            AppAgentRequest::Media { url, .. } if url == "https://relay.example/media/a.png"
        ));
        assert!(matches!(
            rmp_serde::from_slice(&wire(&AppAgentResponse::Media("iVBO".into()))).unwrap(),
            AppAgentResponse::Media(data) if data == "iVBO"
        ));
    }
}
