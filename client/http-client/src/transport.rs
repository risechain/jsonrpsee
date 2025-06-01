// Implementation note: hyper's API is not adapted to async/await at all, and there's
// unfortunately a lot of boilerplate here that could be removed once/if it gets reworked.
//
// Additionally, despite the fact that hyper is capable of performing requests to multiple different
// servers through the same `hyper::Client`, we don't use that feature on purpose. The reason is
// that we need to be guaranteed that hyper doesn't re-use an existing connection if we ever reset
// the JSON-RPC request id to a value that might have already been used.

use base64::Engine;
use bytes::Bytes;
use http_body;
use http_body::Body as HttpBody;
use http_body_util;
use hyper::http::{HeaderMap, HeaderValue};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use jsonrpsee_core::BoxError;
use jsonrpsee_core::{
	TEN_MB_SIZE_BYTES,
	http_helpers::{self, Body, HttpError},
};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
#[cfg(feature = "http3")]
use std::time::Duration;
use thiserror::Error;
use tower::layer::util::Identity;
use tower::{Layer, Service, ServiceExt};
use url::Url;

use crate::{HttpRequest, HttpResponse};

#[cfg(feature = "tls")]
use crate::{CertificateStore, CustomCertStore};

const CONTENT_TYPE_JSON: &str = "application/json";

/// Wrapper over HTTP transport and connector.
#[derive(Debug)]
pub enum HttpBackend<B = http_body_util::Full<Bytes>> {
	/// Hyper client with https connector.
	#[cfg(feature = "tls")]
	Https(Client<hyper_rustls::HttpsConnector<HttpConnector>, B>),
	/// Hyper client with http connector.
	Http(Client<HttpConnector, B>),
	/// HTTP/3 client implementation.
	#[cfg(feature = "http3")]
	Http3(crate::Http3Client),
}

impl<B> Clone for HttpBackend<B> {
	fn clone(&self) -> Self {
		match self {
			Self::Http(inner) => Self::Http(inner.clone()),
			#[cfg(feature = "tls")]
			Self::Https(inner) => Self::Https(inner.clone()),
			#[cfg(feature = "http3")]
			Self::Http3(inner) => Self::Http3(inner.clone()),
		}
	}
}

impl tower::Service<HttpRequest<Body>> for HttpBackend<http_body_util::Full<Bytes>> {
	type Response = HttpResponse<http_body_util::Full<Bytes>>;
	type Error = Error;
	type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

	fn poll_ready(&mut self, ctx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		match self {
			Self::Http(inner) => inner.poll_ready(ctx).map_err(|e| Error::Http(HttpError::Stream(Box::new(e)))),
			#[cfg(feature = "tls")]
			Self::Https(inner) => inner.poll_ready(ctx).map_err(|e| Error::Http(HttpError::Stream(Box::new(e)))),
			#[cfg(feature = "http3")]
			Self::Http3(inner) => inner.poll_ready(ctx).map_err(|e| Error::Http(HttpError::Stream(Box::new(e)))),
		}
	}

	fn call(&mut self, req: HttpRequest<Body>) -> Self::Future {
		let this = self.clone();

		Box::pin(async move {
			match this {
				Self::Http(mut inner) => {
					let (parts, body) = req.into_parts();

					let bytes = if HttpBody::size_hint(&body).exact() == Some(0) {
						Bytes::new()
					} else {
						match http_body_util::BodyExt::collect(body).await {
							Ok(collected) => collected.to_bytes(),
							Err(e) => {
								return Err(Error::Http(HttpError::Stream(e)));
							}
						}
					};

					let full_body = http_body_util::Full::new(bytes);
					let http_req = HttpRequest::from_parts(parts, full_body);

					let resp = inner.call(http_req).await.map_err(|e| Error::Http(HttpError::Stream(Box::new(e))))?;
					let (parts, body) = resp.into_parts();
					let bytes = match http_body_util::BodyExt::collect(body).await {
						Ok(collected) => collected.to_bytes(),
						Err(e) => {
							return Err(Error::Http(HttpError::Stream(e.into())));
						}
					};
					let full_body = http_body_util::Full::new(bytes);
					Ok(HttpResponse::from_parts(parts, full_body))
				}
				#[cfg(feature = "tls")]
				Self::Https(mut inner) => {
					let (parts, body) = req.into_parts();

					let bytes = if HttpBody::size_hint(&body).exact() == Some(0) {
						Bytes::new()
					} else {
						match http_body_util::BodyExt::collect(body).await {
							Ok(collected) => collected.to_bytes(),
							Err(e) => {
								return Err(Error::Http(HttpError::Stream(e)));
							}
						}
					};

					let full_body = http_body_util::Full::new(bytes);
					let http_req = HttpRequest::from_parts(parts, full_body);

					let resp = inner.call(http_req).await.map_err(|e| Error::Http(HttpError::Stream(Box::new(e))))?;
					let (parts, body) = resp.into_parts();
					let bytes = match http_body_util::BodyExt::collect(body).await {
						Ok(collected) => collected.to_bytes(),
						Err(e) => {
							return Err(Error::Http(HttpError::Stream(e.into())));
						}
					};
					let full_body = http_body_util::Full::new(bytes);
					Ok(HttpResponse::from_parts(parts, full_body))
				}
				#[cfg(feature = "http3")]
				Self::Http3(mut inner) => inner.call(req).await,
			}
		})
	}
}

