//! Provider payloads become ordinary channel replies with typed source tags.

use crate::config::Config;
use anyhow::{bail, Result};
use serde_json::Value;
use uuid::Uuid;

pub(crate) struct Occurrence {
    pub source: &'static str,
    pub channel: Uuid,
    pub delivery: String,
    pub thread: String,
    pub thread_url: String,
    pub reference: String,
    pub content: String,
    pub fields: Vec<(String, String)>,
}

fn text<'a>(v: &'a Value, pointer: &str) -> &'a str {
    v.pointer(pointer).and_then(Value::as_str).unwrap_or("")
}
fn short(raw: &str, n: usize) -> String {
    let mut end = raw.len().min(n);
    while !raw.is_char_boundary(end) {
        end -= 1;
    }
    raw[..end].to_owned()
}
fn required(raw: &str) -> Result<&str> {
    if raw.is_empty() || raw.len() > 4096 {
        bail!("missing or oversized provider field");
    }
    Ok(raw)
}
fn check_labels(value: &Value) -> Result<()> {
    if value.as_array().is_some_and(|labels| labels.len() > 16) {
        bail!("provider payload exceeds the 16-label bound");
    }
    Ok(())
}
fn labels(v: &Value) -> Vec<(String, String)> {
    v.as_array()
        .into_iter()
        .flatten()
        .take(16)
        .filter_map(|label| label.get("name").and_then(Value::as_str))
        .map(|name| ("labels".into(), short(name, 256)))
        .collect()
}
fn base(
    source: &'static str,
    channel: Uuid,
    delivery: &str,
    thread_info: (String, String, String),
    action: String,
    id: String,
    actor: &str,
) -> Occurrence {
    let (thread, url, reference) = thread_info;
    Occurrence {
        source,
        channel,
        delivery: delivery.to_owned(),
        thread: thread.clone(),
        thread_url: url.clone(),
        reference,
        content: String::new(),
        fields: vec![
            ("source".into(), source.into()),
            ("action".into(), action),
            ("id".into(), id),
            ("url".into(), url),
            ("actor".into(), short(actor, 256)),
            ("thread".into(), thread),
        ],
    }
}

