use std::sync::Arc;

use agent_client_protocol::schema::v1 as acp;
use gpui::{App, Entity, SharedString, Task};
use language::LanguageRegistry;
use project::Project;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{LspEditToolOutput, symbol_locator::CodeActionStore};
use crate::{AgentTool, ToolCallEventStream, ToolInput};

/// Applies a code action previously retrieved by get_code_actions.
///
/// You must call get_code_actions first to get the list of available actions,
/// then use the number from that list to choose which action to apply.
///
/// After applying a code action, the list is cleared. If you want to apply
/// another action, call get_code_actions again.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ApplyCodeActionToolInput {
    /// The 1-based index of the code action to apply, from the list
    /// returned by get_code_actions.
    pub index: u32,
}

pub struct ApplyCodeActionTool {
    project: Entity<Project>,
    code_action_store: CodeActionStore,
    language_registry: Arc<LanguageRegistry>,
}

impl ApplyCodeActionTool {
    pub fn new(
        project: Entity<Project>,
        code_action_store: CodeActionStore,
        language_registry: Arc<LanguageRegistry>,
    ) -> Self {
        Self {
            project,
            code_action_store,
            language_registry,
        }
    }
}

impl AgentTool for ApplyCodeActionTool {
    type Input = ApplyCodeActionToolInput;
    type Output = LspEditToolOutput;

    const NAME: &'static str = "apply_code_action";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Other
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        cx: &mut App,
    ) -> SharedString {
        if let Ok(input) = input {
            let title = self
                .code_action_store
                .read(cx)
                .as_ref()
                .and_then(|pending| {
                    let index = input.index.checked_sub(1)? as usize;
                    Some(pending.actions.get(index)?.lsp_action.title().to_string())
                });
            if let Some(title) = title {
                format!("Apply code action: {title}").into()
            } else {
                format!("Apply code action #{}", input.index).into()
            }
        } else {
            "Apply code action".into()
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let project = self.project.clone();
        let store = self.code_action_store.clone();
        let language_registry = self.language_registry.clone();
        cx.spawn(async move |cx| {
            let input = input.recv().await.map_err(|e| {
                LspEditToolOutput::Text(format!("Failed to receive tool input: {e}"))
            })?;

            let pending = store.update(cx, |store, _cx| store.take()).ok_or_else(|| {
                LspEditToolOutput::Text(
                    "No code actions available. Call get_code_actions first.".to_string(),
                )
            })?;

            let output = agent_lsp::apply_code_action(
                project,
                event_stream.lsp_buffer_lease(),
                input.index,
                pending,
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
