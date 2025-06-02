use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::{Buf, Bytes};
use h3::client::{self};
use h3_quinn::Connection as H3QuinnConnection;
use http::Request;
use http_body::Body as HttpBody;
use jsonrpsee_core::http_helpers::{Body, HttpError};
use quinn::{ClientConfig, Connection, Endpoint, TransportConfig, VarInt};
use rustls::RootCertStore;
use tokio::sync::Mutex;
use tower::Service;
use tracing::{debug, info, trace, warn};
use url::Url;

use crate::transport::Error;
use crate::{HttpRequest, HttpResponse};

/// HTTP/3 client implementation with connection pooling and advanced configuration options
#[derive(Clone, Debug)]
pub struct Http3Client {
	/// QUIC endpoint for establishing connections
	endpoint: Arc<Endpoint>,
	/// Configuration for the HTTP/3 client
	config: Http3Config,
	/// TLS client configuration
	client_config: Arc<ClientConfig>,
	/// Connection pool for reusing connections
	connections: Arc<Mutex<HashMap<String, ConnectionEntry>>>,
}

/// Predefined workload profiles for HTTP/3 configuration
///
/// These profiles provide optimized settings for different types of workloads.
#[derive(Clone, Debug, Copy)]
pub enum WorkloadProfile {
	/// Optimized for low latency mode
	///
	/// This profile prioritizes response time over throughput with smaller
	/// buffer sizes and more aggressive timeouts.
	LowLatency,

	/// Optimized for high throughput mode
	///
	/// This profile prioritizes data transfer rates over latency with larger
	/// buffer sizes and more relaxed timeouts.
	HighThroughput,

	/// Balanced profile with moderate settings
	///
	/// This profile provides a good balance between latency and throughput.
	Balanced,

	/// Custom profile with user-defined settings
	///
	/// Use this when we want to manually configure all HTTP/3 settings.
	Custom,
}

/// Configuration options for HTTP/3 client
#[derive(Clone, Debug, Copy)]
pub struct Http3Config {
	/// Maximum number of concurrent requests allowed
	pub max_concurrent_requests: usize,
	/// Request timeout duration
	pub request_timeout: Option<Duration>,
	/// Maximum size of request body in bytes
	pub max_request_body_size: usize,
	/// Maximum size of response body in bytes
	pub max_response_body_size: usize,
	/// Enable 0-RTT for faster connection establishment
	pub enable_0rtt: bool,
	/// Maximum idle timeout for QUIC connections
	pub max_idle_timeout: Duration,
	/// Interval for sending keep-alive probes
	pub keep_alive_interval: Option<Duration>,
	/// Maximum number of connections to maintain per host
	pub max_connections_per_host: usize,
	/// Timeout duration for idle connections
	pub connection_idle_timeout: Duration,
	/// Enable adaptive flow control for better performance
	pub enable_adaptive_flow_control: bool,
	/// Enable various performance optimizations
	pub enable_performance_optimizations: bool,
	/// Multiplier for maximum concurrent streams
	pub max_concurrent_streams_multiplier: u32,
	/// Size of the receive window in bytes
	pub receive_window_size: u32,
	/// Size of the send buffer in bytes
	pub send_buffer_size: u32,
	/// Enable BBR congestion control algorithm
	pub enable_bbr_congestion_control: bool,
	/// Initial round-trip time estimate in milliseconds
	pub initial_rtt_ms: u32,
	/// Factor by which buffers can grow
	pub buffer_growth_factor: u32,
	/// Certificate verification mode
	pub certificate_verification_mode: CertificateVerificationMode,
}

/// Certificate verification modes for HTTP/3 client
#[derive(Clone, Debug, Copy)]
pub enum CertificateVerificationMode {
	/// Standard verification using system trust store (default, recommended for production)
	Standard,

	/// Accept self-signed certificates (for testing only)
	AcceptSelfSigned,

	/// Skip certificate verification entirely (dangerous, for testing only)
	NoVerification,
}

impl Default for CertificateVerificationMode {
	fn default() -> Self {
		Self::Standard
	}
}

