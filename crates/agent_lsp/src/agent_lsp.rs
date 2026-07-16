use std::cell::RefCell;
use std::collections::VecDeque;
use std::fmt::{self, Write as _};
use std::rc::Rc;
use std::time::Duration;

use collections::{HashMap, HashSet};
use gpui::{App, AsyncApp, Entity};
use language::{Buffer, BufferId, DiagnosticSeverity, Location, OffsetRangeExt as _};
use lsp::{LanguageServerName, LanguageServerSelector};
use project::lsp_store::{FormatTrigger, LspFormatTarget, OpenLspBufferHandle, SymbolLocation};
use project::{CodeAction, HoverBlockKind, Project, Symbol};
use serde::{Deserialize, Serialize};
use text::{Anchor, Point, ToPoint as _, ToPointUtf16 as _};
use util::paths::PathStyle;

pub const MAX_LINE_DISPLAY_LEN: usize = 200;
const MAX_WORKSPACE_SYMBOL_RESULTS: usize = 100;

#[derive(Clone, Default)]
pub struct LspBufferLease {
    handles: Rc<RefCell<HashMap<BufferId, OpenLspBufferHandle>>>,
}

impl LspBufferLease {
    pub fn acquire(&self, project: &Entity<Project>, buffer: &Entity<Buffer>, cx: &mut AsyncApp) {
        let buffer_id = buffer.read_with(cx, |buffer, _cx| buffer.remote_id());
        if self.handles.borrow().contains_key(&buffer_id) {
            return;
        }

        let handle = project.update(cx, |project, cx| {
            project.register_buffer_with_language_servers(buffer, cx)
        });
        self.handles.borrow_mut().insert(buffer_id, handle);
    }

    pub fn release(&self, cx: &mut AsyncApp) {
        cx.update(|_cx| self.handles.borrow_mut().clear());
    }

