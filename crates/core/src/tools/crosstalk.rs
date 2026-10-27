use std::path::Path;
use std::sync::Arc;

use super::{ToolDef, ToolOutput};
use crate::crosstalk::Client;

/// The model-facing crosstalk tools. `accept`, `add-hub` and `forget` of
/// every peer stay user-only: they are TUI commands.
pub fn tools(client: Arc<Client>) -> (WhoamiTool, PeersTool, SendTool, ConnectTool, ForgetTool) {
    (
        WhoamiTool(client.clone()),
        PeersTool(client.clone()),
        SendTool(client.clone()),
        ConnectTool(client.clone()),
        ForgetTool(client),
    )
}

pub struct WhoamiTool(Arc<Client>);
pub struct PeersTool(Arc<Client>);
pub struct SendTool(Arc<Client>);
pub struct ConnectTool(Arc<Client>);
pub struct ForgetTool(Arc<Client>);

fn str_arg<'a>(input: &'a serde_json::Value, key: &str) -> Result<&'a str, ToolOutput> {
    input
        .get(key)
        .and_then(|v| v.as_str())
        .filter(|v| !v.is_empty())
        .ok_or_else(|| ToolOutput::error(format!("Missing required parameter: {key}")))
}

async fn run(client: &Client, cmd: &str, args: &[&str]) -> ToolOutput {
    match client.request(cmd, args).await {
        Ok(reply) => ToolOutput::success(reply),
        Err(e) => ToolOutput::error(e.to_string()),
    }
}

fn peer_schema(extra: serde_json::Value) -> serde_json::Value {
    let mut properties = serde_json::json!({
        "peer": { "type": "string", "description": "Peer id (crosstalk_...)" }
    });
    let mut required = vec!["peer".to_string()];

    if let Some(extra) = extra.as_object() {
        for (key, value) in extra {
            properties[key] = value.clone();
            required.push(key.clone());
        }
    }

    serde_json::json!({ "type": "object", "properties": properties, "required": required })
}

impl ToolDef for WhoamiTool {
    fn name(&self) -> &'static str {
        "CrosstalkWhoami"
    }

    fn description(&self) -> &'static str {
        "Show this session's crosstalk id and hub connections."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _input: &serde_json::Value, _cwd: &Path) -> ToolOutput {
        run(&self.0, "whoami", &[]).await
    }
}

impl ToolDef for PeersTool {
    fn name(&self) -> &'static str {
        "CrosstalkPeers"
    }

    fn description(&self) -> &'static str {
        "List crosstalk peers and their pairing state."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _input: &serde_json::Value, _cwd: &Path) -> ToolOutput {
        run(&self.0, "peers", &[]).await
    }
}

impl ToolDef for SendTool {
    fn name(&self) -> &'static str {
        "CrosstalkSend"
    }

    fn description(&self) -> &'static str {
        "Send a message to a paired crosstalk peer: another person's agent. The hub keeps \
         no queue; an offline peer does not get the message. Max 16000 characters."
    }

    fn input_schema(&self) -> serde_json::Value {
        peer_schema(serde_json::json!({
            "text": { "type": "string", "description": "Message text" }
        }))
    }

    async fn execute(&self, input: &serde_json::Value, _cwd: &Path) -> ToolOutput {
        let (peer, text) = match (str_arg(input, "peer"), str_arg(input, "text")) {
            (Ok(peer), Ok(text)) => (peer, text),
            (Err(e), _) | (_, Err(e)) => return e,
        };

        run(&self.0, "send", &[peer, text]).await
    }
}

impl ToolDef for ConnectTool {
    fn name(&self) -> &'static str {
        "CrosstalkConnect"
    }

    fn description(&self) -> &'static str {
        "Ask another crosstalk session to pair. Its user must accept. The peer is an id, or \
         host:id for another hub."
    }

    fn input_schema(&self) -> serde_json::Value {
        peer_schema(serde_json::json!({
            "description": {
                "type": "string",
                "description": "Who this session is, shown to the other user (max 140 characters)"
            }
        }))
    }

    async fn execute(&self, input: &serde_json::Value, _cwd: &Path) -> ToolOutput {
        let (peer, description) = match (str_arg(input, "peer"), str_arg(input, "description")) {
            (Ok(peer), Ok(description)) => (peer, description),
            (Err(e), _) | (_, Err(e)) => return e,
        };

        run(&self.0, "connect", &[peer, description]).await
    }
}

impl ToolDef for ForgetTool {
    fn name(&self) -> &'static str {
        "CrosstalkForget"
    }

    fn description(&self) -> &'static str {
        "Drop one crosstalk peer."
    }

    fn input_schema(&self) -> serde_json::Value {
        peer_schema(serde_json::json!({}))
    }

    async fn execute(&self, input: &serde_json::Value, _cwd: &Path) -> ToolOutput {
        // Without an id the daemon drops every peer and the identity.
        match str_arg(input, "peer") {
            Ok(peer) => run(&self.0, "forget", &[peer]).await,
            Err(e) => e,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_tool_lets_the_model_accept_or_add_a_hub() {
        let client = Arc::new(Client::at("test", std::path::PathBuf::from("/nowhere.sock")));
        let (whoami, peers, send, connect, forget) = tools(client);
        let names = [
            ToolDef::name(&whoami),
            ToolDef::name(&peers),
            ToolDef::name(&send),
            ToolDef::name(&connect),
            ToolDef::name(&forget),
        ];

        assert_eq!(
            names,
            ["CrosstalkWhoami", "CrosstalkPeers", "CrosstalkSend", "CrosstalkConnect", "CrosstalkForget"]
        );
    }
}
