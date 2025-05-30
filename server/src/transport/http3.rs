
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use h3::quic::BidiStream;
use h3::server::{Connection, RequestStream};
use h3_quinn::quinn;
use h3_quinn::quinn::{Endpoint, ServerConfig, TransportConfig, VarInt};
use h3_quinn::quinn::congestion;
use jsonrpsee_core::server::rpc_module::Methods;
use jsonrpsee_core::server::{ConnectionState, RpcServiceT};
use jsonrpsee_types::{ErrorObject, ErrorObjectOwned, Request, Response};
use tokio::sync::{mpsc, Semaphore};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestPriority {
    Critical = 0,
    
    High = 1,
    
    Normal = 2,
    
    Low = 3,
}

impl Default for RequestPriority {
    fn default() -> Self {
        Self::Normal
    }
}

impl From<RequestPriority> for h3::Priority {
    fn from(priority: RequestPriority) -> Self {
        match priority {
            RequestPriority::Critical => h3::Priority { urgency: 0, incremental: false },
            RequestPriority::High => h3::Priority { urgency: 1, incremental: false },
            RequestPriority::Normal => h3::Priority { urgency: 3, incremental: true },
            RequestPriority::Low => h3::Priority { urgency: 7, incremental: true },
        }
    }
}

fn extract_priority_from_headers(headers: &http::HeaderMap) -> Option<RequestPriority> {
    headers.get("x-request-priority").and_then(|value| {
        value.to_str().ok().and_then(|s| match s.to_lowercase().as_str() {
            "critical" => Some(RequestPriority::Critical),
            "high" => Some(RequestPriority::High),
            "normal" => Some(RequestPriority::Normal),
            "low" => Some(RequestPriority::Low),
            _ => None,
        })
    })
}

use crate::future::BoxFuture;
use crate::transport::http::process_single_request;
use crate::{BatchRequestConfig, ConnectionGuard, ServerConfig as JsonRpcServerConfig, StopHandle};

#[derive(Debug, Clone)]
pub struct Http3Config {
    pub max_connections: usize,
    pub max_request_body_size: u32,
    pub max_response_body_size: u32,
    pub max_requests_per_batch: usize,
    pub batch_config: BatchRequestConfig,
    pub server_config: ServerConfig,
    pub enable_0rtt: bool,
    pub max_concurrent_requests_per_connection: usize,
    pub max_idle_timeout: Duration,
    pub keep_alive_interval: Option<Duration>,
    pub initial_congestion_window: u32,
    pub min_congestion_window: u32,
	pub enable_bbr_congestion_control: bool,
	pub optimistic_initial_rtt:bool,
	pub enable_performance_optimizations: bool,
}

impl Default for Http3Config {
    fn default() -> Self {
        Self {
            max_connections: 1000,                    // Increased from 100 to 1000 for much higher concurrency
            max_request_body_size: 32 * 1024 * 1024,  // 32 MiB (increased from 10 MiB)
            max_response_body_size: 32 * 1024 * 1024, // 32 MiB (increased from 10 MiB)
            max_requests_per_batch: 100,              // Increased from 20 to 100 for better batch processing
            batch_config: BatchRequestConfig::default(),
            server_config: ServerConfig::default(),
            enable_0rtt: true,                        // Enable 0-RTT for faster connections
            max_concurrent_requests_per_connection: 2048, // Increased from 100 to 2048 to match client
            max_idle_timeout: Duration::from_secs(300),   // Increased from 30s to 300s for longer connection lifetime
            keep_alive_interval: Some(Duration::from_secs(1)), // Reduced from 5s to 1s for more responsive keepalive
            initial_congestion_window: 64,            // Doubled from 32 to 64 for faster initial sending
            min_congestion_window: 4,                 // Doubled from 2 to 4 for better minimum performance
            enable_bbr_congestion_control: true,      // Enable BBR congestion control
            optimistic_initial_rtt: true,             // Enable optimistic initial RTT
            enable_performance_optimizations: true,   // Enable all performance optimizations
        }
    }
}

