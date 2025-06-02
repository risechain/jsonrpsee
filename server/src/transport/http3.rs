use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::{Buf, Bytes};
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use h3::server::{Connection, RequestStream};
use h3_quinn::{Connection as H3Connection, quinn};
use http::{Request, Response, StatusCode};
use jsonrpsee_core::BoxError;
use jsonrpsee_core::server::Methods;
use quinn::crypto::rustls::QuicServerConfig;
use quinn::{Endpoint, ServerConfig as QuinnServerConfig, TransportConfig, VarInt};
use rustls::ServerConfig as RustlsServerConfig;
use tokio::sync::Semaphore;
use tower::Service;
use tracing::{debug, error, info, trace};

use crate::{
	ConnectionGuard, ConnectionState, HttpBody, HttpRequest, HttpResponse, LOG_TARGET, ServerConfig, StopHandle,
};

/// HTTP/3 server configuration
#[derive(Debug, Clone)]
pub struct Http3Config {
	/// Maximum number of concurrent connections
	pub max_connections: usize,
	/// Maximum number of concurrent requests per connection
	pub max_concurrent_requests_per_connection: usize,
	/// Maximum idle timeout for connections
	pub max_idle_timeout: Duration,
	/// Keep-alive interval
	pub keep_alive_interval: Option<Duration>,
	/// Enable 0-RTT
	pub enable_0rtt: bool,
	/// Maximum number of bidirectional streams
	pub max_bidi_streams: u32,
	/// Maximum number of unidirectional streams
	pub max_uni_streams: u32,
	/// Initial congestion window
	pub initial_congestion_window: u32,
	/// Enable BBR congestion control
	pub enable_bbr: bool,
	/// Certificate configuration
	pub cert_config: CertificateConfig,
}

/// Certificate configuration for HTTP/3
#[derive(Debug, Clone)]
pub enum CertificateConfig {
	/// Use a self-signed certificate (for testing)
	SelfSigned {
		/// DNS name for the certificate
		dns_name: String,
	},
	/// Use provided certificate and key
	Custom {
		/// Certificate chain in PEM format
		cert_chain: Vec<u8>,
		/// Private key in PEM format
		private_key: Vec<u8>,
	},
}

impl Default for Http3Config {
	fn default() -> Self {
		Self {
			max_connections: 1000,
			max_concurrent_requests_per_connection: 100,
			max_idle_timeout: Duration::from_secs(30),
			keep_alive_interval: Some(Duration::from_secs(5)),
			enable_0rtt: true,
			max_bidi_streams: 100,
			max_uni_streams: 100,
			initial_congestion_window: 14720, // ~10 packets
			enable_bbr: true,
			cert_config: CertificateConfig::SelfSigned { dns_name: "localhost".to_string() },
		}
	}
}

#[derive(Debug)]
/// HTTP/3 server handle
pub struct Http3Server {
	endpoint: Endpoint,
	local_addr: SocketAddr,
}

impl Http3Server {
	/// Create a new HTTP/3 server
	pub async fn new(addr: SocketAddr, config: Http3Config) -> Result<Self, BoxError> {
		// Create TLS configuration
		let tls_config = create_tls_config(&config.cert_config)?;

		// Create QUIC transport configuration
		let mut transport_config = TransportConfig::default();
		transport_config.max_concurrent_bidi_streams(VarInt::from_u32(config.max_bidi_streams));
		transport_config.max_concurrent_uni_streams(VarInt::from_u32(config.max_uni_streams));
		transport_config.max_idle_timeout(Some(config.max_idle_timeout.try_into().unwrap()));

		if let Some(interval) = config.keep_alive_interval {
			transport_config.keep_alive_interval(Some(interval));
		}

		if config.enable_bbr {
			transport_config.congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default()));
		}

		// Create server configuration
		let quic_config = QuicServerConfig::try_from(tls_config)?;
		let mut server_config = QuinnServerConfig::with_crypto(Arc::new(quic_config));
		server_config.transport_config(Arc::new(transport_config));

		if config.enable_0rtt {
			server_config.retry_token_lifetime(Duration::from_secs(3600));
		}

		// Create endpoint
		let endpoint = Endpoint::server(server_config, addr)?;
		let local_addr = endpoint.local_addr()?;

		info!(target: LOG_TARGET, "HTTP/3 server listening on {}", local_addr);

		Ok(Self { endpoint, local_addr })
	}

	/// Get the local address the server is bound to
	pub fn local_addr(&self) -> SocketAddr {
		self.local_addr
	}

	/// Start accepting connections
	pub async fn start<S, Svc>(
		self,
		service_builder: S,
		methods: Methods,
		server_cfg: ServerConfig,
		conn_guard: ConnectionGuard,
		stop_handle: StopHandle,
	) where
		S: Fn(ConnectionState) -> Svc + Send + Sync + 'static + Clone,
		Svc: Service<HttpRequest, Response = HttpResponse, Error = BoxError> + Send + 'static + Clone,
		Svc::Future: Send + 'static,
	{
		let max_concurrent_requests = Arc::new(Semaphore::new(server_cfg.max_connections as usize));

		tokio::spawn(async move {
			accept_loop(
				self.endpoint,
				service_builder,
				methods,
				server_cfg,
				conn_guard,
				stop_handle,
				max_concurrent_requests,
			)
			.await;
		});
	}
}

