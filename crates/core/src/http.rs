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
    pub redirects: Redirects,
}

/// What a request does with a redirect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Redirects {
    /// Return the redirect response. A redirect can send the headers and
    /// the body to another host: the API key and the conversation.
    Refuse,
    /// Follow up to 10 redirects, for the Fetch tool.
    Follow,
}

impl Request {
    pub fn new(method: Method, url: impl Into<String>, timeout: Duration) -> Self {
        Self {
            method,
            url: url.into(),
            headers: HeaderMap::new(),
            body: Vec::new(),
            timeout,
            redirects: Redirects::Refuse,
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
    /// Returns redirects: for the model API.
    direct: reqwest::Client,
    /// Follows redirects: for the Fetch tool.
    following: reqwest::Client,
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
        let client = |policy| {
            reqwest::Client::builder()
                .redirect(policy)
                .build()
                .expect("failed to build HTTP client")
        };

        Self {
            direct: client(reqwest::redirect::Policy::none()),
            following: client(reqwest::redirect::Policy::limited(10)),
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Http for LocalHttp {
    fn send(&self, request: Request) -> BoxFuture<'_, Result<Response, Error>> {
        use futures::StreamExt;

        Box::pin(async move {
            let client = match request.redirects {
                Redirects::Refuse => &self.direct,
                Redirects::Follow => &self.following,
            };

            let response = client
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

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// A server that answers each request with `head`, and keeps the
    /// requests it got.
    fn server(head: String) -> (u16, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();

        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                log.lock().unwrap().push(String::from_utf8_lossy(&buf[..n]).into_owned());
                let _ = stream.write_all(head.as_bytes());
            }
        });

        (port, seen)
    }

    #[tokio::test]
    async fn a_request_does_not_follow_a_redirect_by_default() {
        let (other, seen) = server("HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n".to_string());
        let (api, _) = server(format!(
            "HTTP/1.1 307 Temporary Redirect\r\nlocation: http://127.0.0.1:{other}/steal\r\ncontent-length: 0\r\n\r\n"
        ));

        let mut request = Request::new(Method::POST, format!("http://127.0.0.1:{api}/v1/messages"), Duration::from_secs(5));
        request.headers.insert("x-api-key", "sk-ant-secret".parse().unwrap());
        request.body = b"the conversation".to_vec();

        let response = LocalHttp::new().send(request).await.unwrap();

        assert_eq!(response.status, StatusCode::TEMPORARY_REDIRECT);
        assert!(seen.lock().unwrap().is_empty(), "the other host got: {:?}", seen.lock().unwrap());
    }

    #[tokio::test]
    async fn a_fetch_follows_a_redirect_when_asked() {
        let (other, seen) = server("HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok".to_string());
        let (start, _) = server(format!(
            "HTTP/1.1 302 Found\r\nlocation: http://127.0.0.1:{other}/page\r\ncontent-length: 0\r\n\r\n"
        ));

        let mut request = Request::new(Method::GET, format!("http://127.0.0.1:{start}/"), Duration::from_secs(5));
        request.redirects = Redirects::Follow;

        let response = LocalHttp::new().send(request).await.unwrap();

        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(seen.lock().unwrap().len(), 1);
    }
}
