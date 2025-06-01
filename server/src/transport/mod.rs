/// HTTP related server functionality.
pub mod http;
/// WebSocket related server functionality.
pub mod ws;

#[cfg(feature = "http3")]
pub mod http3;