impl Http3Config {
	/// Create a configuration optimized for a specific workload profile
	pub fn for_workload(profile: WorkloadProfile) -> Self {
		match profile {
			WorkloadProfile::LowLatency => Self {
				max_concurrent_requests: 50,
				request_timeout: Some(Duration::from_secs(30)),
				max_request_body_size: 10 * 1024 * 1024,  // 10 MiB
				max_response_body_size: 10 * 1024 * 1024, // 10 MiB
				enable_0rtt: true,
				max_idle_timeout: Duration::from_secs(15),
				keep_alive_interval: Some(Duration::from_secs(2)),
				max_connections_per_host: 4,
				connection_idle_timeout: Duration::from_secs(30),
				enable_adaptive_flow_control: true,
				enable_performance_optimizations: true,
				max_concurrent_streams_multiplier: 1,
				receive_window_size: 8 * 1024 * 1024,
				send_buffer_size: 4 * 1024 * 1024,
				enable_bbr_congestion_control: true,
				initial_rtt_ms: 20,
				buffer_growth_factor: 1,
				certificate_verification_mode: CertificateVerificationMode::Standard,
			},
			WorkloadProfile::HighThroughput => Self {
				max_concurrent_requests: 200,
				request_timeout: Some(Duration::from_secs(120)),
				max_request_body_size: 20 * 1024 * 1024,  // 20 MiB
				max_response_body_size: 20 * 1024 * 1024, // 20 MiB
				enable_0rtt: true,
				max_idle_timeout: Duration::from_secs(60),
				keep_alive_interval: Some(Duration::from_secs(10)),
				max_connections_per_host: 16,
				connection_idle_timeout: Duration::from_secs(120),
				enable_adaptive_flow_control: true,
				enable_performance_optimizations: true,
				max_concurrent_streams_multiplier: 4,
				receive_window_size: 32 * 1024 * 1024,
				send_buffer_size: 16 * 1024 * 1024,
				enable_bbr_congestion_control: true,
				initial_rtt_ms: 100,
				buffer_growth_factor: 3,
				certificate_verification_mode: CertificateVerificationMode::Standard,
			},
			WorkloadProfile::Balanced => Self::default(),
			WorkloadProfile::Custom => Self::default(),
		}
	}
}

impl Default for Http3Config {
	fn default() -> Self {
		Self {
			max_concurrent_requests: 100,
			request_timeout: Some(Duration::from_secs(60)),
			max_request_body_size: 15 * 1024 * 1024,  // 15 MiB
			max_response_body_size: 15 * 1024 * 1024, // 15 MiB
			enable_0rtt: true,
			max_idle_timeout: Duration::from_secs(30),
			keep_alive_interval: Some(Duration::from_secs(5)),
			max_connections_per_host: 8,
			connection_idle_timeout: Duration::from_secs(60),
			enable_adaptive_flow_control: true,
			enable_performance_optimizations: true,
			max_concurrent_streams_multiplier: 2,
			receive_window_size: 16 * 1024 * 1024,
			send_buffer_size: 8 * 1024 * 1024,
			enable_bbr_congestion_control: true,
			initial_rtt_ms: 50,
			buffer_growth_factor: 2,
			certificate_verification_mode: CertificateVerificationMode::Standard,
		}
	}
}

/// Connection health metrics for HTTP/3 connections
#[derive(Debug, Clone, Copy)]
pub struct ConnectionHealthMetrics {
	/// Average round-trip time for the connection
	pub avg_rtt: Option<Duration>,
	/// Packet loss rate (0.0-1.0)
	pub packet_loss_rate: f32,
	/// Throughput in bits per second
	pub throughput_bps: u64,
	/// Connection stability score (0.0-1.0)
	pub connection_stability_score: f32,
	/// Last time the metrics were updated
	pub last_updated: std::time::Instant,
	/// Total bytes sent over this connection
	pub bytes_sent: u64,
	/// Total bytes received over this connection
	pub bytes_received: u64,
	/// Number of connection errors encountered
	pub connection_errors: u32,
}

