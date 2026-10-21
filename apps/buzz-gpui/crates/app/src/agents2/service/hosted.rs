//! Hosted agents (buzz-app's `RemoteAgentType`, #870): agents of a type
//! whose provider holds the key and runs them, such as a plugin's. The app
//! makes one through the type's steps, signs only the owner's attestation
//! and the owner's agent list, and shows it as running elsewhere.

use super::super::listings::{self, Listing};
use super::super::registry::{self, RemoteAgentType};
use super::{Agents2, selected_origin};
use crate::runtime::Runtime;
use buzz_relay_client::RelayHttp;
use gpui_kit::*;
use std::rc::Rc;

/// Why a second change to a hosted agent waits (`exclusive`).
const CHANGING: &str = "Wait for the agent's last change to finish";

fn provider(type_key: &str, cx: &App) -> Result<Rc<dyn RemoteAgentType>, String> {
    registry::type_of(type_key, cx)
        .and_then(|kind| kind.kind.remote())
        .ok_or_else(|| "That agent type is not available".to_owned())
}

/// Whether `type_key` is a hosted type.
pub fn is_hosted(type_key: &str, cx: &App) -> bool {
    provider(type_key, cx).is_ok()
}

impl Agents2 {
    /// Makes a hosted agent of `type_key` named `name` in the selected
    /// community: its provider makes the key, the owner attests it, the
    /// provider starts it, and the owner's list names it. Resolves with
    /// its pubkey.
    pub fn create_hosted(
        &mut self,
        type_key: &str,
        name: &str,
        picture: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<String, String>> {
        let name = name.trim().to_owned();
        if name.is_empty() {
            return Task::ready(Err("Name the agent".into()));
        }
        let remote = match provider(type_key, cx) {
            Ok(remote) => remote,
            Err(error) => return Task::ready(Err(error)),
        };
        let Some(origin) = selected_origin(cx) else {
            return Task::ready(Err("Connect to its community first".into()));
        };
        let Some(binding) = self
            .bound
            .get(&origin)
            .and_then(|bound| bound.binding.as_ref())
        else {
            return Task::ready(Err("Connect to its community first".into()));
        };
        let (community, viewer, identity, session) = (
            binding.destination.clone(),
            binding.viewer.clone(),
            binding.identity.clone(),
            binding.session.clone(),
        );
        let provision = remote.provision(&name, &community, cx);
        let type_key = type_key.to_owned();
        cx.spawn(async move |this, cx| {
            let pubkey = provision.await?;
            let valid = pubkey.len() == 64
                && pubkey
                    .bytes()
                    .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'));
            if !valid {
                return Err("The provider did not return the agent's key".into());
            }
            // Taken for its whole setting up, as `exclusive(pubkey)` holds it.
            let taken = this
                .update(cx, |this, _| {
                    let taken = pubkey == viewer
                        || this.saved().any(|saved| saved.summary.pubkey == pubkey)
                        // Listed in any community bound here.
                        || this.bound.keys().any(|origin| {
                            this.listed
                                .rows_of(origin)
                                .iter()
                                .any(|row| row.pubkey == pubkey)
                        });
                    taken || !this.hosted_busy.insert(pubkey.clone())
                })
                .unwrap_or(true);
            if taken {
                return Err("The provider returned a key that is already in use".into());
            }
            let done = async {
                let auth: Vec<String> = identity
                    .attest_agent(&pubkey)
                    .map_err(|error| error.to_string())
                    .and_then(|auth| {
                        serde_json::from_str(&auth).map_err(|error| error.to_string())
                    })?;
                let activate =
                    cx.update(|cx| remote.activate(&pubkey, &name, &community, auth, cx));
                activate.await?;
                let entry = Listing {
                    pubkey: pubkey.clone(),
                    name: name.clone(),
                    picture: picture.clone(),
                    runtime: Some(type_key.clone()),
                    prior: None,
                };
                let http = RelayHttp::new(session.origin().clone(), identity.clone());
                let write = {
                    let (entry, owner, runtime) =
                        (entry.clone(), identity.clone(), type_key.clone());
                    cx.update(|cx| {
                        Runtime::handle(cx).spawn(async move {
                            listings::publish(&http, &owner, &entry, &runtime).await
                        })
                    })
                };
                let prior = write
                    .await
                    .unwrap_or_else(|_| Err("The agent list was not updated. Retry.".into()))?;
                this.update(cx, |this, cx| {
                    this.listed.set_row(
                        &origin,
                        &pubkey,
                        Some(Listing {
                            prior: Some(prior),
                            ..entry
                        }),
                    );
                    cx.notify();
                })
                .map_err(|_| "Agents closed".to_owned())?;
                // Listed with its picture either way; its profile is the
                // provider's.
                if let Some(picture) = picture {
                    let profile = cx.update(|cx| {
                        remote.profile(&pubkey, &community, &name, Some(&picture), cx)
                    });
                    if let Err(error) = profile.await {
                        tracing::warn!(%error, "hosted agent picture not published");
                    }
                }
                Ok::<(), String>(())
            }
            .await;
            let _ = this.update(cx, |this, _| this.hosted_busy.remove(&pubkey));
            done.map(|()| pubkey)
        })
    }

    /// Deletes a hosted agent: its provider stops it and drops its key,
    /// then the owner's list forgets it.
    pub fn remove_hosted(
        &mut self,
        pubkey: &str,
        cx: &mut Context<Self>,
    ) -> Task<Result<(), String>> {
        let Some(agent) = self
            .elsewhere(cx)
            .into_iter()
            .find(|agent| agent.pubkey == pubkey)
        else {
            return Task::ready(Err("No such hosted agent".into()));
        };
        let remote = match provider(&agent.agent_type, cx) {
            Ok(remote) => remote,
            Err(error) => return Task::ready(Err(error)),
        };
        let Some(origin) = selected_origin(cx) else {
            return Task::ready(Err("Connect to its community first".into()));
        };
        let Some(binding) = self
            .bound
            .get(&origin)
            .and_then(|bound| bound.binding.as_ref())
        else {
            return Task::ready(Err("Connect to its community first".into()));
        };
        let (identity, session) = (binding.identity.clone(), binding.session.clone());
        // One change to a hosted agent at a time.
        if !self.hosted_busy.insert(agent.pubkey.clone()) {
            return Task::ready(Err(CHANGING.into()));
        }
        let remove = remote.remove(&agent.pubkey, &agent.relay, cx);
        let pubkey = agent.pubkey;
        cx.spawn(async move |this, cx| {
            let done = async {
                remove.await?;
                // Archived, its record deleted, its channels left, as a local
                // agent's deletion cleans up (`removeRelayAgent`).
                let cleanup = {
                    let pubkey = pubkey.clone();
                    cx.update(|cx| {
                        Runtime::handle(cx).spawn(async move {
                            super::super::cleanup::clean_hosted(&session, &identity, &pubkey).await
                        })
                    })
                };
                cleanup
                    .await
                    .unwrap_or_else(|_| Err("Record deletion is not confirmed. Retry.".into()))?;
                this.update(cx, |this, cx| {
                    this.listed.set_row(&origin, &pubkey, None);
                    cx.notify();
                })
                .map_err(|_| "Agents closed".to_owned())
            }
            .await;
            let _ = this.update(cx, |this, _| this.hosted_busy.remove(&pubkey));
            done
        })
    }

    /// Renames a hosted agent or changes its picture (`saveHosted`): its
    /// provider updates the profile, then the owner's list says so. Its
    /// settings are the provider's. `picture` `Some("")` removes it.
    pub fn save_hosted(
        &mut self,
        pubkey: &str,
        name: Option<String>,
        picture: Option<String>,
        settings: bool,
        cx: &mut Context<Self>,
    ) -> Task<Result<(), String>> {
        if settings {
            return Task::ready(Err(
                "A hosted agent's settings save through its provider".into()
            ));
        }
        let Some(agent) = self
            .elsewhere(cx)
            .into_iter()
            .find(|agent| agent.pubkey == pubkey)
        else {
            return Task::ready(Err("No such hosted agent".into()));
        };
        let remote = match provider(&agent.agent_type, cx) {
            Ok(remote) => remote,
            Err(error) => return Task::ready(Err(error)),
        };
        let name = name.map_or_else(|| agent.name.clone(), |name| name.trim().to_owned());
        if name.is_empty() {
            return Task::ready(Err("Name the agent".into()));
        }
        let picture = match picture {
            None => agent.picture.clone(),
            Some(picture) if picture.is_empty() => None,
            Some(picture) => Some(picture),
        };
        if name == agent.name && picture == agent.picture {
            return Task::ready(Ok(()));
        }
        let Some(origin) = selected_origin(cx) else {
            return Task::ready(Err("Connect to its community first".into()));
        };
        let Some(binding) = self
            .bound
            .get(&origin)
            .and_then(|bound| bound.binding.as_ref())
        else {
            return Task::ready(Err("Connect to its community first".into()));
        };
        let (identity, session) = (binding.identity.clone(), binding.session.clone());
        if !self.hosted_busy.insert(agent.pubkey.clone()) {
            return Task::ready(Err(CHANGING.into()));
        }
        let prior = self
            .listed
            .rows_of(&origin)
            .into_iter()
            .find(|row| row.pubkey == agent.pubkey)
            .and_then(|row| row.prior);
        let profile = remote.profile(&agent.pubkey, &agent.relay, &name, picture.as_deref(), cx);
        let (pubkey, runtime) = (agent.pubkey, agent.agent_type);
        cx.spawn(async move |this, cx| {
            let done = async {
                profile.await?;
                let entry = Listing {
                    pubkey: pubkey.clone(),
                    name,
                    picture,
                    runtime: Some(runtime.clone()),
                    prior,
                };
                let http = RelayHttp::new(session.origin().clone(), identity.clone());
                let write = {
                    let entry = entry.clone();
                    cx.update(|cx| {
                        Runtime::handle(cx).spawn(async move {
                            listings::publish(&http, &identity, &entry, &runtime).await
                        })
                    })
                };
                let prior = write
                    .await
                    .unwrap_or_else(|_| Err("The agent list was not updated. Retry.".into()))?;
                this.update(cx, |this, cx| {
                    this.listed.set_row(
                        &origin,
                        &pubkey,
                        Some(Listing {
                            prior: Some(prior),
                            ..entry
                        }),
                    );
                    cx.notify();
                })
                .map_err(|_| "Agents closed".to_owned())
            }
            .await;
            let _ = this.update(cx, |this, _| this.hosted_busy.remove(&pubkey));
            done
        })
    }
}
