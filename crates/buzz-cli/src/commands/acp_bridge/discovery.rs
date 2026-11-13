//! Discover canonical threads from agent participation, including later joins.
//! The cursor orders participation events. A prefix check suppresses roots
//! already represented on earlier pages without a growing list of seen ids.
//! Writer reads keep a lagging replica from hiding a previously observed witness.
//! Each session reports the working directory of its recorded project, and a
//! `cwd` filter keeps only sessions with that directory (see `projects`).

use super::*;

const MAX_QUERIES: usize = 64;

struct Reads<'a, R> {
    relay: &'a R,
    remaining: usize,
}

impl<R: BridgeRelay> Reads<'_, R> {
    async fn query(&mut self, filter: Value) -> Result<Option<Vec<Event>>, CliError> {
        if self.remaining == 0 {
            return Ok(None);
        }
        self.remaining -= 1;
        self.relay.query(filter).await.map(Some)
    }
}

struct Position {
    time: u64,
    id: EventId,
}

impl Position {
    fn parse(raw: &str) -> Result<Self, CliError> {
        let (time, id) = parse_cursor(raw)?;
        Ok(Self { time, id })
    }

    fn contains(&self, event: &Event) -> bool {
        event.created_at.as_secs() > self.time
            || (event.created_at.as_secs() == self.time && event.id <= self.id)
    }
}

fn participation_filters(scope: &Scope) -> Vec<Value> {
    vec![
        mention_filter(scope),
        json!({"kinds":[9],"#h":[scope.channel.to_string()],"authors":[scope.agent.to_hex()],"limit":QUERY_PAGE,"consistency":"strong"}),
    ]
}

fn advance(filters: &mut [Value], after: &str) -> Result<(), CliError> {
    for filter in filters {
        apply_cursor(filter, after)?;
    }
    Ok(())
}

fn page(mut events: Vec<Event>) -> Result<Vec<Event>, CliError> {
    if events.len() > 2 * QUERY_PAGE {
        return Err(problem("relay exceeded participation page bound"));
    }
    // The HTTP bridge concatenates filter results, including overlap. Merge
    // before truncation so its per-filter limits cannot hide a global row.
    newest_first(&mut events);
    events.dedup_by_key(|event| event.id);
    events.truncate(QUERY_PAGE);
    Ok(events)
}

fn thread_id(event: &Event) -> Option<EventId> {
    match buzz_core::nip10::parse_thread_markers(&event.tags).resolve() {
        Some((root, _)) => EventId::from_hex(&root).ok(),
        None => Some(event.id),
    }
}

/// Has this thread already had a valid participation event at or above the
/// incoming cursor? SQL narrows by channel, e tag, and author/mention first.
async fn represented_before<R: BridgeRelay>(
    reads: &mut Reads<'_, R>,
    scope: &Scope,
    root: &Event,
    before: &Position,
) -> Result<Option<bool>, CliError> {
    if involves(scope, root) && before.contains(root) {
        return Ok(Some(true));
    }
    let mut filters = participation_filters(scope);
    for filter in &mut filters {
        filter["#e"] = json!([root.id.to_hex()]);
        filter["since"] = json!(before.time);
    }
    let mut previous = None;
    for _ in 0..MAX_SCAN / QUERY_PAGE {
        let Some(events) = reads.query(json!(filters)).await? else {
            return Ok(None);
        };
        let events = page(events)?;
        let full = events.len() == QUERY_PAGE;
        for event in &events {
            if !before.contains(event) {
                return Ok(Some(false));
            }
            if scoped(scope, event) && involves(scope, event) && thread_id(event) == Some(root.id) {
                return Ok(Some(true));
            }
        }
        if !full {
            return Ok(Some(false));
        }
        let next = cursor(
            events
                .last()
                .ok_or_else(|| problem("missing participation cursor"))?,
        );
        if previous.as_ref() == Some(&next) {
            return Err(problem("relay did not advance participation cursor"));
        }
        advance(&mut filters, &next)?;
        previous = Some(next);
    }
    Err(problem(
        "thread participation prefix exceeds 1000-event bound",
    ))
}

fn response(sessions: Vec<Value>, next: Option<String>) -> Value {
    match next {
        Some(next) => json!({"sessions":sessions,"nextCursor":next}),
        None => json!({"sessions":sessions}),
    }
}

fn budget_response(sessions: Vec<Value>, next: Option<String>) -> Result<Value, CliError> {
    if next.is_none() {
        return Err(problem(
            "session discovery read bound reached without progress; retry",
        ));
    }
    Ok(response(sessions, next))
}

