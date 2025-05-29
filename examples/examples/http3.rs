use std::net::SocketAddr;
use std::time::Instant;

use clap::Parser;
use jsonrpsee::core::client::ClientT;
use jsonrpsee::http_client::HttpClientBuilder;
use jsonrpsee::rpc_params;
use jsonrpsee::server::{RpcModule, ServerBuilder, ServerConfig};
use jsonrpsee::types::ErrorObjectOwned;
use tracing_subscriber::util::SubscriberInitExt;

#[derive(Parser, Debug)]
#[clap(author, version, about = "HTTP/3 benchmark example for jsonrpsee")]
struct Args {
	#[clap(short = 'p', long, default_value = "27")]
	payload_size: usize,

	#[clap(short, long)]
	url: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	let args = Args::parse();

	let filter = tracing_subscriber::EnvFilter::try_from_default_env()?
		.add_directive("jsonrpsee[method_call{name = \"say_hello\"}]=trace".parse()?);
	tracing_subscriber::FmtSubscriber::builder().with_env_filter(filter).finish().try_init()?;

	let server_addr = match args.url {
		Some(url) => url,
		None => {
			let addr = run_server().await?;
			format!("http3://{}", addr)
		}
	};

	tracing::info!("Using server URL: {}", server_addr);
	tracing::info!("Payload size: {} elements", args.payload_size);

	let client = HttpClientBuilder::default().enable_http3().build(&server_addr).await?;

	let payload = vec![1_u64; args.payload_size];
	let params = rpc_params![payload];

	let start = Instant::now();
	let response: Result<String, _> = client.request("say_hello", params).await;
	let elapsed = start.elapsed();

	match response {
		Ok(result) => {
			println!("Response: {}", result);
			println!("Time: {:?}", elapsed);
		}
		Err(e) => {
			eprintln!("Error: {:?}", e);
		}
	}

	Ok(())
}

async fn run_server() -> anyhow::Result<SocketAddr> {
	let config = ServerConfig::builder().enable_http3().build();
	let server = ServerBuilder::default().set_config(config).build("127.0.0.1:34727".parse::<SocketAddr>()?).await?;

	let mut module = RpcModule::new(());

	module.register_method("say_hello", |params, _, _| -> Result<String, ErrorObjectOwned> {
		let params: Vec<Vec<u64>> = params.parse()?;
		let total_elements: usize = params.iter().map(|v| v.len()).sum();
		Ok(format!("Received payload with {} elements", total_elements))
	})?;

	let addr = server.local_addr()?;
	tracing::info!("Server listening on {}", addr);

	let handle = server.start(module);
	tokio::spawn(handle.stopped());

	Ok(addr)
}
