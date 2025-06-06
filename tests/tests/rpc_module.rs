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

mod helpers;

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use futures::StreamExt;
use helpers::{init_logger, pipe_from_stream_and_drop};
use jsonrpsee::core::{EmptyServerParams, SubscriptionError};
use jsonrpsee::core::{RpcResult, server::*};
use jsonrpsee::types::error::{ErrorCode, ErrorObject, INVALID_PARAMS_MSG, PARSE_ERROR_CODE};
use jsonrpsee::types::{ErrorObjectOwned, Response, ResponsePayload};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use tokio::sync::mpsc;
use tokio::time::interval;
use tokio_stream::wrappers::IntervalStream;

// Helper macro to assert that a binding is of a specific type.
macro_rules! assert_type {
	( $ty:ty, $expected:expr $(,)?) => {{
		fn assert_type<Expected>(_expected: &Expected) {}
		assert_type::<$ty>($expected)
	}};
}

#[test]
fn rpc_modules_with_different_contexts_can_be_merged() {
	let cx = Vec::<u8>::new();
	let mut mod1 = RpcModule::new(cx);
	mod1.register_method("bla with Vec context", |_, _, _| ()).unwrap();
	let mut mod2 = RpcModule::new(String::new());
	mod2.register_method("bla with String context", |_, _, _| ()).unwrap();

	mod1.merge(mod2).unwrap();

	assert!(mod1.method("bla with Vec context").is_some());
	assert!(mod1.method("bla with String context").is_some());
}

#[test]
fn flatten_rpc_modules() {
	let mod1 = RpcModule::new(String::new());
	assert_type!(RpcModule<String>, &mod1);
	let unit_mod = mod1.remove_context();
	assert_type!(RpcModule<()>, &unit_mod);
}

#[test]
fn rpc_context_modules_can_register_subscriptions() {
	let mut cxmodule = RpcModule::new(());
	cxmodule.register_subscription("hi", "hi", "goodbye", |_, _, _, _| async { Ok(()) }).unwrap();

	assert!(cxmodule.method("hi").is_some());
	assert!(cxmodule.method("goodbye").is_some());
}

#[test]
fn rpc_register_alias() {
	let mut module = RpcModule::new(());

	module.register_method("hello_world", |_, _, _| RpcResult::Ok(())).unwrap();
	module.register_alias("hello_foobar", "hello_world").unwrap();

	assert!(module.method("hello_world").is_some());
	assert!(module.method("hello_foobar").is_some());
}

#[tokio::test]
async fn calling_method_without_server() {
	// Call sync method with no params
	let mut module = RpcModule::new(());
	module.register_method("boo", |_, _, _| String::from("boo!")).unwrap();

	let res: String = module.call("boo", EmptyServerParams::new()).await.unwrap();
	assert_eq!(&res, "boo!");

	// Call sync method with params
	module
		.register_method::<Result<u16, ErrorObjectOwned>, _>("foo", |params, _, _| {
			let n: u16 = params.one()?;
			Ok(n * 2)
		})
		.unwrap();
	let res: u64 = module.call("foo", [3_u64]).await.unwrap();
	assert_eq!(res, 6);

	// Call sync method with bad param
	let err = module.call::<_, EmptyServerParams>("foo", (false,)).await.unwrap_err();
	assert!(matches!(
		err,
		MethodsError::JsonRpc(err) if err.code() == ErrorCode::InvalidParams.code() && err.message() == INVALID_PARAMS_MSG && err.data().unwrap().get().contains("invalid type: boolean `false`, expected u16 at line 1 column 6")
	));

	// Call async method with params and context
	struct MyContext;
	impl MyContext {
		fn roo(&self, things: Vec<u8>) -> u16 {
			things.iter().sum::<u8>().into()
		}
	}
	let mut module = RpcModule::new(MyContext);
	module
		.register_async_method("roo", |params, ctx, _| {
			let ns: Vec<u8> = params.parse().expect("valid params please");
			async move { ctx.roo(ns) }
		})
		.unwrap();
	let res: u64 = module.call("roo", [12, 13]).await.unwrap();
	assert_eq!(res, 25);
}