    pub fn release_after(self, delay: Duration, cx: &AsyncApp) {
        if self.handles.borrow().is_empty() {
            return;
        }

        let timer = cx.background_executor().timer(delay);
        cx.spawn(async move |cx| {
            timer.await;
            self.release(cx);
        })
        .detach();
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SymbolLocator {
    pub file_path: String,
    pub line: u32,
    pub symbol_name: String,
}

impl SymbolLocator {
    pub fn new(file_path: String, line: u32, symbol_name: String) -> Self {
        Self {
            file_path,
            line,
            symbol_name,
        }
    }

    pub async fn resolve(
        &self,
        project: &Entity<Project>,
        lease: &LspBufferLease,
        cx: &mut AsyncApp,
    ) -> Result<ResolvedSymbol, String> {
        let Self {
            file_path,
            line,
            symbol_name,
        } = self;

        let open_buffer_task = project.update(cx, |project, cx| {
            let Some(project_path) = project.find_project_path(file_path, cx) else {
                return Err(format!("Could not find path '{file_path}' in project"));
            };
            Ok(project.open_buffer(project_path, cx))
        })?;

        let buffer = open_buffer_task
            .await
            .map_err(|error| format!("Failed to open '{}': {error}", self.file_path))?;
        lease.acquire(project, &buffer, cx);

        let (position, line_text, truncated) = buffer.read_with(cx, |buffer, _cx| {
            let snapshot = buffer.snapshot();
            let row = line.saturating_sub(1);

            if row > snapshot.max_point().row {
                let line_count = snapshot.max_point().row + 1;
                return Err(format!(
                    "Line {line} is beyond the end of '{file_path}' (file has {line_count} lines)",
                ));
            }

            let line_len = snapshot.line_len(row);
            let truncated = line_len as usize > MAX_LINE_DISPLAY_LEN;
            let line_start = Point::new(row, 0);
            let line_end = Point::new(row, line_len);
            let line_chars = || {
                snapshot
                    .text_for_range(line_start..line_end)
                    .flat_map(|chunk| chunk.chars())
            };

            let byte_offset = find_in_char_iter(line_chars(), symbol_name).ok_or_else(|| {
                let preview: String = line_chars()
                    .skip_while(|character| character.is_whitespace())
                    .take(MAX_LINE_DISPLAY_LEN)
                    .collect();
                format!(
                    "Could not find symbol '{symbol_name}' on line {line} of '{file_path}'. Line content: {preview}"
                )
            })?;

            let symbol_start = snapshot.anchor_before(Point::new(row, byte_offset as u32));
            let line_text: String = line_chars()
                .skip_while(|character| character.is_whitespace())
                .take(MAX_LINE_DISPLAY_LEN)
                .collect::<String>()
                .trim_end()
                .to_string();

            Ok((symbol_start, line_text, truncated))
        })?;

        Ok(ResolvedSymbol {
            buffer,
            position,
            line_text,
            truncated,
        })
    }
}

pub struct ResolvedSymbol {
    pub buffer: Entity<Buffer>,
    pub position: Anchor,
    pub line_text: String,
    pub truncated: bool,
}

pub struct PendingCodeActions {
    pub actions: Vec<CodeAction>,
    pub buffer: Entity<Buffer>,
}

pub struct CodeActionsOutput {
    pub text: String,
    pub pending: Option<PendingCodeActions>,
}

#[derive(Clone, Debug)]
pub struct EditDiff {
    pub path: String,
    pub abs_path: String,
    pub line: Option<u32>,
    pub old_text: String,
    pub new_text: String,
}

#[derive(Clone, Debug)]
pub struct EditOutput {
    pub text: String,
    pub diffs: Vec<EditDiff>,
}

#[derive(Clone, Debug)]
pub enum EditOperationOutput {
    Text(String),
    Edited(EditOutput),
}

impl EditOperationOutput {
    pub fn text(&self) -> &str {
        match self {
            Self::Text(text) => text,
            Self::Edited(output) => &output.text,
        }
    }
}

fn snapshot_open_buffer_texts(
    project: &Entity<Project>,
    cx: &mut AsyncApp,
) -> HashMap<BufferId, String> {
    project.read_with(cx, |project, cx| {
        project
            .buffer_store()
            .read(cx)
            .buffers()
            .map(|buffer| {
                let buffer = buffer.read(cx);
                (buffer.remote_id(), buffer.text())
            })
            .collect::<HashMap<_, _>>()
    })
}

fn build_diffs_from_touched_buffers<'a>(
    touched: impl IntoIterator<Item = &'a Entity<Buffer>>,
    primary_buffer_id: Option<BufferId>,
    pre_snapshot: &HashMap<BufferId, String>,
    cx: &mut AsyncApp,
) -> Vec<EditDiff> {
    let mut diffs = Vec::new();
    for buffer in touched {
        let (id, path, abs_path, new_text) = buffer.read_with(cx, |buffer, cx| {
            let path = buffer
                .file()
                .map(|file| file.full_path(cx).display().to_string())
                .unwrap_or_else(|| "<untitled>".to_string());
            let abs_path = buffer
                .file()
                .and_then(|file| file.as_local().map(|local| local.abs_path(cx)))
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default();
            (buffer.remote_id(), path, abs_path, buffer.text())
        });
        let old_text = pre_snapshot.get(&id).cloned().unwrap_or_default();
        diffs.push(EditDiff {
            path,
            abs_path,
            line: None,
            old_text,
            new_text,
        });
        if let Some(primary) = primary_buffer_id
            && id == primary
            && diffs.len() > 1
        {
            let last = diffs.len() - 1;
            diffs.swap(0, last);
        }
    }
    diffs
}

pub struct LocationDisplay {
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub snippet: String,
    pub truncated: bool,
}

impl LocationDisplay {
    pub fn from_location(location: &Location, cx: &App) -> Self {
        let snapshot = location.buffer.read(cx).snapshot();
        let range =
            location.range.start.to_point(&snapshot)..location.range.end.to_point(&snapshot);
        let path = location
            .buffer
            .read(cx)
            .file()
            .map(|file| file.full_path(cx).display().to_string())
            .unwrap_or_else(|| "<untitled>".to_string());

        let start_line = range.start.row + 1;
        let end_line = range.end.row + 1;
        let line_len = snapshot.line_len(range.start.row);
        let truncated = line_len as usize > MAX_LINE_DISPLAY_LEN;
        let snippet: String = snapshot
            .text_for_range(Point::new(range.start.row, 0)..Point::new(range.start.row, line_len))
            .flat_map(|chunk| chunk.chars())
            .skip_while(|character| character.is_whitespace())
            .take(MAX_LINE_DISPLAY_LEN)
            .collect::<String>();
        let snippet = snippet.trim_end().to_string();

        Self {
            path,
            start_line,
            end_line,
            snippet,
            truncated,
        }
    }
}

impl fmt::Display for LocationDisplay {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let truncated_label = if self.truncated { " (truncated)" } else { "" };
        if self.start_line == self.end_line {
            writeln!(
                formatter,
                "{}#L{}{truncated_label}",
                self.path, self.start_line
            )?;
        } else {
            writeln!(
                formatter,
                "{}#L{}-{}{truncated_label}",
                self.path, self.start_line, self.end_line
            )?;
        }
        writeln!(formatter, "```")?;
        writeln!(formatter, "{}", self.snippet)?;
        write!(formatter, "```")
    }
}

