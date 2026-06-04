use std::sync::Arc;

use crate::{AgentTool, ToolCallEventStream, ToolInput};
use agent_client_protocol::schema::v1 as acp;
use gpui::{App, Entity, SharedString, Task};
use project::Project;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// List the language servers currently registered for this Zed project, including name, worktree, and running status.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ListLanguageServersToolInput {}

pub struct ListLanguageServersTool {
    project: Entity<Project>,
}

impl ListLanguageServersTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl AgentTool for ListLanguageServersTool {
    type Input = ListLanguageServersToolInput;
    type Output = String;

    const NAME: &'static str = "list_language_servers";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Read
    }

    fn initial_title(
        &self,
        _input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        "List language servers".into()
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let project = self.project.clone();
        cx.spawn(async move |cx| {
            input
                .recv()
                .await
                .map_err(|error| format!("Failed to receive tool input: {error}"))?;
            agent_lsp::list_language_servers(project, cx).await
        })
    }
}
