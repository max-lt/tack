use std::sync::Arc;

use crate::fs::Fs;
use crate::http::Http;

/// What a session may use outside its own memory. The tools and the model
/// API client reach the world only through these handles.
#[derive(Clone)]
pub struct Capabilities {
    pub fs: Arc<dyn Fs>,
    pub http: Arc<dyn Http>,
}

impl Capabilities {
    /// The process's own disk and network.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn local() -> Self {
        Self {
            fs: Arc::new(crate::fs::LocalFs),
            http: Arc::new(crate::http::LocalHttp::new()),
        }
    }
}
