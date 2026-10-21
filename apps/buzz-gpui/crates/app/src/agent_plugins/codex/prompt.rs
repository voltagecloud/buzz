//! What Codex is told (buzz-app's `prompt.ts`): the Buzz base prompt as
//! developer instructions, which keep Codex's own coding prompt, and each
//! request as quoted event data.

use super::config::Config;
use crate::agent_plugins::claude_code::prompt::escaped;
use crate::agent_plugins::harness::{base_prompt, core_memory_section};
use crate::buzz_mcp::BuzzEvent;
use serde::Serialize;

/// Where a request belongs: a channel, and the thread when each thread is
/// its own session.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Conversation {
    pub channel_id: String,
    pub name: String,
    pub root: Option<String>,
    /// An owner's Me chat: one channel conversation, posted at its top
    /// level.
    pub top_level: bool,
}

/// JSON with `<` and `>` escaped, so quoted data cannot close a section.
pub fn quoted(value: &impl Serialize) -> String {
    escaped(&serde_json::to_string(value).unwrap_or_default())
}

pub(crate) use crate::buzz_mcp::tools::root_of;

/// The Buzz guidance a Codex thread starts with.
pub fn developer_instructions(name: &str, pubkey: &str) -> String {
    format!(
        "You are {name} (pubkey {pubkey}), a coding agent in Buzz.
The buzz namespace provides the shared Buzz tools, including messages, reactions, reads, uploads, canvas, and encrypted memory. Buzz access and signing are handled by the host. Do not invoke the buzz CLI or look for credentials. Use Codex's native coding tools in the workspace.

{base}

{buzz}",
        base = base_prompt(),
        buzz = crate::buzz_mcp::BUZZ_PROMPT.trim(),
    )
}

/// Codex truncates the middle of any additional-context value over 1,000
/// tokens, which it estimates as 4 UTF-8 bytes each.
const CONTEXT_VALUE_BYTES: usize = 4_000;

/// The agent's current identity, instructions, team sections (empty for
/// none) and core memory (`Some(None)` when it has none, `None` if unread)
/// as turn context: a resumed Codex
/// thread keeps its original developer instructions. Codex orders values
/// by key, so text over its per-value limit continues in
/// `buzz.instructions.001` onward, split at line breaks where possible.
pub fn turn_context(
    name: &str,
    pubkey: &str,
    settings: &Config,
    team: &str,
    memory: Option<Option<&str>>,
) -> serde_json::Map<String, serde_json::Value> {
    let core = core_memory_section(memory);
    let text = format!(
        "You are {name} (pubkey {pubkey}).\nCurrent Buzz custom instructions replace all prior Buzz custom instructions. An empty section means no custom instructions apply.\n<agent-instructions>\n{}\n</agent-instructions>\n\nCurrent team sections replace all prior team sections. None means you are on no team.{}{}",
        settings.instructions,
        if team.is_empty() {
            String::new()
        } else {
            format!("\n{team}")
        },
        if core.is_empty() {
            String::new()
        } else {
            format!("\n\nCurrent core memory replaces all prior core memory.\n{core}")
        }
    );
    let mut values = vec![String::new()];
    let mut size = 0;
    for line in text.split_inclusive('\n') {
        // A line over the limit splits by character.
        let pieces: Vec<&str> = if line.len() > CONTEXT_VALUE_BYTES {
            line.char_indices()
                .map(|(at, char)| &line[at..at + char.len_utf8()])
                .collect()
        } else {
            vec![line]
        };
        for piece in pieces {
            if size + piece.len() > CONTEXT_VALUE_BYTES {
                values.push(String::new());
                size = 0;
            }
            values.last_mut().expect("one value").push_str(piece);
            size += piece.len();
        }
    }
    values
        .into_iter()
        .enumerate()
        .map(|(index, value)| {
            let key = if index == 0 {
                "buzz.instructions".to_string()
            } else {
                format!("buzz.instructions.{index:03}")
            };
            (
                key,
                serde_json::json!({ "kind": "application", "value": value }),
            )
        })
        .collect()
}

#[derive(Serialize)]
struct Context<'a> {
    channel_id: &'a str,
    channel_name: &'a str,
    session_scope: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_thread: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    posting: Option<&'a str>,
}

