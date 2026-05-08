//! Smoke test for the headless MCP engine daemon.
//!
//! Spins up `engine_server::serve_on` in-process on an ephemeral port,
//! connects a `WireTransport` client over `TcpStream`, exercises the
//! happy path of every wire RPC the supervisor cares about today, and
//! verifies cleanup on disconnect.

use std::sync::Arc;
use std::time::Duration;

use rnme::execution::SpawnOptions;
use rnme::mcp::transport::WireTransport;
use rnme::mcp::wire::{
    CorrelationId, Event, Request, Response, SubscriptionId, WireMessage,
};
use rnme::task::Registry;
use tokio::net::{TcpListener, TcpStream};

const __RNME_GROUP: &str = "";

/// Spin up the engine server in-process. Returns the port plus a
/// JoinHandle whose completion signals the server's shutdown.
async fn spawn_engine() -> (u16, tokio::task::JoinHandle<i32>) {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind ephemeral port");
    let port = listener.local_addr().expect("local_addr").port();

    // Use the inventory-built registry so `:list` (and friends) are
    // available to spawn.
    let registry = Arc::new(Registry::from_inventory());

    let join = tokio::spawn(async move {
        rnme::mcp::engine_server::serve_on(listener, registry, 1).await
    });

    (port, join)
}

/// Send a request and wait for the matching response (skipping events).
async fn rpc(
    transport: &mut WireTransport<TcpStream>,
    correlation: CorrelationId,
    body: Request,
) -> Result<Response, rnme::mcp::wire::RpcError> {
    transport
        .send(&WireMessage::Request {
            id: correlation,
            body,
        })
        .await
        .expect("send request");

    loop {
        let frame = transport.recv().await.expect("recv frame");
        match frame {
            WireMessage::Response { id, body } if id == correlation => return body,
            WireMessage::Event(_) | WireMessage::Response { .. } => {
                // skip events / unrelated responses
            }
            WireMessage::Request { .. } => panic!("client should not receive Request frames"),
        }
    }
}

#[tokio::test]
async fn engine_server_handles_list_spawn_subscribe_and_cleanup() {
    let (port, server_join) = spawn_engine().await;

    let stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect to engine");
    let mut transport = WireTransport::new(stream);

    // ---------------------------------------------------------------
    // 1. ListTasks
    // ---------------------------------------------------------------
    let list_resp = rpc(&mut transport, CorrelationId(1), Request::ListTasks)
        .await
        .expect("ListTasks ok");
    let infos = match list_resp {
        Response::ListTasks(v) => v,
        other => panic!("expected ListTasks response, got {other:?}"),
    };
    assert!(
        infos.iter().any(|i| i.name == "list" && i.group == "builtin"),
        "registry should include the builtin :list task"
    );

    // ---------------------------------------------------------------
    // 2. SpawnTask :list (no args) — captures task_id + initial_seq
    // ---------------------------------------------------------------
    let spawn_resp = rpc(
        &mut transport,
        CorrelationId(2),
        Request::SpawnTask {
            name: ":list".into(),
            args: vec![],
            opts: SpawnOptions::default(),
        },
    )
    .await
    .expect("SpawnTask ok");
    let (task_id, initial_seq) = match spawn_resp {
        Response::SpawnTask {
            task_id,
            initial_seq,
        } => (task_id, initial_seq),
        other => panic!("expected SpawnTask response, got {other:?}"),
    };

    // ---------------------------------------------------------------
    // 3. SubscribeLogs from `initial_seq` — should yield :list output
    // ---------------------------------------------------------------
    let sub_resp = rpc(
        &mut transport,
        CorrelationId(3),
        Request::SubscribeLogs {
            task_id,
            filter: None,
            from_seq: Some(initial_seq),
        },
    )
    .await
    .expect("SubscribeLogs ok");
    let sub_id = match sub_resp {
        Response::SubscribeLogs { subscription_id } => subscription_id,
        other => panic!("expected SubscribeLogs, got {other:?}"),
    };

    // Read frames until we get at least one Event::Log for this sub.
    let mut got_log = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        let frame = match tokio::time::timeout(Duration::from_secs(2), transport.recv()).await {
            Ok(Ok(f)) => f,
            Ok(Err(e)) => panic!("transport error while waiting for log event: {e:?}"),
            Err(_) => continue,
        };
        if let WireMessage::Event(Event::Log {
            subscription_id, ..
        }) = &frame
            && *subscription_id == sub_id
        {
            got_log = true;
            break;
        }
    }
    assert!(got_log, "expected at least one Event::Log on sub {sub_id:?}");

    // ---------------------------------------------------------------
    // 4. Unsubscribe
    // ---------------------------------------------------------------
    let unsub_resp = rpc(
        &mut transport,
        CorrelationId(4),
        Request::UnsubscribeLogs {
            subscription_id: sub_id,
        },
    )
    .await
    .expect("UnsubscribeLogs ok");
    assert!(matches!(unsub_resp, Response::UnsubscribeLogs));

    // Unsubscribing a now-unknown id should yield NotFound.
    let bad_unsub = rpc(
        &mut transport,
        CorrelationId(5),
        Request::UnsubscribeLogs {
            subscription_id: SubscriptionId(99_999),
        },
    )
    .await;
    assert!(
        matches!(bad_unsub, Err(rnme::mcp::wire::RpcError::NotFound(_))),
        "expected NotFound on unknown subscription, got {bad_unsub:?}"
    );

    // ---------------------------------------------------------------
    // 5. Drop the connection — engine should clean up and exit.
    // ---------------------------------------------------------------
    drop(transport);

    let exit_code = tokio::time::timeout(Duration::from_secs(5), server_join)
        .await
        .expect("server should exit within 5s of disconnect")
        .expect("server task panicked");
    assert_eq!(exit_code, 0, "expected clean shutdown exit 0");
}
