#[cfg(not(target_arch = "wasm32"))]
pub mod bash;
pub mod edit;
pub mod fetch;
#[cfg(feature = "git")]
pub mod git;
pub mod glob;
pub mod grep;
pub mod list;
pub mod read;
#[cfg(feature = "search")]
pub mod search;
pub mod write;

use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use crate::capabilities::Capabilities;
use crate::permission;
use crate::sandbox::FileChange;

// ---------------------------------------------------------------------------
// Tool output
// ---------------------------------------------------------------------------

pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    /// Files a sandboxed command changed. The session asks for permission
    /// before it writes them to the workspace.
    pub changes: Vec<FileChange>,
}

impl ToolOutput {
    pub fn success(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            changes: Vec::new(),
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            changes: Vec::new(),
        }
    }

    pub fn with_changes(mut self, changes: Vec<FileChange>) -> Self {
        self.changes = changes;
        self
    }
}

// ---------------------------------------------------------------------------
// ToolDef — the user-facing trait (uses async fn directly)
// ---------------------------------------------------------------------------

pub trait ToolDef: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn input_schema(&self) -> serde_json::Value;
    fn execute(
        &self,
        input: &serde_json::Value,
        cwd: &Path,
    ) -> impl Future<Output = ToolOutput> + Send;
}

// ---------------------------------------------------------------------------
// ToolDefDyn — object-safe wrapper for dyn dispatch
// ---------------------------------------------------------------------------

pub trait ToolDefDyn: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn input_schema(&self) -> serde_json::Value;
    fn execute_dyn<'a>(
        &'a self,
        input: &'a serde_json::Value,
        cwd: &'a Path,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>>;
}

impl<T: ToolDef> ToolDefDyn for T {
    fn name(&self) -> &'static str {
        ToolDef::name(self)
    }

    fn description(&self) -> &'static str {
        ToolDef::description(self)
    }

    fn input_schema(&self) -> serde_json::Value {
        ToolDef::input_schema(self)
    }

    fn execute_dyn<'a>(
        &'a self,
        input: &'a serde_json::Value,
        cwd: &'a Path,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
        Box::pin(ToolDef::execute(self, input, cwd))
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

pub struct ToolRegistry {
    tools: Vec<Box<dyn ToolDefDyn>>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    pub fn register(&mut self, tool: impl ToolDef + 'static) {
        self.tools.push(Box::new(tool));
    }

    /// Return tool definitions formatted for the Claude API `tools` parameter.
    pub fn api_definitions(&self) -> Vec<serde_json::Value> {
        self.tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.name(),
                    "description": t.description(),
                    "input_schema": t.input_schema(),
                })
            })
            .collect()
    }

    pub fn get(&self, name: &str) -> Option<&dyn ToolDefDyn> {
        self.tools
            .iter()
            .find(|t| t.name() == name)
            .map(|t| t.as_ref())
    }
}

/// Create a registry with the default set of tools, bound to `caps`.
pub fn default_registry(caps: &Capabilities) -> ToolRegistry {
    let mut r = ToolRegistry::new();

    // Bash needs a real OS: a wasm guest has no processes to spawn.
    #[cfg(not(target_arch = "wasm32"))]
    r.register(bash::BashTool::new(caps.sandbox.clone()));

    r.register(read::ReadTool::new(caps.fs.clone()));
    r.register(write::WriteTool::new(caps.fs.clone()));
    r.register(edit::EditTool::new(caps.fs.clone()));
    r.register(glob::GlobTool);
    r.register(grep::GrepTool);
    r.register(list::ListTool::new(caps.fs.clone()));
    r.register(fetch::FetchTool::new(caps.http.clone()));

    #[cfg(feature = "git")]
    r.register(git::GitTool);

    #[cfg(feature = "search")]
    r.register(search::SearchTool::new());

    r
}

// ---------------------------------------------------------------------------
// Permission mapping
// ---------------------------------------------------------------------------

/// Map an API tool call to the core permission system.
pub fn to_permission_tool<'a>(
    name: &str,
    input: &'a serde_json::Value,
) -> Option<permission::Tool<'a>> {
    match name {
        "Bash" => {
            let command = input.get("command").and_then(|c| c.as_str()).unwrap_or("");
            Some(permission::Tool::Bash { command })
        }
        "Read" => {
            let path = input
                .get("file_path")
                .and_then(|p| p.as_str())
                .unwrap_or("");

            Some(permission::Tool::Read {
                path: Path::new(path),
            })
        }
        "Write" => {
            let path = input
                .get("file_path")
                .and_then(|p| p.as_str())
                .unwrap_or("");

            Some(permission::Tool::Write {
                path: Path::new(path),
            })
        }
        "Edit" => {
            let path = input
                .get("file_path")
                .and_then(|p| p.as_str())
                .unwrap_or("");

            Some(permission::Tool::Edit {
                path: Path::new(path),
            })
        }
        "Fetch" => {
            let url = input.get("url").and_then(|u| u.as_str()).unwrap_or("");
            let method = input
                .get("method")
                .and_then(|m| m.as_str())
                .unwrap_or("GET");
            Some(permission::Tool::Fetch { url, method })
        }
        "Glob" => Some(permission::Tool::Glob),
        "Grep" => Some(permission::Tool::Grep),
        "List" => Some(permission::Tool::List),
        "Git" => {
            let subcommand = input
                .get("subcommand")
                .and_then(|s| s.as_str())
                .unwrap_or("");
            Some(permission::Tool::Git { subcommand })
        }
        "Search" => Some(permission::Tool::Search),
        _ => None,
    }
}
