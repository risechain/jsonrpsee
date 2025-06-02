use std::net::SocketAddr;
use std::time::{Duration, Instant};

use clap::Parser;
use jsonrpsee::core::client::ClientT;
use jsonrpsee::http_client::{CertificateVerificationMode, HttpClientBuilder};
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
		Some(url) => {
			// Ensure URL has http3:// scheme
			if !url.starts_with("http3://") { format!("http3://{}", url) } else { url }
		}
		None => {
			let addr = run_server().await?;
			format!("http3://{}", addr)
		}
	};

	tracing::info!("Using server URL: {}", server_addr);
	tracing::info!("Payload size: {} elements", args.payload_size);

	let client = HttpClientBuilder::default()
		.enable_http3()
		.with_http3_certificate_verification(CertificateVerificationMode::AcceptSelfSigned)
		.build(&server_addr)
		.await?;

	let payload = vec![1_u64; args.payload_size];
	let params = rpc_params![payload];

	let start = Instant::now();
	let response: Result<String, _> = client.request("say_hello", params.clone()).await;
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

	// Add a small delay
	tokio::time::sleep(Duration::from_millis(500)).await;

	// Send the 2nd request
	println!("\nMaking a second request to verify server is still running...");
	let start2 = Instant::now();
	let response2: Result<String, _> = client.request("say_hello 222", params.clone()).await;
	let elapsed2 = start2.elapsed();

	match response2 {
		Ok(result) => {
			println!("Second response: {}", result);
			println!("Time: {:?}", elapsed2);
			println!("✅ Server is confirmed to be running in the background!");
		}
		Err(e) => {
			eprintln!("Error on second request: {:?}", e);
			eprintln!("❌ Server might not be running anymore.");
		}
	}

	Ok(())
}

async fn run_server() -> anyhow::Result<SocketAddr> {
	let http3_config = jsonrpsee::server::http3::Http3Config {
		max_connections: 1000,
		max_concurrent_requests_per_connection: 100,
		max_idle_timeout: Duration::from_secs(30),
		keep_alive_interval: Some(Duration::from_secs(5)),
		enable_0rtt: true,
		max_bidi_streams: 100,
		max_uni_streams: 100,
		initial_congestion_window: 14720,
		enable_bbr: true,
		cert_config: jsonrpsee::server::http3::CertificateConfig::SelfSigned { dns_name: "localhost".to_string() },
	};

	let config = ServerConfig::builder().enable_http3().with_http3_config(http3_config).build();
	let server = ServerBuilder::default().set_config(config).build("127.0.0.1:34727".parse::<SocketAddr>()?).await?;

	let mut module = RpcModule::new(());

	module.register_method("say_hello", |params, _, _| -> Result<String, ErrorObjectOwned> {
		let params: Vec<Vec<u64>> = params.parse()?;
		let total_elements: usize = params.iter().map(|v| v.len()).sum();
		Ok(format!("Received payload with {} elements", total_elements))
	})?;

	module.register_method("status", |_, _, _| -> Result<String, ErrorObjectOwned> {
		Ok("Server is running".to_string())
	})?;

	let addr = server.local_addr()?;
	tracing::info!("Server listening on {}", addr);

	let handle = server.start(module);
	tokio::spawn(async move {
		handle.stopped().await;
		println!("Server has stopped running");
	});

	Ok(addr)
}
