// Copyright 2019-2021 Parity Technologies (UK) Ltd.
//
// Permission is hereby granted, free of charge, to any
// person obtaining a copy of this software and associated
// documentation files (the "Software"), to deal in the
// Software without restriction, including without
// limitation the rights to use, copy, modify, merge,
// publish, distribute, sublicense, and/or sell copies of
// the Software, and to permit persons to whom the Software
// is furnished to do so, subject to the following
// conditions:
//
// The above copyright notice and this permission notice
// shall be included in all copies or substantial portions
// of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF
// ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED
// TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A
// PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT
// SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
// CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION
// OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR
// IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

#![cfg(test)]
#![allow(clippy::disallowed_names)]

mod helpers;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use futures::stream::FuturesUnordered;
use futures::{StreamExt, TryStreamExt, channel::mpsc};
use helpers::{
	connect_over_socks_stream, init_logger, pipe_from_stream_and_drop, server, server_with_cors,
	server_with_health_api, server_with_subscription, server_with_subscription_and_handle,
};
use http_body_util::BodyExt;
use hyper::http::HeaderValue;
use hyper_util::rt::TokioExecutor;
use jsonrpsee::core::client::SubscriptionCloseReason;
use jsonrpsee::core::client::{ClientT, Error, IdKind, Subscription, SubscriptionClientT};
use jsonrpsee::core::params::{ArrayParams, BatchRequestBuilder};
use jsonrpsee::core::{JsonValue, SubscriptionError};
use jsonrpsee::http_client::HttpClientBuilder;
use jsonrpsee::server::middleware::http::HostFilterLayer;
use jsonrpsee::server::{ConnectionGuard, ServerBuilder, ServerConfig, ServerHandle};
use jsonrpsee::types::error::{ErrorObject, UNKNOWN_ERROR_CODE};
use jsonrpsee::ws_client::WsClientBuilder;
use jsonrpsee::{ResponsePayload, RpcModule, rpc_params};
use jsonrpsee_test_utils::TimeoutFutureExt;
use tokio::time::interval;
use tokio_stream::wrappers::IntervalStream;
use tower_http::cors::CorsLayer;

use crate::helpers::server_with_sleeping_subscription;

type HttpBody = http_body_util::Full<hyper::body::Bytes>;

#[tokio::test]
async fn ws_subscription_works() {
	init_logger();

	let server_addr = server_with_subscription().await;
	let server_url = format!("ws://{}", server_addr);
	let client = WsClientBuilder::default().build(&server_url).await.unwrap();
	let mut hello_sub: Subscription<String> =
		client.subscribe("subscribe_hello", rpc_params![], "unsubscribe_hello").await.unwrap();
	let mut foo_sub: Subscription<u64> =
		client.subscribe("subscribe_foo", rpc_params![], "unsubscribe_foo").await.unwrap();

	for _ in 0..10 {
		let hello = hello_sub.next().await.unwrap().unwrap();
		let foo = foo_sub.next().await.unwrap().unwrap();
		assert_eq!(hello, "hello from subscription".to_string());
		assert_eq!(foo, 1337);
	}
}

#[tokio::test]
async fn ws_subscription_works_over_proxy_stream() {
	init_logger();

	let server_addr = server_with_subscription().await;
	let target_url = format!("ws://{}", server_addr);

	let socks_stream = connect_over_socks_stream(server_addr).await;
	let client = WsClientBuilder::default().build_with_stream(target_url, socks_stream).await.unwrap();

	let mut hello_sub: Subscription<String> =
		client.subscribe("subscribe_hello", rpc_params![], "unsubscribe_hello").await.unwrap();
	let mut foo_sub: Subscription<u64> =
		client.subscribe("subscribe_foo", rpc_params![], "unsubscribe_foo").await.unwrap();

	for _ in 0..10 {
		let hello = hello_sub.next().await.unwrap().unwrap();
		let foo = foo_sub.next().await.unwrap().unwrap();
		assert_eq!(hello, "hello from subscription".to_string());
		assert_eq!(foo, 1337);
	}
}

#[tokio::test]
async fn ws_unsubscription_works() {
	init_logger();

	let (tx, mut rx) = futures::channel::mpsc::channel(1);
	let server_addr = server_with_sleeping_subscription(tx).await;
	let server_url = format!("ws://{}", server_addr);
	let client = WsClientBuilder::default().build(&server_url).await.unwrap();

	let sub: Subscription<usize> =
		client.subscribe("subscribe_sleep", rpc_params![], "unsubscribe_sleep").await.unwrap();

	sub.unsubscribe().with_default_timeout().await.unwrap().unwrap();

	let res = rx.next().with_default_timeout().await.expect("Test must complete in 1 min");
	// When the subscription is closed a message is sent out on this channel.
	assert!(res.is_some());
}

#[tokio::test]
async fn ws_unsubscription_works_over_proxy_stream() {
	init_logger();

	let (tx, mut rx) = futures::channel::mpsc::channel(1);
	let server_addr = server_with_sleeping_subscription(tx).await;
	let server_url = format!("ws://{}", server_addr);

	let socks_stream = connect_over_socks_stream(server_addr).await;
	let client = WsClientBuilder::default().build_with_stream(&server_url, socks_stream).await.unwrap();

	let sub: Subscription<usize> =
		client.subscribe("subscribe_sleep", rpc_params![], "unsubscribe_sleep").await.unwrap();

	sub.unsubscribe().with_default_timeout().await.unwrap().unwrap();

	let res = rx.next().with_default_timeout().await.expect("Test must complete in 1 min");
	// When the subscription is closed a message is sent out on this channel.
	assert!(res.is_some());
}

#[tokio::test]
async fn ws_subscription_with_input_works() {
	init_logger();

	let server_addr = server_with_subscription().await;
	let server_url = format!("ws://{}", server_addr);
	let client = WsClientBuilder::default().build(&server_url).await.unwrap();
	let mut add_one: Subscription<u64> =
		client.subscribe("subscribe_add_one", rpc_params![1], "unsubscribe_add_one").await.unwrap();

	for i in 2..4 {
		let next = add_one.next().await.unwrap().unwrap();
		assert_eq!(next, i);
	}
}

#[tokio::test]
async fn ws_subscription_with_input_works_over_proxy_stream() {
	init_logger();

	let server_addr = server_with_subscription().await;
	let server_url = format!("ws://{}", server_addr);

	let socks_stream = connect_over_socks_stream(server_addr).await;
	let client = WsClientBuilder::default().build_with_stream(&server_url, socks_stream).await.unwrap();

	let mut add_one: Subscription<u64> =
		client.subscribe("subscribe_add_one", rpc_params![1], "unsubscribe_add_one").await.unwrap();

	for i in 2..4 {
		let next = add_one.next().await.unwrap().unwrap();
		assert_eq!(next, i);
	}
}

#[tokio::test]
async fn ws_method_call_works() {
	init_logger();

	let server_addr = server().await;
	let server_url = format!("ws://{}", server_addr);
	let client = WsClientBuilder::default().build(&server_url).await.unwrap();
	let response: String = client.request("say_hello", rpc_params![]).await.unwrap();
	assert_eq!(&response, "hello");
}

