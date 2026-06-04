use std::sync::Arc;

use agent_client_protocol::schema::v1 as acp;
use gpui::{App, Entity, SharedString, Task};
use project::Project;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::symbol_locator::{CodeActionStore, SymbolLocator};
use crate::{AgentTool, ToolCallEventStream, ToolInput};

/// Gets the list of available code actions at a symbol location from the language server.
///
/// Code actions include quick fixes, refactorings, and other automated transformations suggested by the language server (e.g. "Add missing import", "Extract to function").
///
/// Returns a numbered list of available actions. Use apply_code_action with the corresponding number to apply one.
///
/// Before using this tool, use read_file or grep to find the exact symbol name and line number.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct GetCodeActionsToolInput {
    /// The symbol to get code actions for.
    pub symbol: SymbolLocator,
}

pub struct GetCodeActionsTool {
    project: Entity<Project>,
    code_action_store: CodeActionStore,
}

impl GetCodeActionsTool {
    pub fn new(project: Entity<Project>, code_action_store: CodeActionStore) -> Self {
        Self {
            project,
            code_action_store,
        }
    }
}

impl AgentTool for GetCodeActionsTool {
    type Input = GetCodeActionsToolInput;
    type Output = String;

    const NAME: &'static str = "get_code_actions";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Search
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        if let Ok(input) = input {
            format!("Get code actions for `{}`", input.symbol.symbol_name).into()
        } else {
            "Get code actions".into()
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let project = self.project.clone();
        let store = self.code_action_store.clone();
        cx.spawn(async move |cx| {
            let input = input
                .recv()
                .await
                .map_err(|e| format!("Failed to receive tool input: {e}"))?;

            let symbol = agent_lsp::SymbolLocator::new(
                input.symbol.file_path,
                input.symbol.line,
                input.symbol.symbol_name,
            );
            let output =
                agent_lsp::get_code_actions(project, symbol, "apply_code_action", cx).await?;

            store.update(cx, |store, _cx| {
                *store = output.pending;
            });

            Ok(output.text)
        })
    }
}