fn find_in_char_iter(chars: impl Iterator<Item = char>, needle: &str) -> Option<usize> {
    let needle_chars: Vec<char> = needle.chars().collect();
    if needle_chars.is_empty() {
        return Some(0);
    }

    let mut window: VecDeque<char> = VecDeque::with_capacity(needle_chars.len());
    let mut byte_offsets: VecDeque<usize> = VecDeque::with_capacity(needle_chars.len());
    let mut byte_offset = 0usize;

    for character in chars {
        window.push_back(character);
        byte_offsets.push_back(byte_offset);
        byte_offset += character.len_utf8();

        if window.len() > needle_chars.len() {
            window.pop_front();
            byte_offsets.pop_front();
        }

        if window.len() == needle_chars.len()
            && window
                .iter()
                .zip(needle_chars.iter())
                .all(|(left, right)| left == right)
        {
            return byte_offsets.front().copied();
        }
    }

    None
}

pub fn buffer_has_running_language_server(
    project: &Entity<Project>,
    buffer: &Entity<Buffer>,
    cx: &mut AsyncApp,
) -> bool {
    project.update(cx, |project, cx| {
        if !project.is_local() {
            return true;
        }
        buffer.update(cx, |buffer, cx| {
            project.lsp_store().update(cx, |store, cx| {
                !store
                    .language_servers_for_local_buffer(buffer, cx)
                    .is_empty()
            })
        })
    })
}

pub async fn diagnostics(
    project: Entity<Project>,
    lease: &LspBufferLease,
    path: Option<String>,
    min_severity: Option<String>,
    cx: &mut AsyncApp,
) -> Result<String, String> {
    let min_severity = parse_min_severity(min_severity.as_deref())?;
    match path {
        Some(ref path) if !path.is_empty() => {
            diagnostics_for_path(project, lease, path, min_severity, cx).await
        }
        _ => diagnostics_for_project(project, cx).await,
    }
}

/// Parse the `min_severity` argument (default `warning`). Diagnostics less severe than this are
/// not reported; lowering it to `information`/`hint` also surfaces lower-tier lints (e.g. an
/// unused argument in a pure internal helper) so the agent can decide whether to act on them.
fn parse_min_severity(min_severity: Option<&str>) -> Result<DiagnosticSeverity, String> {
    let Some(min_severity) = min_severity else {
        return Ok(DiagnosticSeverity::WARNING);
    };
    Ok(match min_severity.to_ascii_lowercase().as_str() {
        "error" => DiagnosticSeverity::ERROR,
        "warning" => DiagnosticSeverity::WARNING,
        "information" | "info" => DiagnosticSeverity::INFORMATION,
        "hint" => DiagnosticSeverity::HINT,
        other => {
            return Err(format!(
                "invalid min_severity {other:?}; expected one of: error, warning, information, hint"
            ));
        }
    })
}

/// Lower rank = more severe (`ERROR` = 0 … `HINT` = 3). A diagnostic is reported when its rank
/// is `<=` the requested minimum severity's rank.
fn severity_rank(severity: DiagnosticSeverity) -> u8 {
    match severity {
        DiagnosticSeverity::ERROR => 0,
        DiagnosticSeverity::WARNING => 1,
        DiagnosticSeverity::INFORMATION => 2,
        DiagnosticSeverity::HINT => 3,
        _ => u8::MAX,
    }
}

fn severity_label(severity: DiagnosticSeverity) -> Option<&'static str> {
    Some(match severity {
        DiagnosticSeverity::ERROR => "error",
        DiagnosticSeverity::WARNING => "warning",
        DiagnosticSeverity::INFORMATION => "information",
        DiagnosticSeverity::HINT => "hint",
        _ => return None,
    })
}