#[tokio::test]
async fn ws_method_call_works_over_proxy_stream() {
	init_logger();

	let server_addr = server().await;
	let server_url = format!("ws://{}", server_addr);

	let socks_stream = connect_over_socks_stream(server_addr).await;

	let client = WsClientBuilder::default().build_with_stream(&server_url, socks_stream).await.unwrap();
	let response: String = client.request("say_hello", rpc_params![]).await.unwrap();
	assert_eq!(&response, "hello");
}

#[tokio::test]
async fn connection_id_extension_with_different_ws_clients() {
	init_logger();

	let server_addr = server().await;
	let server_url = format!("ws://{}", server_addr);
	let client = WsClientBuilder::default().build(&server_url).await.unwrap();

	// Connection ID does not change for the same client.
	let connection_id: usize = client.request("get_connection_id", rpc_params![]).await.unwrap();
	let identical_connection_id: usize = client.request("get_connection_id", rpc_params![]).await.unwrap();
	assert_eq!(connection_id, identical_connection_id);

	// Connection ID is different for different clients.
	let second_client = WsClientBuilder::default().build(&server_url).await.unwrap();
	let second_connection_id: usize = second_client.request("get_connection_id", rpc_params![]).await.unwrap();
	assert_ne!(connection_id, second_connection_id);
}

#[tokio::test]
async fn connection_guard_extension_with_different_ws_clients() {
	init_logger();

	let server_addr = server().await;
	let server_url = format!("ws://{}", server_addr);

	// First connected retrieves initial information from ConnectionGuard.
	let first_client = WsClientBuilder::default().build(&server_url).await.unwrap();
	let first_max_connections: usize = first_client.request("get_max_connections", rpc_params![]).await.unwrap();
	let first_available_connections: usize =
		first_client.request("get_available_connections", rpc_params![]).await.unwrap();

	assert_eq!(first_available_connections, first_max_connections - 1);

	// Second client ensure max connections stays the same, but available connections is decreased.
	let second_client = WsClientBuilder::default().build(&server_url).await.unwrap();
	let second_max_connections: usize = second_client.request("get_max_connections", rpc_params![]).await.unwrap();
	let second_available_connections: usize =
		second_client.request("get_available_connections", rpc_params![]).await.unwrap();

	assert_eq!(second_max_connections, first_max_connections);
	assert_eq!(second_available_connections, second_max_connections - 2);
}

#[tokio::test]
async fn ws_method_call_str_id_works() {
	init_logger();

	let server_addr = server().await;
	let server_url = format!("ws://{}", server_addr);
	let client = WsClientBuilder::default().id_format(IdKind::String).build(&server_url).await.unwrap();
	let response: String = client.request("say_hello", rpc_params![]).await.unwrap();
	assert_eq!(&response, "hello");
}

#[tokio::test]
async fn ws_method_call_str_id_works_over_proxy_stream() {
	init_logger();

	let server_addr = server().await;
	let server_url = format!("ws://{}", server_addr);

	let socks_stream = connect_over_socks_stream(server_addr).await;

	let client = WsClientBuilder::default()
		.id_format(IdKind::String)
		.build_with_stream(&server_url, socks_stream)
		.await
		.unwrap();
	let response: String = client.request("say_hello", rpc_params![]).await.unwrap();
	assert_eq!(&response, "hello");
}

#[tokio::test]
async fn http_method_call_works() {
	init_logger();

	let server_addr = server().await;
	let uri = format!("http://{}", server_addr);
	let client = HttpClientBuilder::default().build(&uri).unwrap();
	let response: String = client.request("say_hello", rpc_params![]).await.unwrap();
	assert_eq!(&response, "hello");
}

#[tokio::test]
async fn http_method_call_str_id_works() {
	init_logger();

	let server_addr = server().await;
	let uri = format!("http://{}", server_addr);
	let client = HttpClientBuilder::default().id_format(IdKind::String).build(&uri).unwrap();
	let response: String = client.request("say_hello", rpc_params![]).await.unwrap();
	assert_eq!(&response, "hello");
}

#[tokio::test]
async fn ws_subscription_several_clients() {
	init_logger();

	let server_addr = server_with_subscription().await;
	let server_url = format!("ws://{}", server_addr);

	let mut clients = Vec::with_capacity(10);
	for _ in 0..10 {
		let client = WsClientBuilder::default().build(&server_url).await.unwrap();
		let hello_sub: Subscription<JsonValue> =
			client.subscribe("subscribe_hello", rpc_params![], "unsubscribe_hello").await.unwrap();
		let foo_sub: Subscription<JsonValue> =
			client.subscribe("subscribe_foo", rpc_params![], "unsubscribe_foo").await.unwrap();
		clients.push((client, hello_sub, foo_sub))
	}
}

#[tokio::test]
async fn ws_subscription_several_clients_with_drop() {
	init_logger();

	let server_addr = server_with_subscription().await;
	let server_url = format!("ws://{}", server_addr);

	let mut clients = Vec::with_capacity(10);
	for _ in 0..10 {
		let client = WsClientBuilder::default()
			.max_buffer_capacity_per_subscription(u32::MAX as usize)
			.build(&server_url)
			.await
			.unwrap();
		let hello_sub: Subscription<String> =
			client.subscribe("subscribe_hello", rpc_params![], "unsubscribe_hello").await.unwrap();
		let foo_sub: Subscription<u64> =
			client.subscribe("subscribe_foo", rpc_params![], "unsubscribe_foo").await.unwrap();
		clients.push((client, hello_sub, foo_sub))
	}

	for _ in 0..10 {
		for (_client, hello_sub, foo_sub) in &mut clients {
			let hello = hello_sub.next().await.unwrap().unwrap();
			let foo = foo_sub.next().await.unwrap().unwrap();
			assert_eq!(&hello, "hello from subscription");
			assert_eq!(foo, 1337);
		}
	}

	for i in 0..5 {
		let (client, hello_sub, foo_sub) = clients.remove(i);
		drop(hello_sub);
		drop(foo_sub);
		assert!(client.is_connected());
		drop(client);
	}

	// make sure nothing weird happened after dropping half of the clients (should be `unsubscribed` in the server)
	// would be good to know that subscriptions actually were removed but not possible to verify at
	// this layer.
	for _ in 0..10 {
		for (client, hello_sub, foo_sub) in &mut clients {
			assert!(client.is_connected());
			let hello = hello_sub.next().await.unwrap().unwrap();
			let foo = foo_sub.next().await.unwrap().unwrap();
			assert_eq!(&hello, "hello from subscription");
			assert_eq!(foo, 1337);
		}
	}
}