/// Shape-check the unsigned GitHub event header against authenticated body fields.
/// Display/actions derive from signed payload data, never the header alone.
pub(crate) fn github(
    config: &Config,
    payload: &Value,
    delivery: &str,
    event_header: &str,
) -> Result<Vec<Occurrence>> {
    let inferred = if payload.get("review").is_some() {
        "pull_request_review"
    } else if payload.get("comment").is_some() && payload.get("pull_request").is_some() {
        "pull_request_review_comment"
    } else if payload.get("comment").is_some() && payload.get("issue").is_some() {
        "issue_comment"
    } else if payload.get("pull_request").is_some() {
        "pull_request"
    } else if payload.get("check_run").is_some() {
        "check_run"
    } else {
        return Ok(vec![]);
    };
    if event_header != inferred {
        bail!("GitHub event header does not match signed payload shape");
    }
    let repo = required(text(payload, "/repository/full_name"))?;
    let Some(channel) = config.github_repos.get(repo).copied() else {
        return Ok(vec![]);
    };
    let action = required(text(payload, "/action"))?;
    let actor = text(payload, "/sender/login");
    let mut targets = Vec::new();
    let object = if inferred == "check_run" {
        &payload["check_run"]
    } else if inferred == "issue_comment" {
        &payload["issue"]
    } else {
        &payload["pull_request"]
    };
    if inferred == "check_run" {
        let prs = object["pull_requests"].as_array();
        if prs.is_some_and(|prs| prs.len() > 4) {
            bail!("check run has more than four pull requests");
        }
        for pr in prs.into_iter().flatten() {
            if let Some(number) = pr["number"].as_u64() {
                targets.push((
                    format!("pr:{number}"),
                    format!("{repo}#{number}"),
                    format!("https://github.com/{repo}/pull/{number}"),
                ));
            }
        }
        if targets.is_empty() {
            let sha = required(text(object, "/head_sha"))?;
            targets.push((
                format!("commit:{sha}"),
                format!("{repo}@{}", short(sha, 12)),
                format!("https://github.com/{repo}/commit/{sha}"),
            ));
        }
    } else {
        // Ordinary GitHub issues are outside the requested PR review surface.
        if inferred == "issue_comment" && object.get("pull_request").is_none() {
            return Ok(vec![]);
        };
        let number = object["number"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("missing PR number"))?;
        targets.push((
            format!("pr:{number}"),
            format!("{repo}#{number}"),
            format!("https://github.com/{repo}/pull/{number}"),
        ));
    }
    check_labels(&object["labels"])?;
    let detail = if payload.get("comment").is_some() {
        &payload["comment"]
    } else if payload.get("review").is_some() {
        &payload["review"]
    } else {
        object
    };
    let id = detail.get("id").map(Value::to_string).unwrap_or_default();
    let title = short(
        if text(object, "/title").is_empty() {
            text(object, "/name")
        } else {
            text(object, "/title")
        },
        256,
    );
    let body = short(
        if text(detail, "/body").is_empty() {
            text(detail, "/output/summary")
        } else {
            text(detail, "/body")
        },
        6000,
    );
    let outcome = if inferred == "check_run" {
        text(object, "/conclusion")
    } else if inferred == "pull_request_review" {
        text(detail, "/state")
    } else {
        ""
    };
    let mut results = Vec::new();
    for (thread, reference, url) in targets {
        let mut item = base(
            "github",
            channel,
            delivery,
            (format!("{repo}:{thread}"), url, reference.clone()),
            format!("{inferred}.{action}"),
            id.clone(),
            actor,
        );
        item.fields.extend([
            ("repo".into(), repo.into()),
            ("ref".into(), reference),
            ("title".into(), title.clone()),
        ]);
        item.fields.extend(labels(&object["labels"]));
        if inferred == "pull_request" && action == "labeled" {
            let name = text(payload, "/label/name");
            if !name.is_empty() {
                item.fields.push(("added-labels".into(), short(name, 256)));
            }
        }
        for (field, pointer) in [("state", "/state"), ("conclusion", "/conclusion")] {
            let value = if field == "state"
                && object.get("merged").and_then(Value::as_bool) == Some(true)
            {
                "merged"
            } else if field == "state" && text(object, pointer).is_empty() {
                text(object, "/status")
            } else {
                text(object, pointer)
            };
            if !value.is_empty() {
                item.fields.push((field.into(), short(value, 128)));
            }
        }
        if inferred == "pull_request_review" {
            item.fields
                .push(("review-state".into(), short(text(detail, "/state"), 128)));
        }
        if let Some(url) = detail.get("html_url").and_then(Value::as_str) {
            item.fields
                .iter_mut()
                .filter(|(name, _)| name == "url")
                .for_each(|(_, value)| *value = short(url, 4096));
        }
        item.content = format!(
            "GitHub {} — {inferred}.{action} by {actor}\n{title}\n{outcome}\n{body}\n{}",
            item.reference, item.thread_url
        );
        results.push(item);
    }
    Ok(results)
}

pub(crate) fn linear_issue(payload: &Value) -> Option<&str> {
    (text(payload, "/type") == "Comment")
        .then(|| text(payload, "/data/issueId"))
        .filter(|id| !id.is_empty())
}