async fn diagnostics_for_path(
    project: Entity<Project>,
    lease: &LspBufferLease,
    path: &str,
    min_severity: DiagnosticSeverity,
    cx: &mut AsyncApp,
) -> Result<String, String> {
    let open_buffer_task = project.update(cx, |project, cx| {
        let Some(project_path) = project.find_project_path(path, cx) else {
            return Err(format!("Could not find path {path} in project"));
        };
        Ok(project.open_buffer(project_path, cx))
    })?;

    let buffer = open_buffer_task
        .await
        .map_err(|error| format!("Failed to open '{path}': {error}"))?;

    // Servers that only analyze open documents need the buffer registered before the pull.
    // The turn-scoped lease keeps it open for subsequent LSP operations without tying its
    // lifetime to the thread's edit-review state.
    lease.acquire(&project, &buffer, cx);

    let lsp_store = project.read_with(cx, |project, _cx| project.lsp_store());
    let pull_result = lsp_store
        .update(cx, |lsp_store, cx| {
            lsp_store.pull_diagnostics_for_buffer(buffer.clone(), cx)
        })
        .await;
    if let Err(error) = &pull_result {
        log::warn!("Failed to pull diagnostics, using cached: {error:#}");
    }
    let refreshed = pull_result.is_ok();

    if !buffer_has_running_language_server(&project, &buffer, cx) {
        return Ok(format!(
            "No language server is running for `{path}`. Diagnostics from Zed reflect language-server output, so without a server this result cannot be trusted as 'no errors'."
        ));
    }

    let mut output = String::new();
    let snapshot = buffer.read_with(cx, |buffer, _cx| buffer.snapshot());

    for (_, group) in snapshot.diagnostic_groups(None) {
        let entry = &group.entries[group.primary_ix];
        // Skip anything less severe than the requested minimum (default WARNING). Callers can
        // lower `min_severity` to also surface information/hint-level lints (e.g. an unused
        // argument in a pure internal helper) so the agent can decide whether to act on them.
        if severity_rank(entry.diagnostic.severity) > severity_rank(min_severity) {
            continue;
        }
        let Some(severity) = severity_label(entry.diagnostic.severity) else {
            continue;
        };
        let range = entry.range.to_point(&snapshot);

        let _ = writeln!(
            output,
            "{severity} at line {}: {}",
            range.start.row + 1,
            entry.diagnostic.message
        );
    }

    let freshness = diagnostics_freshness_message(refreshed);
    if output.is_empty() {
        Ok(format!(
            "{freshness}\n\nFile doesn't have any diagnostics at `{}` severity or higher!",
            severity_label(min_severity).unwrap_or("warning"),
        ))
    } else {
        Ok(format!("{freshness}\n\n{output}"))
    }
}

async fn diagnostics_for_project(
    project: Entity<Project>,
    cx: &mut AsyncApp,
) -> Result<String, String> {
    let lsp_store = project.read_with(cx, |project, _cx| project.lsp_store());
    let refreshed = lsp_store
        .update(cx, |lsp_store, cx| {
            lsp_store.pull_workspace_diagnostics_once(cx)
        })
        .await;
    if !refreshed {
        log::warn!("Failed to pull workspace diagnostics, using cached");
    }

    let (output, has_diagnostics) = project.read_with(cx, |project, cx| {
        let mut output = String::new();
        let mut has_diagnostics = false;

        for (project_path, _, summary) in project.diagnostic_summaries(true, cx) {
            if summary.error_count > 0 || summary.warning_count > 0 {
                let Some(worktree) = project.worktree_for_id(project_path.worktree_id, cx) else {
                    continue;
                };

                has_diagnostics = true;
                let _ = writeln!(
                    output,
                    "{}: {} error(s), {} warning(s)",
                    worktree.read(cx).absolutize(&project_path.path).display(),
                    summary.error_count,
                    summary.warning_count
                );
            }
        }

        (output, has_diagnostics)
    });

    let freshness = diagnostics_freshness_message(refreshed);
    if has_diagnostics {
        Ok(format!("{freshness}\n\n{output}"))
    } else if !refreshed {
        Ok(format!(
            "{freshness}\n\nNo errors or warnings reported. This may mean no language server is currently running for this project's files."
        ))
    } else {
        Ok(format!(
            "{freshness}\n\nNo errors or warnings found in the project."
        ))
    }
}

fn diagnostics_freshness_message(refreshed: bool) -> &'static str {
    if refreshed {
        "Diagnostics successfully refreshed."
    } else {
        "Failed to refresh diagnostics. Diagnostics may be stale."
    }
}

pub async fn find_references(
    project: Entity<Project>,
    lease: &LspBufferLease,
    symbol: SymbolLocator,
    cx: &mut AsyncApp,
) -> Result<String, String> {
    let resolved = symbol.resolve(&project, lease, cx).await?;

    if !buffer_has_running_language_server(&project, &resolved.buffer, cx) {
        return Ok(format!(
            "No language server is running for `{}`. Enable or install a language server in Zed before relying on LSP-backed lookups.",
            symbol.file_path
        ));
    }

    let references_task = project.update(cx, |project, cx| {
        project.references(&resolved.buffer, resolved.position, cx)
    });

    let references = match references_task
        .await
        .map_err(|error| format!("Find references failed: {error}"))?
    {
        Some(references) => references,
        None => {
            return Ok(format!(
                "No language server capable of finding references is available for `{}`.",
                symbol.file_path
            ));
        }
    };

    if references.is_empty() {
        return Ok(format!("No references found for '{}'.", symbol.symbol_name));
    }

    let mut output = format!(
        "Found {} references to `{}`:\n",
        references.len(),
        symbol.symbol_name
    );
    for location in &references {
        let display = location
            .buffer
            .read_with(cx, |_, cx| LocationDisplay::from_location(location, cx));
        let _ = write!(output, "\n## {display}\n");
    }

    Ok(output)
}

