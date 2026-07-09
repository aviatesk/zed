use crate::{AgentTool, ToolCallEventStream, ToolInput};
use agent_client_protocol::schema::v1 as acp;
use futures::FutureExt as _;
use gpui::{App, Entity, Task};
use project::Project;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use ui::SharedString;
use util::markdown::MarkdownInlineCode;

type Result<T, E = String> = core::result::Result<T, E>;

/// Get errors and warnings for the project or a specific file.
///
/// This tool can be invoked after a series of edits to determine if further edits are necessary, or if the user asks to fix errors or warnings in their codebase.
///
/// When a path is provided, shows all diagnostics for that specific file.
/// When no path is provided, shows a summary of error and warning counts for all files in the project.
///
/// This tool attempts to refresh diagnostics before returning.
/// If refreshing diagnostics fails (for example, if the language server does not support pull-based diagnostics), it will return any diagnostics already present.
/// Note that, in this case, the results may be out-of-date, and may or may not reflect the most recent edits.
/// If this happens, do not attempt to re-run this tool in the hope that refreshing will later succeed. Failures are typically persistent.
///
/// <example>
/// To get diagnostics for a specific file:
/// {
///     "path": "src/main.rs"
/// }
///
/// To get a project-wide diagnostic summary:
/// {}
/// </example>
///
/// <guidelines>
/// - If you think you can fix a diagnostic, make 1-2 attempts and then give up.
/// - Don't remove code you've generated just because you can't fix an error. The user can help you fix it.
/// </guidelines>
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct DiagnosticsToolInput {
    /// The path to get diagnostics for. If not provided, returns a project-wide summary.
    ///
    /// This path should never be absolute, and the first component
    /// of the path should always be a root directory in a project.
    ///
    /// <example>
    /// If the project has the following root directories:
    ///
    /// - lorem
    /// - ipsum
    ///
    /// If you wanna access diagnostics for `dolor.txt` in `ipsum`, you should use the path `ipsum/dolor.txt`.
    /// </example>
    pub path: Option<String>,

    /// Minimum severity to report for a per-file query: "error", "warning", "information",
    /// or "hint". Defaults to "warning". Lower it to also surface information/hint-level lints
    /// (e.g. an unused argument in a pure internal helper). Ignored for the project-wide summary.
    pub min_severity: Option<String>,
}

pub struct DiagnosticsTool {
    project: Entity<Project>,
}

impl DiagnosticsTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl AgentTool for DiagnosticsTool {
    type Input = DiagnosticsToolInput;
    type Output = String;

    const NAME: &'static str = "diagnostics";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Read
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        if let Some(path) = input.ok().and_then(|input| match input.path {
            Some(path) if !path.is_empty() => Some(path),
            _ => None,
        }) {
            format!("Check diagnostics for {}", MarkdownInlineCode(&path)).into()
        } else {
            "Check project diagnostics".into()
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let project = self.project.clone();
        cx.spawn(async move |cx| {
            let input = input.recv().await.map_err(|e| e.to_string())?;

            futures::select! {
                result = agent_lsp::diagnostics(project, input.path, input.min_severity, cx).fuse() => result,
                _ = event_stream.cancelled_by_user().fuse() => {
                    Err("Diagnostics cancelled by user".to_string())
                }
            }
        })
    }
}
