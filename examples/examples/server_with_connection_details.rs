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

use std::net::SocketAddr;

use jsonrpsee::core::middleware::{Batch, Notification, Request, RpcServiceT};
use jsonrpsee::core::{SubscriptionResult, async_trait};
use jsonrpsee::proc_macros::rpc;
use jsonrpsee::server::PendingSubscriptionSink;
use jsonrpsee::types::{ErrorObject, ErrorObjectOwned};
use jsonrpsee::ws_client::WsClientBuilder;
use jsonrpsee::{ConnectionId, Extensions};

#[rpc(server, client)]
pub trait Rpc {
	/// method with connection ID.
	#[method(name = "connectionIdMethod", with_extensions)]
	async fn method(&self) -> Result<usize, ErrorObjectOwned>;

	#[subscription(name = "subscribeConnectionId", item = usize, with_extensions)]
	async fn sub(&self) -> SubscriptionResult;

	#[subscription(name = "subscribeSyncConnectionId", item = usize, with_extensions)]
	fn sub2(&self) -> SubscriptionResult;
}

struct LoggingMiddleware<S>(S);

impl<S> RpcServiceT for LoggingMiddleware<S>
where
	S: RpcServiceT,
{
	type MethodResponse = S::MethodResponse;
	type NotificationResponse = S::NotificationResponse;
	type BatchResponse = S::BatchResponse;

	fn call<'a>(&self, request: Request<'a>) -> impl Future<Output = Self::MethodResponse> + Send + 'a {
		tracing::info!("Received request: {:?}", request);
		assert!(request.extensions().get::<ConnectionId>().is_some());

		self.0.call(request)
	}

	fn batch<'a>(&self, batch: Batch<'a>) -> impl Future<Output = Self::BatchResponse> + Send + 'a {
		tracing::info!("Received batch: {:?}", batch);
		self.0.batch(batch)
	}

	fn notification<'a>(&self, n: Notification<'a>) -> impl Future<Output = Self::NotificationResponse> + Send + 'a {
		tracing::info!("Received notif: {:?}", n);
		self.0.notification(n)
	}
}

pub struct RpcServerImpl;

#[async_trait]
impl RpcServer for RpcServerImpl {
	async fn method(&self, ext: &Extensions) -> Result<usize, ErrorObjectOwned> {
		let conn_id = ext
			.get::<ConnectionId>()
			.cloned()
			.ok_or_else(|| ErrorObject::owned(0, "No connection details found", None::<()>))?;

		Ok(conn_id.0)
	}

	async fn sub(&self, pending: PendingSubscriptionSink, ext: &Extensions) -> SubscriptionResult {
		let sink = pending.accept().await?;
		let conn_id = ext
			.get::<ConnectionId>()
			.cloned()
			.ok_or_else(|| ErrorObject::owned(0, "No connection details found", None::<()>))?;
		let json = serde_json::value::to_raw_value(&conn_id)
			.map_err(|e| ErrorObject::owned(0, format!("Failed to serialize connection ID: {e}"), None::<()>))?;
		sink.send(json).await?;
		Ok(())
	}

	fn sub2(&self, pending: PendingSubscriptionSink, ext: &Extensions) -> SubscriptionResult {
		let conn_id = ext.get::<ConnectionId>().cloned().unwrap();

		tokio::spawn(async move {
			let sink = pending.accept().await.unwrap();
			let json = serde_json::value::to_raw_value(&conn_id).unwrap();
			sink.send(json).await.unwrap();
		});
		Ok(())
	}
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	tracing_subscriber::FmtSubscriber::builder()
		.with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
		.try_init()
		.expect("setting default subscriber failed");

	let server_addr = run_server().await?;
	let url = format!("ws://{}", server_addr);

	let client = WsClientBuilder::default().build(&url).await?;
	let connection_id_first = client.method().await.unwrap();

	// Second call from the same connection ID.
	assert_eq!(client.method().await.unwrap(), connection_id_first);

	// Second client will increment the connection ID.
	let client2 = WsClientBuilder::default().build(&url).await?;
	let connection_id_second = client2.method().await.unwrap();
	assert_ne!(connection_id_first, connection_id_second);

	let mut sub = client.sub().await.unwrap();
	assert_eq!(connection_id_first, sub.next().await.transpose().unwrap().unwrap());

	let mut sub = client2.sub().await.unwrap();
	assert_eq!(connection_id_second, sub.next().await.transpose().unwrap().unwrap());

	Ok(())
}

async fn run_server() -> anyhow::Result<SocketAddr> {
	let rpc_middleware = jsonrpsee::server::middleware::rpc::RpcServiceBuilder::new().layer_fn(LoggingMiddleware);

	let server = jsonrpsee::server::Server::builder().set_rpc_middleware(rpc_middleware).build("127.0.0.1:0").await?;
	let addr = server.local_addr()?;

	let handle = server.start(RpcServerImpl.into_rpc());

	tokio::spawn(handle.stopped());

	Ok(addr)
}