/// Main accept loop for HTTP/3 connections
async fn accept_loop<S, Svc>(
	endpoint: Endpoint,
	service_builder: S,
	methods: Methods,
	server_cfg: ServerConfig,
	conn_guard: ConnectionGuard,
	stop_handle: StopHandle,
	max_concurrent_requests: Arc<Semaphore>,
) where
	S: Fn(ConnectionState) -> Svc + Send + Sync + 'static + Clone,
	Svc: Service<HttpRequest, Response = HttpResponse, Error = BoxError> + Send + 'static + Clone,
	Svc::Future: Send + 'static,
{
	let stopped = stop_handle.clone().shutdown();
	tokio::pin!(stopped);

	loop {
		let incoming = tokio::select! {
			Some(incoming) = endpoint.accept() => incoming,
			_ = &mut stopped => break,
		};

		let remote_addr = incoming.remote_address();
		debug!(target: LOG_TARGET, "New HTTP/3 connection from {}", remote_addr);

		let conn_permit = match conn_guard.try_acquire() {
			Some(permit) => permit,
			None => {
				debug!(target: LOG_TARGET, "Max connections reached, rejecting connection from {}", remote_addr);
				continue;
			}
		};

		let conn_id = rand::random::<u32>();
		let conn_state = ConnectionState::new(stop_handle.clone(), conn_id, conn_permit);
		let service = service_builder(conn_state.clone());

		let methods = methods.clone();
		let server_cfg = server_cfg.clone();
		let max_concurrent_requests = max_concurrent_requests.clone();
		let stop_handle = stop_handle.clone();

		tokio::spawn(async move {
			// Accept the connection
			let connection = match incoming.await {
				Ok(conn) => conn,
				Err(e) => {
					debug!(target: LOG_TARGET, "Failed to accept HTTP/3 connection: {}", e);
					return;
				}
			};

			if let Err(e) = handle_connection(
				connection,
				service,
				methods,
				server_cfg,
				conn_state,
				max_concurrent_requests,
				stop_handle,
			)
			.await
			{
				debug!(target: LOG_TARGET, "HTTP/3 connection error: {}", e);
			}
		});
	}

	info!(target: LOG_TARGET, "HTTP/3 server stopped");
}

/// Handle a single HTTP/3 connection
async fn handle_connection<Svc>(
	connection: quinn::Connection,
	service: Svc,
	methods: Methods,
	server_cfg: ServerConfig,
	conn_state: ConnectionState,
	max_concurrent_requests: Arc<Semaphore>,
	stop_handle: StopHandle,
) -> Result<(), BoxError>
where
	Svc: Service<HttpRequest, Response = HttpResponse, Error = BoxError> + Send + 'static + Clone,
	Svc::Future: Send + 'static,
{
	let h3_conn = H3Connection::new(connection);
	let mut h3: Connection<_, Bytes> = h3::server::Connection::new(h3_conn).await?;

	// Use a local semaphore for this connection to limit concurrent streams
	let connection_semaphore = Arc::new(Semaphore::new(server_cfg.max_request_body_size.min(100) as usize));

	// Process multiple requests concurrently but with controlled concurrency
	let mut futures = FuturesUnordered::new();

	loop {
		tokio::select! {
			result = h3.accept() => {
				match result {
					Ok(Some(req_stream)) => {
						// Try to acquire a permit from both semaphores
						let conn_permit = match connection_semaphore.clone().try_acquire_owned() {
							Ok(p) => p,
							Err(_) => {
								// Too many requests on this connection, reject
								continue;
							}
						};

						let global_permit = match max_concurrent_requests.clone().try_acquire_owned() {
							Ok(p) => p,
							Err(_) => {
								// Too many global requests, reject
								continue;
							}
						};

						let service = service.clone();
						let methods = methods.clone();
						let server_cfg = server_cfg.clone();
						let conn_state = conn_state.clone();

						// Add to our set of futures to process
						futures.push(async move {
							let _conn_permit = conn_permit;
							let _global_permit = global_permit;

							if let Ok((request, stream)) = req_stream.resolve_request().await {
								if let Err(e) = handle_request(
									request, stream, service, methods, server_cfg, conn_state
								).await {
									error!("Error handling HTTP/3 request: {}", e);
								}
							}
						});
					}
					Ok(None) => break,
					Err(e) => {
						debug!("Error accepting HTTP/3 request: {}", e);
						break;
					}
				}
			}
			Some(_) = futures.next(), if !futures.is_empty() => {
				// A request has completed, continue
			}
			_ = stop_handle.clone().shutdown() => break,
		}
	}

	// Wait for remaining requests to complete
	while let Some(_) = futures.next().await {}

	Ok(())
}
/// Handle a single HTTP/3 request
async fn handle_request<Svc, S>(
	request: Request<()>,
	mut stream: RequestStream<S, Bytes>,
	mut service: Svc,
	methods: Methods,
	server_cfg: ServerConfig,
	conn_state: ConnectionState,
) -> Result<(), BoxError>
where
	Svc: Service<HttpRequest, Response = HttpResponse, Error = BoxError>,
	S: h3::quic::SendStream<Bytes> + h3::quic::RecvStream,
	// B: bytes::Buf,
{
	trace!(target: LOG_TARGET, "Handling HTTP/3 request: {:?}", request);

	// Read request body
	let body = read_body_with_limit(&mut stream, server_cfg.max_request_body_size).await?;

	// Create HTTP request
	let (parts, _) = request.into_parts();
	let mut http_request = HttpRequest::from_parts(parts, HttpBody::from(body));

	// Add connection info to extensions
	http_request.extensions_mut().insert(conn_state.clone());
	http_request.extensions_mut().insert(methods);

	// Process request through service
	let response = match service.call(http_request).await {
		Ok(resp) => resp,
		Err(e) => {
			error!(target: LOG_TARGET, "Service error: {}", e);
			create_error_response(StatusCode::INTERNAL_SERVER_ERROR)
		}
	};

	// Send response
	send_response(stream, response).await
}

