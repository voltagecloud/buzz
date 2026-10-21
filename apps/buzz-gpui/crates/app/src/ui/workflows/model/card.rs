//! What a landing card shows (`workflowCardPresentation.ts`): the trigger
//! icon (or its emoji), up to three action icons, and a one-sentence
//! description.

use super::yaml::{self, get};
use serde_yaml::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CardIcon {
    Delay,
    Diff,
    Message,
    Reaction,
    Schedule,
    Webhook,
    Workflow,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Presentation {
    /// Action icons with their step keys.
    pub actions: Vec<(CardIcon, String)>,
    pub description: String,
    pub trigger_emoji: Option<String>,
    pub trigger_icon: CardIcon,
}

fn trigger(trigger: Option<&serde_yaml::Mapping>) -> (CardIcon, &'static str, Option<String>) {
    let on = trigger.and_then(|t| get(t, "on")).and_then(Value::as_str);
    match on {
        Some("message_posted") => (CardIcon::Message, "When a message is posted", None),
        Some("reaction_added") => (
            CardIcon::Reaction,
            "When a reaction is added",
            trigger
                .and_then(|t| get(t, "emoji"))
                .and_then(Value::as_str)
                .map(str::to_owned),
        ),
        Some("diff_posted") => (CardIcon::Diff, "When a diff is posted", None),
        Some("schedule") => (CardIcon::Schedule, "On a schedule", None),
        Some("webhook") => (CardIcon::Webhook, "When a webhook is received", None),
        _ => (CardIcon::Workflow, "When its trigger matches", None),
    }
}

fn action(action: Option<&Value>) -> (CardIcon, &'static str) {
    match action.and_then(Value::as_str) {
        Some("send_message" | "send_dm") => (CardIcon::Message, "send a message"),
        Some("delay") => (CardIcon::Delay, "wait"),
        Some("add_reaction") => (CardIcon::Reaction, "add a reaction"),
        Some("call_webhook") => (CardIcon::Webhook, "call a webhook"),
        _ => (CardIcon::Workflow, "run an action"),
    }
}

/// `a`, `a and b`, `a, b, and c`.
fn join(labels: &[&str]) -> String {
    match labels {
        [] => "run no actions".into(),
        [one] => (*one).into(),
        [one, two] => format!("{one} and {two}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}

pub fn presentation(text: &str) -> Presentation {
    let Ok(document) = yaml::parse(text) else {
        return Presentation {
            actions: Vec::new(),
            description: "Review this workflow configuration.".into(),
            trigger_emoji: None,
            trigger_icon: CardIcon::Workflow,
        };
    };
    let document = document.as_mapping();
    let (icon, label, emoji) = trigger(
        document
            .and_then(|d| get(d, "trigger"))
            .and_then(Value::as_mapping),
    );
    let actions: Vec<(CardIcon, &str, String)> = document
        .and_then(|d| get(d, "steps"))
        .and_then(Value::as_sequence)
        .map(|steps| {
            steps
                .iter()
                .enumerate()
                .map(|(index, step)| {
                    let fields = step.as_mapping();
                    let (icon, label) = action(fields.and_then(|f| get(f, "action")));
                    let key = fields
                        .and_then(|f| get(f, "id"))
                        .and_then(Value::as_str)
                        .map_or_else(|| format!("unidentified-{index}"), str::to_owned);
                    (icon, label, key)
                })
                .collect()
        })
        .unwrap_or_default();
    let labels: Vec<&str> = actions.iter().map(|(_, label, _)| *label).collect();
    Presentation {
        description: format!("{label}, {}.", join(&labels)),
        actions: actions
            .into_iter()
            .map(|(icon, _, key)| (icon, key))
            .collect(),
        trigger_emoji: emoji,
        trigger_icon: icon,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn describes_the_trigger_and_actions() {
        let card = presentation(
            "name: x\ntrigger:\n  on: reaction_added\n  emoji: 👀\nsteps:\n  - id: a\n    action: send_message\n  - id: b\n    action: delay\n  - action: call_webhook\n",
        );
        assert_eq!(card.trigger_icon, CardIcon::Reaction);
        assert_eq!(card.trigger_emoji.as_deref(), Some("👀"));
        assert_eq!(
            card.description,
            "When a reaction is added, send a message, wait, and call a webhook."
        );
        assert_eq!(
            card.actions,
            [
                (CardIcon::Message, "a".to_string()),
                (CardIcon::Delay, "b".to_string()),
                (CardIcon::Webhook, "unidentified-2".to_string()),
            ]
        );
        assert_eq!(
            presentation("trigger: {on: schedule}\nsteps: [{action: delay}, {action: x}]")
                .description,
            "On a schedule, wait and run an action."
        );
        assert_eq!(
            presentation("trigger: {on: webhook}").description,
            "When a webhook is received, run no actions."
        );
        assert_eq!(
            presentation("name: [").description,
            "Review this workflow configuration."
        );
        assert_eq!(
            presentation("- a").description,
            "When its trigger matches, run no actions."
        );
    }
}