impl Default for ConnectionHealthMetrics {
	fn default() -> Self {
		Self::new()
	}
}

impl ConnectionHealthMetrics {
	/// Create new connection health metrics with default values
	pub fn new() -> Self {
		Self {
			avg_rtt: None,
			packet_loss_rate: 0.0,
			throughput_bps: 0,
			connection_stability_score: 1.0,
			last_updated: std::time::Instant::now(),
			bytes_sent: 0,
			bytes_received: 0,
			connection_errors: 0,
		}
	}

	/// Updates the throughput metric based on bytes transferred and elapsed time
	///
	/// Calculates bits per second based on the provided byte count and duration.
	pub fn update_throughput(&mut self, bytes: u64, elapsed: Duration) {
		if elapsed.as_secs_f64() > 0.0 {
			self.throughput_bps = (bytes as f64 / elapsed.as_secs_f64()) as u64;
		}
	}

	/// Updates the packet loss rate based on lost and total packets
	///
	/// Calculates the ratio of lost packets to total packets.
	pub fn update_packet_loss(&mut self, lost: u32, total: u32) {
		if total > 0 {
			self.packet_loss_rate = lost as f32 / total as f32;
		}
	}

	/// Updates the average round-trip time with a new measurement
	///
	/// Uses an exponential moving average to smooth RTT values.
	pub fn update_rtt(&mut self, new_rtt: Duration) {
		self.avg_rtt = if let Some(avg) = self.avg_rtt {
			Some(Duration::from_micros((avg.as_micros() as u64 * 3 + new_rtt.as_micros() as u64) / 4))
		} else {
			Some(new_rtt)
		};
	}

	/// Calculates a stability score based on various metrics
	///
	/// Returns a value between 0.0 and 1.0 where higher values indicate better stability.
	pub fn calculate_stability_score(&mut self) -> f32 {
		let rtt_factor = if let Some(rtt) = self.avg_rtt {
			1.0 - (rtt.as_millis() as f32 / 500.0).min(1.0)
		} else {
			0.5 // Neutral if we don't have RTT data
		};

		let loss_factor = 1.0 - self.packet_loss_rate;

		let error_factor = 1.0 - (self.connection_errors as f32 / 100.0).min(1.0);

		let throughput_factor = (self.throughput_bps as f32 / 1_000_000.0).min(1.0);

		self.connection_stability_score =
			(rtt_factor * 0.3 + loss_factor * 0.3 + error_factor * 0.2 + throughput_factor * 0.2).clamp(0.0, 1.0);

		self.connection_stability_score
	}

	/// Updates metrics from QUIC connection statistics
	///
	/// Extracts and processes relevant metrics from the quinn connection stats.
	pub fn update_from_stats(&mut self, stats: &quinn::ConnectionStats) {
		self.update_rtt(stats.path.rtt);

		let lost_packets = stats.path.lost_packets as u32;
		let total_packets = stats.path.sent_packets as u32;
		self.update_packet_loss(lost_packets, total_packets);

		self.calculate_stability_score();

		self.last_updated = std::time::Instant::now();
	}
}

/// Connection entry with health metrics for HTTP/3 connections
struct ConnectionEntry {
	/// The QUIC connection
	connection: Connection,
	/// When the connection was last used
	last_used: std::time::Instant,
	/// Number of requests made on this connection
	request_count: usize,
	/// Total bytes sent over this connection
	bytes_sent: usize,
	/// Total bytes received over this connection
	bytes_received: usize,
	/// Health score for connection quality (0-10)
	health_score: f32,
	/// Detailed health metrics for this connection
	health_metrics: ConnectionHealthMetrics,
}

impl std::fmt::Debug for ConnectionEntry {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("ConnectionEntry")
			.field("last_used", &self.last_used)
			.field("request_count", &self.request_count)
			.field("bytes_sent", &self.bytes_sent)
			.field("bytes_received", &self.bytes_received)
			.field("health_score", &self.health_score)
			.field("health_metrics", &self.health_metrics)
			.finish()
	}
}

