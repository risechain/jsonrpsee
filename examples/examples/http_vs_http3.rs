use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use jsonrpsee::core::client::ClientT;
use jsonrpsee::http_client::HttpClientBuilder;
use jsonrpsee::rpc_params;
use jsonrpsee::server::{RpcModule, ServerBuilder};
use jsonrpsee::types::ErrorObjectOwned;

#[cfg(feature = "http3")]
use jsonrpsee::http_client::CertificateVerificationMode;
#[cfg(feature = "http3")]
use jsonrpsee::server::ServerConfig;

#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Args {
	#[clap(short = 't', long, default_value = "http")]
	protocol: String,

	#[clap(short = 'p', long, default_value = "27")]
	payload_size: usize,

	#[clap(short, long, default_value = "100")]
	requests: usize,

	#[clap(short, long, default_value = "10")]
	connections: usize,

	#[clap(short, long)]
	url: Option<String>,

	#[clap(short, long)]
	server: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	let args = Args::parse();

	if std::env::var("RUST_LOG").is_err() {
		unsafe {
			std::env::set_var("RUST_LOG", "info");
		}
	}
	tracing_subscriber::fmt::init();

	if args.server {
		let server_addr = run_server(args.protocol == "http3").await?;
		tracing::info!("Server listening on {}", server_addr);

		tokio::signal::ctrl_c().await?;
		return Ok(());
	}

	let server_url = match args.url {
		Some(url) => url,
		None => {
			let server_addr = run_server(args.protocol == "http3").await?;
			match args.protocol.as_str() {
				"http" => format!("http://{}", server_addr),
				"http3" => format!("http3://{}", server_addr),
				_ => return Err(anyhow::anyhow!("Invalid protocol: {}", args.protocol)),
			}
		}
	};

	if server_url.contains("127.0.0.1:0") {
		return Err(anyhow::anyhow!("Invalid server address: {}", server_url));
	}

	tracing::info!("Using server URL: {}", server_url);
	tracing::info!("Protocol: {}", args.protocol);
	tracing::info!("Payload size: {} elements", args.payload_size);
	tracing::info!("Requests: {}", args.requests);
	tracing::info!("Connections: {}", args.connections);

	let mut clients = Vec::with_capacity(args.connections);
	for _ in 0..args.connections {
		let client = if args.protocol == "http3" {
			#[cfg(feature = "http3")]
			{
				tracing::info!("Creating HTTP/3 client with URL: {}", server_url);
				let builder = HttpClientBuilder::default();
				let client = builder
					.enable_http3()
					.with_http3_certificate_verification(CertificateVerificationMode::AcceptSelfSigned)
					.build(&server_url)
					.await?;
				tracing::info!("HTTP/3 client created successfully");
				client
			}
			#[cfg(not(feature = "http3"))]
			{
				return Err(anyhow::anyhow!("HTTP/3 support not enabled. Compile with --features http3"));
			}
		} else {
			tracing::info!("Creating HTTP client with URL: {}", server_url);
			let client = HttpClientBuilder::default().build(&server_url).await?;
			tracing::info!("HTTP client created successfully");
			client
		};
		clients.push(Arc::new(client));
	}

	let payload = vec![1_u64; args.payload_size];

	let start = Instant::now();
	let mut handles = Vec::with_capacity(args.requests);

	for i in 0..args.requests {
		let client = clients[i % args.connections].clone();
		let payload = payload.clone();

		let handle = tokio::spawn(async move {
			let params = rpc_params![payload];
			let start = Instant::now();
			let response: Result<String, _> = client.request("benchmark", params).await;
			let elapsed = start.elapsed();

			match response {
				Ok(_) => (true, elapsed),
				Err(e) => {
					tracing::error!("Request failed: {:?}", e);
					(false, elapsed)
				}
			}
		});

		handles.push(handle);
	}

	let mut results = Vec::with_capacity(args.requests);
	for handle in handles {
		results.push(handle.await?);
	}

	let total_elapsed = start.elapsed();
	let successful = results.iter().filter(|(success, _)| *success).count();
	let total_requests = results.len();

	let mut latencies: Vec<Duration> = results.into_iter().map(|(_, elapsed)| elapsed).collect();
	latencies.sort();

	let avg_latency = if !latencies.is_empty() {
		latencies.iter().sum::<Duration>() / latencies.len() as u32
	} else {
		Duration::ZERO
	};

	let min_latency = latencies.first().unwrap_or(&Duration::ZERO);
	let max_latency = latencies.last().unwrap_or(&Duration::ZERO);
	let median_latency = if !latencies.is_empty() { latencies[latencies.len() / 2] } else { Duration::ZERO };

	let p95_latency =
		if !latencies.is_empty() { latencies[(latencies.len() as f64 * 0.95) as usize] } else { Duration::ZERO };

	let p99_latency =
		if !latencies.is_empty() { latencies[(latencies.len() as f64 * 0.99) as usize] } else { Duration::ZERO };

	let requests_per_second =
		if total_elapsed.as_secs_f64() > 0.0 { total_requests as f64 / total_elapsed.as_secs_f64() } else { 0.0 };

	println!("\n=== Benchmark Results ===");
	println!("Protocol:           {}", args.protocol);
	println!("Payload size:       {} elements", args.payload_size);
	println!("Total requests:     {}", total_requests);
	println!(
		"Successful:         {} ({}%)",
		successful,
		if total_requests > 0 { (successful as f64 / total_requests as f64) * 100.0 } else { 0.0 }
	);
	println!("Total time:         {:.2?}", total_elapsed);
	println!("Requests/second:    {:.2}", requests_per_second);
	println!("Average latency:    {:.2?}", avg_latency);
	println!("Minimum latency:    {:.2?}", min_latency);
	println!("Maximum latency:    {:.2?}", max_latency);
	println!("Median latency:     {:.2?}", median_latency);
	println!("95th percentile:    {:.2?}", p95_latency);
	println!("99th percentile:    {:.2?}", p99_latency);

	Ok(())
}

async fn run_server(use_http3: bool) -> anyhow::Result<SocketAddr> {
	let port = match std::env::var("SERVER_PORT") {
		Ok(port) => port.parse::<u16>()?,
		Err(_) => 0, // Use random port if not specified
	};

	let addr = format!("127.0.0.1:{}", port).parse::<SocketAddr>()?;

	let server = if use_http3 {
		#[cfg(feature = "http3")]
		{
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
				cert_config: jsonrpsee::server::http3::CertificateConfig::SelfSigned {
					dns_name: "localhost".to_string(),
				},
			};

			let config = ServerConfig::builder().enable_http3().with_http3_config(http3_config).build();

			ServerBuilder::default().set_config(config).build(addr).await?
		}
		#[cfg(not(feature = "http3"))]
		{
			return Err(anyhow::anyhow!("HTTP/3 support not enabled. Compile with --features http3"));
		}
	} else {
		ServerBuilder::default().build(addr).await?
	};
	let mut module = RpcModule::new(());

	module.register_method("benchmark", |params, _, _| -> Result<String, ErrorObjectOwned> {
		let params: Vec<Vec<u64>> = params.parse()?;
		let total_elements: usize = params.iter().map(|v| v.len()).sum();
		Ok(format!("Processed payload with {} elements", total_elements))
	})?;

	let addr = server.local_addr()?;
	tracing::info!("Server listening on {}", addr);

	let handle = server.start(module);
	tokio::spawn(handle.stopped());

	Ok(addr)
}