/// Builder for [`HttpTransportClient`].
#[derive(Debug)]
pub struct HttpTransportClientBuilder<L> {
	/// Certificate store.
	#[cfg(feature = "tls")]
	pub(crate) certificate_store: CertificateStore,
	/// Configurable max request body size
	pub(crate) max_request_size: u32,
	/// Configurable max response body size
	pub(crate) max_response_size: u32,
	/// Custom headers to pass with every request.
	pub(crate) headers: HeaderMap,
	/// Service builder
	pub(crate) service_builder: tower::ServiceBuilder<L>,
	/// TCP_NODELAY
	pub(crate) tcp_no_delay: bool,
	/// Enable HTTP/3 support
	#[cfg(feature = "http3")]
	pub(crate) enable_http3: bool,
	/// HTTP/3 workload profile for optimized performance
	#[cfg(feature = "http3")]
	pub(crate) http3_workload_profile: Option<crate::http3::WorkloadProfile>,
	#[cfg(feature = "http3")]
	pub(crate) http3_certificate_verification_mode: Option<crate::http3::CertificateVerificationMode>,
}

impl Default for HttpTransportClientBuilder<Identity> {
	fn default() -> Self {
		Self::new()
	}
}

impl HttpTransportClientBuilder<Identity> {
	/// Create a new [`HttpTransportClientBuilder`].
	pub fn new() -> Self {
		Self {
			#[cfg(feature = "tls")]
			certificate_store: CertificateStore::Native,
			max_request_size: TEN_MB_SIZE_BYTES,
			max_response_size: TEN_MB_SIZE_BYTES,
			headers: HeaderMap::new(),
			service_builder: tower::ServiceBuilder::new(),
			tcp_no_delay: true,
			#[cfg(feature = "http3")]
			enable_http3: false,
			#[cfg(feature = "http3")]
			http3_workload_profile: None,
			#[cfg(feature = "http3")]
			http3_certificate_verification_mode: None,
		}
	}
}

impl<L> HttpTransportClientBuilder<L> {
	#[cfg(feature = "http3")]
	/// Enable HTTP/3 support.
	///
	/// URLs with the `http3://` scheme will be handled by the HTTP/3 client implementation.
	pub fn enable_http3(mut self) -> Self {
		self.enable_http3 = true;
		self
	}

	/// Set the HTTP/3 workload profile for optimized performance.
	///
	#[cfg(feature = "http3")]
	pub fn with_http3_workload_profile(mut self, profile: crate::http3::WorkloadProfile) -> Self {
		self.http3_workload_profile = Some(profile);
		self
	}

	/// See docs [`crate::HttpClientBuilder::with_custom_cert_store`] for more information.
	#[cfg(feature = "tls")]
	pub fn with_custom_cert_store(mut self, cfg: CustomCertStore) -> Self {
		self.certificate_store = CertificateStore::Custom(cfg);
		self
	}

	/// Set the maximum size of a request body in bytes. Default is 10 MiB.
	pub fn max_request_size(mut self, size: u32) -> Self {
		self.max_request_size = size;
		self
	}

	/// Set the maximum size of a response in bytes. Default is 10 MiB.
	pub fn max_response_size(mut self, size: u32) -> Self {
		self.max_response_size = size;
		self
	}

	/// Set a custom header passed to the server with every request (default is none).
	///
	/// The caller is responsible for checking that the headers do not conflict or are duplicated.
	pub fn set_headers(mut self, headers: HeaderMap) -> Self {
		self.headers = headers;
		self
	}

	/// Configure `TCP_NODELAY` on the socket to the supplied value `nodelay`.
	///
	/// Default is `true`.
	pub fn set_tcp_no_delay(mut self, no_delay: bool) -> Self {
		self.tcp_no_delay = no_delay;
		self
	}

