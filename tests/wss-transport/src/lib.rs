#[path = "../../../libs/hbb_common/src/strict_wss.rs"]
mod strict_wss;

#[path = "../../../src/wss_retry.rs"]
mod wss_retry;

#[path = "../../../libs/hbb_common/src/deployment.rs"]
mod deployment;

#[cfg(test)]
mod tests {
    use super::deployment;
    use super::strict_wss::{self, SendState, MAX_FRAME_PAYLOAD};
    use bytes::Bytes;
    use futures::{SinkExt, StreamExt};
    use std::{io::ErrorKind, sync::Arc, time::Duration};
    use tokio::{
        net::{TcpListener, TcpStream},
        time::timeout,
    };
    use tokio_rustls::{
        rustls::{pki_types::PrivatePkcs8KeyDer, ServerConfig},
        TlsAcceptor,
    };
    use tokio_tungstenite::{
        accept_async_with_config, connect_async,
        tungstenite::{
            protocol::{
                frame::{
                    coding::{Data, OpCode},
                    Frame,
                },
                Message, Role, WebSocketConfig,
            },
            Error as WsError,
        },
        WebSocketStream,
    };

    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

    #[test]
    fn accepts_only_configured_id_and_relay_endpoints() {
        for path in ["/ws/id", "/ws/relay"] {
            for authority in [
                deployment::ID_SERVER.to_owned(),
                format!("{}:443", deployment::ID_SERVER),
                deployment::ID_SERVER.to_uppercase(),
            ] {
                deployment::validate_endpoint(&format!("wss://{authority}{path}")).unwrap();
            }
        }
    }

    #[test]
    fn rejects_other_domains_schemes_ports_paths_and_url_metadata() {
        for endpoint in [
            "wss://example.com/ws/id",
            "ws://remote.yingluozhiwei.cn/ws/id",
            "https://remote.yingluozhiwei.cn/ws/id",
            "wss://remote.yingluozhiwei.cn:8443/ws/id",
            "wss://remote.yingluozhiwei.cn:abc/ws/id",
            "wss://remote.yingluozhiwei.cn:65536/ws/id",
            "wss://remote.yingluozhiwei.cn:/ws/id",
            "wss://remote.yingluozhiwei.cn/ws/other",
            "wss://remote.yingluozhiwei.cn/ws/id/",
            "wss://remote.yingluozhiwei.cn/other/../ws/id",
            "wss://user@remote.yingluozhiwei.cn/ws/id",
            "wss://@remote.yingluozhiwei.cn/ws/id",
            "wss://remote.yingluozhiwei.cn/ws/id?",
            "wss://remote.yingluozhiwei.cn/ws/id?token=value",
            "wss://remote.yingluozhiwei.cn/ws/id#fragment",
            "wss://remote.yingluozhiwei.cn/ws/%69d",
            "wss://remote.yingluozhiwei.cn.example.com/ws/id",
            "wss://127.0.0.1/ws/id",
        ] {
            assert!(
                deployment::validate_endpoint(endpoint).is_err(),
                "accepted {endpoint}"
            );
        }
    }