#[tokio::test]
async fn calling_method_without_server_using_proc_macro() {
	use jsonrpsee::{core::async_trait, proc_macros::rpc};
	// Setup
	#[derive(Debug, Deserialize, Serialize)]
	#[allow(unreachable_pub)]
	pub struct Gun {
		shoots: bool,
	}

	#[derive(Debug, Deserialize, Serialize)]
	#[allow(unreachable_pub)]
	pub struct Beverage {
		ice: bool,
	}

	#[rpc(server)]
	pub trait Cool {
		/// Sync method, no params.
		#[method(name = "rebel_without_cause")]
		fn rebel_without_cause(&self) -> RpcResult<bool>;

		/// Sync method.
		#[method(name = "rebel")]
		fn rebel(&self, gun: Gun, map: HashMap<u8, u8>) -> RpcResult<String>;

		/// Async method.
		#[method(name = "revolution")]
		async fn can_have_any_name(&self, beverage: Beverage, some_bytes: Vec<u8>) -> RpcResult<String>;

		/// Async method with option.
		#[method(name = "can_have_options")]
		async fn can_have_options(&self, x: usize) -> RpcResult<Option<String>>;
	}

	struct CoolServerImpl;

	#[async_trait]
	impl CoolServer for CoolServerImpl {
		fn rebel_without_cause(&self) -> RpcResult<bool> {
			Ok(false)
		}

		fn rebel(&self, gun: Gun, map: HashMap<u8, u8>) -> RpcResult<String> {
			Ok(format!("{} {:?}", map.values().len(), gun))
		}

		async fn can_have_any_name(&self, beverage: Beverage, some_bytes: Vec<u8>) -> RpcResult<String> {
			Ok(format!("drink: {:?}, phases: {:?}", beverage, some_bytes))
		}

		async fn can_have_options(&self, x: usize) -> RpcResult<Option<String>> {
			match x {
				0 => Ok(Some("one".to_string())),
				1 => Ok(None),
				_ => Err(ErrorObject::owned(1, "too big number".to_string(), None::<()>)),
			}
		}
	}
	let module = CoolServerImpl.into_rpc();

	// Call sync method with no params
	let res: bool = module.call("rebel_without_cause", EmptyServerParams::new()).await.unwrap();
	assert!(!res);

	// Call sync method with params
	let res: String = module.call("rebel", (Gun { shoots: true }, HashMap::<u8, u8>::default())).await.unwrap();
	assert_eq!(&res, "0 Gun { shoots: true }");

	// Call sync method with bad params
	let err = module.call::<_, EmptyServerParams>("rebel", (Gun { shoots: true }, false)).await.unwrap_err();

	assert!(matches!(err,
		MethodsError::JsonRpc(err) if err.data().unwrap().get().contains("invalid type: boolean `false`, expected a map at line 1 column 5") &&
		err.code() == ErrorCode::InvalidParams.code() && err.message() == INVALID_PARAMS_MSG
	));

	// Call async method with params and context
	let result: String = module.call("revolution", (Beverage { ice: true }, vec![1, 2, 3])).await.unwrap();
	assert_eq!(&result, "drink: Beverage { ice: true }, phases: [1, 2, 3]");

	// Call async method with option which is `Some`
	let result: Option<String> = module.call("can_have_options", vec![0]).await.unwrap();
	assert_eq!(result, Some("one".to_string()));

	// Call async method with option which is `None`
	let result: Option<String> = module.call("can_have_options", vec![1]).await.unwrap();
	assert_eq!(result, None);

	// Call async method with option which should `Err`.
	let err = module.call::<_, Option<String>>("can_have_options", vec![2]).await.unwrap_err();
	assert!(matches!(err,
		MethodsError::JsonRpc(err) if err.message() == "too big number"
	));
}

