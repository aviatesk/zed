use std::sync::Arc;

use super::symbol_locator::SymbolLocator;
use crate::{AgentTool, ToolCallEventStream, ToolInput};
use agent_client_protocol::schema::v1 as acp;
use gpui::{App, Entity, SharedString, Task};
use project::Project;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Gets hover information (type, signature, documentation) for a symbol using the language server.
///
/// Before using this tool, use read_file or grep to find the exact symbol name and line number.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct HoverToolInput {
    /// The symbol to request hover information for.
    pub symbol: SymbolLocator,
}

pub struct HoverTool {
    project: Entity<Project>,
}

impl HoverTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl AgentTool for HoverTool {
    type Input = HoverToolInput;
    type Output = String;

    const NAME: &'static str = "hover";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Read
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        if let Ok(input) = input {
            format!("Get hover info for `{}`", input.symbol.symbol_name).into()
        } else {
            "Get hover info".into()
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
            agent_lsp::hover(
                project,
                agent_lsp::SymbolLocator::new(
                    input.symbol.file_path,
                    input.symbol.line,
                    input.symbol.symbol_name,
                ),
                cx,
            )
            .await
        })
    }
}
