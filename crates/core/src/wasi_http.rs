//! `Http` on top of the WASI 0.3 `wasi:http/client` import.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use futures::future::BoxFuture;
use futures::stream::{self, Stream, StreamExt};
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};

use crate::http::{Error, Http, Request, Response};

wit_bindgen::generate!({
    world: "guest",
    path: "wit",
    generate_all,
});

use wasi::http::client;
use wasi::http::types::{
    ErrorCode, Fields, Method as WasiMethod, Request as WasiRequest, RequestOptions,
    Response as WasiResponse, Scheme,
};
use wit_bindgen::rt::async_support::StreamResult;

/// Response bodies are read in pieces of this size.
const CHUNK_SIZE: usize = 64 * 1024;

/// Sends requests through the host's `wasi:http/client`.
pub struct WasiHttp;

impl Http for WasiHttp {
    fn send(&self, request: Request) -> BoxFuture<'_, Result<Response, Error>> {
        Box::pin(SingleThreaded(send(request)))
    }
}

async fn send(request: Request) -> Result<Response, Error> {
    let url = url::Url::parse(&request.url).map_err(|e| Error::Other(e.to_string()))?;

    let entries = request
        .headers
        .iter()
        .map(|(name, value)| (name.to_string(), value.as_bytes().to_vec()))
        .collect::<Vec<_>>();
    let headers = Fields::from_list(&entries).map_err(|e| Error::Other(format!("{e:?}")))?;

    let (mut body_tx, body_rx) = wit_stream::new::<u8>();
    let (trailers_tx, trailers_rx) = wit_future::new(|| Ok(None));
    let (outgoing, transmit) = WasiRequest::new(
        headers,
        Some(body_rx),
        trailers_rx,
        Some(options(request.timeout)),
    );

    set_target(&outgoing, request.method, &url)?;

    // The host reads the body while `send` waits for the response.
    let body = request.body;
    wit_bindgen::spawn_local(async move {
        let _ = body_tx.write_all(body).await;
        drop(body_tx);
        drop(trailers_tx);
        let _ = transmit.await;
    });

    let incoming = client::send(outgoing).await.map_err(map_error)?;

    let status = StatusCode::from_u16(incoming.get_status_code())
        .map_err(|e| Error::Other(e.to_string()))?;

    let mut headers = HeaderMap::new();
    for (name, value) in incoming.get_headers().copy_all() {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_bytes(&value),
        ) {
            headers.append(name, value);
        }
    }

    let (done_tx, done_rx) = wit_future::new(|| Ok(()));
    let (reader, _trailers) = WasiResponse::consume_body(incoming, done_rx);

    // `done_tx` lives in the state: dropping it tells the host the body is
    // fully read.
    let body = stream::unfold(Some((reader, done_tx)), |state| async move {
        let (mut reader, done_tx) = state?;

        loop {
            let (result, chunk) = reader.read(Vec::with_capacity(CHUNK_SIZE)).await;
            let ended = !matches!(result, StreamResult::Complete(_));

            if !chunk.is_empty() {
                let next = (!ended).then_some((reader, done_tx));
                return Some((Ok(Bytes::from(chunk)), next));
            }

            if ended {
                return None;
            }
        }
    });

    Ok(Response {
        status,
        headers,
        body: SingleThreaded(body).boxed(),
    })
}

fn options(timeout: Duration) -> RequestOptions {
    let options = RequestOptions::new();
    let nanos = Some(timeout.as_nanos() as u64);

    // A host may not support these; the request then goes out without them.
    let _ = options.set_connect_timeout(nanos);
    let _ = options.set_first_byte_timeout(nanos);
    let _ = options.set_between_bytes_timeout(nanos);

    options
}

fn set_target(request: &WasiRequest, method: Method, url: &url::Url) -> Result<(), Error> {
    let method = match method {
        Method::GET => WasiMethod::Get,
        Method::HEAD => WasiMethod::Head,
        Method::POST => WasiMethod::Post,
        Method::PUT => WasiMethod::Put,
        Method::DELETE => WasiMethod::Delete,
        Method::CONNECT => WasiMethod::Connect,
        Method::OPTIONS => WasiMethod::Options,
        Method::TRACE => WasiMethod::Trace,
        Method::PATCH => WasiMethod::Patch,
        other => WasiMethod::Other(other.to_string()),
    };

    let scheme = match url.scheme() {
        "https" => Scheme::Https,
        "http" => Scheme::Http,
        other => Scheme::Other(other.to_string()),
    };

    let authority = match (url.host_str(), url.port()) {
        (Some(host), Some(port)) => format!("{host}:{port}"),
        (Some(host), None) => host.to_string(),
        (None, _) => return Err(Error::Other(format!("URL has no host: {url}"))),
    };

    let path = match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_string(),
    };

    let invalid = |part: &str| Error::Other(format!("invalid {part} in {url}"));

    request
        .set_method(&method)
        .map_err(|()| invalid("method"))?;
    request
        .set_scheme(Some(&scheme))
        .map_err(|()| invalid("scheme"))?;
    request
        .set_authority(Some(&authority))
        .map_err(|()| invalid("authority"))?;
    request
        .set_path_with_query(Some(&path))
        .map_err(|()| invalid("path"))?;

    Ok(())
}

fn map_error(e: ErrorCode) -> Error {
    match e {
        ErrorCode::ConnectionTimeout
        | ErrorCode::ConnectionReadTimeout
        | ErrorCode::ConnectionWriteTimeout
        | ErrorCode::HttpResponseTimeout => Error::Timeout,
        ErrorCode::DnsTimeout
        | ErrorCode::DnsError(_)
        | ErrorCode::DestinationNotFound
        | ErrorCode::DestinationUnavailable
        | ErrorCode::DestinationIpProhibited
        | ErrorCode::DestinationIpUnroutable
        | ErrorCode::ConnectionRefused
        | ErrorCode::ConnectionTerminated => Error::Connect(format!("{e:?}")),
        other => Error::Other(format!("{other:?}")),
    }
}

/// The bindings' futures and streams are not `Send`. wasm32-wasip2 runs one
/// thread, so they never cross one, and the `Send` bound of `Http` holds.
struct SingleThreaded<T>(T);

// SAFETY: wasm32-wasip2 has a single thread; nothing is ever sent across.
unsafe impl<T> Send for SingleThreaded<T> {}

impl<F: Future> Future for SingleThreaded<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        // SAFETY: structural pinning of the only field.
        unsafe { self.map_unchecked_mut(|s| &mut s.0) }.poll(cx)
    }
}

impl<S: Stream> Stream for SingleThreaded<S> {
    type Item = S::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<S::Item>> {
        // SAFETY: structural pinning of the only field.
        unsafe { self.map_unchecked_mut(|s| &mut s.0) }.poll_next(cx)
    }
}