pub(crate) fn linear(
    config: &Config,
    payload: &Value,
    delivery: &str,
    routed: Option<Uuid>,
) -> Result<Vec<Occurrence>> {
    let kind = text(payload, "/type");
    if kind != "Issue" && kind != "Comment" {
        return Ok(vec![]);
    };
    let data = &payload["data"];
    let channel = config
        .linear_projects
        .get(text(data, "/projectId"))
        .or_else(|| config.linear_teams.get(text(data, "/teamId")))
        .copied()
        .or(routed)
        .or_else(|| {
            config
                .linear_webhooks
                .get(text(payload, "/webhookId"))
                .copied()
        });
    let Some(channel) = channel else {
        if kind == "Comment" {
            bail!("Linear issue route is unavailable; deliver the issue before its comments");
        }
        return Ok(vec![]);
    };
    let id = required(text(data, "/id"))?;
    let issue = if kind == "Comment" {
        required(text(data, "/issueId"))?
    } else {
        id
    };
    let url = required(text(payload, "/url"))?
        .split('#')
        .next()
        .unwrap_or_default()
        .to_owned();
    let reference = if text(data, "/identifier").is_empty() {
        issue
    } else {
        text(data, "/identifier")
    };
    let actor = text(payload, "/actor/name");
    let mut action = format!(
        "{}.{}",
        kind.to_ascii_lowercase(),
        required(text(payload, "/action"))?
    );
    check_labels(&data["labelIds"])?;
    check_labels(&data["labels"])?;
    let label_entries: Vec<(String, String)> = if let Some(ids) = data["labelIds"].as_array() {
        ids.iter()
            .take(16)
            .filter_map(Value::as_str)
            .map(|id| {
                let enriched = data["labels"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|label| text(label, "/id") == id)
                    .map(|label| text(label, "/name"));
                let name = enriched
                    .filter(|name| !name.is_empty())
                    .or_else(|| config.linear_labels.get(id).map(String::as_str))
                    .unwrap_or(id);
                (id.to_owned(), short(name, 256))
            })
            .collect()
    } else {
        data["labels"]
            .as_array()
            .into_iter()
            .flatten()
            .take(16)
            .map(|label| {
                (
                    text(label, "/id").to_owned(),
                    short(text(label, "/name"), 256),
                )
            })
            .collect()
    };
    let mut fields: Vec<_> = label_entries
        .iter()
        .map(|(_, name)| ("labels".into(), name.clone()))
        .collect();
    let previous = payload.pointer("/updatedFrom/labelIds");
    for (id, name) in label_entries {
        let added = text(payload, "/action") == "create"
            || (text(payload, "/action") == "update"
                && previous.is_some_and(|old| {
                    old.is_null()
                        || old
                            .as_array()
                            .is_some_and(|ids| !ids.contains(&Value::String(id.clone())))
                }));
        if added {
            fields.push(("added-labels".into(), name));
            if text(payload, "/action") == "update" {
                action = "issue.labeled".into();
            }
        }
    }
    let mut item = base(
        "linear",
        channel,
        delivery,
        (format!("issue:{issue}"), url, reference.into()),
        action.clone(),
        id.into(),
        actor,
    );
    item.fields
        .iter_mut()
        .filter(|(name, _)| name == "url")
        .for_each(|(_, url)| *url = short(text(payload, "/url"), 4096));
    item.fields.extend(fields);
    for (field, pointer) in [
        ("team", "/teamId"),
        ("title", "/title"),
        ("state", "/state/name"),
    ] {
        let value = text(data, pointer);
        if !value.is_empty() {
            item.fields.push((field.into(), short(value, 256)));
        }
    }
    item.fields.push(("ref".into(), reference.into()));
    item.content = format!(
        "Linear {} — {action} by {actor}\n{}\n{}\n{}",
        item.reference,
        short(text(data, "/title"), 256),
        short(
            if kind == "Comment" {
                text(data, "/body")
            } else {
                text(data, "/description")
            },
            6000
        ),
        item.thread_url
    );
    Ok(vec![item])
}

pub(crate) fn slack(config: &Config, payload: &Value, delivery: &str) -> Result<Vec<Occurrence>> {
    let event = &payload["event"];
    if text(event, "/type") != "message" || text(event, "/channel_type") != "channel" {
        return Ok(vec![]);
    };
    let team = required(text(payload, "/team_id"))?;
    let channel = required(text(event, "/channel"))?;
    let Some(buzz_channel) = config
        .slack_channels
        .get(&format!("{team}/{channel}"))
        .copied()
    else {
        return Ok(vec![]);
    };
    let subtype = text(event, "/subtype");
    if !subtype.is_empty() && subtype != "message_changed" {
        return Ok(vec![]);
    };
    let message = if subtype == "message_changed" {
        &event["message"]
    } else {
        event
    };
    let ts = required(text(message, "/ts"))?;
    if !ts.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        bail!("invalid Slack timestamp");
    }
    let root = if text(message, "/thread_ts").is_empty() {
        ts
    } else {
        text(message, "/thread_ts")
    };
    let url = format!(
        "{}/archives/{channel}/p{}",
        config.slack_workspace_url.trim_end_matches('/'),
        root.replace('.', "")
    );
    let action = if subtype == "message_changed" {
        "message.updated"
    } else {
        "message.created"
    };
    let mut item = base(
        "slack",
        buzz_channel,
        delivery,
        (
            format!("{team}:{channel}:{root}"),
            url,
            format!("{team}/{channel}"),
        ),
        action.into(),
        ts.into(),
        text(message, "/user"),
    );
    let occurrence_url = format!(
        "{}/archives/{channel}/p{}",
        config.slack_workspace_url.trim_end_matches('/'),
        ts.replace('.', "")
    );
    item.fields
        .iter_mut()
        .filter(|(name, _)| name == "url")
        .for_each(|(_, url)| *url = occurrence_url.clone());
    item.fields.extend([
        ("team".into(), team.into()),
        ("channel".into(), channel.into()),
    ]);
    item.content = format!(
        "Slack {} — {action} by {}\n{}\n{}",
        item.reference,
        text(message, "/user"),
        short(text(message, "/text"), 6000),
        item.thread_url
    );
    Ok(vec![item])
}