	/// Configure a tower service.
	pub fn set_service<T>(self, service: tower::ServiceBuilder<T>) -> HttpTransportClientBuilder<T> {
		HttpTransportClientBuilder {
			#[cfg(feature = "tls")]
			certificate_store: self.certificate_store,
			headers: self.headers,
			max_request_size: self.max_request_size,
			max_response_size: self.max_response_size,
			service_builder: service,
			tcp_no_delay: self.tcp_no_delay,
			#[cfg(feature = "http3")]
			enable_http3: self.enable_http3,
			#[cfg(feature = "http3")]
			http3_workload_profile: self.http3_workload_profile,
			#[cfg(feature = "http3")]
			http3_certificate_verification_mode: self.http3_certificate_verification_mode,
		}
	}

	/// Build a [`HttpTransportClient`].
	pub async fn build<S, B>(self, target: impl AsRef<str>) -> Result<HttpTransportClient<S>, Error>
	where
		L: Layer<HttpBackend, Service = S>,
		S: Service<HttpRequest, Response = HttpResponse<B>, Error = Error> + Clone,
		B: http_body::Body<Data = Bytes> + Send + 'static,
		B::Data: Send,
		B::Error: Into<BoxError>,
	{
		#[allow(unused_mut)]
		let mut max_request_size = self.max_request_size;
		#[allow(unused_mut)]
		let mut max_response_size = self.max_response_size;
		#[allow(unused_mut)]
		let mut headers = self.headers;
		#[allow(unused_mut)]
		let mut service_builder = self.service_builder;
		#[allow(unused_mut)]
		let mut tcp_no_delay = self.tcp_no_delay;

		#[cfg(feature = "tls")]
		let certificate_store = self.certificate_store;

		let mut url = Url::parse(target.as_ref()).map_err(|e| Error::Url(format!("Invalid URL: {e}")))?;

		if url.host_str().is_none() {
			return Err(Error::Url("Invalid host".into()));
		}
		url.set_fragment(None);

		let client = match url.scheme() {
			"http" => {
				let mut connector = HttpConnector::new();
				connector.set_nodelay(tcp_no_delay);
				HttpBackend::Http(Client::builder(TokioExecutor::new()).build(connector))
			}
			#[cfg(feature = "tls")]
			"https" => {
				// Make sure that the TLS provider is set. If not, set a default one.
				// Otherwise, creating `tls` configuration may panic if there are multiple
				// providers available due to `rustls` features (e.g. both `ring` and `aws-lc-rs`).
				// Function returns an error if the provider is already installed, and we're fine with it.
				let _ = rustls::crypto::ring::default_provider().install_default();

				let mut http_conn = HttpConnector::new();
				http_conn.set_nodelay(tcp_no_delay);
				http_conn.enforce_http(false);

				let https_conn = match certificate_store {
					CertificateStore::Native => {
						use rustls_platform_verifier::ConfigVerifierExt;

						hyper_rustls::HttpsConnectorBuilder::new()
							.with_tls_config(rustls::ClientConfig::with_platform_verifier())
							.https_or_http()
							.enable_all_versions()
							.wrap_connector(http_conn)
					}

					CertificateStore::Custom(tls_config) => hyper_rustls::HttpsConnectorBuilder::new()
						.with_tls_config(tls_config)
						.https_or_http()
						.enable_all_versions()
						.wrap_connector(http_conn),
				};

				HttpBackend::Https(Client::builder(TokioExecutor::new()).build(https_conn))
			}
			#[cfg(feature = "http3")]
			"http3" => {
				#[cfg(feature = "http3")]
				let enable_http3 = self.enable_http3;

				if !enable_http3 {
					return Err(Error::Url(
						"HTTP/3 support is not enabled. Use enable_http3() method to enable it.".into(),
					));
				}

				let http3_config = match self.http3_workload_profile {
					Some(profile) => {
						let mut config = crate::Http3Config::for_workload(profile);
						config.max_request_body_size = max_request_size as usize;
						config.max_response_body_size = max_response_size as usize;
						config
					}
					None => crate::Http3Config {
						max_concurrent_requests: 100,
						request_timeout: Some(std::time::Duration::from_secs(60)),
						max_request_body_size: max_request_size as usize,
						max_response_body_size: max_response_size as usize,
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
						certificate_verification_mode: self
							.http3_certificate_verification_mode
							.unwrap_or(crate::http3::CertificateVerificationMode::Standard),
					},
				};

				let http3_client = crate::Http3Client::new(&url, http3_config).await.map_err(|e| match e {
					Error::Url(msg) => Error::Url(msg),
					_ => Error::Http(jsonrpsee_core::http_helpers::HttpError::Stream(Box::new(e))),
				})?;

				HttpBackend::Http3(http3_client)
			}
			_ => {
				#[cfg(all(feature = "tls", feature = "http3"))]
				let err = "URL scheme not supported, expects 'http', 'https', or 'http3'";
				#[cfg(all(feature = "tls", not(feature = "http3")))]
				let err = "URL scheme not supported, expects 'http' or 'https'";
				#[cfg(all(not(feature = "tls"), feature = "http3"))]
				let err = "URL scheme not supported, expects 'http' or 'http3'";
				#[cfg(not(any(feature = "tls", feature = "http3")))]
				let err = "URL scheme not supported, expects 'http'";
				return Err(Error::Url(err.into()));
			}
		};

		// Cache request headers: 2 default headers, followed by user custom headers.
		// Maintain order for headers in case of duplicate keys:
		// https://datatracker.ietf.org/doc/html/rfc7230#section-3.2.2
		let mut cached_headers = HeaderMap::with_capacity(2 + headers.len());
		cached_headers.insert(hyper::header::CONTENT_TYPE, HeaderValue::from_static(CONTENT_TYPE_JSON));
		cached_headers.insert(hyper::header::ACCEPT, HeaderValue::from_static(CONTENT_TYPE_JSON));
		for (key, value) in headers.into_iter() {
			if let Some(key) = key {
				cached_headers.insert(key, value);
			}
		}

		if let Some(pwd) = url.password() {
			if !cached_headers.contains_key(hyper::header::AUTHORIZATION) {
				let digest = base64::engine::general_purpose::STANDARD.encode(format!("{}:{pwd}", url.username()));
				cached_headers.insert(
					hyper::header::AUTHORIZATION,
					HeaderValue::from_str(&format!("Basic {digest}"))
						.map_err(|_| Error::Url("Header value `authorization basic user:pwd` invalid".into()))?,
				);
			}
		}

		Ok(HttpTransportClient {
			target: url.as_str().to_owned(),
			client: service_builder.service(client),
			max_request_size,
			max_response_size,
			headers: cached_headers,
		})
	}
}