#[tokio::test]
async fn ws_subscription_close_on_lagging() {
	init_logger();

	let server_addr = server_with_subscription().await;
	let server_url = format!("ws://{}", server_addr);

	let client = WsClientBuilder::default().max_buffer_capacity_per_subscription(4).build(&server_url).await.unwrap();
	let mut hello_sub: Subscription<JsonValue> =
		client.subscribe("subscribe_hello", rpc_params![], "unsubscribe_hello").await.unwrap();

	// Don't poll the subscription stream for 2 seconds, should be full now.
	tokio::time::sleep(Duration::from_secs(2)).await;

	// Lagged
	assert!(matches!(hello_sub.close_reason(), Some(SubscriptionCloseReason::Lagged)));

	// Drain the subscription.
	for _ in 0..4 {
		assert!(hello_sub.next().with_default_timeout().await.unwrap().is_some());
	}

	// It should be dropped when lagging.
	assert!(hello_sub.next().with_default_timeout().await.unwrap().is_none());

	// The client should still be useable => make sure it still works.
	let _hello_req: JsonValue = client.request("say_hello", rpc_params![]).await.unwrap();

	// The same subscription should be possible to register again.
	let mut other_sub: Subscription<JsonValue> =
		client.subscribe("subscribe_hello", rpc_params![], "unsubscribe_hello").await.unwrap();

	assert!(other_sub.next().with_default_timeout().await.unwrap().is_some());
	assert!(client.is_connected());
}

#[tokio::test]
async fn ws_making_more_requests_than_allowed_should_not_deadlock() {
	init_logger();

	let server_addr = server().await;
	let server_url = format!("ws://{}", server_addr);
	let client = Arc::new(WsClientBuilder::default().max_concurrent_requests(2).build(&server_url).await.unwrap());

	let mut requests = Vec::new();

	for _ in 0..6 {
		let c = client.clone();
		requests.push(tokio::spawn(async move { c.request::<String, ArrayParams>("say_hello", rpc_params![]).await }));
	}

	for req in requests {
		let _ = req.await.unwrap();
	}
}

#[tokio::test]
async fn https_works() {
	init_logger();

	let client = HttpClientBuilder::default().build("https://kusama-rpc.polkadot.io").unwrap();
	let response: String = client.request("system_chain", rpc_params![]).await.unwrap();
	assert_eq!(&response, "Kusama");
}

#[tokio::test]
async fn wss_works() {
	init_logger();

	let client = WsClientBuilder::default().build("wss://kusama-rpc.polkadot.io").await.unwrap();
	let response: String = client.request("system_chain", rpc_params![]).await.unwrap();
	assert_eq!(&response, "Kusama");
}

#[tokio::test]
async fn ws_unsubscribe_releases_request_slots() {
	init_logger();

	let server_addr = server_with_subscription().await;
	let server_url = format!("ws://{}", server_addr);

	let client = WsClientBuilder::default().max_concurrent_requests(1).build(&server_url).await.unwrap();

	let sub1: Subscription<JsonValue> =
		client.subscribe("subscribe_hello", rpc_params![], "unsubscribe_hello").await.unwrap();
	drop(sub1);
	let _: Subscription<JsonValue> =
		client.subscribe("subscribe_hello", rpc_params![], "unsubscribe_hello").await.unwrap();
}

#[tokio::test]
async fn server_should_be_able_to_close_subscriptions() {
	init_logger();

	let server_addr = server_with_subscription().await;
	let server_url = format!("ws://{}", server_addr);

	let client = WsClientBuilder::default().build(&server_url).await.unwrap();

	let mut sub: Subscription<String> =
		client.subscribe("subscribe_noop", rpc_params![], "unsubscribe_noop").await.unwrap();

	assert!(sub.next().await.is_none());
}

#[tokio::test]
async fn ws_close_pending_subscription_when_server_terminated() {
	init_logger();

	let (server_addr, server_handle) = server_with_subscription_and_handle().await;
	let server_url = format!("ws://{}", server_addr);

	let c1 = WsClientBuilder::default().build(&server_url).await.unwrap();

	let mut sub: Subscription<String> =
		c1.subscribe("subscribe_hello", rpc_params![], "unsubscribe_hello").await.unwrap();

	assert!(matches!(sub.next().await, Some(Ok(_))));

	server_handle.stop().unwrap();
	server_handle.stopped().await;

	let sub2: Result<Subscription<String>, _> =
		c1.subscribe("subscribe_hello", rpc_params![], "unsubscribe_hello").await;

	// no new request should be accepted.
	assert!(sub2.is_err());

	// consume final message
	for _ in 0..2 {
		match sub.next().await {
			// All good, exit test
			None => return,
			// Try again
			_ => continue,
		}
	}

	panic!("subscription keeps sending messages after server shutdown");
}

#[tokio::test]
async fn ws_server_should_stop_subscription_after_client_drop() {
	use futures::{SinkExt, StreamExt, channel::mpsc};
	use jsonrpsee::{RpcModule, server::ServerBuilder};

	init_logger();

	let server = ServerBuilder::default().build("127.0.0.1:0").await.unwrap();
	let server_url = format!("ws://{}", server.local_addr().unwrap());

	let (tx, mut rx) = mpsc::channel(1);
	let mut module = RpcModule::new(tx);

	module
		.register_subscription(
			"subscribe_hello",
			"subscribe_hello",
			"unsubscribe_hello",
			|_, pending, mut tx, _| async move {
				let sink = pending.accept().await?;
				let msg = serde_json::value::to_raw_value(&1)?;
				sink.send(msg).await?;
				sink.closed().await;
				let send_back = Arc::make_mut(&mut tx);
				send_back.feed("Subscription terminated by remote peer").await.unwrap();

				Ok(())
			},
		)
		.unwrap();

	let _handle = server.start(module);

	let client = WsClientBuilder::default().build(&server_url).await.unwrap();

	let mut sub: Subscription<usize> =
		client.subscribe("subscribe_hello", rpc_params![], "unsubscribe_hello").await.unwrap();

	let res = sub.next().await.unwrap().unwrap();

	assert_eq!(res, 1);
	drop(client);
	let close_err = rx.next().await.unwrap();

	// assert that the server received `SubscriptionClosed` after the client was dropped.
	assert_eq!(close_err, "Subscription terminated by remote peer");
}

#[tokio::test]
async fn ws_server_stop_subscription_when_dropped() {
	use jsonrpsee::{RpcModule, server::ServerBuilder};

	init_logger();

	let server = ServerBuilder::default().build("127.0.0.1:0").await.unwrap();
	let server_url = format!("ws://{}", server.local_addr().unwrap());

	let mut module = RpcModule::new(());

	module
		.register_subscription("subscribe_nop", "h", "unsubscribe_nop", |_params, _pending, _ctx, _| async { Ok(()) })
		.unwrap();

	let _handle = server.start(module);
	let client = WsClientBuilder::default().build(&server_url).await.unwrap();

	assert!(client.subscribe::<String, ArrayParams>("subscribe_nop", rpc_params![], "unsubscribe_nop").await.is_err());
}