impl Http3Client {
	/// Creates a new HTTP/3 client with the specified configuration.
	///
	/// # Arguments
	///
	/// * `_url` - The base URL for the client (currently unused)
	/// * `config` - Configuration options for the HTTP/3 client
	///
	/// # Returns
	///
	/// Returns a Result containing the new HTTP/3 client instance or an Error
	pub async fn new(_url: &Url, config: Http3Config) -> Result<Self, Error> {
		let mut root_store = RootCertStore::empty();

		// Load native certificates for standard verification
		if matches!(config.certificate_verification_mode, CertificateVerificationMode::Standard) {
			for cert in rustls_native_certs::load_native_certs().expect("could not load platform certs") {
				root_store.add(cert).map_err(|e| {
					Error::Http(HttpError::Stream(Box::new(std::io::Error::other(format!(
						"Failed to add certificate to root store: {}",
						e
					)))))
				})?;
			}
		}

		// let mut tls_config = rustls::ClientConfig::builder().with_root_certificates(root_store).with_no_client_auth();
		// tls_config.alpn_protocols = vec![b"h3".to_vec()];

		let tls_config = match config.certificate_verification_mode {
			CertificateVerificationMode::Standard => {
				// Standard verification with system trust store
				let mut config =
					rustls::ClientConfig::builder().with_root_certificates(root_store).with_no_client_auth();

				config.alpn_protocols = vec![b"h3".to_vec()];
				config
			}

			/*CertificateVerificationMode::AcceptSelfSigned => {
				// Custom verifier that accepts self-signed certificates
				#[derive(Debug)]
				struct SelfSignedCertVerifier {
					root_store: RootCertStore,
				}

				impl rustls::client::danger::ServerCertVerifier for SelfSignedCertVerifier {
					fn verify_server_cert(
						&self,
						end_entity: &rustls::pki_types::CertificateDer<'_>,
						intermediates: &[rustls::pki_types::CertificateDer<'_>],
						server_name: &rustls::pki_types::ServerName<'_>,
						ocsp_response: &[u8],
						now: rustls::pki_types::UnixTime,
					) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
						// First try standard verification
						let standard_verifier =
							rustls::client::WebPkiServerVerifier::builder(self.root_store.clone().into())
								.build()
								.map_err(|e| rustls::Error::General(e.to_string()))?;

						let standard_result = standard_verifier.verify_server_cert(
							end_entity,
							intermediates,
							server_name,
							ocsp_response,
							now,
						);

						// If standard verification fails, accept it anyway (self-signed)
						if standard_result.is_err() {
							debug!(
								"Standard certificate verification failed, accepting as possible self-signed certificate"
							);
							Ok(rustls::client::danger::ServerCertVerified::assertion())
						} else {
							standard_result
						}
					}

					fn verify_tls12_signature(
						&self,
						message: &[u8],
						cert: &rustls::pki_types::CertificateDer<'_>,
						dss: &rustls::DigitallySignedStruct,
					) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
						// Try standard verification first
						let standard_verifier =
							rustls::client::WebPkiServerVerifier::builder(self.root_store.clone().into()).build();

						if let Ok(verifier) = standard_verifier {
							if let Ok(result) = verifier.verify_tls12_signature(message, cert, dss) {
								return Ok(result);
							}
						}

						// Accept signature if verification fails
						Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
					}

					fn verify_tls13_signature(
						&self,
						message: &[u8],
						cert: &rustls::pki_types::CertificateDer<'_>,
						dss: &rustls::DigitallySignedStruct,
					) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
						// Try standard verification first
						let standard_verifier =
							rustls::client::WebPkiServerVerifier::builder(self.root_store.clone().into()).build();

						if let Ok(verifier) = standard_verifier {
							if let Ok(result) = verifier.verify_tls13_signature(message, cert, dss) {
								return Ok(result);
							}
						}

						// Accept signature if verification fails
						Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
					}
					fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
						// Return standard supported schemes or fallback to common ones
						let standard_verifier =
							rustls::client::WebPkiServerVerifier::builder(self.root_store.clone().into()).build();

						if let Ok(verifier) = standard_verifier {
							verifier.supported_verify_schemes()
						} else {
							vec![
								rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
								rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
								rustls::SignatureScheme::RSA_PSS_SHA256,
								rustls::SignatureScheme::RSA_PKCS1_SHA256,
							]
						}
					}
				}

				let mut config = rustls::ClientConfig::builder()
					.dangerous()
					.with_custom_certificate_verifier(Arc::new(SelfSignedCertVerifier {
						root_store: root_store.clone(),
					}))
					.with_no_client_auth();

				config.alpn_protocols = vec![b"h3".to_vec()];
				config
			}*/
			CertificateVerificationMode::AcceptSelfSigned | CertificateVerificationMode::NoVerification => {
				// No verification at all (dangerous, only for testing)
				#[derive(Debug)]
				struct NoVerification;

				impl rustls::client::danger::ServerCertVerifier for NoVerification {
					fn verify_server_cert(
						&self,
						_: &rustls::pki_types::CertificateDer<'_>,
						_: &[rustls::pki_types::CertificateDer<'_>],
						_: &rustls::pki_types::ServerName<'_>,
						_: &[u8],
						_: rustls::pki_types::UnixTime,
					) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
						Ok(rustls::client::danger::ServerCertVerified::assertion())
					}

					fn verify_tls12_signature(
						&self,
						_: &[u8],
						_: &rustls::pki_types::CertificateDer<'_>,
						_: &rustls::DigitallySignedStruct,
					) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
						Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
					}

					fn verify_tls13_signature(
						&self,
						_: &[u8],
						_: &rustls::pki_types::CertificateDer<'_>,
						_: &rustls::DigitallySignedStruct,
					) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
						Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
					}

					fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
						vec![
							rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
							rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
							rustls::SignatureScheme::RSA_PSS_SHA256,
							rustls::SignatureScheme::RSA_PKCS1_SHA256,
						]
					}
				}