pub async fn run_http3_server<M>(
    addr: SocketAddr,
    methods: M,
    config: Http3Config,
    connection_guard: ConnectionGuard,
    stop_handle: StopHandle,
) -> Result<JoinHandle<()>, ErrorObjectOwned>
where
    M: Into<Methods> + Send + Sync + Clone + 'static,
{
    let methods = methods.into();

    let mut transport_config = TransportConfig::default();
    transport_config.max_concurrent_bidi_streams(VarInt::from_u32((config.max_concurrent_requests_per_connection * 2) as u32));
    transport_config.max_concurrent_uni_streams(VarInt::from_u32((config.max_concurrent_requests_per_connection * 2) as u32));
    transport_config.max_idle_timeout(Some(config.max_idle_timeout.try_into().unwrap()));

    if let Some(interval) = config.keep_alive_interval {
        transport_config.keep_alive_interval(Some(interval));
    }

    transport_config.initial_window(config.initial_congestion_window);
    transport_config.min_window(config.min_congestion_window);

    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(config.server_config));

    if config.enable_0rtt {
        server_config.use_retry(false); // Required for 0-RTT
        server_config.accept_zero_rtt = true;
        debug!("0-RTT connections enabled for HTTP/3 server");
    }

	if config.enable_performance_optimizations {
		debug!("Performance optimizations enabled for HTTP/3 server");
	}

    server_config.transport_config(Arc::new(transport_config));

    let endpoint = Endpoint::server(server_config, addr).map_err(|e| {
        ErrorObject::owned(
            4000,
            format!("Failed to create HTTP/3 endpoint: {}", e),
            None::<()>,
        )
    })?;

    info!("HTTP/3 server listening on {}", addr);

    let handle = tokio::spawn(async move {
        let mut incoming = endpoint.accept().await;

        while let Some(conn) = incoming.next().await {
            if stop_handle.is_stopped() {
                break;
            }

            let conn_guard = match connection_guard.try_acquire() {
                Ok(guard) => guard,
                Err(_) => {
                    debug!("Max connections reached, rejecting connection");
                    continue;
                }
            };

            let methods = methods.clone();
            let config = config.clone();
            let stop = stop_handle.clone();

            tokio::spawn(async move {
                let connection = match conn.await {
                    Ok(c) => {
                        debug!("Accepted HTTP/3 connection from {}", c.remote_address());
                        c
                    },
                    Err(e) => {
                        debug!("Failed to accept HTTP/3 connection: {}", e);
                        return;
                    }
                };

                let _ = handle_http3_connection(connection, methods, config, stop).await;
                drop(conn_guard);
            });
        }
    });

    Ok(handle)
}

