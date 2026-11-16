//! Native widgets and the real session, with an unreachable loopback relay.
use super::*;
use gpui_kit::test::TestWindowExt;

#[gpui_kit::test]
fn offline_preview_keeps_consent_off_and_stop_remains_keyboard_accessible(cx: &mut TestAppContext) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let owner = Arc::new(buzz_identity::Identity::ephemeral());
    let session = Arc::new(Session::open(
        runtime.handle(),
        buzz_relay_client::RelayOrigin::parse("http://127.0.0.1:9").unwrap(),
        owner,
        buzz_relay_client::Store::in_memory().unwrap(),
    ));
    cx.update(|cx| {
        gpui_kit::init(cx);
        buzz_ui::init(cx);
    });
    let (view, cx) = cx.add_window_view({
        let session = session.clone();
        move |window, cx| {
            TeamStatus::for_session(
                session,
                "22222222-2222-4222-8222-222222222222".into(),
                "aa".repeat(32),
                vec![buzz_identity::Identity::ephemeral().pubkey_hex()],
                window,
                cx,
            )
        }
    });
    cx.update(|window, cx| {
        window.render_frame(cx);
        let preview = window.find("preview-team-status");
        assert_eq!(preview.role(), Some(Role::Button));
        assert_eq!(preview.label(), Some("Preview exact publication"));
        window.click("preview-team-status", cx);
    });
    // Pump GPUI while the real session reports the unavailable relay.
    for _ in 0..200 {
        cx.run_until_parked();
        if cx.read(|cx| !view.read(cx).busy) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    cx.read(|cx| {
        let panel = view.read(cx);
        assert!(!panel.busy);
        assert!(!panel.message.is_empty());
        assert!(panel.preview.is_none());
    });
    assert_eq!(session.team_status_state().sharing, 0);
    assert_eq!(session.team_status_state().pending, 0);
    cx.update(|window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("confirm-shareable-status").is_none());
        assert_ne!(window.find("stop-team-status").disabled(), Some(true));
        // Tab through actual native controls to the recovery action, then Space.
        for _ in 0..24 {
            if window.find("stop-team-status").focused() == Some(true) {
                break;
            }
            window.press("tab", cx);
        }
        assert_eq!(window.find("stop-team-status").focused(), Some(true));
        window.press("space", cx);
    });
    cx.read(|cx| assert!(view.read(cx).message.starts_with("Sharing stopped")));
    assert_eq!(session.team_status_state().sharing, 0);
}
