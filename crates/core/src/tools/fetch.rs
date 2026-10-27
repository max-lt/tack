use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use http::header::{CONTENT_TYPE, HeaderName, HeaderValue, USER_AGENT};

use super::{ToolDef, ToolOutput};
use crate::http::{self as net, Http, Method, Request};

const TIMEOUT: Duration = Duration::from_secs(30);

pub struct FetchTool {
    http: Arc<dyn Http>,
}

impl FetchTool {
    pub fn new(http: Arc<dyn Http>) -> Self {
        Self { http }
    }
}

impl ToolDef for FetchTool {
    fn name(&self) -> &'static str {
        "Fetch"
    }

    fn description(&self) -> &'static str {
        "Make HTTP requests. Supports GET, POST, PUT, PATCH, DELETE with headers and body. \
         Returns status code, response headers, and body. \
         Use this instead of curl/wget via Bash."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The URL to fetch"
                },
                "method": {
                    "type": "string",
                    "enum": ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD"],
                    "description": "HTTP method (default: GET)"
                },
                "headers": {
                    "type": "object",
                    "description": "HTTP headers as key-value pairs",
                    "additionalProperties": { "type": "string" }
                },
                "body": {
                    "type": "string",
                    "description": "Request body (for POST/PUT/PATCH)"
                },
                "max_bytes": {
                    "type": "integer",
                    "description": "Max response body size in bytes (default: 1048576 = 1MB)"
                }
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, input: &serde_json::Value, _cwd: &Path) -> ToolOutput {
        let url = match input.get("url").and_then(|v| v.as_str()) {
            Some(u) => u,
            None => return ToolOutput::error("Missing required parameter: url"),
        };

        let method_str = input
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or("GET");

        let method = match method_str.to_uppercase().as_str() {
            "GET" => Method::GET,
            "POST" => Method::POST,
            "PUT" => Method::PUT,
            "PATCH" => Method::PATCH,
            "DELETE" => Method::DELETE,
            "HEAD" => Method::HEAD,
            other => return ToolOutput::error(format!("Unsupported HTTP method: {other}")),
        };

        let max_bytes = input
            .get("max_bytes")
            .and_then(|v| v.as_u64())
            .unwrap_or(1_048_576) as usize;

        let mut request = Request::new(method.clone(), url, TIMEOUT);
        request.redirects = crate::http::Redirects::Follow;

        // Default User-Agent mimics Firefox to avoid bot-blocking
        request.headers.insert(
            USER_AGENT,
            HeaderValue::from_static(
                "Mozilla/5.0 (X11; Linux x86_64; rv:133.0) Gecko/20100101 Firefox/133.0",
            ),
        );

        // Custom headers
        if let Some(headers_obj) = input.get("headers").and_then(|v| v.as_object()) {
            for (key, val) in headers_obj {
                let name = match key.parse::<HeaderName>() {
                    Ok(n) => n,
                    Err(e) => {
                        return ToolOutput::error(format!("Invalid header name '{key}': {e}"));
                    }
                };
                let value = match val.as_str() {
                    Some(v) => match v.parse::<HeaderValue>() {
                        Ok(hv) => hv,
                        Err(e) => {
                            return ToolOutput::error(format!(
                                "Invalid header value for '{key}': {e}"
                            ));
                        }
                    },
                    None => continue,
                };
                request.headers.insert(name, value);
            }
        }

        // Body
        if let Some(body) = input.get("body").and_then(|v| v.as_str()) {
            request.body = body.as_bytes().to_vec();
        }

        // Execute
        let response = match self.http.send(request).await {
            Ok(r) => r,
            Err(net::Error::Timeout) => {
                return ToolOutput::error(format!("Request timed out: {url}"));
            }
            Err(net::Error::Connect(_)) => {
                return ToolOutput::error(format!("Connection failed: {url}"));
            }
            Err(e) => return ToolOutput::error(format!("HTTP request failed: {e}")),
        };

        let status = response.status;
        let status_line = format!(
            "{} {}",
            status.as_u16(),
            status.canonical_reason().unwrap_or("")
        );

        // Collect response headers
        let mut resp_headers = String::new();
        for (name, value) in &response.headers {
            if let Ok(v) = value.to_str() {
                resp_headers.push_str(&format!("{name}: {v}\n"));
            }
        }

        let content_type = response
            .headers
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        // HEAD requests: no body
        if method == Method::HEAD {
            return ToolOutput::success(format!("HTTP {status_line}\n\n{resp_headers}"));
        }

        // Read body with size limit
        let body_bytes = match response.bytes().await {
            Ok(b) => b,
            Err(e) => return ToolOutput::error(format!("Failed to read response body: {e}")),
        };

        let truncated = body_bytes.len() > max_bytes;
        let body_slice = if truncated {
            &body_bytes[..max_bytes]
        } else {
            &body_bytes[..]
        };

        // Determine if binary
        let is_binary = !content_type.contains("text")
            && !content_type.contains("json")
            && !content_type.contains("xml")
            && !content_type.contains("javascript")
            && !content_type.contains("html")
            && !content_type.contains("css")
            && !content_type.contains("svg")
            && body_slice.contains(&0);

        let body_text = if is_binary {
            format!("<binary data, {} bytes>", body_bytes.len())
        } else {
            let text = String::from_utf8_lossy(body_slice).to_string();
            if truncated {
                format!(
                    "{text}\n\n... truncated ({} bytes total, showing first {max_bytes})",
                    body_bytes.len()
                )
            } else {
                text
            }
        };

        let output = format!("HTTP {status_line}\n\n{resp_headers}\n{body_text}");
        ToolOutput::success(output)
    }
}