#[tokio::test]
async fn subscribing_without_server() {
	init_logger();

	let mut module = RpcModule::new(());
	module
		.register_subscription("my_sub", "my_sub", "my_unsub", |_, pending, _, _| async move {
			let mut stream_data = vec!['0', '1', '2'];

			let sink = pending.accept().await.unwrap();

			while let Some(letter) = stream_data.pop() {
				tracing::debug!("This is your friendly subscription sending data.");
				let msg = serde_json::value::to_raw_value(&letter).unwrap();
				sink.send(msg).await.unwrap();
				tokio::time::sleep(std::time::Duration::from_millis(500)).await;
			}

			Err("closed successfully".into())
		})
		.unwrap();

	let mut my_sub = module.subscribe_unbounded("my_sub", EmptyServerParams::new()).await.unwrap();

	for i in (0..=2).rev() {
		let (val, id) = my_sub.next::<char>().await.unwrap().unwrap();
		assert_eq!(val, std::char::from_digit(i, 10).unwrap());
		assert_eq!(&id, my_sub.subscription_id());
	}

	assert!(my_sub.next::<char>().await.is_none());
}

#[tokio::test]
async fn close_test_subscribing_without_server() {
	init_logger();

	let mut module = RpcModule::new(());
	module
		.register_subscription("my_sub", "my_sub", "my_unsub", |_, pending, _, _| async move {
			let sink = pending.accept().await?;
			let msg = serde_json::value::to_raw_value("lo")?;

			// make sure to only send one item
			sink.send(msg.clone()).await?;
			sink.closed().await;

			sink.send(msg).await.expect("The sink should be closed");

			Ok(())
		})
		.unwrap();

	let mut my_sub = module.subscribe_unbounded("my_sub", EmptyServerParams::new()).await.unwrap();
	let (val, id) = my_sub.next::<String>().await.unwrap().unwrap();
	assert_eq!(&val, "lo");
	assert_eq!(&id, my_sub.subscription_id());
	let mut my_sub2 =
		std::mem::ManuallyDrop::new(module.subscribe_unbounded("my_sub", EmptyServerParams::new()).await.unwrap());

	// Close the subscription to ensure it doesn't return any items.
	my_sub.close();

	// The first subscription was not closed using the unsubscribe method and
	// it will be treated as the connection was closed.
	assert!(my_sub.next::<String>().await.is_none());

	// The second subscription still works
	let (val, _) = my_sub2.next::<String>().await.unwrap().unwrap();
	assert_eq!(val, "lo".to_string());
	// Simulate a rude client that disconnects suddenly.
	unsafe {
		std::mem::ManuallyDrop::drop(&mut my_sub2);
	}

	assert!(my_sub.next::<String>().await.is_none());
}

#[tokio::test]
async fn subscribing_without_server_bad_params() {
	let mut module = RpcModule::new(());
	module
		.register_subscription("my_sub", "my_sub", "my_unsub", |params, pending, _, _| async move {
			let p = match params.one::<String>() {
				Ok(p) => p,
				Err(e) => {
					let _ = pending.reject(e).await;
					return Ok(());
				}
			};

			let sink = pending.accept().await?;
			let msg = serde_json::value::to_raw_value(&p)?;
			sink.send(msg).await?;

			Ok(())
		})
		.unwrap();

	let sub = module.subscribe_unbounded("my_sub", EmptyServerParams::new()).await.unwrap_err();

	assert!(
		matches!(sub, MethodsError::JsonRpc(e) if e.data().unwrap().get().contains("invalid length 0, expected an array of length 1 at line 1 column 2") && e.code() == ErrorCode::InvalidParams.code()
				&& e.message() == INVALID_PARAMS_MSG
		)
	);
}

#[tokio::test]
async fn subscribing_without_server_indicates_close() {
	let mut module = RpcModule::new(());
	module
		.register_subscription("my_sub", "my_sub", "my_unsub", |_, pending, _, _| async move {
			let sink = pending.accept().await?;

			for m in 0..5 {
				let msg = serde_json::value::to_raw_value(&m)?;
				sink.send(msg).await?;
			}

			Ok(())
		})
		.unwrap();

	let mut sub = module.subscribe_unbounded("my_sub", EmptyServerParams::new()).await.unwrap();

	for _ in 0..5 {
		assert!(sub.next::<usize>().await.is_some());
	}
	assert!(sub.next::<usize>().await.is_none());
}