    #[tokio::test]
    async fn refuses_plain_websocket_before_connecting() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/ws/id", listener.local_addr().unwrap());
        let error = strict_wss::connect(&url, 1000).await.unwrap_err();
        assert!(error.to_string().contains("certificate-verified wss://"));
        assert!(timeout(Duration::from_millis(30), listener.accept())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn fragmented_messages_interoperate_with_a_bounded_frame_server() {
        timeout(TEST_TIMEOUT, async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("ws://{}/ws/relay", listener.local_addr().unwrap());
            let messages: Vec<Bytes> = [
                0,
                1,
                MAX_FRAME_PAYLOAD,
                MAX_FRAME_PAYLOAD + 1,
                4 * MAX_FRAME_PAYLOAD,
                4 * MAX_FRAME_PAYLOAD + 19,
            ]
            .into_iter()
            .map(|len| Bytes::from((0..len).map(|i| (i % 251) as u8).collect::<Vec<_>>()))
            .collect();
            let expected = messages.clone();
            let server = tokio::spawn(async move {
                let (tcp, _) = listener.accept().await.unwrap();
                let mut ws = accept_async_with_config(
                    tcp,
                    Some(WebSocketConfig::default().max_frame_size(Some(MAX_FRAME_PAYLOAD))),
                )
                .await
                .unwrap();
                for bytes in expected {
                    // The first relay frame remains Binary; no initial Ping.
                    // A second Binary instead of a continuation would return a
                    // partial message here and fail the exact payload equality.
                    assert_eq!(
                        ws.next().await.unwrap().unwrap(),
                        Message::Binary(bytes.clone())
                    );
                    ws.send(Message::Binary(bytes)).await.unwrap();
                }
            });
            let (mut ws, _) = connect_async(url).await.unwrap();
            let mut state = SendState::default();
            for bytes in messages {
                state
                    .send_binary(&mut ws, bytes.clone(), 1000)
                    .await
                    .unwrap();
                assert_eq!(
                    strict_wss::next_data(&mut ws, &state)
                        .await
                        .unwrap()
                        .unwrap(),
                    Message::Binary(bytes)
                );
                state.check_ready().unwrap();
            }
            server.await.unwrap();
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn fragmented_receive_handles_interleaved_ping_and_close() {
        timeout(TEST_TIMEOUT, async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("ws://{}/ws/id", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (tcp, _) = listener.accept().await.unwrap();
                let mut ws = accept_async_with_config(tcp, None).await.unwrap();
                ws.send(Message::Frame(Frame::message(
                    b"first".to_vec(),
                    OpCode::Data(Data::Binary),
                    false,
                )))
                .await
                .unwrap();
                ws.send(Message::Ping(Bytes::from_static(b"control")))
                    .await
                    .unwrap();
                ws.send(Message::Frame(Frame::message(
                    b"second".to_vec(),
                    OpCode::Data(Data::Continue),
                    true,
                )))
                .await
                .unwrap();
                assert_eq!(
                    ws.next().await.unwrap().unwrap(),
                    Message::Pong(Bytes::from_static(b"control"))
                );
                ws.send(Message::Close(None)).await.unwrap();
                assert!(matches!(
                    ws.next().await.unwrap().unwrap(),
                    Message::Close(_)
                ));
            });
            let (mut ws, _) = connect_async(url).await.unwrap();
            let state = SendState::default();
            assert_eq!(
                strict_wss::next_data(&mut ws, &state)
                    .await
                    .unwrap()
                    .unwrap(),
                Message::Binary(Bytes::from_static(b"firstsecond"))
            );
            // This drives the automatic Pong before receiving the Close.
            assert!(strict_wss::next_data(&mut ws, &state).await.is_none());
            server.await.unwrap();
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn legacy_websocket_preserves_signaling_and_one_mib_bidirectional_relay() {
        use tokio_tungstenite_legacy::{
            accept_async_with_config as legacy_accept,
            tungstenite::protocol::{Message as LegacyMessage, WebSocketConfig as LegacyConfig},
        };
        timeout(TEST_TIMEOUT, async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let host = listener.local_addr().unwrap();
            // Fixtures follow hbb_common/protos/rendezvous.proto: field 6
            // RegisterPeer(id="123456789", serial=1), and field 18 RequestRelay.
            let register = Bytes::from_static(b"\x32\x0d\x0a\x09123456789\x10\x01");
            let register_response = Bytes::from_static(b"\x3a\x02\x10\x01");
            let request_relay =
                Bytes::from_static(b"\x92\x01\x1d\x0a\x09123456789\x12\x10loopback-session");
            let forward = Bytes::from(
                (0..1024 * 1024)
                    .map(|i| (i % 251) as u8)
                    .collect::<Vec<_>>(),
            );
            let reverse = Bytes::from(
                (0..1024 * 1024)
                    .map(|i| (i % 239) as u8)
                    .collect::<Vec<_>>(),
            );
            let server_register = register.clone();
            let server_response = register_response.clone();
            let server_relay = request_relay.clone();
            let server_forward = forward.clone();
            let server_reverse = reverse.clone();
            let server = tokio::spawn(async move {
                let mut config = LegacyConfig::default();
                config.max_frame_size = Some(MAX_FRAME_PAYLOAD);
                let (tcp, _) = listener.accept().await.unwrap();
                let mut id = legacy_accept(tcp, Some(config)).await.unwrap();
                assert_eq!(
                    id.next().await.unwrap().unwrap(),
                    LegacyMessage::Binary(server_register.to_vec())
                );
                id.send(LegacyMessage::Binary(server_response.to_vec()))
                    .await
                    .unwrap();
                let (tcp, _) = listener.accept().await.unwrap();
                let mut first = legacy_accept(tcp, Some(config)).await.unwrap();
                assert_eq!(
                    first.next().await.unwrap().unwrap(),
                    LegacyMessage::Binary(server_relay.to_vec())
                );
                let (tcp, _) = listener.accept().await.unwrap();
                let mut second = legacy_accept(tcp, Some(config)).await.unwrap();
                assert_eq!(
                    second.next().await.unwrap().unwrap(),
                    LegacyMessage::Binary(server_relay.to_vec())
                );
                let message = first.next().await.unwrap().unwrap();
                assert_eq!(message, LegacyMessage::Binary(server_forward.to_vec()));
                second.send(message).await.unwrap();
                let message = second.next().await.unwrap().unwrap();
                assert_eq!(message, LegacyMessage::Binary(server_reverse.to_vec()));
                first.send(message).await.unwrap();
            });
            let (mut id, _) = connect_async(format!("ws://{host}/ws/id")).await.unwrap();
            let mut id_state = SendState::default();
            id_state.send_binary(&mut id, register, 1000).await.unwrap();
            assert_eq!(
                strict_wss::next_data(&mut id, &id_state)
                    .await
                    .unwrap()
                    .unwrap(),
                Message::Binary(register_response)
            );
            let (mut first, _) = connect_async(format!("ws://{host}/ws/relay"))
                .await
                .unwrap();
            let mut first_state = SendState::default();
            first_state
                .send_binary(&mut first, request_relay.clone(), 1000)
                .await
                .unwrap();
            let (mut second, _) = connect_async(format!("ws://{host}/ws/relay"))
                .await
                .unwrap();
            let mut second_state = SendState::default();
            second_state
                .send_binary(&mut second, request_relay, 1000)
                .await
                .unwrap();
            first_state
                .send_binary(&mut first, forward.clone(), 1000)
                .await
                .unwrap();
            assert_eq!(
                strict_wss::next_data(&mut second, &second_state)
                    .await
                    .unwrap()
                    .unwrap(),
                Message::Binary(forward)
            );
            second_state
                .send_binary(&mut second, reverse.clone(), 1000)
                .await
                .unwrap();
            assert_eq!(
                strict_wss::next_data(&mut first, &first_state)
                    .await
                    .unwrap()
                    .unwrap(),
                Message::Binary(reverse)
            );
            server.await.unwrap();
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn send_timeout_requires_reconnect() {
        let (client, _stalled_peer) = tokio::io::duplex(64);
        let mut ws = WebSocketStream::from_raw_socket(client, Role::Client, None).await;
        let mut state = SendState::default();
        let error = state
            .send_binary(&mut ws, Bytes::from(vec![9; 3 * MAX_FRAME_PAYLOAD]), 20)
            .await
            .unwrap_err();
        assert!(error
            .downcast_ref::<tokio::time::error::Elapsed>()
            .is_some());
        assert_eq!(
            state.check_ready().unwrap_err().kind(),
            ErrorKind::BrokenPipe
        );
        assert!(state
            .send_binary(&mut ws, Bytes::from_static(b"next"), 1000)
            .await
            .is_err());
        assert_eq!(
            strict_wss::next_data(&mut ws, &state)
                .await
                .unwrap()
                .unwrap_err()
                .kind(),
            ErrorKind::BrokenPipe
        );
    }

    #[tokio::test]
    async fn externally_cancelled_send_requires_reconnect() {
        let (client, _stalled_peer) = tokio::io::duplex(64);
        let mut ws = WebSocketStream::from_raw_socket(client, Role::Client, None).await;
        let mut state = SendState::default();
        assert!(timeout(
            Duration::from_millis(20),
            state.send_binary(&mut ws, Bytes::from(vec![7; 3 * MAX_FRAME_PAYLOAD]), 0)
        )
        .await
        .is_err());
        assert_eq!(
            state.check_ready().unwrap_err().kind(),
            ErrorKind::BrokenPipe
        );
        assert!(state
            .send_binary(&mut ws, Bytes::from_static(b"next"), 1000)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn untrusted_tls_certificate_is_rejected_on_every_connection() {
        timeout(TEST_TIMEOUT, async {
            let certified =
                rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
            let key = PrivatePkcs8KeyDer::from(certified.key_pair.serialize_der());
            let config = ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(vec![certified.cert.der().clone()], key.into())
                .unwrap();
            let acceptor = TlsAcceptor::from(Arc::new(config));
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!(
                "wss://localhost:{}/ws/id",
                listener.local_addr().unwrap().port()
            );
            let server = tokio::spawn(async move {
                for _ in 0..2 {
                    let (tcp, _) = listener.accept().await.unwrap();
                    if let Ok(tls) = acceptor.accept(tcp).await {
                        // Schannel can finish TLS before it validates the peer
                        // chain. The client must still reject the certificate
                        // before sending a successful HTTP WebSocket Upgrade.
                        assert!(tokio_tungstenite::accept_async(tls).await.is_err());
                    }
                }
                // A failed handshake must not retry with verification disabled.
                assert!(timeout(Duration::from_millis(50), listener.accept())
                    .await
                    .is_err());
            });
            for _ in 0..2 {
                let error = strict_wss::connect(&url, 1000).await.unwrap_err();
                assert!(
                    matches!(error.downcast_ref::<WsError>(), Some(WsError::Tls(_))),
                    "{error:?}"
                );
            }
            server.await.unwrap();
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn stalled_tls_handshake_uses_the_connection_deadline() {
        timeout(TEST_TIMEOUT, async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!(
                "wss://localhost:{}/ws/id",
                listener.local_addr().unwrap().port()
            );
            let server = tokio::spawn(async move {
                let (tcp, _) = listener.accept().await.unwrap();
                let _ws_peer: TcpStream = tcp;
                tokio::time::sleep(Duration::from_millis(250)).await;
            });
            let error = strict_wss::connect(&url, 50).await.unwrap_err();
            assert!(
                error
                    .downcast_ref::<tokio::time::error::Elapsed>()
                    .is_some(),
                "{error:?}"
            );
            server.await.unwrap();
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    #[ignore = "Requires an explicitly configured public WSS endpoint"]
    async fn verified_public_wss_handshake() {
        let url = std::env::var("RUSTDESK_WSS_TEST_URL").expect(
            "Set RUSTDESK_WSS_TEST_URL to a WSS endpoint with a publicly trusted certificate",
        );
        let mut ws = strict_wss::connect(&url, 10_000).await.unwrap();
        assert!(!matches!(
            ws.get_ref(),
            tokio_tungstenite::MaybeTlsStream::Plain(_)
        ));
        ws.close(None).await.unwrap();
    }
}