pub async fn go_to_definition(
    project: Entity<Project>,
    lease: &LspBufferLease,
    symbol: SymbolLocator,
    cx: &mut AsyncApp,
) -> Result<String, String> {
    let resolved = symbol.resolve(&project, lease, cx).await?;

    if !buffer_has_running_language_server(&project, &resolved.buffer, cx) {
        return Ok(format!(
            "No language server is running for `{}`. Enable or install a language server in Zed before relying on LSP-backed lookups.",
            symbol.file_path
        ));
    }

    let definitions_task = project.update(cx, |project, cx| {
        project.definitions(&resolved.buffer, resolved.position, cx)
    });

    let definitions = match definitions_task
        .await
        .map_err(|error| format!("Go to definition failed: {error}"))?
    {
        Some(definitions) => definitions,
        None => {
            return Ok(format!(
                "No language server capable of resolving definitions is available for `{}`.",
                symbol.file_path
            ));
        }
    };

    if definitions.is_empty() {
        return Ok(format!("No definition found for '{}'.", symbol.symbol_name));
    }

    let mut output = String::new();
    if definitions.len() == 1 {
        let _ = writeln!(output, "Definition of `{}`:", symbol.symbol_name);
    } else {
        let _ = writeln!(
            output,
            "Found {} definitions of `{}`:",
            definitions.len(),
            symbol.symbol_name
        );
    }

    for link in &definitions {
        let display = link
            .target
            .buffer
            .read_with(cx, |_, cx| LocationDisplay::from_location(&link.target, cx));
        let _ = write!(output, "\n## {display}\n");
    }

    Ok(output)
}

pub async fn hover(
    project: Entity<Project>,
    lease: &LspBufferLease,
    symbol: SymbolLocator,
    cx: &mut AsyncApp,
) -> Result<String, String> {
    let resolved = symbol.resolve(&project, lease, cx).await?;

    if !buffer_has_running_language_server(&project, &resolved.buffer, cx) {
        return Ok(format!(
            "No language server is running for `{}`. Enable or install a language server in Zed before requesting hover info.",
            symbol.file_path
        ));
    }

    let position_utf16 = resolved
        .buffer
        .read_with(cx, |buffer, _cx| resolved.position.to_point_utf16(buffer));
    let hovers = project
        .update(cx, |project, cx| {
            project.hover(&resolved.buffer, position_utf16, cx)
        })
        .await
        .unwrap_or_default();

    if hovers.is_empty() || hovers.iter().all(|hover| hover.is_empty()) {
        return Ok(format!(
            "No hover information available for '{}' at {}:{}.",
            symbol.symbol_name, symbol.file_path, symbol.line
        ));
    }

    let mut output = format!("Hover info for `{}`:\n", symbol.symbol_name);
    for hover in &hovers {
        if hover.is_empty() {
            continue;
        }
        for block in &hover.contents {
            output.push('\n');
            match &block.kind {
                HoverBlockKind::PlainText | HoverBlockKind::Markdown => {
                    output.push_str(&block.text)
                }
                HoverBlockKind::Code { language } => {
                    let _ = writeln!(output, "```{language}");
                    output.push_str(&block.text);
                    if !block.text.ends_with('\n') {
                        output.push('\n');
                    }
                    output.push_str("```");
                }
            }
            if !output.ends_with('\n') {
                output.push('\n');
            }
        }
    }

    Ok(output)
}

pub async fn rename_symbol(
    project: Entity<Project>,
    lease: &LspBufferLease,
    symbol: SymbolLocator,
    new_name: String,
    cx: &mut AsyncApp,
) -> Result<EditOperationOutput, String> {
    let resolved = symbol.resolve(&project, lease, cx).await?;

    if !buffer_has_running_language_server(&project, &resolved.buffer, cx) {
        return Ok(EditOperationOutput::Text(format!(
            "No language server is running for `{}`. Enable or install a language server in Zed before attempting an LSP-backed rename.",
            symbol.file_path
        )));
    }

    let starting_buffer_id = resolved
        .buffer
        .read_with(cx, |buffer, _cx| buffer.remote_id());
    let pre_snapshot = snapshot_open_buffer_texts(&project, cx);

    let rename_task = project.update(cx, |project, cx| {
        project.perform_rename(
            resolved.buffer.clone(),
            resolved.position,
            new_name.clone(),
            None,
            cx,
        )
    });

    let transaction = rename_task
        .await
        .map_err(|error| format!("Rename failed: {error}"))?;

    if transaction.0.is_empty() {
        return Ok(EditOperationOutput::Text(format!(
            "No changes were made. The language server could not rename '{}'.",
            symbol.symbol_name
        )));
    }

    let touched_buffers = transaction.0.keys().cloned().collect::<HashSet<_>>();
    project
        .update(cx, |project, cx| project.save_buffers(touched_buffers, cx))
        .await
        .map_err(|error| format!("Rename succeeded, but failed to save renamed files: {error}"))?;

    let diffs = build_diffs_from_touched_buffers(
        transaction.0.keys(),
        Some(starting_buffer_id),
        &pre_snapshot,
        cx,
    );

    let mut text = format!(
        "Renamed `{}` to `{}` in {} file(s):\n",
        symbol.symbol_name,
        new_name,
        diffs.len()
    );
    for diff in &diffs {
        let _ = writeln!(text, "- {}", diff.path);
    }

    Ok(EditOperationOutput::Edited(EditOutput { text, diffs }))
}

