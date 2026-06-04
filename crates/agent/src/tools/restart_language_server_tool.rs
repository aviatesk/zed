use std::sync::Arc;

use crate::{AgentTool, ToolCallEventStream, ToolInput};
use agent_client_protocol::schema::v1 as acp;
use gpui::{App, Entity, SharedString, Task};
use project::Project;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Restart language server(s) with the given name across the project.
///
/// Use sparingly. Restarting is expensive and IDE features for that language may be temporarily unavailable while the server re-indexes. Call list_language_servers first to get the exact server name, and provide a short reason.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct RestartLanguageServerToolInput {
    /// Language server name as reported by list_language_servers, e.g. "rust-analyzer".
    pub name: String,
    /// Short justification visible in the tool result and logs.
    pub reason: String,
}

pub struct RestartLanguageServerTool {
    project: Entity<Project>,
}

impl RestartLanguageServerTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl AgentTool for RestartLanguageServerTool {
    type Input = RestartLanguageServerToolInput;
    type Output = String;

    const NAME: &'static str = "restart_language_server";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Other
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        if let Ok(input) = input {
            format!("Restart language server `{}`", input.name).into()
        } else {
            "Restart language server".into()
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let project = self.project.clone();
        cx.spawn(async move |cx| {
            let input = input
                .recv()
                .await
                .map_err(|error| format!("Failed to receive tool input: {error}"))?;
            agent_lsp::restart_language_server(project, input.name, input.reason, cx).await
        })
    }
}
