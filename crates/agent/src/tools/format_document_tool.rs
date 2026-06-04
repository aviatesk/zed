use std::sync::Arc;

use crate::{AgentTool, ToolCallEventStream, ToolInput};
use agent_client_protocol::schema::v1 as acp;
use gpui::{App, Entity, SharedString, Task};
use language::LanguageRegistry;
use project::Project;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use util::markdown::MarkdownInlineCode;

use super::LspEditToolOutput;

/// Format a project file using the formatter Zed has configured for this language.
///
/// The formatted buffer is saved to disk through Zed's format-on-save pipeline.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct FormatDocumentToolInput {
    /// Project-relative path of the file to format.
    pub file_path: String,
}

pub struct FormatDocumentTool {
    project: Entity<Project>,
    language_registry: Arc<LanguageRegistry>,
}

impl FormatDocumentTool {
    pub fn new(project: Entity<Project>, language_registry: Arc<LanguageRegistry>) -> Self {
        Self {
            project,
            language_registry,
        }
    }
}

impl AgentTool for FormatDocumentTool {
    type Input = FormatDocumentToolInput;
    type Output = LspEditToolOutput;

    const NAME: &'static str = "format_document";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Edit
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        if let Ok(input) = input {
            format!("Format {}", MarkdownInlineCode(&input.file_path)).into()
        } else {
            "Format document".into()
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
            let input = input.recv().await.map_err(|error| {
                LspEditToolOutput::Text(format!("Failed to receive tool input: {error}"))
            })?;
            let output = agent_lsp::format_document(project, input.file_path, cx)
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