pub async fn get_code_actions(
    project: Entity<Project>,
    lease: &LspBufferLease,
    symbol: SymbolLocator,
    apply_tool_name: &str,
    cx: &mut AsyncApp,
) -> Result<CodeActionsOutput, String> {
    let resolved = symbol.resolve(&project, lease, cx).await?;

    if !buffer_has_running_language_server(&project, &resolved.buffer, cx) {
        return Ok(CodeActionsOutput {
            text: format!(
                "No language server is running for `{}`. Enable or install a language server in Zed before requesting LSP-backed code actions.",
                symbol.file_path
            ),
            pending: None,
        });
    }

    let actions_task = project.update(cx, |project, cx| {
        let range = resolved.position..resolved.position;
        project.code_actions(&resolved.buffer, range, None, cx)
    });

    let actions = match actions_task
        .await
        .map_err(|error| format!("Failed to get code actions: {error}"))?
    {
        Some(actions) => actions,
        None => {
            return Ok(CodeActionsOutput {
                text: format!(
                    "No language server capable of providing code actions is available for `{}`.",
                    symbol.file_path
                ),
                pending: None,
            });
        }
    };

    if actions.is_empty() {
        return Ok(CodeActionsOutput {
            text: format!(
                "No code actions available for '{}' at this location.",
                symbol.symbol_name
            ),
            pending: None,
        });
    }

    let mut text = format!("Found {} code action(s):\n", actions.len());
    for (index, action) in actions.iter().enumerate() {
        let _ = writeln!(text, "{}. {}", index + 1, action.lsp_action.title());
    }
    let _ = write!(
        text,
        "\nUse `{apply_tool_name}` with the number of the action you want to apply."
    );

    Ok(CodeActionsOutput {
        text,
        pending: Some(PendingCodeActions {
            actions,
            buffer: resolved.buffer,
        }),
    })
}

pub async fn apply_code_action(
    project: Entity<Project>,
    lease: &LspBufferLease,
    index: u32,
    pending: PendingCodeActions,
    cx: &mut AsyncApp,
) -> Result<EditOperationOutput, String> {
    let zero_based_index = index
        .checked_sub(1)
        .ok_or_else(|| "Index must be 1 or greater.".to_string())?;

    let action = pending
        .actions
        .get(zero_based_index as usize)
        .cloned()
        .ok_or_else(|| {
            format!(
                "Index {index} is out of range. There were {} code action(s) available.",
                pending.actions.len()
            )
        })?;

    let title = action.lsp_action.title().to_string();
    let buffer = pending.buffer.clone();
    lease.acquire(&project, &buffer, cx);
    let starting_buffer_id = buffer.read_with(cx, |buffer, _cx| buffer.remote_id());
    let pre_snapshot = snapshot_open_buffer_texts(&project, cx);

    let apply_task = project.update(cx, |project, cx| {
        project.apply_code_action(buffer, action, true, cx)
    });

    let transaction = apply_task
        .await
        .map_err(|error| format!("Failed to apply code action '{title}': {error}"))?;

    if transaction.0.is_empty() {
        return Ok(EditOperationOutput::Text(format!(
            "Code action '{title}' was applied but produced no changes."
        )));
    }

    let touched_buffers = transaction.0.keys().cloned().collect::<HashSet<_>>();
    project
        .update(cx, |project, cx| project.save_buffers(touched_buffers, cx))
        .await
        .map_err(|error| {
            format!("Code action '{title}' applied but saving touched files failed: {error}")
        })?;

    let diffs = build_diffs_from_touched_buffers(
        transaction.0.keys(),
        Some(starting_buffer_id),
        &pre_snapshot,
        cx,
    );

    let mut text = format!(
        "Applied code action '{title}'. Modified {} file(s):\n",
        diffs.len()
    );
    for diff in &diffs {
        let _ = writeln!(text, "- {}", diff.path);
    }

    Ok(EditOperationOutput::Edited(EditOutput { text, diffs }))
}