				let mut config = rustls::ClientConfig::builder()
					.dangerous()
					.with_custom_certificate_verifier(Arc::new(NoVerification))
					.with_no_client_auth();

				config.alpn_protocols = vec![b"h3".to_vec()];
				config
			}
		};

		let mut transport_config = TransportConfig::default();
		transport_config.max_concurrent_bidi_streams(VarInt::from_u32(config.max_concurrent_requests as u32));
		transport_config.max_concurrent_uni_streams(VarInt::from_u32(config.max_concurrent_requests as u32));
		transport_config.max_idle_timeout(Some(config.max_idle_timeout.try_into().unwrap()));

		if let Some(interval) = config.keep_alive_interval {
			transport_config.keep_alive_interval(Some(interval));
		}

		let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls_config).map_err(|_e| {
			Error::Http(HttpError::Stream(Box::new(std::io::Error::other("Failed to create QUIC client config"))))
		})?;
		let mut client_config = ClientConfig::new(Arc::new(crypto));
		client_config.transport_config(Arc::new(transport_config));

		let mut endpoint = Endpoint::client("0.0.0.0:0".parse().unwrap())
			.map_err(|e| Error::Http(HttpError::Stream(Box::new(std::io::Error::other(format!("{}", e))))))?;
		endpoint.set_default_client_config(client_config.clone());

		info!("HTTP/3 client initialized with optimized configuration");

		Ok(Self {
			endpoint: Arc::new(endpoint),
			config,
			client_config: Arc::new(client_config),
			connections: Arc::new(Mutex::new(HashMap::new())),
		})
	}
	/// Gets a connection from the pool or creates a new one
	///
	/// This method handles connection pooling, health monitoring, and connection establishment.
	async fn get_connection(&self, url: &Url) -> Result<Connection, Error> {
		let host = url.host_str().ok_or_else(|| Error::Url("Missing host in URL".into()))?;
		let port = url.port().unwrap_or(443);
		let key = format!("{}:{}", host, port);

		{
			let connections = self.connections.lock().await;
			if let Some(entry) = connections.get(&key) {
				if entry.connection.close_reason().is_none()
					&& entry.last_used.elapsed() < self.config.connection_idle_timeout
					&& entry.health_score >= 5.0
				{
					trace!("Reusing existing connection with health score {}", entry.health_score);
					return Ok(entry.connection.clone());
				}
			}
		}

		{
			let mut connections = self.connections.lock().await;

			// Check again in case another thread created a connection while we were waiting
			if let Some(entry) = connections.get_mut(&key) {
				if entry.connection.close_reason().is_none()
					&& entry.last_used.elapsed() < self.config.connection_idle_timeout
				{
					// Only update health metrics occasionally to reduce overhead
					if entry.request_count % 10 == 0 {
						let stats = entry.connection.stats();
						entry.health_metrics.update_from_stats(&stats);

						// Simplified health score calculation
						entry.health_score = 8.0 + entry.health_metrics.connection_stability_score * 2.0;
					}

					if entry.health_score >= 5.0 {
						entry.last_used = std::time::Instant::now();
						entry.request_count += 1;
						return Ok(entry.connection.clone());
					} else {
						debug!(
							"Existing connection has poor health score ({}), creating new connection",
							entry.health_score
						);
					}
				}
			}

			// Clean up stale connections
			connections.retain(|_, entry| {
				entry.connection.close_reason().is_none()
					&& entry.last_used.elapsed() < self.config.connection_idle_timeout
			});

			// Check connection limit
			let host_connections = connections.iter().filter(|(k, _)| k.starts_with(&format!("{}:", host))).count();

			if host_connections >= self.config.max_connections_per_host {
				// Find the best existing connection instead of creating a new one
				let best_entry = connections.iter().filter(|(k, _)| k.starts_with(&format!("{}:", host))).max_by(
					|(_, a), (_, b)| a.health_score.partial_cmp(&b.health_score).unwrap_or(std::cmp::Ordering::Equal),
				);

				if let Some((_, entry)) = best_entry {
					debug!(
						"Reusing best existing connection (score: {}) to stay within connection limit",
						entry.health_score
					);
					return Ok(entry.connection.clone());
				}
			}

			// Create new connection
			debug!("Creating new HTTP/3 connection to {}:{}", host, port);

			let server_name = host.to_string();
			let addr =
				format!("{}:{}", host, port).parse().map_err(|e| Error::Url(format!("Invalid address: {}", e)))?;

			// Optimize transport config for the specific use case
			let mut transport_config = TransportConfig::default();
			transport_config.max_concurrent_bidi_streams(VarInt::from_u32(
				self.config.max_concurrent_requests as u32 * self.config.max_concurrent_streams_multiplier,
			));
			transport_config.max_concurrent_uni_streams(VarInt::from_u32(
				self.config.max_concurrent_requests as u32 * self.config.max_concurrent_streams_multiplier,
			));
			transport_config.receive_window(VarInt::from_u32(self.config.receive_window_size));
			transport_config.send_window(VarInt::from_u32(self.config.send_buffer_size).into());
			transport_config.initial_rtt(Duration::from_millis(self.config.initial_rtt_ms as u64));

			if self.config.enable_bbr_congestion_control {
				transport_config
					.congestion_controller_factory(std::sync::Arc::new(quinn::congestion::BbrConfig::default()));
			}

			let mut client_config = self.client_config.as_ref().clone();
			client_config.transport_config(Arc::new(transport_config));

			let mut endpoint = self.endpoint.as_ref().clone();
			endpoint.set_default_client_config(client_config);

			// Connect with timeout to avoid hanging
			let connecting = self
				.endpoint
				.connect(addr, &server_name)
				.map_err(|e| Error::Http(HttpError::Stream(Box::new(std::io::Error::other(format!("{}", e))))))?;

			let connection = match tokio::time::timeout(Duration::from_secs(5), connecting).await {
				Ok(Ok(conn)) => conn,
				Ok(Err(e)) => {
					return Err(Error::Http(HttpError::Stream(Box::new(std::io::Error::other(format!("{}", e))))));
				}
				Err(_) => return Err(Error::RequestTimeout),
			};

			let stats = connection.stats();
			let initial_rtt = stats.path.rtt;

			let mut health_metrics = ConnectionHealthMetrics::new();
			health_metrics.update_rtt(initial_rtt);

			connections.insert(
				key,
				ConnectionEntry {
					connection: connection.clone(),
					last_used: std::time::Instant::now(),
					request_count: 0,
					bytes_sent: 0,
					bytes_received: 0,
					health_score: 8.0, // Start with good health score for new connections
					health_metrics,
				},
			);

			info!("Added new HTTP/3 connection to pool with initial RTT: {:?}", initial_rtt);

			Ok(connection)
		}
	}
}