#[tokio::test]
async fn subscribe_unsubscribe_without_server() {
	let mut module = RpcModule::new(());
	module
		.register_subscription("my_sub", "my_sub", "my_unsub", |_, pending, _, _| async move {
			let interval = interval(Duration::from_millis(200));
			let stream = IntervalStream::new(interval).map(move |_| 1);
			pipe_from_stream_and_drop(pending, stream).await.map_err(Into::into)
		})
		.unwrap();

	async fn subscribe_and_assert(module: &RpcModule<()>) {
		let sub = module.subscribe_unbounded("my_sub", EmptyServerParams::new()).await.unwrap();
		let ser_id = serde_json::to_string(sub.subscription_id()).unwrap();

		// Unsubscribe should be valid.
		let unsub_req = format!("{{\"jsonrpc\":\"2.0\",\"method\":\"my_unsub\",\"params\":[{}],\"id\":1}}", ser_id);
		let (resp, _) = module.raw_json_request(&unsub_req, 1).await.unwrap();

		assert_eq!(resp.get(), r#"{"jsonrpc":"2.0","id":1,"result":true}"#);

		// Unsubscribe already performed; should be error.
		let unsub_req = format!("{{\"jsonrpc\":\"2.0\",\"method\":\"my_unsub\",\"params\":[{}],\"id\":1}}", ser_id);
		let (resp, _) = module.raw_json_request(&unsub_req, 2).await.unwrap();

		assert_eq!(resp.get(), r#"{"jsonrpc":"2.0","id":1,"result":false}"#);
	}

	let sub1 = subscribe_and_assert(&module);
	let sub2 = subscribe_and_assert(&module);

	futures::future::join(sub1, sub2).await;
}

#[tokio::test]
async fn rejected_subscription_without_server() {
	let mut module = RpcModule::new(());
	module
		.register_subscription("my_sub", "my_sub", "my_unsub", |_, pending, _, _| async move {
			let err = ErrorObject::borrowed(PARSE_ERROR_CODE, "rejected", None);
			pending.reject(err.into_owned()).await;
			Ok(())
		})
		.unwrap();

	let sub_err = module.subscribe_unbounded("my_sub", EmptyServerParams::new()).await.unwrap_err();
	assert!(
		matches!(sub_err, MethodsError::JsonRpc(e) if e.message().contains("rejected") && e.code() == PARSE_ERROR_CODE)
	);
}

#[tokio::test]
async fn reject_works() {
	init_logger();

	let mut module = RpcModule::new(());
	module
		.register_subscription("my_sub", "my_sub", "my_unsub", |_, pending, _, _| async move {
			pending.reject(ErrorObject::owned(PARSE_ERROR_CODE, "rejected", None::<()>)).await;
			tokio::time::sleep(std::time::Duration::from_millis(100)).await;
			Err("do not send".into())
		})
		.unwrap();

	let (rp, mut stream) = module.raw_json_request(r#"{"jsonrpc":"2.0","method":"my_sub","id":0}"#, 1).await.unwrap();
	assert_eq!(rp.get(), r#"{"jsonrpc":"2.0","id":0,"error":{"code":-32700,"message":"rejected"}}"#);
	assert!(stream.recv().await.is_none());
}

#[tokio::test]
async fn bounded_subscription_works() {
	init_logger();

	let (tx, mut rx) = mpsc::unbounded_channel::<String>();
	let mut module = RpcModule::new(tx);

	module
		.register_subscription("my_sub", "my_sub", "my_unsub", |_, pending, mut ctx, _| async move {
			let mut sink = pending.accept().await?;

			let mut stream = IntervalStream::new(interval(std::time::Duration::from_millis(100)))
				.enumerate()
				.map(|(n, _)| n)
				.take(6);
			let fail = std::sync::Arc::make_mut(&mut ctx);
			let mut buf = VecDeque::new();

			while let Some(n) = stream.next().await {
				let msg = serde_json::value::to_raw_value(&n).expect("usize serialize infallible; qed");

				match sink.try_send(msg) {
					Err(TrySendError::Closed(_)) => panic!("This is a bug"),
					Err(TrySendError::Full(m)) => {
						buf.push_back(m);
					}
					Ok(_) => (),
				}
			}

			if !buf.is_empty() {
				fail.send("Full".to_string()).unwrap();
			}

			while let Some(m) = buf.pop_front() {
				match sink.try_send(m) {
					Err(TrySendError::Closed(_)) => panic!("This is a bug"),
					Err(TrySendError::Full(m)) => {
						buf.push_front(m);
					}
					Ok(_) => (),
				}

				tokio::time::sleep(std::time::Duration::from_millis(100)).await;
			}

			Ok(())
		})
		.unwrap();

	// create a bounded subscription and don't poll it
	// after 3 items has been produced messages will be dropped.
	let mut sub = module.subscribe("my_sub", EmptyServerParams::new(), 3).await.unwrap();

	// assert that some items couldn't be sent.
	assert_eq!(rx.recv().await, Some("Full".to_string()));

	// the subscription should continue produce items are consumed
	// and the failed messages should be able to go succeed.
	for exp in 0..6 {
		let (item, _) = sub.next::<usize>().await.unwrap().unwrap();
		assert_eq!(item, exp);
	}
}

#[tokio::test]
async fn serialize_sub_error_json() {
	#[derive(Serialize, Deserialize)]
	struct MyError {
		number: u32,
		address: String,
	}

	init_logger();

	let mut module = RpcModule::new(());
	module
		.register_subscription("my_sub", "my_sub", "my_unsub", |_, pending, _, _| async move {
			let _ = pending.accept().await?;
			tokio::time::sleep(std::time::Duration::from_millis(100)).await;
			let json =
				serde_json::value::to_raw_value(&MyError { number: 11, address: "State street 1337".into() }).unwrap();
			Err(SubscriptionError::from_json(json))
		})
		.unwrap();

	let (sub_id, notif) =
		run_subscription(r#"{"jsonrpc":"2.0","method":"my_sub","params":[true],"id":0}"#, &module).await;

	assert_eq!(
		format!(
			r#"{{"jsonrpc":"2.0","method":"my_sub","params":{{"subscription":{},"error":{{"number":11,"address":"State street 1337"}}}}}}"#,
			sub_id,
		),
		notif.get()
	);

	assert!(serde_json::from_str::<jsonrpsee::types::response::SubscriptionError<MyError>>(notif.get()).is_ok());
}

#[tokio::test]
async fn serialize_sub_error_str() {
	#[derive(Serialize, Deserialize)]
	struct MyError {
		number: u32,
		address: String,
	}

	init_logger();

	let mut module = RpcModule::new(());
	module
		.register_subscription("my_sub", "my_sub", "my_unsub", |_, pending, _, _| async move {
			let _ = pending.accept().await?;
			tokio::time::sleep(std::time::Duration::from_millis(100)).await;
			let s = serde_json::to_string(&MyError { number: 11, address: "State street 1337".into() }).unwrap();
			Err(s.into())
		})
		.unwrap();

	let (sub_id, notif) = run_subscription(r#"{"jsonrpc":"2.0","method":"my_sub","id":0}"#, &module).await;

	assert_eq!(
		format!(
			r#"{{"jsonrpc":"2.0","method":"my_sub","params":{{"subscription":{},"error":"{{\"number\":11,\"address\":\"State street 1337\"}}"}}}}"#,
			sub_id,
		),
		notif.get()
	);

	assert!(serde_json::from_str::<jsonrpsee::types::response::SubscriptionError<MyError>>(notif.get()).is_err());
}

#[tokio::test]
async fn subscription_close_response_works() {
	use jsonrpsee::SubscriptionCloseResponse;

	init_logger();

	let mut module = RpcModule::new(());

	module
		.register_subscription("my_sub", "my_sub", "my_unsub", |params, pending, _, _| async move {
			let x = match params.one::<usize>() {
				Ok(op) => op,
				Err(e) => {
					pending.reject(e).await;
					return SubscriptionCloseResponse::None;
				}
			};

			let _sink = pending.accept().await.unwrap();
			let msg = serde_json::value::to_raw_value(&x).unwrap();

			SubscriptionCloseResponse::Notif(msg.into())
		})
		.unwrap();

	// ensure subscription with raw_json_request works.
	{
		let (sub_id, notif) =
			run_subscription(r#"{"jsonrpc":"2.0","method":"my_sub","params":[1],"id":0}"#, &module).await;

		assert_eq!(
			format!(r#"{{"jsonrpc":"2.0","method":"my_sub","params":{{"subscription":{},"result":1}}}}"#, sub_id),
			notif.get()
		);
	}

	// ensure subscribe API works.
	{
		let mut sub = module.subscribe_unbounded("my_sub", [1]).await.unwrap();
		let (rx, _id) = sub.next::<usize>().await.unwrap().unwrap();
		assert_eq!(rx, 1);
	}
}

#[tokio::test]
async fn method_response_notify_on_completion() {
	use jsonrpsee::server::ResponsePayload;

	init_logger();

	// The outcome of the response future is sent out on this channel
	// to test whether the call produced a valid response or not.
	let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

	let module = {
		let mut module = RpcModule::new(tx);

		module
			.register_method("hey", |params, ctx, _| {
				let kind: String = params.one().unwrap();
				let server_sender = ctx.clone();

				let (rp, rp_future) = if kind == "success" {
					ResponsePayload::success("lo").notify_on_completion()
				} else {
					ResponsePayload::error(ErrorCode::InvalidParams).notify_on_completion()
				};

				tokio::spawn(async move {
					let rp = rp_future.await;
					server_sender.send(rp).unwrap();
				});

				rp
			})
			.unwrap();

		module
	};

	// Successful call should return a successful notification.
	assert_eq!(module.call::<_, String>("hey", ["success"]).await.unwrap(), "lo");
	assert!(matches!(rx.recv().await, Some(Ok(_))));

	// Low level call should also work.
	let (rp, _) =
		module.raw_json_request(r#"{"jsonrpc":"2.0","method":"hey","params":["success"],"id":0}"#, 1).await.unwrap();
	assert_eq!(rp.get(), r#"{"jsonrpc":"2.0","id":0,"result":"lo"}"#);
	assert!(matches!(rx.recv().await, Some(Ok(_))));

	// Error call should return a failed notification.
	assert!(module.call::<_, String>("hey", ["not success"]).await.is_err());
	assert!(matches!(rx.recv().await, Some(Err(MethodResponseError::JsonRpcError))));
}

#[tokio::test]
async fn conn_id_in_rpc_module_without_server() {
	let mut module = RpcModule::new(());
	module
		.register_method("get_conn_id", |_, _, ext| match ext.get::<ConnectionId>() {
			Some(id) => Ok(id.0),
			None => Err(ErrorObject::owned(1, "no connection id".to_string(), None::<()>)),
		})
		.unwrap();

	assert!(
		matches!(module.call::<_, usize>("get_conn_id", EmptyServerParams::new()).await, Ok(conn_id) if conn_id == 0)
	);
}

async fn run_subscription(req: &str, rpc: &RpcModule<()>) -> (u64, Box<RawValue>) {
	let (rp, mut stream) = rpc.raw_json_request(req, 1).await.unwrap();
	let resp = serde_json::from_str::<Response<u64>>(rp.get()).unwrap();
	let sub_resp = stream.recv().await.unwrap();

	let sub_id = match resp.payload {
		ResponsePayload::Success(val) => val,
		_ => panic!("Expected valid response"),
	};

	(*sub_id, sub_resp)
}