pub async fn format_document(
    project: Entity<Project>,
    lease: &LspBufferLease,
    file_path: String,
    cx: &mut AsyncApp,
) -> Result<EditOperationOutput, String> {
    let open_buffer_task = project.update(cx, |project, cx| {
        let Some(project_path) = project.find_project_path(&file_path, cx) else {
            return Err(format!("Could not find path '{file_path}' in project"));
        };
        Ok(project.open_buffer(project_path, cx))
    })?;

    let buffer = open_buffer_task
        .await
        .map_err(|error| format!("Failed to open '{file_path}': {error}"))?;
    lease.acquire(&project, &buffer, cx);

    if !buffer_has_running_language_server(&project, &buffer, cx) {
        return Ok(EditOperationOutput::Text(format!(
            "No language server is running for `{file_path}`. Enable or install a language server in Zed before formatting."
        )));
    }

    let (old_full_text, abs_path) = buffer.read_with(cx, |buffer, cx| {
        let abs_path = buffer
            .file()
            .and_then(|file| file.as_local().map(|local| local.abs_path(cx)))
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default();
        (buffer.text(), abs_path)
    });

    let format_task = project.update(cx, |project, cx| {
        let mut buffers = HashSet::default();
        buffers.insert(buffer.clone());
        project.format(
            buffers,
            LspFormatTarget::Buffers,
            true,
            FormatTrigger::Manual,
            cx,
        )
    });

    let transaction = format_task
        .await
        .map_err(|error| format!("Format failed for '{file_path}': {error}"))?;

    let touched_files = transaction.0.len();
    let new_full_text = buffer.read_with(cx, |buffer, _cx| buffer.text());

    let buffers = HashSet::from_iter([buffer.clone()]);
    project
        .update(cx, |project, cx| project.save_buffers(buffers, cx))
        .await
        .map_err(|error| format!("Format applied in-memory but save failed: {error}"))?;

    Ok(EditOperationOutput::Edited(EditOutput {
        text: format!(
            "Formatted `{file_path}` and saved to disk ({touched_files} buffer(s) touched)."
        ),
        diffs: vec![EditDiff {
            path: file_path,
            abs_path,
            line: None,
            old_text: old_full_text,
            new_text: new_full_text,
        }],
    }))
}

pub async fn workspace_symbol(
    project: Entity<Project>,
    query: String,
    cx: &mut AsyncApp,
) -> Result<String, String> {
    let query = query.trim().to_string();
    if query.is_empty() {
        return Err("`query` must not be empty. Provide a symbol name or substring to avoid returning the entire workspace symbol index.".to_string());
    }

    let symbols_task = project.update(cx, |project, cx| project.symbols(&query, cx));
    let symbols = symbols_task
        .await
        .map_err(|error| format!("Workspace symbol search failed: {error}"))?;

    if symbols.is_empty() {
        return Ok(format!("No workspace symbols found matching `{query}`."));
    }

    let total_count = symbols.len();
    let query_lowercase = query.to_lowercase();
    let filtered_symbols = symbols
        .iter()
        .filter(|symbol| symbol_matches_query(symbol, &query_lowercase))
        .collect::<Vec<_>>();

    let (symbols_to_display, match_count, client_filtered, fell_back_to_server_results) =
        if filtered_symbols.is_empty() {
            (symbols.iter().collect::<Vec<_>>(), total_count, false, true)
        } else {
            let match_count = filtered_symbols.len();
            (
                filtered_symbols,
                match_count,
                match_count != total_count,
                false,
            )
        };

    let shown_count = symbols_to_display.len().min(MAX_WORKSPACE_SYMBOL_RESULTS);
    let omitted_count = match_count.saturating_sub(shown_count);
    let mut output = if fell_back_to_server_results {
        format!(
            "The language server returned {total_count} symbol(s) for `{query}`, but none matched the query after client-side filtering."
        )
    } else if client_filtered {
        format!(
            "Found {match_count} symbol(s) matching `{query}` after client-side filtering ({total_count} returned by the language server)."
        )
    } else {
        format!("Found {match_count} symbol(s) matching `{query}`.")
    };

    if omitted_count > 0 {
        let _ = write!(
            output,
            " Showing first {shown_count}; {omitted_count} omitted. Refine the query for narrower results."
        );
    }
    if fell_back_to_server_results || client_filtered || omitted_count > 0 {
        output.push_str(" Some language servers return overly broad workspace symbol results, so this tool filters and caps output.");
    }
    output.push('\n');

    for symbol in symbols_to_display.iter().take(MAX_WORKSPACE_SYMBOL_RESULTS) {
        format_symbol(&mut output, symbol);
    }
    Ok(output)
}