#[tokio::test]
async fn ws_server_notify_client_on_disconnect() {
	use futures::channel::oneshot;

	init_logger();

	let (server_addr, server_handle) = server_with_subscription_and_handle().await;
	let server_url = format!("ws://{}", server_addr);

	let (up_tx, up_rx) = oneshot::channel();
	let (dis_tx, mut dis_rx) = oneshot::channel();
	let (multiple_tx, multiple_rx) = oneshot::channel();

	tokio::spawn(async move {
		let client = WsClientBuilder::default().build(&server_url).await.unwrap();
		// Validate server is up.
		client.request::<String, ArrayParams>("say_hello", rpc_params![]).await.unwrap();

		// Signal client is waiting for the server to disconnect.
		up_tx.send(()).unwrap();

		client.on_disconnect().await;

		// Signal disconnect finished.
		dis_tx.send(()).unwrap();

		// Call `on_disconnect` a few more times to ensure it does not block.
		client.on_disconnect().await;
		client.on_disconnect().await;
		multiple_tx.send(()).unwrap();
	});

	// Ensure the client validated the server and is waiting for the disconnect.
	up_rx.await.unwrap();

	// Let A = dis_rx try_recv and server stop
	//     B = client on_disconnect
	//
	// Precautionary wait to ensure that a buggy `on_disconnect` (B) cannot be called
	// after the server shutdowns (A).
	tokio::time::sleep(Duration::from_secs(5)).await;

	// Make sure the `on_disconnect` method did not return before stopping the server.
	assert_eq!(dis_rx.try_recv().unwrap(), None);

	server_handle.stop().unwrap();
	server_handle.stopped().await;

	// The `on_disconnect()` method returned.
	dis_rx.await.unwrap();

	// Multiple `on_disconnect()` calls did not block.
	multiple_rx.await.unwrap();
}

#[tokio::test]
async fn ws_server_notify_client_on_disconnect_with_closed_server() {
	init_logger();

	let (server_addr, server_handle) = server_with_subscription_and_handle().await;
	let server_url = format!("ws://{}", server_addr);

	let client = WsClientBuilder::default().build(&server_url).await.unwrap();
	// Validate server is up.
	client.request::<String, ArrayParams>("say_hello", rpc_params![]).await.unwrap();

	// Stop the server.
	server_handle.stop().unwrap();
	server_handle.stopped().await;

	// Ensure `on_disconnect` returns when the call is made after the server is closed.
	client.on_disconnect().await;
}

#[tokio::test]
async fn ws_server_cancels_subscriptions_on_reset_conn() {
	init_logger();

	let (tx, rx) = mpsc::channel(1);
	let server_url = format!("ws://{}", helpers::server_with_sleeping_subscription(tx).await);

	let client = WsClientBuilder::default().build(&server_url).await.unwrap();
	let mut subs = Vec::new();

	for _ in 0..10 {
		subs.push(
			client
				.subscribe::<usize, ArrayParams>("subscribe_sleep", rpc_params![], "unsubscribe_sleep")
				.await
				.unwrap(),
		);
	}

	// terminate connection.
	drop(client);

	let rx_len = rx.take(10).fold(0, |acc, _| async move { acc + 1 }).await;

	assert_eq!(rx_len, 10);
}

#[tokio::test]
async fn ws_server_subscribe_with_stream() {
	init_logger();

	let addr = server_with_subscription().await;
	let server_url = format!("ws://{}", addr);

	let client = WsClientBuilder::default().build(&server_url).await.unwrap();
	let mut sub1: Subscription<usize> =
		client.subscribe("subscribe_5_ints", rpc_params![], "unsubscribe_5_ints").await.unwrap();
	let mut sub2: Subscription<usize> =
		client.subscribe("subscribe_5_ints", rpc_params![], "unsubscribe_5_ints").await.unwrap();

	let (r1, r2) = futures::future::try_join(
		sub1.by_ref().take(2).try_collect::<Vec<_>>(),
		sub2.by_ref().take(3).try_collect::<Vec<_>>(),
	)
	.await
	.unwrap();

	assert_eq!(r1, vec![1, 2]);
	assert_eq!(r2, vec![1, 2, 3]);

	// Be rude, don't run the destructor
	std::mem::forget(sub2);

	// sub1 is still in business, read remaining items.
	assert_eq!(sub1.by_ref().take(3).try_collect::<Vec<usize>>().await.unwrap(), vec![3, 4, 5]);

	assert!(sub1.next().await.is_none());
}

#[tokio::test]
async fn ws_server_pipe_from_stream_should_cancel_tasks_immediately() {
	init_logger();

	let (tx, rx) = mpsc::channel(1);
	let server_url = format!("ws://{}", helpers::server_with_sleeping_subscription(tx).await);

	let client = WsClientBuilder::default().build(&server_url).await.unwrap();
	let mut subs = Vec::new();

	for _ in 0..10 {
		subs.push(
			client.subscribe::<i32, ArrayParams>("subscribe_sleep", rpc_params![], "unsubscribe_sleep").await.unwrap(),
		)
	}

	// This will call the `unsubscribe method`.
	drop(subs);

	let rx_len = rx.take(10).fold(0, |acc, _| async move { acc + 1 }).await;

	assert_eq!(rx_len, 10);
}

#[tokio::test]
async fn ws_batch_works() {
	init_logger();

	let server_addr = server().await;
	let server_url = format!("ws://{}", server_addr);
	let client = WsClientBuilder::default().build(&server_url).await.unwrap();

	let mut batch = BatchRequestBuilder::new();
	batch.insert("say_hello", rpc_params![]).unwrap();
	batch.insert("slow_hello", rpc_params![]).unwrap();

	let res = client.batch_request::<String>(batch).await.unwrap();
	assert_eq!(res.len(), 2);
	assert_eq!(res.num_successful_calls(), 2);
	assert_eq!(res.num_failed_calls(), 0);
	let responses: Vec<_> = res.into_ok().unwrap().collect();
	assert_eq!(responses, vec!["hello".to_string(), "hello".to_string()]);

	let mut batch = BatchRequestBuilder::new();
	batch.insert("say_hello", rpc_params![]).unwrap();
	batch.insert("err", rpc_params![]).unwrap();

	let res = client.batch_request::<String>(batch).await.unwrap();
	assert_eq!(res.len(), 2);
	assert_eq!(res.num_successful_calls(), 1);
	assert_eq!(res.num_failed_calls(), 1);

	let ok_responses: Vec<_> = res.iter().filter_map(|r| r.as_ref().ok()).collect();
	let err_responses: Vec<_> = res.iter().filter_map(|r| r.clone().err()).collect();
	assert_eq!(ok_responses, vec!["hello"]);
	assert_eq!(err_responses, vec![ErrorObject::borrowed(UNKNOWN_ERROR_CODE, "err", None)]);
}

