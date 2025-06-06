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

//! Example of using proc macro to generate working client and server.

#![cfg(test)]

mod helpers;

use std::net::SocketAddr;

use helpers::init_logger;
use jsonrpsee::core::client::{ClientT, Error, SubscriptionClientT};
use jsonrpsee::http_client::HttpClientBuilder;
use jsonrpsee::rpc_params;
use jsonrpsee::server::ServerBuilder;
use jsonrpsee::types::error::{ErrorCode, INVALID_PARAMS_MSG};

use jsonrpsee::ws_client::*;
use serde_json::json;

mod rpc_impl {
	use jsonrpsee::core::server::{IntoSubscriptionCloseResponse, PendingSubscriptionSink, SubscriptionCloseResponse};
	use jsonrpsee::core::{SubscriptionResult, async_trait};
	use jsonrpsee::proc_macros::rpc;
	use jsonrpsee::types::{ErrorObject, ErrorObjectOwned};

	pub struct CustomSubscriptionRet;

	impl IntoSubscriptionCloseResponse for CustomSubscriptionRet {
		fn into_response(self) -> SubscriptionCloseResponse {
			SubscriptionCloseResponse::None
		}
	}

	pub struct MyError;

	impl From<MyError> for ErrorObjectOwned {
		fn from(_: MyError) -> Self {
			ErrorObject::owned(1, "my_error", None::<()>)
		}
	}

	#[rpc(client, server, namespace = "foo")]
	pub trait Rpc {
		#[method(name = "foo")]
		async fn async_method(&self, param_a: u8, param_b: String) -> Result<u16, ErrorObjectOwned>;

		#[method(name = "bar")]
		fn sync_method(&self) -> Result<u16, ErrorObjectOwned>;

		#[subscription(name = "sub", unsubscribe = "unsub", item = String)]
		async fn sub(&self) -> SubscriptionResult;
		#[subscription(name = "echo", unsubscribe = "unsubscribe_echo", aliases = ["alias_echo"], item = u32)]
		async fn sub_with_params(&self, val: u32) -> SubscriptionResult;

		#[subscription(name = "not-result", unsubscribe = "unsubscribe-not-result", item = String)]
		async fn sub_not_result(&self);

		#[subscription(name = "custom", unsubscribe = "unsubscribe_custom", item = String)]
		async fn sub_custom_ret(&self, x: usize) -> CustomSubscriptionRet;

		#[subscription(name = "unit_type", unsubscribe = "unsubscribe_unit_type", item = String)]
		async fn sub_unit_type(&self, x: usize);

		#[subscription(name = "sync_sub", unsubscribe = "sync_unsub", item = String)]
		fn sync_sub(&self);

		#[method(name = "params")]
		fn params(&self, a: u8, b: &str) -> Result<String, ErrorObjectOwned> {
			Ok(format!("Called with: {}, {}", a, b))
		}

		#[method(name = "optional_params")]
		fn optional_params(&self, a: u32, b: Option<u32>, c: Option<u32>) -> Result<String, ErrorObjectOwned> {
			Ok(format!("Called with: {}, {:?}, {:?}", a, b, c))
		}

		#[method(name = "lifetimes")]
		fn lifetimes(
			&self,
			a: &str,
			b: &'_ str,
			c: std::borrow::Cow<'_, str>,
			d: Option<std::borrow::Cow<'_, str>>,
		) -> Result<String, ErrorObjectOwned> {
			Ok(format!("Called with: {}, {}, {}, {:?}", a, b, c, d))
		}

		#[method(name = "zero_copy_cow")]
		fn zero_copy_cow(&self, a: std::borrow::Cow<'_, str>) -> Result<String, ErrorObjectOwned> {
			Ok(format!("Zero copy params: {}", matches!(a, std::borrow::Cow::Borrowed(_))))
		}

		#[method(name = "blocking_call", blocking)]
		fn blocking_call(&self) -> Result<u32, ErrorObjectOwned> {
			std::thread::sleep(std::time::Duration::from_millis(50));
			Ok(42)
		}

		#[method(name = "blocking_call_custom_err", blocking)]
		fn blocking_call_custom_err(&self) -> Result<usize, MyError> {
			std::thread::sleep(std::time::Duration::from_millis(50));
			Ok(42)
		}

		#[method(name = "blocking_call_custom_err_with_params", blocking)]
		fn blocking_call_custom_err_with_params(&self, x: usize) -> Result<usize, MyError> {
			std::thread::sleep(std::time::Duration::from_millis(50));
			Ok(x)
		}