/// List canonical roots in participation order, preserving a bounded scan position.
/// Roots outside the `cwd` filter still advance the cursor.
pub(super) async fn history_list<R: BridgeRelay>(
    relay: &R,
    scope: &Scope,
    params: &Value,
) -> Result<Value, CliError> {
    // A filter that is not an absolute path matches no session.
    let only = params
        .get("cwd")
        .and_then(Value::as_str)
        .map(|cwd| projects::filter_cwd(&scope.home, cwd));
    let mut fallback = None;
    let trusted = authors(scope);
    let raw = params
        .get("cursor")
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| problem("invalid session cursor"))
        })
        .transpose()?;
    let before = raw.map(Position::parse).transpose()?;
    let mut filters = participation_filters(scope);
    if let Some(raw) = raw {
        advance(&mut filters, raw)?;
    }
    let mut reads = Reads {
        relay,
        remaining: MAX_QUERIES,
    };
    let mut roots: HashMap<EventId, Option<Event>> = HashMap::new();
    let mut seen = HashSet::new();
    let mut sessions = Vec::new();
    let mut next: Option<String> = None;
    let mut previous = raw.map(str::to_owned);
    for _ in 0..MAX_SCAN / QUERY_PAGE {
        let Some(events) = reads.query(json!(filters)).await? else {
            return budget_response(sessions, next);
        };
        let events = page(events)?;
        let full = events.len() == QUERY_PAGE;
        let mut missing = HashSet::new();
        for event in &events {
            if !scoped(scope, event) || !involves(scope, event) {
                continue;
            }
            if root_for(scope, event) {
                roots.insert(event.id, Some(event.clone()));
            } else if let Some(id) = thread_id(event) {
                if !roots.contains_key(&id) {
                    missing.insert(id);
                }
            }
        }
        // A root present later in the same batch does not need an HTTP read.
        missing.retain(|id| !roots.contains_key(id));
        if !missing.is_empty() {
            let filter = json!({"kinds":[9],"#h":[scope.channel.to_string()],"ids":missing.iter().map(|id|id.to_hex()).collect::<Vec<_>>(),"limit":missing.len(),"consistency":"strong"});
            let Some(found) = reads.query(filter).await? else {
                return budget_response(sessions, next);
            };
            if found.len() > QUERY_PAGE {
                return Err(problem("relay exceeded thread-root lookup bound"));
            }
            for id in &missing {
                roots.insert(*id, None);
            }
            for root in found {
                if missing.contains(&root.id) && root_for(scope, &root) {
                    roots.insert(root.id, Some(root));
                }
            }
        }
        for event in events {
            if scoped(scope, &event) && involves(scope, &event) {
                if let Some(id) = thread_id(&event) {
                    if let Some(Some(root)) = roots.get(&id) {
                        if seen.insert(id) {
                            if let Some(before) = &before {
                                let Some(represented) =
                                    represented_before(&mut reads, scope, root, before).await?
                                else {
                                    return budget_response(sessions, next);
                                };
                                if represented {
                                    next = Some(cursor(&event));
                                    continue;
                                }
                            }
                            let cwd =
                                projects::session_cwd(&scope.home, root, &trusted, &mut fallback);
                            if only
                                .as_ref()
                                .is_some_and(|only| only.as_ref() != Some(&cwd))
                            {
                                next = Some(cursor(&event));
                                continue;
                            }
                            let title: String = root.content.chars().take(120).collect();
                            let updated = chrono::DateTime::from_timestamp(
                                i64::try_from(event.created_at.as_secs())
                                    .map_err(|_| problem("invalid session timestamp"))?,
                                0,
                            )
                            .ok_or_else(|| problem("invalid session timestamp"))?
                            .to_rfc3339();
                            sessions.push(json!({"sessionId":root.id.to_hex(),"cwd":cwd.to_string_lossy(),"title":title,"updatedAt":updated}));
                        }
                    }
                }
            }
            next = Some(cursor(&event));
            if sessions.len() == LIST_PAGE {
                return Ok(response(sessions, next));
            }
        }
        if !full {
            return Ok(response(sessions, None));
        }
        let after = next
            .as_ref()
            .ok_or_else(|| problem("missing session cursor"))?;
        if previous.as_ref() == Some(after) {
            return Err(problem("relay did not advance session cursor"));
        }
        advance(&mut filters, after)?;
        previous = Some(after.clone());
    }
    Ok(response(sessions, next))
}
