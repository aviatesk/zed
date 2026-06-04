use std::sync::Arc;

use acp_thread::Diff;
use gpui::{App, AppContext as _};
use language::LanguageRegistry;
use language_model::LanguageModelToolResultContent;
use serde::{Deserialize, Serialize};

use crate::ToolCallEventStream;

#[derive(Debug, Serialize, Deserialize)]
pub enum LspEditToolOutput {
    Text(String),
    Edited {
        text: String,
        diffs: Vec<LspEditDiff>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LspEditDiff {
    pub path: String,
    pub old_text: String,
    pub new_text: String,
}

impl LspEditToolOutput {
    pub fn from_agent_lsp(output: agent_lsp::EditOperationOutput) -> Self {
        match output {
            agent_lsp::EditOperationOutput::Text(text) => Self::Text(text),
            agent_lsp::EditOperationOutput::Edited(output) => Self::Edited {
                text: output.text,
                diffs: output
                    .diffs
                    .into_iter()
                    .map(|diff| LspEditDiff {
                        path: diff.path,
                        old_text: diff.old_text,
                        new_text: diff.new_text,
                    })
                    .collect(),
            },
        }
    }

    pub fn text(&self) -> &str {
        match self {
            Self::Text(text) => text,
            Self::Edited { text, .. } => text,
        }
    }

    pub fn emit_diffs(
        &self,
        event_stream: &ToolCallEventStream,
        language_registry: Arc<LanguageRegistry>,
        cx: &mut App,
    ) {
        let Self::Edited { diffs, .. } = self else {
            return;
        };

        for diff in diffs {
            event_stream.update_diff(cx.new(|cx| {
                Diff::finalized(
                    diff.path.clone(),
                    Some(diff.old_text.clone()),
                    diff.new_text.clone(),
                    language_registry.clone(),
                    cx,
                )
            }));
        }
    }
}

impl From<LspEditToolOutput> for LanguageModelToolResultContent {
    fn from(output: LspEditToolOutput) -> Self {
        output.text().to_string().into()
    }
}