		#[method(name = "my_err")]
		fn custom_error(&self) -> Result<(), MyError> {
			Err(MyError)
		}

		#[method(name = "my_err_with_param")]
		fn custom_error_with_param(&self, _s: String) -> Result<(), MyError> {
			Err(MyError)
		}
	}

	pub struct RpcServerImpl;

	#[async_trait]
	impl RpcServer for RpcServerImpl {
		async fn async_method(&self, _param_a: u8, _param_b: String) -> Result<u16, ErrorObjectOwned> {
			Ok(42)
		}

		fn sync_method(&self) -> Result<u16, ErrorObjectOwned> {
			Ok(10)
		}

		async fn sub(&self, pending: PendingSubscriptionSink) -> SubscriptionResult {
			let sink = pending.accept().await?;
			let msg1 = serde_json::value::to_raw_value(&"Response_A").unwrap();
			let msg2 = serde_json::value::to_raw_value(&"Response_B").unwrap();
			sink.send(msg1).await?;
			sink.send(msg2).await?;

			Ok(())
		}

		async fn sub_with_params(&self, pending: PendingSubscriptionSink, val: u32) -> SubscriptionResult {
			let sink = pending.accept().await?;
			let json = serde_json::value::to_raw_value(&val).unwrap();
			sink.send(json.clone()).await?;
			sink.send(json).await?;

			Ok(())
		}

		async fn sub_not_result(&self, pending: PendingSubscriptionSink) {
			let sink = pending.accept().await.unwrap();
			let msg = serde_json::value::to_raw_value("lo").unwrap();
			sink.send(msg).await.unwrap();
		}

		async fn sub_custom_ret(&self, _pending: PendingSubscriptionSink, _x: usize) -> CustomSubscriptionRet {
			CustomSubscriptionRet
		}

		fn sync_sub(&self, pending: PendingSubscriptionSink) {
			tokio::spawn(async move {
				let sink = pending.accept().await.unwrap();
				let msg = serde_json::value::to_raw_value("hello").unwrap();
				sink.send(msg).await.unwrap();
			});
		}

		async fn sub_unit_type(&self, _pending: PendingSubscriptionSink, _x: usize) {}
	}
}

// Use generated implementations of server and client.
use jsonrpsee::core::params::{ArrayParams, ObjectParams};
use rpc_impl::{RpcClient, RpcServer, RpcServerImpl};

pub async fn server() -> SocketAddr {
	let server = ServerBuilder::default().build("127.0.0.1:0").await.unwrap();
	let addr = server.local_addr().unwrap();
	let handle = server.start(RpcServerImpl.into_rpc());

	tokio::spawn(handle.stopped());

	addr
}

#[tokio::test]
async fn proc_macros_generic_ws_client_api() {
	init_logger();

	let server_addr = server().await;
	let server_url = format!("ws://{}", server_addr);
	let client = WsClientBuilder::default().build(&server_url).await.unwrap();

	assert_eq!(client.async_method(10, "a".into()).await.unwrap(), 42);
	assert_eq!(client.sync_method().await.unwrap(), 10);

	// Sub without params
	let mut sub = client.sub().await.unwrap();
	let first_recv = sub.next().await.unwrap().unwrap();
	assert_eq!(first_recv, "Response_A".to_string());
	let second_recv = sub.next().await.unwrap().unwrap();
	assert_eq!(second_recv, "Response_B".to_string());

	// Sub with params
	let mut sub = client.sub_with_params(42).await.unwrap();
	let first_recv = sub.next().await.unwrap().unwrap();
	assert_eq!(first_recv, 42);
	let second_recv = sub.next().await.unwrap().unwrap();
	assert_eq!(second_recv, 42);
}

#[tokio::test]
async fn macro_param_parsing() {
	let module = RpcServerImpl.into_rpc();

	let res: String = module.call("foo_params", [json!(42_u64), json!("Hello")]).await.unwrap();

	assert_eq!(&res, "Called with: 42, Hello");
}

