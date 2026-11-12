use super::*;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
async fn pairing_uses_encrypted_owner_profile_and_only_valid_correlated_reply() {
    for row in [
        "valid",
        "wrong-signer",
        "wrong-recipient",
        "expired",
        "missing",
        "timeout",
    ] {
        let owner = Keys::generate();
        let manager = Keys::generate();
        let expected = manager.public_key();
        let owner_pub = owner.public_key();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let relay = format!("ws://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            ws.send(Message::Text(
                json!(["AUTH", "challenge"]).to_string().into(),
            ))
            .await
            .unwrap();
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let frame: Value = serde_json::from_str(&text).unwrap();
                match frame[0].as_str().unwrap() {
                    "AUTH" => ws
                        .send(Message::Text(
                            json!(["OK", frame[1]["id"], true, ""]).to_string().into(),
                        ))
                        .await
                        .unwrap(),
                    "REQ" => ws
                        .send(Message::Text(
                            json!(["EOSE", "paseo-pairing"]).to_string().into(),
                        ))
                        .await
                        .unwrap(),
                    "EVENT" => {
                        let event: nostr::Event = serde_json::from_value(frame[1].clone()).unwrap();
                        let opened = buzz_session_control::open_request(&event, &manager).unwrap();
                        assert_eq!(opened.signer, owner_pub);
                        assert_eq!(opened.request.op, Op::Profile);
                        assert!(opened.auth_tag.is_none());
                        assert_eq!(opened.request.payload, json!({}));
                        assert!(!event.content.contains("profile"));
                        ws.send(Message::Text(
                            json!(["OK", event.id, true, ""]).to_string().into(),
                        ))
                        .await
                        .unwrap();
                        if row == "timeout" {
                            tokio::time::sleep(Duration::from_secs(2)).await;
                            return;
                        }
                        let signer = if row == "wrong-signer" {
                            Keys::generate()
                        } else {
                            manager.clone()
                        };
                        let now = Timestamp::now().as_secs();
                        let response = if row == "missing" {
                            json!({"ok":true})
                        } else {
                            json!({"ok":true,"profile":{"private":"MUST_NOT_LEAK"},"paseo":{"password":URL_SAFE_NO_PAD.encode([7u8;32]),"server_id":"srv_fixture","offer_url":"https://app.paseo.sh/#offer=fixture","gateway_pubkey":expected}})
                        };
                        let reply = buzz_session_control::build_reply(
                            &signer,
                            &owner_pub,
                            &event.id,
                            &buzz_session_control::Reply::done(opened.request.request_id, response),
                            Timestamp::from(now),
                        )
                        .unwrap();
                        let reply = if matches!(row, "wrong-recipient" | "expired") {
                            let tags = reply
                                .tags
                                .iter()
                                .map(|tag| match tag.as_slice()[0].as_str() {
                                    "p" if row == "wrong-recipient" => {
                                        nostr::Tag::public_key(Keys::generate().public_key())
                                    }
                                    "expiration" if row == "expired" => {
                                        nostr::Tag::expiration(Timestamp::from(now - 1))
                                    }
                                    _ => tag.clone(),
                                })
                                .collect::<Vec<_>>();
                            nostr::EventBuilder::new(reply.kind, reply.content)
                                .tags(tags)
                                .custom_created_at(reply.created_at)
                                .sign_with_keys(&signer)
                                .unwrap()
                        } else {
                            reply
                        };
                        ws.send(Message::Text(
                            json!(["EVENT", "paseo-pairing", reply]).to_string().into(),
                        ))
                        .await
                        .unwrap();
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        return;
                    }
                    other => panic!("unexpected {other}"),
                }
            }
        });
        let result = exchange(&relay, &owner, expected, Duration::from_millis(500)).await;
        if row == "valid" {
            let (public, password) = result.unwrap();
            assert_eq!(password.as_str(), URL_SAFE_NO_PAD.encode([7u8; 32]));
            assert_eq!(public.as_object().unwrap().len(), 3);
            assert!(public.get("password").is_none());
            assert!(!public.to_string().contains("MUST_NOT_LEAK"));
        } else {
            assert!(result.is_err(), "{row}");
        }
        task.abort();
    }
}