async fn handle_http3_connection<M>(
    connection: quinn::Connection,
    methods: M,
    config: Http3Config,
    stop_handle: StopHandle,
) -> Result<(), ErrorObjectOwned>
where
    M: RpcServiceT + Send + Sync + Clone + 'static,
{
    let mut h3_conn = h3::server::Connection::new(h3_quinn::Connection::new(connection))
        .await
        .map_err(|e| {
            ErrorObject::owned(
                4001,
                format!("Failed to create HTTP/3 connection: {}", e),
                None::<()>,
            )
        })?;

    let critical_semaphore = Arc::new(Semaphore::new(
        (config.max_concurrent_requests_per_connection as f32 * 0.2) as usize
    ));
    let high_semaphore = Arc::new(Semaphore::new(
        (config.max_concurrent_requests_per_connection as f32 * 0.3) as usize
    ));
    let normal_semaphore = Arc::new(Semaphore::new(
        (config.max_concurrent_requests_per_connection as f32 * 0.4) as usize
    ));
    let low_semaphore = Arc::new(Semaphore::new(
        (config.max_concurrent_requests_per_connection as f32 * 0.1) as usize
    ));
    
    let (critical_tx, mut critical_rx) = mpsc::channel::<(RequestStream, mpsc::Sender<()>)>(100);
    let (high_tx, mut high_rx) = mpsc::channel::<(RequestStream, mpsc::Sender<()>)>(100);
    let (normal_tx, mut normal_rx) = mpsc::channel::<(RequestStream, mpsc::Sender<()>)>(100);
    let (low_tx, mut low_rx) = mpsc::channel::<(RequestStream, mpsc::Sender<()>)>(100);
    
    let methods_critical = methods.clone();
    let config_critical = config.clone();
    let stop_critical = stop_handle.clone();
    tokio::spawn(async move {
        process_priority_queue(
            methods_critical, 
            config_critical, 
            stop_critical, 
            critical_semaphore, 
            &mut critical_rx, 
            RequestPriority::Critical
        ).await;
    });
    
    let methods_high = methods.clone();
    let config_high = config.clone();
    let stop_high = stop_handle.clone();
    tokio::spawn(async move {
        process_priority_queue(
            methods_high, 
            config_high, 
            stop_high, 
            high_semaphore, 
            &mut high_rx, 
            RequestPriority::High
        ).await;
    });
    
    let methods_normal = methods.clone();
    let config_normal = config.clone();
    let stop_normal = stop_handle.clone();
    tokio::spawn(async move {
        process_priority_queue(
            methods_normal, 
            config_normal, 
            stop_normal, 
            normal_semaphore, 
            &mut normal_rx, 
            RequestPriority::Normal
        ).await;
    });
    
    let methods_low = methods.clone();
    let config_low = config.clone();
    let stop_low = stop_handle.clone();
    tokio::spawn(async move {
        process_priority_queue(
            methods_low, 
            config_low, 
            stop_low, 
            low_semaphore, 
            &mut low_rx, 
            RequestPriority::Low
        ).await;
    });

    loop {
        if stop_handle.is_stopped() {
            break;
        }

        let request = tokio::select! {
            req = h3_conn.accept() => {
                match req {
                    Ok(Some(req)) => req,
                    Ok(None) => break,
                    Err(e) => {
                        debug!("HTTP/3 connection error: {}", e);
                        break;
                    }
                }
            }
            _ = stop_handle.clone().shutdown() => break,
        };
        
        let (parts, _) = request.clone().into_parts();
        let priority = parts.extensions.get::<h3::Priority>().map(|p| {
            match p.urgency {
                0 => RequestPriority::Critical,
                1 => RequestPriority::High,
                2..=5 => RequestPriority::Normal,
                _ => RequestPriority::Low,
            }
        }).or_else(|| {
            extract_priority_from_headers(&parts.headers)
        }).unwrap_or_default();
        
        let (done_tx, done_rx) = mpsc::channel::<()>(1);
        
        let send_result = match priority {
            RequestPriority::Critical => critical_tx.send((request, done_tx)).await,
            RequestPriority::High => high_tx.send((request, done_tx)).await,
            RequestPriority::Normal => normal_tx.send((request, done_tx)).await,
            RequestPriority::Low => low_tx.send((request, done_tx)).await,
        };
        
        if send_result.is_err() {
            debug!("Failed to enqueue request with priority {:?}", priority);
        } else {
            debug!("Enqueued request with priority {:?}", priority);
            
            if priority == RequestPriority::Critical {
                let _ = done_rx.recv().await;
            }
        }
    }
    
    Ok(())
}

async fn process_priority_queue<M>(
    methods: M,
    config: Http3Config,
    stop_handle: StopHandle,
    semaphore: Arc<Semaphore>,
    receiver: &mut mpsc::Receiver<(RequestStream, mpsc::Sender<()>)>,
    priority: RequestPriority,
) 
where
    M: RpcServiceT + Send + Sync + Clone + 'static,
{
    debug!("Started priority queue processor for {:?} requests", priority);
    
    let cpu_priority = match priority {
        RequestPriority::Critical => tokio::task::TaskPriority::high(),
        RequestPriority::High => tokio::task::TaskPriority::high(),
        RequestPriority::Normal => tokio::task::TaskPriority::medium(),
        RequestPriority::Low => tokio::task::TaskPriority::low(),
    };
    
    while let Some((request, done_tx)) = receiver.recv().await {
        if stop_handle.is_stopped() {
            break;
        }
        
        let permit = match semaphore.clone().acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => {
                debug!("Failed to acquire semaphore for {:?} priority request", priority);
                let _ = done_tx.send(()).await;
                continue;
            }
        };
        
        let methods = methods.clone();
        let config_clone = config.clone();
        let stop = stop_handle.clone();
        
        let handle = tokio::task::Builder::new()
            .name(&format!("http3-{:?}-request", priority))
            .priority(cpu_priority)
            .spawn(async move {
                let start = std::time::Instant::now();
                let result = process_http3_request_stream(request, methods, config_clone, stop).await;
                
                let elapsed = start.elapsed();
                if let Err(e) = &result {
                    debug!("Error processing {:?} priority request: {}", priority, e);
                } else {
                    debug!("Processed {:?} priority request in {:?}", priority, elapsed);
                }
                
                let _ = done_tx.send(()).await;
                drop(permit);
            });
            
        if let Err(e) = handle {
            debug!("Failed to spawn task for {:?} priority request: {}", priority, e);
            let _ = done_tx.send(()).await;
            drop(permit);
        }
    }
    
    debug!("Priority queue processor for {:?} requests stopped", priority);
}

