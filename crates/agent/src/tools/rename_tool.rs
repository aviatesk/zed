use std::sync::Arc;

use agent_client_protocol::schema::v1 as acp;
use gpui::{App, Entity, SharedString, Task};
use language::LanguageRegistry;
use project::Project;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{LspEditToolOutput, symbol_locator::SymbolLocator};
use crate::{AgentTool, ToolCallEventStream, ToolInput};

/// Renames a symbol across the project using the language server.
///
/// This performs a semantic rename, updating all references to the symbol across all files in the project. The language server determines which occurrences to rename based on the symbol's type and scope.
///
/// Before using this tool, use read_file or grep to find the exact symbol name and line number.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct RenameToolInput {
    /// The symbol to rename.
    pub symbol: SymbolLocator,

    /// The new name for the symbol.
    pub new_name: String,
}

pub struct RenameTool {
    project: Entity<Project>,
    language_registry: Arc<LanguageRegistry>,
}

impl RenameTool {
    pub fn new(project: Entity<Project>, language_registry: Arc<LanguageRegistry>) -> Self {
        Self {
            project,
            language_registry,
        }
    }
}

impl AgentTool for RenameTool {
    type Input = RenameToolInput;
    type Output = LspEditToolOutput;

    const NAME: &'static str = "rename_symbol";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Other
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        if let Ok(input) = input {
            format!(
                "Rename `{}` to `{}`",
                input.symbol.symbol_name, input.new_name
            )
            .into()
        } else {
            "Rename symbol".into()
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let project = self.project.clone();
        let language_registry = self.language_registry.clone();
        cx.spawn(async move |cx| {
            let input = input.recv().await.map_err(|e| {
                LspEditToolOutput::Text(format!("Failed to receive tool input: {e}"))
            })?;

            let output = agent_lsp::rename_symbol(
                project,
                agent_lsp::SymbolLocator::new(
                    input.symbol.file_path,
                    input.symbol.line,
                    input.symbol.symbol_name,
                ),
                input.new_name,
                cx,
            )
            .await
            .map(LspEditToolOutput::from_agent_lsp)
            .map_err(LspEditToolOutput::Text)?;

            cx.update(|cx| output.emit_diffs(&event_stream, language_registry, cx));
            Ok(output)
        })
    }

    fn replay(
        &self,
        _input: Self::Input,
        output: Self::Output,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> anyhow::Result<()> {
        output.emit_diffs(&event_stream, self.language_registry.clone(), cx);
        Ok(())
    }
}