#[tokio::test]
async fn macro_optional_param_parsing() {
	let module = RpcServerImpl.into_rpc();

	// Optional param omitted at tail
	let res: String = module.call("foo_optional_params", [42_u64, 70]).await.unwrap();
	assert_eq!(&res, "Called with: 42, Some(70), None");

	// Optional param using `null`
	let res: String = module.call("foo_optional_params", [json!(42_u64), json!(null), json!(70_u64)]).await.unwrap();

	assert_eq!(&res, "Called with: 42, None, Some(70)");

	// Named params using a map
	let (resp, _) = module
		.raw_json_request(r#"{"jsonrpc":"2.0","method":"foo_optional_params","params":{"a":22,"c":50},"id":0}"#, 1)
		.await
		.unwrap();
	assert_eq!(resp.get(), r#"{"jsonrpc":"2.0","id":0,"result":"Called with: 22, None, Some(50)"}"#);
}

#[tokio::test]
async fn macro_lifetimes_parsing() {
	let module = RpcServerImpl.into_rpc();

	let res: String = module.call("foo_lifetimes", ["foo", "bar", "baz", "qux"]).await.unwrap();

	assert_eq!(&res, "Called with: foo, bar, baz, Some(\"qux\")");
}

#[tokio::test]
async fn macro_zero_copy_cow() {
	init_logger();

	let module = RpcServerImpl.into_rpc();

	let (resp, _) = module
		.raw_json_request(r#"{"jsonrpc":"2.0","method":"foo_zero_copy_cow","params":["foo"],"id":0}"#, 1)
		.await
		.unwrap();

	// std::borrow::Cow<str> always deserialized to owned variant here
	assert_eq!(resp.get(), r#"{"jsonrpc":"2.0","id":0,"result":"Zero copy params: false"}"#);

	// serde_json will have to allocate a new string to replace `\t` with byte 0x09 (tab)
	let (resp, _) = module
		.raw_json_request(r#"{"jsonrpc":"2.0","method":"foo_zero_copy_cow","params":["\tfoo"],"id":0}"#, 1)
		.await
		.unwrap();
	assert_eq!(resp.get(), r#"{"jsonrpc":"2.0","id":0,"result":"Zero copy params: false"}"#);
}

#[tokio::test]
async fn namespace_separator_dot_formatting_works() {
	use jsonrpsee::core::async_trait;
	use jsonrpsee::proc_macros::rpc;
	use serde_json::json;

	#[rpc(server, namespace = "foo", namespace_separator = ".")]
	pub trait DotSeparatorRpc {
		#[method(name = "dot")]
		fn dot(&self, a: u32, b: &str) -> Result<String, jsonrpsee::types::ErrorObjectOwned>;
	}

	struct DotImpl;

	#[async_trait]
	impl DotSeparatorRpcServer for DotImpl {
		fn dot(&self, a: u32, b: &str) -> Result<String, jsonrpsee::types::ErrorObjectOwned> {
			Ok(format!("Called with: {}, {}", a, b))
		}
	}

	let module = DotImpl.into_rpc();
	let res: String = module.call("foo.dot", [json!(1_u64), json!("test")]).await.unwrap();

	assert_eq!(&res, "Called with: 1, test");
}

#[tokio::test]
async fn namespace_separator_slash_formatting_works() {
	use jsonrpsee::core::async_trait;
	use jsonrpsee::proc_macros::rpc;
	use serde_json::json;

	#[rpc(server, namespace = "math", namespace_separator = "/")]
	pub trait SlashSeparatorRpc {
		#[method(name = "add")]
		fn add(&self, x: i32, y: i32) -> Result<i32, jsonrpsee::types::ErrorObjectOwned>;
	}

	struct SlashImpl;

	#[async_trait]
	impl SlashSeparatorRpcServer for SlashImpl {
		fn add(&self, x: i32, y: i32) -> Result<i32, jsonrpsee::types::ErrorObjectOwned> {
			Ok(x + y)
		}
	}

	let module = SlashImpl.into_rpc();
	let result: i32 = module.call("math/add", [json!(20), json!(22)]).await.unwrap();

	assert_eq!(result, 42);
}

// Disabled on MacOS as GH CI timings on Mac vary wildly (~100ms) making this test fail.
#[cfg(not(target_os = "macos"))]
#[ignore]
#[tokio::test]
async fn multiple_blocking_calls_overlap() {
	use jsonrpsee::core::EmptyServerParams;
	use std::time::{Duration, Instant};

	let module = RpcServerImpl.into_rpc();

	let futures =
		std::iter::repeat_with(|| module.call::<_, u64>("foo_blocking_call", EmptyServerParams::new())).take(4);
	let now = Instant::now();
	let results = futures::future::join_all(futures).await;
	let elapsed = now.elapsed();

	for result in results {
		assert_eq!(result.unwrap(), 42);
	}

	// Each request takes 50ms, added 50ms margin for scheduling
	assert!(elapsed < Duration::from_millis(100), "Expected less than 100ms, got {:?}", elapsed);
}

#[tokio::test]
async fn subscriptions_do_not_work_for_http_servers() {
	let htserver = ServerBuilder::default().build("127.0.0.1:0").await.unwrap();
	let addr = htserver.local_addr().unwrap();
	let htserver_url = format!("http://{}", addr);
	let _handle = htserver.start(RpcServerImpl.into_rpc());

	let htclient = HttpClientBuilder::default().build(&htserver_url).unwrap();

	assert_eq!(htclient.sync_method().await.unwrap(), 10);
	assert!(htclient.sub().await.is_err());
	assert!(matches!(htclient.sub().await, Err(Error::HttpNotImplemented)));
	assert_eq!(htclient.sync_method().await.unwrap(), 10);
}

#[tokio::test]
async fn calls_with_bad_params() {
	init_logger();

	let server_addr = server().await;
	let server_url = format!("ws://{}", server_addr);
	let client = WsClientBuilder::default().build(&server_url).await.unwrap();

	// Sub with faulty params as array.
	let err: Error = client
		.subscribe::<String, ArrayParams>("foo_echo", rpc_params!["0x0"], "foo_unsubscribe_echo")
		.await
		.unwrap_err();

	assert!(
		matches!(err, Error::Call(e) if e.data().unwrap().get().contains("invalid type: string \\\"0x0\\\", expected u32") && e.code() == ErrorCode::InvalidParams.code()
			&& e.message() == INVALID_PARAMS_MSG)
	);

	// Call with faulty params as array.
	let err: Error = client.request::<String, ArrayParams>("foo_foo", rpc_params!["faulty", "ok"]).await.unwrap_err();
	assert!(
		matches!(err, Error::Call(e) if e.data().unwrap().get().contains("invalid type: string \\\"faulty\\\", expected u8") && e.code() == ErrorCode::InvalidParams.code()
		&& e.message() == INVALID_PARAMS_MSG)
	);

	// Sub with faulty params as map.
	let mut params = ObjectParams::new();
	params.insert("val", "0x0").unwrap();

	let err: Error =
		client.subscribe::<String, ObjectParams>("foo_echo", params, "foo_unsubscribe_echo").await.unwrap_err();
	assert!(
		matches!(err, Error::Call(e) if e.data().unwrap().get().contains("invalid type: string \\\"0x0\\\", expected u32") && e.code() == ErrorCode::InvalidParams.code()
				&& e.message() == INVALID_PARAMS_MSG
		)
	);

	// Call with faulty params as map.
	let mut params = ObjectParams::new();
	params.insert("param_a", 1).unwrap();
	params.insert("param_b", 2).unwrap();

	let err: Error = client.request::<String, ObjectParams>("foo_foo", params).await.unwrap_err();

	assert!(
		matches!(err, Error::Call(e) if e.data().unwrap().get().contains("invalid type: integer `2`, expected a string") && e.code() == ErrorCode::InvalidParams.code()
				&& e.message() == INVALID_PARAMS_MSG
		)
	);
}

#[tokio::test]
async fn calls_with_object_params_works() {
	let server = ServerBuilder::default().build("127.0.0.1:0").await.unwrap();
	let addr = server.local_addr().unwrap();
	let server_url = format!("ws://{}", addr);
	let _handle = server.start(RpcServerImpl.into_rpc());
	let client = WsClientBuilder::default().build(&server_url).await.unwrap();

	// snake_case params
	let mut params = ObjectParams::new();
	params.insert("param_a", 0).unwrap();
	params.insert("param_b", "0x0").unwrap();

	assert_eq!(client.request::<u64, ObjectParams>("foo_foo", params).await.unwrap(), 42);

	// camelCase params.
	let mut params = ObjectParams::new();
	params.insert("paramA", 0).unwrap();
	params.insert("paramB", "0x0").unwrap();

	assert_eq!(client.request::<u64, ObjectParams>("foo_foo", params).await.unwrap(), 42);
}

#[tokio::test]
async fn sync_sub_works() {
	let server = ServerBuilder::default().build("127.0.0.1:0").await.unwrap();
	let addr = server.local_addr().unwrap();
	let server_url = format!("ws://{}", addr);
	let _handle = server.start(RpcServerImpl.into_rpc());
	let client = WsClientBuilder::default().build(&server_url).await.unwrap();

	let mut sub = client.sync_sub().await.unwrap();

	assert_eq!(sub.next().await.unwrap().unwrap(), "hello");
}