/// One request as Codex reads it; `steering` marks a follow-up to work in
/// progress.
pub fn turn_input(
    event: &BuzzEvent,
    conversation: &Conversation,
    text: &str,
    interest: &str,
    steering: bool,
) -> String {
    let section = if steering {
        "new-message-arrived-while-you-were-working"
    } else {
        "buzz-event"
    };
    let context = Context {
        channel_id: &conversation.channel_id,
        channel_name: &conversation.name,
        session_scope: if conversation.root.is_some() {
            "thread"
        } else {
            "channel"
        },
        session_thread: conversation
            .root
            .as_deref()
            .filter(|_| !conversation.top_level),
        reply_to: (!conversation.top_level).then(|| root_of(event)),
        posting: conversation.top_level.then_some("top-level"),
    };
    let top_level = if conversation.top_level {
        format!("{}\n", crate::buzz_mcp::TOP_LEVEL_PROMPT)
    } else {
        String::new()
    };
    let interest = if interest.is_empty() {
        String::new()
    } else {
        format!("\n<interest>\n{}\n</interest>", quoted(&interest))
    };
    format!(
        "<context>\n{}\n</context>\n{top_level}<{section}>\n{}\nRequest: {}\n</{section}>{interest}",
        quoted(&context),
        quoted(event),
        quoted(&text),
    )
}

/// The Codex thread's name, so a session started in Buzz is easy to find:
/// its [`session_title`], then its [`session_tag`].
pub fn session_name(agent: &str, pubkey: &str, conversation: &Conversation) -> String {
    format!(
        "{} · {}",
        session_title(agent, conversation),
        session_tag(
            pubkey,
            &conversation.channel_id,
            conversation.root.as_deref()
        )
    )
}

/// Names the agent's conversation in its thread names, so its threads can
/// be found from any copy of the app, each of which saves its own
/// bindings: `buzz:` and a 64-bit FNV-1a hash of the agent and the
/// conversation.
pub fn session_tag(pubkey: &str, channel_id: &str, root: Option<&str>) -> String {
    let key =
        serde_json::to_string(&[pubkey, channel_id, root.unwrap_or("channel")]).unwrap_or_default();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in key.bytes() {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
    }
    format!("buzz:{hash:016x}")
}

