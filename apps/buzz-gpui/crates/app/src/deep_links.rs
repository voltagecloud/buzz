//! `buzz://` links from message bodies, the OS and other instances
//! (`docs/deep-links.md` in buzz-app).
//!
//! Only the link forms are parsed here. A link carries no community, so a
//! conversation binds to the selected one, and a valid address is never
//! authorization: the session still decides what the viewer may read.

use buzz_relay_client::projects::EntityRoute;
use buzz_relay_client::{EventId, RelayOrigin, parse_buzz_link};
use url::Url;

/// What a `buzz://` link asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Link {
    /// `buzz://channel/<id>[/<event>]` or
    /// `buzz://message?channel=<id>&id=<event>[&thread=<root>]`. The thread
    /// is a hint only; the root is re-derived from signed ancestry.
    Conversation {
        channel: String,
        message: Option<EventId>,
        thread: Option<EventId>,
    },
    /// `buzz://join?relay=<url>&code=<code>`.
    Join { relay: RelayOrigin, code: String },
    /// `buzz://repo|project|pr|issue?owner=&d=[&tab=&commit=|&id=]`, which
    /// the Projects page opens in the selected community.
    Entity(EntityRoute),
}

/// Parses one of the accepted forms; anything else, unknown hosts included,
/// is `None`. The scheme is matched in any case, as a URL parser does
/// (`isBuzzLink`), so `BUZZ://channel/x` in a message opens; links from the
/// OS stay canonical ([`from_args`]).
pub fn parse(href: &str) -> Option<Link> {
    let href = match href.get(..5) {
        Some(scheme) if scheme.eq_ignore_ascii_case("buzz:") => format!("buzz:{}", &href[5..]),
        _ => return None,
    };
    let href = href.as_str();
    if let Some(link) = parse_buzz_link(href) {
        return Some(Link::Conversation {
            channel: link.channel,
            message: link.message,
            thread: link.thread,
        });
    }
    if let Some(route) = EntityRoute::parse_link(href) {
        return Some(Link::Entity(route));
    }
    join(href)
}

/// `parseInviteLink`: one relay, one code of at most 256 characters, at
/// most one policy receipt; other parameters are ignored.
fn join(href: &str) -> Option<Link> {
    if href.len() > 4096 || !(href.starts_with("buzz://join?") || href.starts_with("buzz://join/?"))
    {
        return None;
    }
    let url = Url::parse(href).ok()?;
    if url.host_str() != Some("join")
        || !matches!(url.path(), "" | "/")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    let all = |key: &str| {
        url.query_pairs()
            .filter(|(name, _)| name == key)
            .map(|(_, value)| value.into_owned())
            .collect::<Vec<_>>()
    };
    let (relay, code, receipts) = (all("relay"), all("code"), all("policy_receipt"));
    if relay.len() != 1
        || code.len() != 1
        || receipts.len() > 1
        || code[0].is_empty()
        || code[0].chars().count() > 256
        || receipts.first().is_some_and(String::is_empty)
    {
        return None;
    }
    let relay = RelayOrigin::parse(&relay[0]).ok()?;
    Some(Link::Join {
        relay,
        code: code[0].clone(),
    })
}

/// Registers this binary for `buzz://` links in the user's applications,
/// as buzz-app's deep-link plugin repairs portable and unpackaged
/// installs at launch. Development builds leave any installed handler
/// alone. On macOS the bundle's Info.plist registers it.
pub fn register_scheme() {
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    if !cfg!(debug_assertions) {
        register_desktop_file();
    }
    #[cfg(windows)]
    if !cfg!(debug_assertions) {
        register_url_protocol();
    }
}

/// Packaged installs register through their desktop file.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn register_desktop_file() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    if exe.starts_with("/usr") || exe.starts_with("/opt") || exe.starts_with("/nix/store") {
        return;
    }
    let Some(applications) = dirs::data_dir().map(|dir| dir.join("applications")) else {
        return;
    };
    let spawned = std::thread::Builder::new()
        .name("buzz-scheme".into())
        .spawn(move || {
            let entry = handler_entry(&exe.to_string_lossy());
            let file = applications.join(HANDLER);
            if std::fs::read_to_string(&file).ok().as_deref() != Some(entry.as_str()) {
                let written = std::fs::create_dir_all(&applications)
                    .and_then(|()| std::fs::write(&file, &entry));
                if let Err(error) = written {
                    tracing::warn!(%error, "could not register buzz:// for this binary");
                    return;
                }
                let _ = std::process::Command::new("update-desktop-database")
                    .arg(&applications)
                    .status();
            }
            let set = std::process::Command::new("xdg-mime")
                .args(["default", HANDLER, "x-scheme-handler/buzz"])
                .status();
            if !set.is_ok_and(|status| status.success()) {
                tracing::warn!("could not make Buzz the buzz:// handler");
            }
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "could not register buzz://");
    }
}

