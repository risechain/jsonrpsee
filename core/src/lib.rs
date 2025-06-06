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

//! Shared utilities for `jsonrpsee`.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

// Macros useful internally within this crate, but not to be exposed outside of it.
#[macro_use]
mod macros;

/// Error type.
pub mod error;

/// Traits
pub mod traits;

/// RPC Parameters.
pub mod params;

cfg_http_helpers! {
	pub mod http_helpers;
}

cfg_server! {
	pub mod id_providers;
	pub mod server;
}

cfg_client! {
	pub mod client;
	pub use client::Error as ClientError;
}

cfg_client_or_server! {
	pub mod middleware;
}

pub use async_trait::async_trait;
pub use error::{RegisterMethodError, SubscriptionError};

/// JSON-RPC result.
pub type RpcResult<T> = std::result::Result<T, jsonrpsee_types::ErrorObjectOwned>;

/// Empty server `RpcParams` type to use while registering modules.
pub type EmptyServerParams = Vec<()>;

#[doc(hidden)]
mod proc_macros_support;

/// Re-exports for proc-macro library to not require any additional
/// dependencies to be explicitly added on the client side.
#[doc(hidden)]
pub mod __reexports {
	pub use async_trait::async_trait;
	pub use serde;
	pub use serde_json;

	// Needed for the params parsing in the proc macro API.
	cfg_client_or_server! {
		pub use tokio;
	}

	pub use super::proc_macros_support::*;
}

pub use serde::{Serialize, de::DeserializeOwned};
pub use serde_json::{
	Value as JsonValue, to_value as to_json_value, value::RawValue as JsonRawValue,
	value::to_raw_value as to_json_raw_value,
};
pub use std::borrow::Cow;

/// Ten megabytes.
pub const TEN_MB_SIZE_BYTES: u32 = 10 * 1024 * 1024;

/// The return type if the subscription wants to return `Result`.
pub type SubscriptionResult = Result<(), SubscriptionError>;

/// Type erased error.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