#[tokio::test]
async fn http_batch_works() {
	init_logger();

	let server_addr = server().await;
	let server_url = format!("http://{}", server_addr);
	let client = HttpClientBuilder::default().build(&server_url).unwrap();

	let mut batch = BatchRequestBuilder::new();
	batch.insert("say_hello", rpc_params![]).unwrap();
	batch.insert("slow_hello", rpc_params![]).unwrap();

	let res = client.batch_request::<String>(batch).await.unwrap();
	assert_eq!(res.len(), 2);
	assert_eq!(res.num_successful_calls(), 2);
	assert_eq!(res.num_failed_calls(), 0);
	let responses: Vec<_> = res.into_ok().unwrap().collect();
	assert_eq!(responses, vec!["hello".to_string(), "hello".to_string()]);

	let mut batch = BatchRequestBuilder::new();
	batch.insert("say_hello", rpc_params![]).unwrap();
	batch.insert("err", rpc_params![]).unwrap();

	let res = client.batch_request::<String>(batch).await.unwrap();
	assert_eq!(res.len(), 2);
	assert_eq!(res.num_successful_calls(), 1);
	assert_eq!(res.num_failed_calls(), 1);

	let ok_responses: Vec<_> = res.iter().filter_map(|r| r.as_ref().ok()).collect();
	let err_responses: Vec<_> = res.iter().filter_map(|r| r.clone().err()).collect();
	assert_eq!(ok_responses, vec!["hello"]);
	assert_eq!(err_responses, vec![ErrorObject::borrowed(UNKNOWN_ERROR_CODE, "err", None)]);
}

#[tokio::test]
async fn ws_server_limit_subs_per_conn_works() {
	use futures::StreamExt;
	use jsonrpsee::types::error::{TOO_MANY_SUBSCRIPTIONS_CODE, TOO_MANY_SUBSCRIPTIONS_MSG};
	use jsonrpsee::{RpcModule, server::ServerBuilder};

	init_logger();

	let config = ServerConfig::builder().max_subscriptions_per_connection(10).build();
	let server = ServerBuilder::with_config(config).build("127.0.0.1:0").await.unwrap();
	let server_url = format!("ws://{}", server.local_addr().unwrap());

	let mut module = RpcModule::new(());

	module
		.register_subscription("subscribe_forever", "n", "unsubscribe_forever", |_, pending, _, _| async move {
			let interval = interval(Duration::from_millis(50));
			let stream = IntervalStream::new(interval).map(move |_| 0_usize);

			pipe_from_stream_and_drop(pending, stream).await.map_err(Into::into)
		})
		.unwrap();
	let _handle = server.start(module);

	let c1 = WsClientBuilder::default().build(&server_url).await.unwrap();
	let c2 = WsClientBuilder::default().build(&server_url).await.unwrap();

	let mut subs1 = Vec::new();
	let mut subs2 = Vec::new();

	for _ in 0..10 {
		subs1.push(
			c1.subscribe::<usize, ArrayParams>("subscribe_forever", rpc_params![], "unsubscribe_forever")
				.await
				.unwrap(),
		);
		subs2.push(
			c2.subscribe::<usize, ArrayParams>("subscribe_forever", rpc_params![], "unsubscribe_forever")
				.await
				.unwrap(),
		);
	}

	let err1 = c1.subscribe::<usize, ArrayParams>("subscribe_forever", rpc_params![], "unsubscribe_forever").await;
	let err2 = c1.subscribe::<usize, ArrayParams>("subscribe_forever", rpc_params![], "unsubscribe_forever").await;

	let data = "\"Exceeded max limit of 10\"";

	assert!(
		matches!(err1, Err(Error::Call(err)) if err.code() == TOO_MANY_SUBSCRIPTIONS_CODE && err.message() == TOO_MANY_SUBSCRIPTIONS_MSG && err.data().unwrap().get() == data)
	);
	assert!(
		matches!(err2, Err(Error::Call(err)) if err.code() == TOO_MANY_SUBSCRIPTIONS_CODE && err.message() == TOO_MANY_SUBSCRIPTIONS_MSG && err.data().unwrap().get() == data)
	);
}

#[tokio::test]
async fn ws_server_unsub_methods_should_ignore_sub_limit() {
	use futures::StreamExt;
	use jsonrpsee::core::client::SubscriptionKind;
	use jsonrpsee::{RpcModule, server::ServerBuilder};

	init_logger();

	let config = ServerConfig::builder().max_subscriptions_per_connection(10).build();
	let server = ServerBuilder::with_config(config).build("127.0.0.1:0").await.unwrap();
	let server_url = format!("ws://{}", server.local_addr().unwrap());

	let mut module = RpcModule::new(());

	module
		.register_subscription("subscribe_forever", "n", "unsubscribe_forever", |_, pending, _, _| async {
			let interval = interval(Duration::from_millis(50));
			let stream = IntervalStream::new(interval).map(move |_| 0_usize);

			pipe_from_stream_and_drop(pending, stream).await.map_err(Into::into)
		})
		.unwrap();
	let _handle = server.start(module);

	let client = WsClientBuilder::default().build(&server_url).await.unwrap();

	// Add 10 subscriptions (this should fill our subscription limit for this connection):
	let mut subs = Vec::new();
	for _ in 0..10 {
		subs.push(
			client
				.subscribe::<usize, ArrayParams>("subscribe_forever", rpc_params![], "unsubscribe_forever")
				.await
				.unwrap(),
		);
	}

	// Get the ID of one of them:
	let last_sub = subs.pop().unwrap();
	let last_sub_id = match last_sub.kind() {
		SubscriptionKind::Subscription(id) => id.clone(),
		_ => panic!("Expected a subscription Id to be present"),
	};

	// Manually call the unsubscribe function for this subscription:
	let res: Result<bool, _> = client.request("unsubscribe_forever", rpc_params![last_sub_id]).await;

	// This should not hit any limits, and unsubscription should have worked:
	assert!(res.is_ok(), "Unsubscription method was successfully called");
	assert!(res.unwrap(), "Unsubscription was successful");
}

