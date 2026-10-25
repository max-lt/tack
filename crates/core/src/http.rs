//! HTTP capability used by the model API client and the Fetch tool.
//!
//! A native build sends requests with reqwest. A sandbox host gives its own
//! implementation, for example on top of `wasi:http/outgoing-handler`, and
//! applies its network policy there.

use std::time::Duration;

use bytes::Bytes;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
pub use http::{HeaderMap, Method, StatusCode};

pub struct Request {
    pub method: Method,
    pub url: String,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
    pub timeout: Duration,
}

impl Request {
    pub fn new(method: Method, url: impl Into<String>, timeout: Duration) -> Self {
        Self {
            method,
            url: url.into(),
            headers: HeaderMap::new(),
            body: Vec::new(),
            timeout,
        }
    }
}

pub struct Response {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: BoxStream<'static, Result<Bytes, Error>>,
}

impl Response {
    /// Read the whole body.
    pub async fn bytes(self) -> Result<Vec<u8>, Error> {
        use futures::TryStreamExt;

        self.body
            .try_fold(Vec::new(), |mut acc, chunk| async move {
                acc.extend_from_slice(&chunk);
                Ok(acc)
            })
            .await
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("request timed out")]
    Timeout,
    #[error("connection failed: {0}")]
    Connect(String),
    #[error("{0}")]
    Other(String),
}

pub trait Http: Send + Sync {
    fn send(&self, request: Request) -> BoxFuture<'_, Result<Response, Error>>;
}

/// Sends requests from the process itself with reqwest.
#[cfg(not(target_arch = "wasm32"))]
pub struct LocalHttp {
    client: reqwest::Client,
}

#[cfg(not(target_arch = "wasm32"))]
impl Default for LocalHttp {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl LocalHttp {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .expect("failed to build HTTP client");

        Self { client }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Http for LocalHttp {
    fn send(&self, request: Request) -> BoxFuture<'_, Result<Response, Error>> {
        use futures::StreamExt;

        Box::pin(async move {
            let response = self
                .client
                .request(request.method, &request.url)
                .headers(request.headers)
                .body(request.body)
                .timeout(request.timeout)
                .send()
                .await
                .map_err(map_error)?;

            Ok(Response {
                status: response.status(),
                headers: response.headers().clone(),
                body: response
                    .bytes_stream()
                    .map(|c| c.map_err(map_error))
                    .boxed(),
            })
        })
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn map_error(e: reqwest::Error) -> Error {
    if e.is_timeout() {
        Error::Timeout
    } else if e.is_connect() {
        Error::Connect(e.to_string())
    } else {
        Error::Other(e.to_string())
    }
}