/// Read request body with size limit
async fn read_body_with_limit<S>(stream: &mut RequestStream<S, Bytes>, max_size: u32) -> Result<Vec<u8>, BoxError>
where
	S: h3::quic::RecvStream,
	// B: bytes::Buf,
{
	let mut body = Vec::new();
	let max_size = max_size as usize;

	while let Some(chunk) = stream.recv_data().await? {
		if body.len() + chunk.remaining() > max_size {
			return Err("Request body too large".into());
		}
		body.extend_from_slice(chunk.chunk());
	}

	Ok(body)
}

/// Send HTTP/3 response
async fn send_response<S>(mut stream: RequestStream<S, Bytes>, response: HttpResponse) -> Result<(), BoxError>
where
	S: h3::quic::SendStream<Bytes>,
	// B: bytes::Buf,
{
	let (parts, body) = response.into_parts();

	// Build H3 response
	let response = Response::builder().status(parts.status).version(http::Version::HTTP_3);

	let mut response = parts.headers.iter().fold(response, |resp, (name, value)| resp.header(name, value));

	// Collect body
	let body_bytes = match http_body_util::BodyExt::collect(body).await {
		Ok(collected) => collected.to_bytes(),
		Err(e) => {
			error!(target: LOG_TARGET, "Failed to collect response body: {}", e);
			Bytes::new()
		}
	};

	// Add content-length header
	response = response.header("content-length", body_bytes.len().to_string());

	let response = response.body(())?;

	// Send response
	stream.send_response(response).await?;

	if !body_bytes.is_empty() {
		stream.send_data(body_bytes).await?;
	}

	stream.finish().await?;

	Ok(())
}

/// Send error response
async fn send_error_response<S>(stream: RequestStream<S, Bytes>, status: StatusCode)
where
	S: h3::quic::SendStream<Bytes>,
	// B: bytes::Buf,
{
	let response = create_error_response(status);
	let _ = send_response(stream, response).await;
}

/// Create error response
fn create_error_response(status: StatusCode) -> HttpResponse {
	Response::builder()
		.status(status)
		.header("content-type", "text/plain")
		.body(HttpBody::from(status.to_string()))
		.unwrap()
}

/// Create TLS configuration
fn create_tls_config(cert_config: &CertificateConfig) -> Result<RustlsServerConfig, BoxError> {
	match cert_config {
		CertificateConfig::SelfSigned { dns_name } => {
			// Generate self-signed certificate
			let cert = rcgen::generate_simple_self_signed(vec![dns_name.clone()])?;

			let cert_der = cert.cert.der().to_vec();
			let key_der = cert.key_pair.serialize_der();

			// Create rustls certificate and key
			let cert = rustls::pki_types::CertificateDer::from(cert_der);
			let key = rustls::pki_types::PrivateKeyDer::from(rustls::pki_types::PrivatePkcs8KeyDer::from(key_der));

			let mut config = rustls::ServerConfig::builder()
				.with_no_client_auth()
				.with_single_cert(vec![cert], key)
				.map_err(|e| format!("Failed to create TLS config: {}", e))?;

			config.alpn_protocols = vec![b"h3".to_vec()];
			Ok(config)
		}
		CertificateConfig::Custom { cert_chain, private_key } => {
			// Parse PEM certificates
			let mut cert_reader = std::io::Cursor::new(cert_chain);
			let certs = rustls_pemfile::certs(&mut cert_reader)
				.collect::<Result<Vec<_>, _>>()
				.map_err(|e| format!("Failed to parse certificates: {}", e))?;

			if certs.is_empty() {
				return Err("No valid certificates found".into());
			}

			// Parse PEM private key
			let key = rustls_pemfile::private_key(&mut private_key.as_slice())?.ok_or("No private key found")?;

			let mut config = rustls::ServerConfig::builder()
				.with_no_client_auth()
				.with_single_cert(certs, key)
				.map_err(|e| format!("Failed to create TLS config: {}", e))?;

			config.alpn_protocols = vec![b"h3".to_vec()];
			Ok(config)
		}
	}
}