/// The user desktop file that hands `buzz://` links to this binary.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
const HANDLER: &str = "buzz-gpui-handler.desktop";

#[cfg(any(target_os = "linux", target_os = "freebsd", test))]
fn handler_entry(exe: &str) -> String {
    // Exec quoting (Desktop Entry spec): quote the path, escape `"`, `` ` ``,
    // `$` and `\`, and double `%`.
    let mut quoted = String::from("\"");
    for c in exe.chars() {
        match c {
            '"' | '`' | '$' | '\\' => {
                quoted.push('\\');
                quoted.push(c);
            }
            '%' => quoted.push_str("%%"),
            _ => quoted.push(c),
        }
    }
    quoted.push('"');
    format!(
        "[Desktop Entry]\nType=Application\nName=Buzz\nExec={quoted} %u\nTerminal=false\n\
         MimeType=x-scheme-handler/buzz\nNoDisplay=true\n"
    )
}

/// The zip has no installer, so the user's URL protocol entry is written,
/// or repaired after the folder moved, at every launch.
#[cfg(windows)]
fn register_url_protocol() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let spawned = std::thread::Builder::new()
        .name("buzz-scheme".into())
        .spawn(move || {
            let exe = exe.to_string_lossy();
            let command = protocol_command(&exe);
            let classes = windows_registry::CURRENT_USER;
            let written = classes.create(r"Software\Classes\buzz").and_then(|key| {
                if key
                    .open(r"shell\open\command")
                    .and_then(|open| open.get_string(""))
                    .is_ok_and(|current| current == command)
                {
                    return Ok(());
                }
                key.set_string("", "URL:Buzz")?;
                key.set_string("URL Protocol", "")?;
                key.create("DefaultIcon")?
                    .set_string("", format!("\"{exe}\",0"))?;
                key.create(r"shell\open\command")?.set_string("", &command)
            });
            if let Err(error) = written {
                tracing::warn!(%error, "could not register buzz:// for this binary");
            }
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "could not register buzz://");
    }
}

/// The command Windows runs for a `buzz://` link. Windows paths cannot
/// hold `"`, so quoting is enough.
#[cfg(any(windows, test))]
fn protocol_command(exe: &str) -> String {
    format!("\"{exe}\" \"%1\"")
}