/// The readable part of a conversation's thread name, which earlier
/// versions used alone: `Buzz #<channel> · thread <root prefix> · <agent>`.
pub fn session_title(agent: &str, conversation: &Conversation) -> String {
    // Each run of whitespace becomes one space, as `/\s+/g` does.
    let mut channel = String::new();
    for char in conversation.name.chars() {
        if !char.is_whitespace() {
            channel.push(char);
        } else if !channel.ends_with(' ') {
            channel.push(' ');
        }
    }
    let channel: String = channel.chars().take(100).collect();
    let thread = conversation.root.as_ref().map_or_else(String::new, |root| {
        format!(" · thread {}", root.chars().take(8).collect::<String>())
    });
    format!("Buzz #{channel}{thread} · {agent}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(tags: Vec<Vec<String>>) -> BuzzEvent {
        BuzzEvent {
            id: "1".repeat(64),
            pubkey: "a".repeat(64),
            created_at: 10,
            kind: 9,
            tags,
            content: "@Codex <b>hi</b>".into(),
            sig: String::new(),
        }
    }

    #[test]
    fn quotes_requests_with_their_conversation() {
        let root = "c".repeat(64);
        let reply = event(vec![
            vec!["h".into(), "chan".into()],
            vec!["e".into(), root.clone(), String::new(), "reply".into()],
        ]);
        let thread = Conversation {
            channel_id: "chan".into(),
            name: "general".into(),
            root: Some(root.clone()),
            top_level: false,
        };
        let input = turn_input(&reply, &thread, "<b>hi</b>", "", false);
        assert!(input.starts_with(&format!(
            "<context>\n{{\"channel_id\":\"chan\",\"channel_name\":\"general\",\"session_scope\":\"thread\",\"session_thread\":\"{root}\",\"reply_to\":\"{root}\"}}\n</context>\n<buzz-event>\n"
        )));
        assert!(input.ends_with("\nRequest: \"\\u003cb\\u003ehi\\u003c/b\\u003e\"\n</buzz-event>"));
        assert!(!input.contains("<b>"));
        let channel = Conversation {
            root: None,
            ..thread
        };
        let steer = turn_input(&reply, &channel, "more", "Watch builds", true);
        assert!(steer.contains("\"session_scope\":\"channel\""));
        assert!(!steer.contains("session_thread"));
        assert!(steer.contains("<new-message-arrived-while-you-were-working>\n"));
        assert!(steer.ends_with("\n<interest>\n\"Watch builds\"\n</interest>"));
        // A top-level request is its own thread.
        let top = event(vec![vec!["h".into(), "chan".into()]]);
        assert!(
            turn_input(&top, &channel, "x", "", false)
                .contains(&format!("\"reply_to\":\"{}\"", "1".repeat(64)))
        );
    }

    #[test]
    fn names_sessions_and_supplies_custom_instructions() {
        let conversation = Conversation {
            channel_id: "chan".into(),
            name: " team\n  chat ".into(),
            root: Some("abcdef0123456789".into()),
            top_level: false,
        };
        assert_eq!(
            session_title("Codex Test", &conversation),
            "Buzz # team chat  · thread abcdef01 · Codex Test"
        );
        let tag = session_tag("b", "chan", Some("abcdef0123456789"));
        assert_eq!(
            session_name("Codex Test", "b", &conversation),
            format!("Buzz # team chat  · thread abcdef01 · Codex Test · {tag}")
        );
        let channel = Conversation {
            root: None,
            name: "x".repeat(120),
            ..conversation
        };
        assert_eq!(
            session_title("C", &channel),
            format!("Buzz #{} · C", "x".repeat(100))
        );
        // Stable across copies of the app, and apart per agent and place.
        assert_eq!(tag, session_tag("b", "chan", Some("abcdef0123456789")));
        assert!(tag.starts_with("buzz:") && tag.len() == 21);
        assert_ne!(tag, session_tag("c", "chan", Some("abcdef0123456789")));
        assert_ne!(
            session_tag("b", "chan", None),
            session_tag("b", "chan", Some("channel2"))
        );
        // FNV-1a over the JSON key, as buzz-app hashes it.
        assert_eq!(tag, "buzz:118714305d2147cb");
        let base = developer_instructions("Codex", "b");
        assert!(base.starts_with("You are Codex (pubkey b), a coding agent in Buzz."));
        assert!(!base.contains("<agent-instructions>"));
        let settings = Config {
            instructions: "Use a marker".into(),
            ..Config::default()
        };
        let context = turn_context("Codex", "b", &settings, "", None);
        assert_eq!(context.len(), 1);
        assert_eq!(context["buzz.instructions"]["kind"], "application");
        assert!(value(&context).starts_with("You are Codex (pubkey b).\n"));
        assert!(
            value(&context)
                .ends_with("<agent-instructions>\nUse a marker\n</agent-instructions>\n\nCurrent team sections replace all prior team sections. None means you are on no team.")
        );
        assert!(
            value(&turn_context(
                "Codex",
                "b",
                &settings,
                "<team>\nT\n</team>",
                None
            ))
            .ends_with("None means you are on no team.\n<team>\nT\n</team>")
        );
    }

    #[test]
    fn core_memory_follows_the_custom_instructions() {
        let settings = Config::default();
        let with = |memory| value(&turn_context("Codex", "b", &settings, "", memory));
        assert!(with(Some(Some(" I am Codex. "))).ends_with(
            "None means you are on no team.\n\nCurrent core memory replaces all prior core memory.\n<core-memory>\nI am Codex.\n</core-memory>"
        ));
        assert!(with(Some(None)).contains(
            "Current core memory replaces all prior core memory.\n<core-memory>\nNo core memory found."
        ));
        assert!(!with(None).contains("<core-memory>"));
        assert!(!with(Some(Some("  "))).contains("<core-memory>"));
        // A thread's developer instructions carry none.
        assert!(!developer_instructions("Codex", "b").contains("<core-memory>"));
    }

    #[test]
    fn splits_long_instructions_so_codex_truncates_none() {
        // Lines of 100 bytes, one far over the limit, and wide characters.
        let lines = "x".repeat(99) + "\n";
        let instructions = format!(
            "{}{}\n{}",
            lines.repeat(60),
            "y".repeat(9_000),
            "é".repeat(3_000)
        );
        let settings = Config {
            instructions: instructions.clone(),
            ..Config::default()
        };
        let context = turn_context("Codex", "b", &settings, "", None);
        let keys: Vec<&String> = context.keys().collect();
        assert_eq!(keys[0], "buzz.instructions");
        assert_eq!(keys[1], "buzz.instructions.001");
        let joined: String = context
            .values()
            .map(|value| value["value"].as_str().unwrap())
            .collect();
        assert!(joined.contains(&instructions), "nothing is lost");
        for value in context.values() {
            assert!(value["value"].as_str().unwrap().len() <= CONTEXT_VALUE_BYTES);
        }
        // Whole lines stay together where they fit.
        let first = context["buzz.instructions"]["value"].as_str().unwrap();
        assert!(first.ends_with('\n'));
    }

    /// The first turn-context value's text.
    fn value(context: &serde_json::Map<String, serde_json::Value>) -> String {
        context["buzz.instructions"]["value"]
            .as_str()
            .unwrap()
            .to_string()
    }
}