/// HTTP Transport Client.
#[derive(Debug, Clone)]
pub struct HttpTransportClient<S> {
	/// Target to connect to.
	target: String,
	/// HTTP client
	client: S,
	/// Configurable max request body size
	max_request_size: u32,
	/// Configurable max response body size
	max_response_size: u32,
	/// Custom headers to pass with every request.
	headers: HeaderMap,
}

impl<B, S> HttpTransportClient<S>
where
	S: Service<HttpRequest, Response = HttpResponse<B>, Error = Error> + Clone,
	B: http_body::Body<Data = Bytes> + Send + 'static,
	B::Data: Send,
	B::Error: Into<BoxError>,
{
	async fn inner_send(&self, body: String) -> Result<HttpResponse<B>, Error> {
		if body.len() > self.max_request_size as usize {
			return Err(Error::RequestTooLarge);
		}

		let mut req = HttpRequest::post(&self.target);
		if let Some(headers) = req.headers_mut() {
			*headers = self.headers.clone();
		}

		let req = req.body(body.into()).expect("URI and request headers are valid; qed");
		let response = self.client.clone().ready().await?.call(req).await?;

		if response.status().is_success() {
			Ok(response)
		} else {
			Err(Error::Rejected { status_code: response.status().into() })
		}
	}

	/// Send serialized message and wait until all bytes from the HTTP message body have been read.
	/// Uses zero-copy abstractions with bytes::Bytes for improved performance.
	pub(crate) async fn send_and_read_body(&self, body: String) -> Result<Bytes, Error> {
		let response = self.inner_send(body).await?;

		let (parts, body) = response.into_parts();
		let (body, _is_single) = http_helpers::read_body(&parts.headers, body, self.max_response_size).await?;

		Ok(body)
	}

	/// Send serialized message without reading the HTTP message body.
	pub(crate) async fn send(&self, body: String) -> Result<(), Error> {
		self.inner_send(body).await?;
		Ok(())
	}
}

/// Error that can happen during a request.
#[derive(Debug, Error)]
pub enum Error {
	/// Invalid URL.
	#[error("Invalid Url: {0}")]
	Url(String),