/// The `buzz:` URL on a command line, if any. The newest intent wins, so
/// the last one counts.
pub fn from_args(args: impl IntoIterator<Item = String>) -> Option<String> {
    args.into_iter()
        .filter(|arg| arg.len() <= 32_768 && arg.starts_with("buzz:"))
        .last()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(byte: u8) -> EventId {
        EventId::from_byte_array([byte; 32])
    }

    #[test]
    fn in_app_links_match_the_scheme_in_any_case() {
        let link = Some(Link::Conversation {
            channel: "general".into(),
            message: None,
            thread: None,
        });
        assert_eq!(parse("BUZZ://channel/general"), link);
        assert_eq!(parse("Buzz://channel/general"), link);
        assert_eq!(parse("buzzz://channel/general"), None);
        assert_eq!(parse("buz"), None);
        // The OS hands over canonical links only.
        assert_eq!(from_args(["BUZZ://channel/general".to_string()]), None);
    }

    #[test]
    fn the_handler_runs_this_binary() {
        let entry = handler_entry("/home/me/My Apps/buzz-gpui");
        assert!(entry.contains("Exec=\"/home/me/My Apps/buzz-gpui\" %u\n"));
        assert!(entry.contains("MimeType=x-scheme-handler/buzz\n"));
        assert!(handler_entry("/a/100%$x").contains("Exec=\"/a/100%%\\$x\" %u"));
    }

    #[test]
    fn the_windows_protocol_runs_this_binary() {
        assert_eq!(
            protocol_command(r"C:\Users\Me\Buzz\bin\buzz-gpui.exe"),
            r#""C:\Users\Me\Buzz\bin\buzz-gpui.exe" "%1""#
        );
    }

    #[test]
    fn parses_conversation_forms() {
        let message = id(1).to_hex();
        let root = id(2).to_hex();
        assert_eq!(
            parse("buzz://channel/general"),
            Some(Link::Conversation {
                channel: "general".into(),
                message: None,
                thread: None,
            })
        );
        assert_eq!(
            parse(&format!("buzz://channel/general/{message}")),
            Some(Link::Conversation {
                channel: "general".into(),
                message: Some(id(1)),
                thread: None,
            })
        );
        assert_eq!(
            parse(&format!(
                "buzz://message?channel=0e9d-217a&id={message}&thread={root}"
            )),
            Some(Link::Conversation {
                channel: "0e9d-217a".into(),
                message: Some(id(1)),
                thread: Some(id(2)),
            })
        );
        // Uppercase hex is accepted and normalized.
        assert_eq!(
            parse(&format!(
                "buzz://message?channel=c&id={}",
                message.to_uppercase()
            )),
            Some(Link::Conversation {
                channel: "c".into(),
                message: Some(id(1)),
                thread: None,
            })
        );
    }

    #[test]
    fn rejects_malformed_and_unknown_links() {
        let message = id(1).to_hex();
        for bad in [
            "buzz://channel/-bad".to_string(),
            "buzz://channel/a/b/c".to_string(),
            "buzz://message?channel=a".to_string(),
            format!("buzz://message?channel=a&id={message}&id={message}"),
            format!("buzz://message?channel=a&id={message}#x"),
            "buzz://repo?owner=x".to_string(),
            "buzz://settings".to_string(),
            "https://channel/general".to_string(),
            "buzz:".to_string(),
            String::new(),
        ] {
            assert_eq!(parse(&bad), None, "{bad}");
        }
    }

    #[test]
    fn parses_entity_links() {
        let owner = "ab".repeat(32);
        let id = "cd".repeat(32);
        let Some(Link::Entity(repo)) = parse(&format!(
            "buzz://repo?owner={}&d=demo&tab=files",
            owner.to_uppercase()
        )) else {
            panic!("a repository link");
        };
        assert_eq!(repo.owner, owner);
        assert_eq!(
            repo.href(),
            format!("buzz://repo?owner={owner}&d=demo&tab=files")
        );
        for href in [
            format!("buzz://project?owner={owner}&d=demo"),
            format!("BUZZ://pr?id={id}&owner={owner}&d=demo"),
            format!("buzz://issue?id={id}&owner={owner}&d=demo"),
        ] {
            assert!(matches!(parse(&href), Some(Link::Entity(_))), "{href}");
        }
        for bad in [
            format!("buzz://repo?owner={owner}"),
            format!("buzz://issue?owner={owner}&d=demo"),
            format!("buzz://repo?owner={owner}&d=demo&tab=overview"),
        ] {
            assert_eq!(parse(&bad), None, "{bad}");
        }
    }

    #[test]
    fn parses_invites() {
        let relay = RelayOrigin::parse("wss://relay.example.com").unwrap();
        assert_eq!(
            parse("buzz://join?relay=wss%3A%2F%2Frelay.example.com&code=abc"),
            Some(Link::Join {
                relay: relay.clone(),
                code: "abc".into(),
            })
        );
        // Windows supplies a root path; unknown parameters are ignored.
        assert_eq!(
            parse("buzz://join/?relay=https://relay.example.com/&code=abc&x=1&x=2"),
            Some(Link::Join {
                relay,
                code: "abc".into(),
            })
        );
        for bad in [
            "buzz://join?relay=example&code=abc",
            "buzz://join?relay=wss://relay.example.com",
            "buzz://join?relay=wss://relay.example.com&code=",
            "buzz://join?relay=wss://a.example&relay=wss://b.example&code=abc",
            "buzz://join?relay=wss://relay.example.com&code=abc&policy_receipt=",
            "buzz://join/extra?relay=wss://relay.example.com&code=abc",
        ] {
            assert_eq!(parse(bad), None, "{bad}");
        }
        let long = format!(
            "buzz://join?relay=wss://relay.example.com&code={}",
            "a".repeat(257)
        );
        assert_eq!(parse(&long), None);
    }

    #[test]
    fn takes_the_last_link_argument() {
        let args = [
            "buzz-gpui",
            "buzz://channel/a",
            "--flag",
            "buzz://channel/b",
        ]
        .map(String::from);
        assert_eq!(from_args(args), Some("buzz://channel/b".into()));
        assert_eq!(from_args(["buzz-gpui".to_string()]), None);
    }
}