#[tokio::test]
async fn http_unsupported_methods_dont_work() {
	use hyper::{Method, Request};
	use hyper_util::client::legacy::Client;

	init_logger();
	let server_addr = server().await;

	let http_client = Client::builder(TokioExecutor::new()).build_http();
	let uri = format!("http://{}", server_addr);

	let req_is_client_error = |method| async {
		let req = Request::builder()
			.method(method)
			.uri(&uri)
			.header("content-type", "application/json")
			.body(HttpBody::from(r#"{ "jsonrpc": "2.0", method: "say_hello", "id": 1 }"#))
			.expect("request builder");

		let res = http_client.request(req).await.unwrap();
		res.status().is_client_error()
	};

	for verb in [Method::GET, Method::PUT, Method::PATCH, Method::DELETE] {
		assert!(req_is_client_error(verb).await);
	}
	assert!(!req_is_client_error(Method::POST).await);
}

#[tokio::test]
async fn http_correct_content_type_required() {
	use hyper::{Method, Request};
	use hyper_util::client::legacy::Client;

	init_logger();

	let server_addr = server().await;
	let http_client = Client::builder(TokioExecutor::new()).build_http();
	let uri = format!("http://{}", server_addr);

	// We don't set content-type at all
	let req = Request::builder()
		.method(Method::POST)
		.uri(&uri)
		.body(HttpBody::from(r#"{ "jsonrpc": "2.0", method: "say_hello", "id": 1 }"#))
		.expect("request builder");

	let res = http_client.request(req).await.unwrap();
	assert!(res.status().is_client_error());

	// We use the wrong content-type
	let req = Request::builder()
		.method(Method::POST)
		.uri(&uri)
		.header("content-type", "application/text")
		.body(HttpBody::from(r#"{ "jsonrpc": "2.0", method: "say_hello", "id": 1 }"#))
		.expect("request builder");

	let res = http_client.request(req).await.unwrap();
	assert!(res.status().is_client_error());

	// We use the correct content-type
	let req = Request::builder()
		.method(Method::POST)
		.uri(&uri)
		.header("content-type", "application/json")
		.body(HttpBody::from(r#"{ "jsonrpc": "2.0", method: "say_hello", "id": 1 }"#))
		.expect("request builder");

	let res = http_client.request(req).await.unwrap();
	assert!(res.status().is_success());
}

#[tokio::test]
async fn http_cors_preflight_works() {
	use hyper::{Method, Request};
	use hyper_util::client::legacy::Client;

	init_logger();

	let cors = CorsLayer::new()
		.allow_methods([Method::POST])
		.allow_origin("https://foo.com".parse::<HeaderValue>().unwrap())
		.allow_headers([hyper::header::CONTENT_TYPE]);
	let (server_addr, _handle) = server_with_cors(cors).await;

	let http_client = Client::builder(TokioExecutor::new()).build_http();
	let uri = format!("http://{}", server_addr);

	// First, make a preflight request.
	// See https://developer.mozilla.org/en-US/docs/Web/HTTP/CORS#preflighted_requests for examples.
	// See https://fetch.spec.whatwg.org/#http-cors-protocol for the spec.
	let preflight_req = Request::builder()
		.method(Method::OPTIONS)
		.uri(&uri)
		.header("host", "bar.com") // <- host that request is being sent _to_
		.header("origin", "https://foo.com") // <- where request is being sent _from_
		.header("access-control-request-method", "POST")
		.header("access-control-request-headers", "content-type")
		.body(HttpBody::default())
		.expect("preflight request builder");

	let has = |v: &[String], s| v.iter().any(|v| v == s);

	let preflight_res = http_client.request(preflight_req).await.unwrap();
	let preflight_headers = preflight_res.headers();

	let allow_origins = comma_separated_header_values(preflight_headers, "access-control-allow-origin");
	let allow_methods = comma_separated_header_values(preflight_headers, "access-control-allow-methods");
	let allow_headers = comma_separated_header_values(preflight_headers, "access-control-allow-headers");

	// We expect the preflight response to tell us that our origin, methods and headers are all OK to use.
	// If they aren't, the browser will not make the actual request. Note that if these `access-control-*`
	// headers aren't return, the default is that the origin/method/headers are not allowed, I think.
	assert!(preflight_res.status().is_success());
	assert!(has(&allow_origins, "https://foo.com") || has(&allow_origins, "*"));
	assert!(has(&allow_methods, "post") || has(&allow_methods, "*"));
	assert!(has(&allow_headers, "content-type") || has(&allow_headers, "*"));

	// Assuming that that was successful, we now make the actual request. No CORS headers are needed here
	// as the browser checked their validity in the preflight request.
	let req = Request::builder()
		.method(Method::POST)
		.uri(&uri)
		.header("host", "bar.com")
		.header("origin", "https://foo.com")
		.header("content-type", "application/json")
		.body(HttpBody::from(r#"{ "jsonrpc": "2.0", method: "say_hello", "id": 1 }"#))
		.expect("actual request builder");

	let res = http_client.request(req).await.unwrap();
	assert!(res.status().is_success());
	assert!(has(&allow_origins, "https://foo.com") || has(&allow_origins, "*"));
}

fn comma_separated_header_values(headers: &hyper::HeaderMap, header: &str) -> Vec<String> {
	headers
		.get_all(header)
		.into_iter()
		.flat_map(|value| value.to_str().unwrap().split(',').map(|val| val.trim()))
		.map(|header| header.to_ascii_lowercase())
		.collect()
}

#[tokio::test]
async fn ws_subscribe_with_bad_params() {
	init_logger();

	let server_addr = server_with_subscription().await;
	let server_url = format!("ws://{}", server_addr);
	let client = WsClientBuilder::default().build(&server_url).await.unwrap();

	let err = client
		.subscribe::<serde_json::Value, ArrayParams>("subscribe_add_one", rpc_params!["0x0"], "unsubscribe_add_one")
		.await
		.unwrap_err();
	assert!(matches!(err, Error::Call(_)));
}

#[tokio::test]
async fn http_health_api_works() {
	use hyper::Request;
	use hyper_util::client::legacy::Client;

	init_logger();

	let (server_addr, _handle) = server_with_health_api().await;

	let http_client = Client::builder(TokioExecutor::new()).build_http();
	let uri = format!("http://{}/health", server_addr);

	let req = Request::builder().method("GET").uri(&uri).body(HttpBody::default()).expect("request builder");
	let res = http_client.request(req).await.unwrap();

	assert!(res.status().is_success());

	let bytes = res.into_body().collect().await.unwrap().to_bytes();
	let out = String::from_utf8(bytes.to_vec()).unwrap();
	assert_eq!(out.as_str(), "{\"health\":true}");
}

#[tokio::test]
async fn ws_host_filtering_wildcard_works() {
	use jsonrpsee::server::*;

	init_logger();

	let middleware =
		tower::ServiceBuilder::new().layer(HostFilterLayer::new(["http://localhost:*", "http://127.0.0.1:*"]).unwrap());

	let server = ServerBuilder::default().set_http_middleware(middleware).build("127.0.0.1:0").await.unwrap();
	let mut module = RpcModule::new(());
	let addr = server.local_addr().unwrap();
	module.register_method("say_hello", |_, _, _| "hello").unwrap();

	let _handle = server.start(module);

	let server_url = format!("ws://{}", addr);
	let client = WsClientBuilder::default().build(&server_url).await.unwrap();

	assert!(client.request::<String, ArrayParams>("say_hello", rpc_params![]).await.is_ok());
}

#[tokio::test]
async fn http_host_filtering_wildcard_works() {
	use jsonrpsee::server::*;

	init_logger();

	let middleware =
		tower::ServiceBuilder::new().layer(HostFilterLayer::new(["http://localhost:*", "http://127.0.0.1:*"]).unwrap());

	let server = ServerBuilder::default().set_http_middleware(middleware).build("127.0.0.1:0").await.unwrap();
	let mut module = RpcModule::new(());
	let addr = server.local_addr().unwrap();
	module.register_method("say_hello", |_, _, _| "hello").unwrap();

	let _handle = server.start(module);

	let server_url = format!("http://{}", addr);
	let client = HttpClientBuilder::default().build(&server_url).unwrap();

	assert!(client.request::<String, ArrayParams>("say_hello", rpc_params![]).await.is_ok());
}

#[tokio::test]
async fn deny_invalid_host() {
	use jsonrpsee::server::*;

	init_logger();

	let middleware = tower::ServiceBuilder::new().layer(HostFilterLayer::new(["example.com"]).unwrap());

	let server = Server::builder().set_http_middleware(middleware).build("127.0.0.1:0").await.unwrap();
	let mut module = RpcModule::new(());
	let addr = server.local_addr().unwrap();
	module.register_method("say_hello", |_, _, _| "hello").unwrap();

	let _handle = server.start(module);

	// HTTP
	{
		let server_url = format!("http://{}", addr);
		let client = HttpClientBuilder::default().build(&server_url).unwrap();
		assert!(client.request::<String, _>("say_hello", rpc_params![]).await.is_err());
	}

	// WebSocket
	{
		let server_url = format!("ws://{}", addr);
		let err = WsClientBuilder::default().build(&server_url).await.unwrap_err();
		assert!(
			matches!(err, Error::Transport(e) if e.to_string().contains("Connection rejected with status code: 403"))
		)
	}
}

#[tokio::test]
async fn disable_host_filter_works() {
	use jsonrpsee::server::*;

	init_logger();

	let middleware = tower::ServiceBuilder::new().layer(HostFilterLayer::disable());

	let server = Server::builder().set_http_middleware(middleware).build("127.0.0.1:0").await.unwrap();
	let mut module = RpcModule::new(());
	let addr = server.local_addr().unwrap();
	module.register_method("say_hello", |_, _, _| "hello").unwrap();

	let _handle = server.start(module);

	// HTTP
	{
		let server_url = format!("http://{}", addr);
		let client = HttpClientBuilder::default().build(&server_url).unwrap();
		assert!(client.request::<String, _>("say_hello", rpc_params![]).await.is_ok());
	}

	// WebSocket
	{
		let server_url = format!("ws://{}", addr);
		assert!(WsClientBuilder::default().build(&server_url).await.is_ok());
	}
}

#[tokio::test]
async fn subscription_option_err_is_not_sent() {
	init_logger();

	let server_addr = server_with_subscription().await;
	let server_url = format!("ws://{}", server_addr);
	let client = WsClientBuilder::default().build(&server_url).await.unwrap();

	let mut sub = client
		.subscribe::<serde_json::Value, ArrayParams>("subscribe_option", rpc_params![], "unsubscribe_option")
		.await
		.unwrap();

	// the subscription never gets a special notification so the client doesn't know
	// that it has been closed.
	assert!(sub.next().with_timeout(std::time::Duration::from_secs(10)).await.is_err());
}

#[tokio::test]
async fn subscription_err_is_sent() {
	init_logger();

	let server_addr = server_with_subscription().await;
	let server_url = format!("ws://{}", server_addr);
	let client = WsClientBuilder::default().build(&server_url).await.unwrap();

	let mut sub = client
		.subscribe::<serde_json::Value, ArrayParams>("subscribe_noop", rpc_params![], "unsubscribe_noop")
		.await
		.unwrap();

	// the subscription is closed once the error notification comes.
	assert!(sub.next().await.is_none());
}

#[tokio::test]
async fn subscription_ok_unit_not_sent() {
	init_logger();

	let server_addr = server_with_subscription().await;
	let server_url = format!("ws://{}", server_addr);
	let client = WsClientBuilder::default().build(&server_url).await.unwrap();

	let mut sub = client
		.subscribe::<serde_json::Value, ArrayParams>("subscribe_unit", rpc_params![], "unsubscribe_unit")
		.await
		.unwrap();

	// Assert that `result: null` is not sent.
	assert!(sub.next().with_timeout(std::time::Duration::from_secs(10)).await.is_err());
}

#[tokio::test]
async fn graceful_shutdown_works() {
	init_logger();

	run_shutdown_test("http").await;
	run_shutdown_test("ws").await;
}

async fn run_shutdown_test_inner<C: ClientT + Send + Sync + 'static>(
	client: Arc<C>,
	handle: ServerHandle,
	call_answered: Arc<AtomicBool>,
	mut call_ack: tokio::sync::mpsc::UnboundedReceiver<()>,
) {
	const LEN: usize = 10;

	let mut calls: FuturesUnordered<_> = (0..LEN)
		.map(|_| {
			let c = client.clone();
			async move { c.request::<String, _>("sleep_20s", rpc_params!()).await }
		})
		.collect();

	let res = tokio::spawn(async move {
		let mut c = 0;
		while let Some(Ok(_)) = calls.next().await {
			c += 1;
		}
		c
	});

	// All calls has been received by server => then stop.
	for _ in 0..LEN {
		call_ack.recv().await.unwrap();
	}

	// Assert that no calls have been answered yet
	// The assumption is that the server should be able to read 10 messages < 10s.
	assert!(!call_answered.load(std::sync::atomic::Ordering::SeqCst));

	// Stop the server.
	handle.stop().unwrap();

	// This is to ensure that server main has some time to receive the stop signal.
	tokio::time::sleep(Duration::from_millis(100)).await;

	// Make a call between the server stop has been requested and until all pending calls have been resolved.
	let c = client.clone();
	let call_after_stop = tokio::spawn(async move { c.request::<String, _>("sleep_20s", rpc_params!()).await });

	handle.stopped().await;

	assert!(call_after_stop.await.unwrap().is_err());

	// The pending calls should be answered before shutdown.
	assert_eq!(res.await.unwrap(), LEN);

	// The server should be closed now.
	assert!(client.request::<String, _>("sleep_20s", rpc_params!()).await.is_err());
}

#[tokio::test]
async fn response_payload_async_api_works() {
	use jsonrpsee::server::{Server, SubscriptionSink};
	use std::sync::Arc;
	use tokio::sync::Mutex as AsyncMutex;

	init_logger();

	let server_addr = {
		#[allow(clippy::type_complexity)]
		let state: Arc<AsyncMutex<Option<(SubscriptionSink, tokio::sync::oneshot::Sender<()>)>>> = Arc::default();

		let mut module = RpcModule::new(state);
		module
			.register_method("get", |_params, ctx, _| {
				let ctx = ctx.clone();
				let (rp, rp_future) = ResponsePayload::success(1).notify_on_completion();

				tokio::spawn(async move {
					// Wait for response to sent to the internal WebSocket message buffer
					// and if that fails just quit because it means that the connection
					// was closed or that method response was an error.
					//
					// You can identify that by matching on the error.
					if rp_future.await.is_err() {
						return;
					}

					if let Some((sink, close)) = ctx.lock().await.take() {
						for idx in 0..3 {
							let msg = serde_json::value::to_raw_value(&idx).unwrap();
							_ = sink.send(msg).await;
						}
						drop(sink);
						drop(close);
					}
				});

				rp
			})
			.unwrap();

		module
			.register_subscription::<Result<(), SubscriptionError>, _, _>(
				"sub",
				"s",
				"unsub",
				|_, pending, ctx, _| async move {
					let sink = pending.accept().await?;
					let (tx, rx) = tokio::sync::oneshot::channel();
					*ctx.lock().await = Some((sink, tx));
					let _ = rx.await;
					Err("Dropped".into())
				},
			)
			.unwrap();

		let server = Server::builder().build("127.0.0.1:0").with_default_timeout().await.unwrap().unwrap();
		let addr = server.local_addr().unwrap();

		let handle = server.start(module);

		tokio::spawn(handle.stopped());

		format!("ws://{addr}")
	};

	let client = jsonrpsee::ws_client::WsClientBuilder::default()
		.build(&server_addr)
		.with_default_timeout()
		.await
		.unwrap()
		.unwrap();

	// Make a subscription which is stored as state in the sequent rpc call "get".
	let sub =
		client.subscribe::<usize, _>("sub", rpc_params!(), "unsub").with_default_timeout().await.unwrap().unwrap();

	// assert that method call was answered
	// and a few notification were sent by
	// the spawned the task.
	//
	// ideally, that ordering should also be tested here
	// but not possible to test properly.
	assert!(client.request::<usize, _>("get", rpc_params!()).await.is_ok());
	assert_eq!(sub.count().await, 3);
}

/// Run shutdown test and it does:
///
/// - Make 10 calls that sleeps for 20 seconds
/// - Once they have all reached the server but before they have responded, stop the server.
/// - Ensure that no calls handled between the server has been requested to stop and until it has been stopped.
/// - All calls should be responded to before the server shuts down.
/// - No calls should be handled after the server has been stopped.
///
async fn run_shutdown_test(transport: &str) {
	let (tx, call_ack) = tokio::sync::mpsc::unbounded_channel();
	let call_answered = Arc::new(AtomicBool::new(false));

	let (handle, addr) = {
		let server = ServerBuilder::default().build("127.0.0.1:0").with_default_timeout().await.unwrap().unwrap();

		let mut module = RpcModule::new((tx, call_answered.clone()));

		module
			.register_async_method("sleep_20s", |_, mut ctx, _| async move {
				let ctx = Arc::make_mut(&mut ctx);
				let _ = ctx.0.send(());
				tokio::time::sleep(Duration::from_secs(20)).await;
				ctx.1.store(true, std::sync::atomic::Ordering::SeqCst);
				"ok"
			})
			.unwrap();
		let addr = server.local_addr().unwrap();

		(server.start(module), addr)
	};

	match transport {
		"ws" => {
			let ws = Arc::new(WsClientBuilder::default().build(format!("ws://{addr}")).await.unwrap());
			run_shutdown_test_inner(ws, handle, call_answered, call_ack).await
		}
		"http" => {
			let http = Arc::new(HttpClientBuilder::default().build(format!("http://{addr}")).unwrap());
			run_shutdown_test_inner(http, handle, call_answered, call_ack).await
		}
		_ => unreachable!("Only `http` and `ws` supported"),
	}
}

#[tokio::test]
async fn server_ws_low_api_works() {
	let local_addr = run_server().await.unwrap();

	let client = WsClientBuilder::default().build(&format!("ws://{}", local_addr)).await.unwrap();
	assert!(matches!(client.request::<String, _>("say_hello", rpc_params![]).await, Ok(r) if r == "hello"));

	async fn run_server() -> anyhow::Result<SocketAddr> {
		use futures_util::future::FutureExt;
		use jsonrpsee::core::{BoxError, middleware::RpcServiceBuilder};
		use jsonrpsee::server::{
			ConnectionGuard, ConnectionState, Methods, ServerConfig, StopHandle, http, serve_with_graceful_shutdown,
			stop_channel, ws,
		};

		let listener = tokio::net::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0))).await?;
		let local_addr = listener.local_addr()?;
		let (stop_handle, server_handle) = stop_channel();

		let mut methods = RpcModule::new(());

		methods.register_async_method("say_hello", |_, _, _| async { "hello" }).unwrap();

		#[derive(Clone)]
		struct PerConnection {
			methods: Methods,
			stop_handle: StopHandle,
			conn_guard: ConnectionGuard,
		}

		let per_conn = PerConnection {
			methods: methods.into(),
			stop_handle: stop_handle.clone(),
			conn_guard: ConnectionGuard::new(100),
		};

		tokio::spawn(async move {
			loop {
				let (sock, _) = tokio::select! {
					res = listener.accept() => {
						match res {
							Ok(sock) => sock,
							Err(e) => {
								tracing::error!("Failed to accept v4 connection: {:?}", e);
								continue;
							}
						}
					}
					_ = per_conn.stop_handle.clone().shutdown() => break,
				};
				let per_conn = per_conn.clone();

				let stop_handle2 = per_conn.stop_handle.clone();
				let per_conn = per_conn.clone();
				let svc = tower::service_fn(move |req| {
					let PerConnection { methods, stop_handle, conn_guard } = per_conn.clone();
					let conn_permit =
						conn_guard.try_acquire().expect("Connection limit is 100 must be work for two connections");

					if ws::is_upgrade_request(&req) {
						let rpc_service = RpcServiceBuilder::new();

						let conn = ConnectionState::new(stop_handle, 0, conn_permit);

						async move {
							match ws::connect(req, ServerConfig::default(), methods, conn, rpc_service).await {
								Ok((rp, conn_fut)) => {
									tokio::spawn(conn_fut);
									Ok(rp)
								}
								Err(rp) => Ok(rp),
							}
						}
						.boxed()
					} else {
						async { Ok::<_, BoxError>(http::response::denied()) }.boxed()
					}
				});

				tokio::spawn(serve_with_graceful_shutdown(sock, svc, stop_handle2.shutdown()));
			}
		});

		tokio::spawn(server_handle.stopped());

		Ok(local_addr)
	}
}

#[tokio::test]
async fn http_connection_guard_works() {
	use jsonrpsee::{RpcModule, server::ServerBuilder};
	use tokio::sync::mpsc;

	init_logger();

	let (tx, mut rx) = mpsc::channel::<()>(1);

	let server_url = {
		let server = ServerBuilder::default().build("127.0.0.1:0").await.unwrap();
		let server_url = format!("http://{}", server.local_addr().unwrap());
		let mut module = RpcModule::new(tx);

		module
			.register_async_method("wait_until", |_, wait, _| async move {
				wait.send(()).await.unwrap();
				wait.closed().await;
				true
			})
			.unwrap();

		module
			.register_async_method("connection_count", |_, _, ctx| async move {
				let conn = ctx.get::<ConnectionGuard>().unwrap();
				conn.max_connections() - conn.available_connections()
			})
			.unwrap();

		let handle = server.start(module);

		tokio::spawn(handle.stopped());

		server_url
	};

	let waiting_calls: Vec<_> = (0..2)
		.map(|_| {
			let client = HttpClientBuilder::default().build(&server_url).unwrap();
			tokio::spawn(async move {
				let _ = client.request::<bool, ArrayParams>("wait_until", rpc_params!()).await;
			})
		})
		.collect();

	// Wait until both calls are ACK:ed by the server.
	rx.recv().await.unwrap();
	rx.recv().await.unwrap();

	// Assert that two calls are waiting to be answered and the current one.
	{
		let client = HttpClientBuilder::default().build(&server_url).unwrap();
		let conn_count = client.request::<usize, ArrayParams>("connection_count", rpc_params!()).await.unwrap();
		assert_eq!(conn_count, 3);
	}

	// Complete the waiting calls.
	drop(rx);
	futures::future::join_all(waiting_calls).await;

	// Assert that connection count is back to 1.
	{
		let client = HttpClientBuilder::default().build(&server_url).unwrap();
		let conn_count = client.request::<usize, ArrayParams>("connection_count", rpc_params!()).await.unwrap();
		assert_eq!(conn_count, 1);
	}
}
