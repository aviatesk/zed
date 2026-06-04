use std::sync::Arc;

use crate::{AgentTool, ToolCallEventStream, ToolInput};
use agent_client_protocol::schema::v1 as acp;
use gpui::{App, Entity, SharedString, Task};
use project::Project;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Search for symbols (functions, types, variables) across the workspace using the language server.
///
/// Results are client-filtered and capped to avoid excessive output from language servers that ignore the query.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct WorkspaceSymbolToolInput {
    /// Search query, typically a non-empty substring of the symbol name.
    pub query: String,
}

pub struct WorkspaceSymbolTool {
    project: Entity<Project>,
}

impl WorkspaceSymbolTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl AgentTool for WorkspaceSymbolTool {
    type Input = WorkspaceSymbolToolInput;
    type Output = String;

    const NAME: &'static str = "workspace_symbol";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Search
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        if let Ok(input) = input {
            format!("Search workspace symbols for `{}`", input.query).into()
        } else {
            "Search workspace symbols".into()
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
            agent_lsp::workspace_symbol(project, input.query, cx).await
        })
    }
}