fn symbol_matches_query(symbol: &Symbol, query_lowercase: &str) -> bool {
    let mut haystack = symbol.name.to_lowercase();
    if let Some(container_name) = &symbol.container_name {
        haystack.push(' ');
        haystack.push_str(&container_name.to_lowercase());
    }
    haystack.push(' ');
    haystack.push_str(&symbol_path(symbol).to_lowercase());

    query_lowercase
        .split_whitespace()
        .all(|query_part| haystack.contains(query_part))
}

fn format_symbol(output: &mut String, symbol: &Symbol) {
    let kind = format!("{:?}", symbol.kind);
    let path = symbol_path(symbol);
    let line = symbol.range.start.0.row + 1;
    let container = symbol
        .container_name
        .as_deref()
        .map(|container| format!(" ({container})"))
        .unwrap_or_default();
    let _ = writeln!(
        output,
        "- {kind} `{}`{container} — {path}:{line}",
        symbol.name
    );
}

fn symbol_path(symbol: &Symbol) -> String {
    match &symbol.path {
        SymbolLocation::InProject(project_path) => {
            project_path.path.display(PathStyle::local()).to_string()
        }
        SymbolLocation::OutsideProject { abs_path, .. } => abs_path.display().to_string(),
    }
}

pub async fn list_language_servers(
    project: Entity<Project>,
    cx: &mut AsyncApp,
) -> Result<String, String> {
    let lines = project.update(cx, |project, cx| {
        let lsp_store = project.lsp_store().read(cx);
        let mut lines = Vec::new();
        for (id, status) in lsp_store.language_server_statuses() {
            let worktree_label = status
                .worktree
                .and_then(|worktree_id| project.worktree_for_id(worktree_id, cx))
                .map(|worktree| {
                    worktree
                        .read(cx)
                        .root_name()
                        .display(PathStyle::local())
                        .to_string()
                })
                .unwrap_or_else(|| "<no worktree>".to_string());
            let pending = if status.pending_work.is_empty() {
                "no pending work".to_string()
            } else {
                format!("{} pending task(s)", status.pending_work.len())
            };
            let version = status
                .server_version
                .as_deref()
                .map(|version| format!(" v{version}"))
                .unwrap_or_default();
            lines.push(format!(
                "- {}{version} [id {}] worktree=`{worktree_label}` — {pending}",
                status.name, id.0,
            ));
        }
        lines
    });

    if lines.is_empty() {
        return Ok("No language servers are currently registered for this project.".to_string());
    }

    let mut output = format!("Language servers in this project ({}):\n", lines.len());
    for line in lines {
        output.push_str(&line);
        output.push('\n');
    }
    output.push_str(
        "\nUse `lsp_restart_language_server` with the desired `name` to restart one (all instances with that name across worktrees will restart).",
    );
    Ok(output)
}

pub async fn restart_language_server(
    project: Entity<Project>,
    name: String,
    reason: String,
    cx: &mut AsyncApp,
) -> Result<String, String> {
    if name.trim().is_empty() {
        return Err("`name` must not be empty.".to_string());
    }
    if reason.trim().is_empty() {
        return Err("`reason` must not be empty — provide a short justification.".to_string());
    }

    let target_name = LanguageServerName::from(name.as_str());

    let (matched_count, total) = project.update(cx, |project, cx| {
        let lsp_store = project.lsp_store().read(cx);
        let matched_count = lsp_store
            .language_server_statuses()
            .filter(|(_, status)| {
                <LanguageServerName as AsRef<str>>::as_ref(&status.name)
                    == <LanguageServerName as AsRef<str>>::as_ref(&target_name)
            })
            .count();
        let total = lsp_store.language_server_statuses().count();
        (matched_count, total)
    });

    if matched_count == 0 {
        return Ok(format!(
            "No language server named `{name}` is registered for this project. Call `lsp_list_language_servers` to see the available names. ({total} server(s) registered overall.)"
        ));
    }

    let mut selectors = HashSet::default();
    selectors.insert(LanguageServerSelector::Name(target_name));

    project.update(cx, |project, cx| {
        let buffers: Vec<_> = project.buffer_store().read(cx).buffers().collect();
        project.restart_language_servers_for_buffers(buffers, selectors, true, cx);
    });

    log::info!(
        "LSP agent restart_language_server: name=`{name}` instances={matched_count} reason=`{reason}`"
    );

    Ok(format!(
        "Restarted `{name}` ({matched_count} instance(s)). The server will re-index, which can take 30–60s for moderate projects. Reason: {reason}"
    ))
}