async fn process_http3_request_stream<M>(
    request: RequestStream,
    methods: M,
    config: Http3Config,
    stop_handle: StopHandle,
) -> Result<(), ErrorObjectOwned>
where
    M: RpcServiceT + Send + Sync + Clone + 'static,
{
    let (parts, mut req_body) = request.into_parts();

    let priority = parts.extensions.get::<h3::Priority>().map(|p| {
        match p.urgency {
            0 => RequestPriority::Critical,
            1 => RequestPriority::High,
            2..=5 => RequestPriority::Normal,
            _ => RequestPriority::Low,
        }
    }).or_else(|| {
        extract_priority_from_headers(&parts.headers)
    }).unwrap_or_default();
    
    let timeout_duration = match priority {
        RequestPriority::Critical => Duration::from_secs(5),  // Shorter timeout for critical requests
        RequestPriority::High => Duration::from_secs(8),
        RequestPriority::Normal => Duration::from_secs(10),
        RequestPriority::Low => Duration::from_secs(15),      // Longer timeout for low priority
    };
    
    debug!("Processing request with priority: {:?}", priority);

    let body = match tokio::time::timeout(
        timeout_duration,
        read_request_body(&mut req_body, config.max_request_body_size),
    ).await {
        Ok(Ok(body)) => body,
        Ok(Err(e)) => return Err(e),
        Err(_) => {
            return Err(ErrorObject::owned(
                4003,
                "Request body read timeout".to_string(),
                None::<()>,
            ));
        }
    };

    let methods = methods.clone();
    let config_clone = config.clone();

    let result = tokio::task::spawn_blocking(move || {
        let connection_state = ConnectionState::default();

        tokio::runtime::Handle::current().block_on(async {
            process_single_request(
                body.as_ref(),
                methods,
                config_clone.max_response_body_size,
                config_clone.max_requests_per_batch,
                config_clone.batch_config,
                connection_state,
            ).await
        })
    }).await;

    match result {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(e),
        Err(e) => Err(ErrorObject::owned(
            4004,
            format!("Task join error: {}", e),
            None::<()>,
        )),
    }
}

async fn read_request_body(
    req_body: &mut h3::server::RequestBody,
    max_size: u32,
) -> Result<bytes::Bytes, ErrorObjectOwned> {
    let mut total_size = 0;
    let mut chunks = Vec::new();
    
    while let Some(chunk) = req_body.data().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                debug!("HTTP/3 body error: {}", e);
                return Err(ErrorObject::owned(
                    4002,
                    format!("Failed to read request body: {}", e),
                    None::<()>,
                ));
            }
        };
        
        let chunk_len = chunk.len();
        total_size += chunk_len;
        
        if total_size > max_size as usize {
            debug!("HTTP/3 request body too large");
            return Err(ErrorObject::owned(
                4002,
                format!("Request body exceeds maximum size of {} bytes", max_size),
                None::<()>,
            ));
        }
        
        chunks.push(chunk);
    }
    
    if chunks.is_empty() {
        Ok(bytes::Bytes::new())
    } else if chunks.len() == 1 {
        Ok(chunks.into_iter().next().unwrap())
    } else {
        let mut combined = bytes::BytesMut::with_capacity(total_size);
        for chunk in chunks {
            combined.extend_from_slice(&chunk);
        }
        Ok(combined.freeze())
    }
}
