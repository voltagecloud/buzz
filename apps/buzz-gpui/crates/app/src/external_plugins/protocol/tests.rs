use super::*;
use core::prelude::v1::test;

#[test]
fn manifests_need_an_id_a_name_and_a_command() {
    let manifest = Manifest::parse(
        r#"{ "id": "com.example.hello", "name": "Hello", "command": ["python3", "plugin.py"] }"#,
    )
    .unwrap();
    assert_eq!(manifest.api_version, API_VERSION);
    assert_eq!(manifest.command_line(), "python3 plugin.py");
    assert!(manifest.permissions.is_empty());

    let manifest = Manifest::parse(
        r#"{ "id": "a.b", "name": "x", "command": ["a"],
             "permissions": ["send-messages", "send-messages"] }"#,
    )
    .unwrap();
    assert_eq!(manifest.permissions, [Permission::SendMessages]);

    for (text, reason) in [
        (
            r#"{ "id": "Hello", "name": "x", "command": ["a"] }"#,
            "not a plugin id",
        ),
        (
            r#"{ "id": "buzz.inbox", "name": "x", "command": ["a"] }"#,
            "reserved",
        ),
        (r#"{ "id": "a.b", "name": " ", "command": ["a"] }"#, "name"),
        (
            r#"{ "id": "a.b", "name": "x", "command": [] }"#,
            "no command",
        ),
        (
            r#"{ "id": "a.b", "name": "x", "command": ["a"], "apiVersion": 2 }"#,
            "API 2",
        ),
        (r#"{ "id": "a.b", "name": "x" }"#, "command"),
        (
            r#"{ "id": "a.b", "name": "x", "command": ["a"], "permissions": ["root"] }"#,
            "unknown variant",
        ),
    ] {
        let error = Manifest::parse(text).unwrap_err();
        assert!(error.contains(reason), "{text}: {error}");
    }
}

#[test]
fn the_disclosure_quotes_arguments_with_spaces() {
    let manifest =
        Manifest::parse(r#"{ "id": "a.b", "name": "x", "command": ["node", "my plugin.js", ""] }"#)
            .unwrap();
    assert_eq!(manifest.command_line(), r#"node "my plugin.js" """#);
}

#[test]
fn contributions_are_checked() {
    let parsed = Contributions::parse(json!({
        "panels": [{
            "id": "weather",
            "title": "Weather",
            "matches": ["https://weather.example/"],
            "launcher": { "placement": "bottom" },
        }],
        "pages": [{ "id": "home", "title": "Home", "icon": "House" }],
    }))
    .unwrap();
    assert_eq!(
        parsed.panels[0].launcher,
        Some(LauncherDecl {
            label: None,
            placement: LauncherPlacement::Bottom,
            target: None,
        })
    );
    assert_eq!(parsed.pages[0].icon.as_deref(), Some("House"));
    assert_eq!(
        Contributions::parse(json!({})).unwrap(),
        Contributions::default()
    );

    for (value, reason) in [
        (
            json!({ "pages": [{ "id": "Home", "title": "x" }] }),
            "not a contribution id",
        ),
        (
            json!({ "pages": [{ "id": "a", "title": "x" }], "panels": [{ "id": "a", "title": "y" }] }),
            "twice",
        ),
        (json!({ "pages": [{ "id": "a", "title": "" }] }), "title"),
        (
            json!({ "panels": [{ "id": "a", "title": "x", "matches": ["h"] }] }),
            "too broadly",
        ),
        (json!({ "pages": "nope" }), "not valid"),
    ] {
        let error = Contributions::parse(value.clone()).unwrap_err();
        assert!(error.contains(reason), "{value}: {error}");
    }
}

#[test]
fn a_notification_category_may_share_its_pages_id() {
    // As Reminders' page and category are both `buzz.reminders/reminders`.
    let contributions = Contributions::parse(json!({
        "pages": [{ "id": "reminders", "title": "Reminders" }],
        "notifications": [{ "id": "reminders", "label": "Reminders" }],
    }))
    .unwrap();
    assert_eq!(contributions.notifications.len(), 1);
}

#[test]
fn trees_parse_with_defaults_and_unknown_elements() {
    let tree = Node::parse(json!({
        "type": "column",
        "children": [
            { "type": "text", "text": "Hi", "style": "heading" },
            { "type": "button", "id": "go", "label": "Go" },
            { "type": "hologram", "id": "x" },
            { "type": "row", "justify": "between", "children": [{ "type": "divider" }] },
            { "type": "input", "id": "note", "clearOnSubmit": true, "version": 2 },
        ],
    }))
    .unwrap();
    let Node::Column { children, gap, .. } = tree else {
        panic!("not a column");
    };
    assert_eq!(gap, Gap::Sm);
    assert_eq!(
        children[0],
        Node::Text {
            text: "Hi".into(),
            style: TextStyle::Heading,
            tone: Tone::Default,
            truncate: false,
            tooltip: None,
        }
    );
    assert!(matches!(
        &children[1],
        Node::Button {
            variant: ButtonStyle::Subtle,
            disabled: false,
            ..
        }
    ));
    assert_eq!(children[2], Node::Unknown);
    assert_eq!(
        children[4],
        Node::Input {
            id: "note".into(),
            value: None,
            placeholder: None,
            version: 2,
            clear_on_submit: true,
            disabled: false,
            grow: false,
        }
    );
}

#[test]
fn trees_are_bounded() {
    let mut deep = json!({ "type": "divider" });
    for _ in 0..40 {
        deep = json!({ "type": "column", "children": [deep] });
    }
    assert!(Node::parse(deep).unwrap_err().contains("nested"));

    let wide: Vec<Value> = (0..6_000).map(|_| json!({ "type": "divider" })).collect();
    let error = Node::parse(json!({ "type": "column", "children": wide })).unwrap_err();
    assert!(error.contains("elements"), "{error}");

    let error = Node::parse(json!({ "type": "text" })).unwrap_err();
    assert!(error.contains("not valid"), "{error}");
}

#[test]
fn lines_are_requests_notifications_or_responses() {
    assert_eq!(
        Incoming::parse(r#"{"jsonrpc":"2.0","id":"a","method":"ui/toast","params":{"title":"x"}}"#)
            .unwrap(),
        Incoming::Request {
            id: json!("a"),
            method: "ui/toast".into(),
            params: json!({ "title": "x" }),
        }
    );
    assert_eq!(
        Incoming::parse(r#"{"jsonrpc":"2.0","method":"view/render"}"#).unwrap(),
        Incoming::Notification {
            method: "view/render".into(),
            params: Value::Null,
        }
    );
    assert_eq!(
        Incoming::parse(r#"{"jsonrpc":"2.0","id":3,"result":{"pages":[]}}"#).unwrap(),
        Incoming::Response {
            id: 3,
            result: Ok(json!({ "pages": [] })),
        }
    );
    assert_eq!(
        Incoming::parse(r#"{"jsonrpc":"2.0","id":4,"error":{"code":1,"message":"no"}}"#).unwrap(),
        Incoming::Response {
            id: 4,
            result: Err("no".into()),
        }
    );
    assert!(Incoming::parse("hello").is_err());
    assert!(Incoming::parse(r#"{"id":"x","result":1}"#).is_err());
    assert!(Incoming::parse("{}").is_err());
}

#[test]
fn messages_are_single_lines() {
    let line = request(1, "initialize", json!({ "text": "a\nb" }));
    assert!(!line.contains('\n'));
    assert_eq!(
        serde_json::from_str::<Value>(&response(&json!(7), Err((METHOD_NOT_FOUND, "no".into()))))
            .unwrap(),
        json!({ "jsonrpc": "2.0", "id": 7, "error": { "code": -32601, "message": "no" } })
    );
}

#[test]
fn every_kind_of_contribution_is_checked() {
    let parsed = Contributions::parse(json!({
        "settingsCards": [{ "id": "card", "title": "Card" }],
        "notifications": [{ "id": "alerts", "label": "Alerts" }],
        "messageActions": [{ "id": "act", "title": "Act", "kinds": [9, 1], "mine": true }],
        "linkRenderers": [{ "id": "links", "matches": ["https://example.com/"] }],
        "messageRenderers": [{ "id": "cards", "prefix": "!card" }],
        "attachmentRenderers": [{ "id": "pdf", "mime": ["application/pdf"] }],
        "composerTools": [{ "id": "tool", "title": "Tool" }],
        "shortcuts": [{ "id": "go", "title": "Go", "keys": ["mod+shift+g"] }],
        "completions": [{ "id": "commands", "title": "Commands", "trigger": "/" }],
        "inlineRenderers": [{ "id": "tickets", "pattern": "BUZZ-[0-9]+" }],
        "accountActions": [{ "id": "about", "title": "About", "icon": "Smiley" }],
        "composerAccessories": [{ "id": "note", "title": "Note", "order": 2 }],
        "pages": [{ "id": "top", "title": "Top", "placement": "topbar" }],
        "panels": [{ "id": "p", "title": "P", "menu": { "label": "Open P" } }],
    }))
    .unwrap();
    assert_eq!(parsed.settings_cards[0].placement, "community");
    assert_eq!(parsed.pages[0].placement, PagePlacement::Topbar);
    assert!(parsed.message_actions[0].mine);
    assert_eq!(parsed.composer_accessories[0].order, 2);
    assert_eq!(parsed.account_actions[0].icon.as_deref(), Some("Smiley"));

    for (value, reason) in [
        (
            json!({ "linkRenderers": [{ "id": "l", "matches": [] }] }),
            "too broadly",
        ),
        (
            json!({ "attachmentRenderers": [{ "id": "a", "mime": [] }] }),
            "MIME",
        ),
        (
            json!({ "shortcuts": [{ "id": "s", "title": "S", "keys": ["shift+"] }] }),
            "has no key",
        ),
        (
            json!({ "shortcuts": [{ "id": "s", "title": "S", "keys": ["hyper+k"] }] }),
            "not a modifier",
        ),
        (
            json!({ "agentTypes": [{ "id": "t", "title": "T",
                "tabs": [{ "id": "attention", "title": "A" }] }] }),
            "not a tab id",
        ),
        (
            json!({ "agentTypes": [{ "id": "t", "title": "T",
                "tabs": [{ "id": "a", "title": "A" }, { "id": "a", "title": "B" }] }] }),
            "twice",
        ),
        (
            json!({ "agentTypes": [{ "id": "t", "title": "T",
                "tabs": [{ "id": "settings", "title": "S" }] }] }),
            "not a tab id",
        ),
        (
            json!({ "agentTypes": [{ "id": "t", "title": "T", "tabs": [
                { "id": "a", "title": "A", "role": "settings" },
                { "id": "b", "title": "B", "role": "settings" }] }] }),
            "more than one Settings tab",
        ),
        (
            json!({ "inlineRenderers": [{ "id": "i", "pattern": "a*" }] }),
            "empty",
        ),
        (
            json!({ "inlineRenderers": [{ "id": "i", "pattern": "(" }] }),
            "not valid",
        ),
        (
            json!({ "notifications": [{ "id": "x", "label": "X" }, { "id": "x", "label": "Y" }] }),
            "twice",
        ),
    ] {
        let error = Contributions::parse(value.clone()).unwrap_err();
        assert!(error.contains(reason), "{value}: {error}");
    }
    assert!(
        Contributions::parse(json!({ "messageRenderers": [{ "id": "k", "kinds": [30023] }] }))
            .is_ok()
    );
}

#[test]
fn chords_parse() {
    assert_eq!(
        Chord::parse("mod+shift+K").unwrap(),
        Chord {
            key: "K".into(),
            primary: true,
            shift: true,
            alt: false
        }
    );
    assert!(Chord::parse("alt+1").unwrap().alt);
    assert!(Chord::parse("mod+").is_err());
    assert!(!Chord::parse("j").unwrap().primary);
    assert!(!Chord::parse("shift+?").unwrap().shift);
    assert!(Chord::parse("shift+j").unwrap().shift);
    assert_eq!(Chord::parse("mod++").unwrap().key, "+");
    assert_eq!(Chord::parse("+").unwrap().key, "+");
}

#[test]
fn agent_types_name_a_settings_tab_and_migration_settings() {
    let parsed = Contributions::parse(json!({ "agentTypes": [{
        "id": "t",
        "title": "T",
        "tabs": [{ "id": "main", "title": "Settings", "role": "settings" }],
        "migrationSettings": [{ "id": "m", "label": "Model", "key": "model", "default": "a" }],
    }] }))
    .unwrap();
    let kind = &parsed.agent_types[0];
    assert_eq!(kind.tabs[0].role, Some(AgentTabRole::Settings));
    assert_eq!(kind.migration_settings[0].key, "model");
    assert_eq!(kind.migration_settings[0].default, Some(json!("a")));
}