	/// Error during the HTTP request, including networking errors and HTTP protocol errors.
	#[error(transparent)]
	Http(#[from] HttpError),

	/// Server returned a non-success status code.
	#[error("Request rejected `{status_code}`")]
	Rejected {
		/// HTTP Status code returned by the server.
		status_code: u16,
	},

	/// Request body too large.
	#[error("The request body was too large")]
	RequestTooLarge,

	/// Invalid certificate store.
	#[error("Invalid certificate store")]
	InvalidCertficateStore,

	/// Request timed out.
	#[error("Request timed out")]
	RequestTimeout,

	/// Request timed out (legacy).
	#[error("Request timed out")]
	Timeout,
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn invalid_http_url_rejected() {
		let err = tokio::runtime::Runtime::new()
			.unwrap()
			.block_on(async { HttpTransportClientBuilder::new().build("ws://localhost:9933").await.unwrap_err() });
		assert!(matches!(err, Error::Url(_)));
	}

	#[cfg(feature = "tls")]
	#[test]
	fn https_works() {
		let client = tokio::runtime::Runtime::new()
			.unwrap()
			.block_on(async { HttpTransportClientBuilder::new().build("https://localhost").await.unwrap() });
		assert_eq!(&client.target, "https://localhost/");
	}

	#[cfg(not(feature = "tls"))]
	#[test]
	fn https_fails_without_tls_feature() {
		let err = tokio::runtime::Runtime::new()
			.unwrap()
			.block_on(async { HttpTransportClientBuilder::new().build("https://localhost").await.unwrap_err() });
		assert!(matches!(err, Error::Url(_)));
	}

	#[test]
	fn faulty_port() {
		let runtime = tokio::runtime::Runtime::new().unwrap();

		let err = runtime
			.block_on(async { HttpTransportClientBuilder::new().build("http://localhost:-43").await.unwrap_err() });
		assert!(matches!(err, Error::Url(_)));

		let err = runtime
			.block_on(async { HttpTransportClientBuilder::new().build("http://localhost:-99999").await.unwrap_err() });
		assert!(matches!(err, Error::Url(_)));
	}

	#[test]
	fn url_with_path_works() {
		let client = tokio::runtime::Runtime::new().unwrap().block_on(async {
			HttpTransportClientBuilder::new().build("http://localhost/my-special-path").await.unwrap()
		});
		assert_eq!(&client.target, "http://localhost/my-special-path");
	}

	#[test]
	fn url_with_query_works() {
		let client = tokio::runtime::Runtime::new().unwrap().block_on(async {
			HttpTransportClientBuilder::new().build("http://127.0.0.1/my?name1=value1&name2=value2").await.unwrap()
		});
		assert_eq!(&client.target, "http://127.0.0.1/my?name1=value1&name2=value2");
	}

	#[test]
	fn url_with_fragment_is_ignored() {
		let client = tokio::runtime::Runtime::new().unwrap().block_on(async {
			HttpTransportClientBuilder::new().build("http://127.0.0.1/my.htm#ignore").await.unwrap()
		});
		assert_eq!(&client.target, "http://127.0.0.1/my.htm");
	}

	#[test]
	fn url_default_port_is_omitted() {
		let client = tokio::runtime::Runtime::new()
			.unwrap()
			.block_on(async { HttpTransportClientBuilder::new().build("http://127.0.0.1:80").await.unwrap() });
		assert_eq!(&client.target, "http://127.0.0.1/");
	}

	#[cfg(feature = "tls")]
	#[test]
	fn https_custom_port_works() {
		let client = tokio::runtime::Runtime::new()
			.unwrap()
			.block_on(async { HttpTransportClientBuilder::new().build("https://localhost:9999").await.unwrap() });
		assert_eq!(&client.target, "https://localhost:9999/");
	}

	#[test]
	fn http_custom_port_works() {
		let client = tokio::runtime::Runtime::new()
			.unwrap()
			.block_on(async { HttpTransportClientBuilder::new().build("http://localhost:9999").await.unwrap() });
		assert_eq!(&client.target, "http://localhost:9999/");
	}

	#[tokio::test]
	async fn request_limit_works() {
		let eighty_bytes_limit = 80;
		let fifty_bytes_limit = 50;

		let client = HttpTransportClientBuilder::new()
			.max_request_size(eighty_bytes_limit)
			.max_response_size(fifty_bytes_limit)
			.build("http://localhost:9933")
			.await
			.unwrap();

		assert_eq!(client.max_request_size, eighty_bytes_limit);
		assert_eq!(client.max_response_size, fifty_bytes_limit);

		let body = "a".repeat(81);
		assert_eq!(body.len(), 81);
		let response = client.send(body).await.unwrap_err();
		assert!(matches!(response, Error::RequestTooLarge));
	}
}