/// Future type for HTTP/3 responses
type Http3ResponseType = Result<HttpResponse<http_body_util::Full<Bytes>>, Error>;
/// Boxed future for HTTP/3 responses
type Http3ResponseFutureInner = Pin<Box<dyn Future<Output = Http3ResponseType> + Send>>;

/// Future returned by HTTP/3 client for response handling
pub struct Http3ResponseFuture {
	/// The inner future that will resolve to the HTTP/3 response
	inner: Http3ResponseFutureInner,
}

impl std::fmt::Debug for Http3ResponseFuture {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Http3ResponseFuture").field("inner", &"<future>").finish()
	}
}

impl Future for Http3ResponseFuture {
	type Output = Result<HttpResponse<http_body_util::Full<Bytes>>, Error>;

	fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
		Pin::new(&mut self.inner).poll(cx)
	}
}

impl Service<HttpRequest<Body>> for Http3Client {
	type Response = HttpResponse<http_body_util::Full<Bytes>>;
	type Error = Error;
	type Future = Http3ResponseFuture;

	fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		Poll::Ready(Ok(()))
	}

	fn call(&mut self, req: HttpRequest<Body>) -> Self::Future {
		let client = self.clone();
		let url = req.uri().to_string().parse::<Url>().unwrap_or_else(|_| {
			warn!("Failed to parse URI as URL: {}", req.uri());
			"https://localhost".parse().unwrap()
		});

		let timeout = self.config.request_timeout;
		let enable_optimizations = self.config.enable_performance_optimizations;

		let fut = Box::pin(async move {
			let start_time = std::time::Instant::now();

			let connection = client.get_connection(&url).await?;

			let connection_time = start_time.elapsed();
			trace!("HTTP/3 connection acquisition took {:?}", connection_time);

			let h3_conn = H3QuinnConnection::new(connection.clone());

			let (parts, body) = req.into_parts();

			let mut request = Request::builder().uri(parts.uri).method(parts.method);

			if enable_optimizations {
				request = request.header("x-http3-optimized", "true").header("connection", "keep-alive");
			}

			for (name, value) in parts.headers.iter() {
				request = request.header(name, value);
			}

			let h3_request = request
				.body(())
				.map_err(|e| Error::Http(HttpError::Stream(Box::new(std::io::Error::other(format!("{}", e))))))?;

			let (mut h3_client, mut send_request) = if enable_optimizations {
				client::builder()
					.max_field_section_size(16 * 1024) // Larger header size
					.build(h3_conn)
					.await
			} else {
				client::builder().build(h3_conn).await
			}
			.map_err(|e| Error::Http(HttpError::Stream(Box::new(std::io::Error::other(format!("{}", e))))))?;

			tokio::spawn(async move {
				let err = h3_client.wait_idle().await;
				warn!("HTTP/3 connection closed: {}", err);
			});

			let request_start = std::time::Instant::now();
			let mut req_stream = if let Some(duration) = timeout {
				tokio::time::timeout(duration, send_request.send_request(h3_request))
					.await
					.map_err(|_| Error::RequestTimeout)?
					.map_err(|e| Error::Http(HttpError::Stream(Box::new(std::io::Error::other(format!("{}", e))))))?
			} else {
				send_request
					.send_request(h3_request)
					.await
					.map_err(|e| Error::Http(HttpError::Stream(Box::new(std::io::Error::other(format!("{}", e))))))?
			};

			let request_time = request_start.elapsed();
			trace!("HTTP/3 request initiation took {:?}", request_time);

			if HttpBody::size_hint(&body).exact() != Some(0) {
				let body_start = std::time::Instant::now();
				let collected = http_body_util::BodyExt::collect(body)
					.await
					.map_err(|e| Error::Http(HttpError::Stream(Box::new(std::io::Error::other(format!("{}", e))))))?;

				let bytes = collected.to_bytes();
				let body_size = bytes.len();

				if !bytes.is_empty() {
					let _chunk_size = if body_size > 1024 * 1024 {
						body_size.min(512 * 1024) // Max 512KB chunks
					} else if body_size > 64 * 1024 {
						body_size.min(64 * 1024) // Max 64KB chunks
					} else {
						body_size
					};

					if let Ok(mut connections) = client.connections.try_lock() {
						if let Some(entry) = connections.get_mut(&format!(
							"{}:{}",
							url.host_str().unwrap_or("localhost"),
							url.port().unwrap_or(443)
						)) {
							entry.bytes_sent += body_size;
						}
					}

					req_stream.send_data(bytes).await.map_err(|e| {
						Error::Http(HttpError::Stream(Box::new(std::io::Error::other(format!("{}", e)))))
					})?;

					trace!("Sent HTTP/3 request body of {} bytes in {:?}", body_size, body_start.elapsed());
				}
			}

			req_stream
				.finish()
				.await
				.map_err(|e| Error::Http(HttpError::Stream(Box::new(std::io::Error::other(format!("{}", e))))))?;

			let response_start = std::time::Instant::now();
			let h3_response = req_stream
				.recv_response()
				.await
				.map_err(|e| Error::Http(HttpError::Stream(Box::new(std::io::Error::other(format!("{}", e))))))?;

			let status = h3_response.status();
			let version = h3_response.version();
			let headers = h3_response.headers().clone();

			let initial_capacity = headers
				.get(http::header::CONTENT_LENGTH)
				.and_then(|v| v.to_str().ok())
				.and_then(|s| s.parse::<usize>().ok())
				.unwrap_or(8 * 1024); // Default 8KB initial capacity

			let mut body_bytes = Vec::with_capacity(initial_capacity);
			let mut total_bytes = 0;

			loop {
				match req_stream.recv_data().await {
					Ok(Some(mut chunk)) => {
						let chunk_size = chunk.remaining();
						let bytes = chunk.copy_to_bytes(chunk_size);

						if body_bytes.capacity() < total_bytes + chunk_size {
							let new_capacity = (total_bytes + chunk_size) * client.config.buffer_growth_factor as usize;
							body_bytes.reserve(new_capacity - body_bytes.capacity());
						}

						body_bytes.extend_from_slice(&bytes);
						total_bytes += chunk_size;
					}
					Ok(None) => break,
					Err(e) => {
						debug!("Error receiving HTTP/3 data: {:?}", e);
						break;
					}
				}
			}

			if let Ok(mut connections) = client.connections.try_lock() {
				if let Some(entry) = connections.get_mut(&format!(
					"{}:{}",
					url.host_str().unwrap_or("localhost"),
					url.port().unwrap_or(443)
				)) {
					entry.bytes_received += total_bytes;

					let stats = connection.stats();
					entry.health_metrics.update_from_stats(&stats);
				}
			}

			let response_time = response_start.elapsed();
			trace!("HTTP/3 response received in {:?} with {} bytes", response_time, total_bytes);

			let response = HttpResponse::builder()
				.status(status)
				.version(version)
				.body(http_body_util::Full::new(Bytes::from(body_bytes)))
				.map_err(|e| Error::Http(HttpError::Stream(Box::new(std::io::Error::other(format!("{}", e))))))?;

			Ok(response)
		});

		Http3ResponseFuture { inner: fut }
	}
}
