//! An interrupted delete/create must not erase the only recovery pointer.
//! Keep one immutable journal Secret per thread until its replacement is bound.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::Secret;
use kube::api::{DeleteParams, PostParams, Preconditions};
use kube::core::DynamicObject;

use super::{
    annotation, api_failure, reason_is, verify_identity, Lifecycle, OperatingMode, ProviderError,
    Session, CHECKPOINT, GENERATION,
};

const JOURNAL_TYPE: &str = "buzz.block.xyz/sandbox-recovery-v1";
const TOMBSTONE: &str = "tombstone.json";

fn journal_name(session: &Session<'_>) -> String {
    format!("{}-recovery", session.name)
}

async fn read(session: &Session<'_>) -> Result<Option<(Secret, DynamicObject)>, ProviderError> {
    let Some(secret) = session
        .secrets
        .get_opt(&journal_name(session))
        .await
        .map_err(|e| {
            format!(
                "could not read Sandbox recovery journal: {}",
                api_failure(&e)
            )
        })?
    else {
        return Ok(None);
    };
    let tombstone = secret
        .data
        .as_ref()
        .and_then(|data| data.get(TOMBSTONE))
        .and_then(|value| serde_json::from_slice::<DynamicObject>(&value.0).ok())
        .filter(|object| {
            secret.immutable == Some(true)
                && secret.type_.as_deref() == Some(JOURNAL_TYPE)
                && object
                    .metadata
                    .uid
                    .as_ref()
                    .is_some_and(|uid| !uid.is_empty())
                && object
                    .metadata
                    .resource_version
                    .as_ref()
                    .is_some_and(|rv| !rv.is_empty())
                && annotation(object, GENERATION).is_some()
                && Lifecycle::of(object) == Lifecycle::Ended
                && OperatingMode::of(object) == OperatingMode::Suspended
        })
        .ok_or("invalid Sandbox recovery journal; retain it for operator recovery")?;
    verify_identity(&tombstone, session.identity, session.scope, session.owner)?;
    Ok(Some((secret, tombstone)))
}

pub(super) async fn load(session: &Session<'_>) -> Result<Option<DynamicObject>, ProviderError> {
    Ok(read(session).await?.map(|(_, tombstone)| tombstone))
}

fn delete_params(secret: &Secret) -> DeleteParams {
    DeleteParams {
        preconditions: Some(Preconditions {
            uid: secret.metadata.uid.clone(),
            resource_version: secret.metadata.resource_version.clone(),
        }),
        ..Default::default()
    }
}

pub(super) async fn preserve(
    session: &Session<'_>,
    tombstone: &DynamicObject,
) -> Result<(), ProviderError> {
    let name = journal_name(session);
    for _ in 0..5 {
        if let Some((secret, recorded)) = read(session).await? {
            if recorded.metadata.uid == tombstone.metadata.uid {
                if annotation(&recorded, GENERATION) != annotation(tombstone, GENERATION)
                    || annotation(&recorded, CHECKPOINT) != annotation(tombstone, CHECKPOINT)
                {
                    return Err("Sandbox recovery journal disagrees with the tombstone".into());
                }
                return Ok(());
            }
            // A later tombstone is still durable while its older journal is replaced.
            match session.secrets.delete(&name, &delete_params(&secret)).await {
                Ok(_) => {}
                Err(error) if reason_is(&error, "NotFound") || reason_is(&error, "Conflict") => {
                    continue
                }
                Err(error) => {
                    return Err(format!(
                        "could not replace Sandbox recovery journal: {}",
                        api_failure(&error)
                    )
                    .into())
                }
            }
        }
        let encoded = serde_json::to_string(tombstone)
            .map_err(|_| "could not encode Sandbox recovery journal")?;
        let secret = Secret {
            metadata: kube::core::ObjectMeta {
                name: Some(name.clone()),
                ..Default::default()
            },
            immutable: Some(true),
            type_: Some(JOURNAL_TYPE.into()),
            // No ownerRef to the old Sandbox: its deletion must leave the journal intact.
            string_data: Some(BTreeMap::from([(TOMBSTONE.into(), encoded)])),
            ..Default::default()
        };
        match session
            .secrets
            .create(&PostParams::default(), &secret)
            .await
        {
            Ok(_) => return Ok(()),
            Err(error) if reason_is(&error, "AlreadyExists") => continue,
            Err(error) => {
                return Err(format!(
                    "could not preserve Sandbox recovery journal ({}); tombstone retained",
                    api_failure(&error)
                )
                .into())
            }
        }
    }
    Err("Sandbox recovery journal kept changing; retry recovery".into())
}

/// Cleanup failure leaves a durable record, never an uncertain recovery: the
/// next deploy of the bound replacement retries it.
pub(super) async fn clear(session: &Session<'_>, previous: &str) {
    let secret = match read(session).await {
        Ok(Some((secret, tombstone))) if annotation(&tombstone, GENERATION) == Some(previous) => {
            secret
        }
        Ok(_) => return,
        Err(error) => {
            eprintln!("Sandbox recovery journal kept: {error}");
            return;
        }
    };
    if let Err(error) = session
        .secrets
        .delete(&journal_name(session), &delete_params(&secret))
        .await
    {
        if !reason_is(&error, "NotFound") {
            eprintln!(
                "Sandbox recovery journal kept; a later deploy removes it: {}",
                api_failure(&error)
            );
        }
    }
}
