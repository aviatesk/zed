//! HTTP MCP bridge that exposes Zed's LSP-backed operations to external
//! ACP agents (Claude Code, Gemini CLI, etc.).
//!
//! At a high level, this module:
//!
//! 1. Binds a TCP listener on `127.0.0.1:0` (ephemeral port) per ACP
//!    connection and serves the MCP protocol over plain HTTP JSON-RPC. The
//!    listening URL is then injected into `NewSessionRequest.mcp_servers`
//!    via `acp::McpServer::Http`, so the external agent treats Zed itself
//!    as just another MCP server.
//!
//! 2. Tool calls arriving on the HTTP server are turned into
//!    `LspBridgeOp` work items and pushed onto the existing ACP
//!    foreground-dispatch channel. The handler on the GPUI side
//!    (`handle_lsp_bridge_work`) runs the actual `Project` / `LspStore`
//!    queries and replies via a oneshot channel.
//!
//! Tools currently implemented:
//!
//! * `lsp_find_references` — LSP-backed reference search.
//! * `lsp_diagnostics` — project-wide or per-file diagnostic summary.
//! * `lsp_rename_symbol` — LSP semantic rename, saving touched buffers.
//! * `lsp_format_document` — formatter via Zed's configured LSP formatter.
//! * `lsp_get_code_actions` / `lsp_apply_code_action` — pair, list and apply
//!   quick fixes / refactorings.
//! * `lsp_hover` — type information and documentation at a symbol.
//! * `lsp_workspace_symbol` — workspace-wide symbol search.
//! * `lsp_list_language_servers` / `lsp_restart_language_server` — pair, list
//!   running servers and restart one by name.
//! * `read_buffer` — read Zed's in-memory buffer (sees unsaved edits).
//! * `apply_text_edit` — buffer-aware edit, always saved to disk so the
//!   language server's didSave-driven diagnostics refresh.

use std::fmt::Write as _;
use std::net::{Ipv4Addr, SocketAddr};

use acp_thread::{BridgeResultEntry, BridgeResultQueue, enqueue_bridge_result};
use anyhow::{Context as _, Result, anyhow};
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use collections::HashSet;
use futures::channel::{mpsc, oneshot};
use gpui::{App, AppContext as _, AsyncApp, Entity, Task, WeakEntity};
use gpui_tokio::Tokio;
use language::Buffer;
use project::Project;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use text::Point;
use tokio::net::TcpListener;
use util::markdown::MarkdownCodeBlock;

const PROTOCOL_VERSION: &str = "2024-11-05";
const SERVER_NAME: &str = "zed-lsp-bridge";
const SERVER_VERSION: &str = "0.1.0";

/// Live handle to an HTTP MCP bridge serving Zed's LSP tools.
///
/// Dropping the handle aborts the server task and releases the port.
pub struct LspMcpBridge {
    url: String,
    _server_task: Task<()>,
}

impl LspMcpBridge {
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Start a new HTTP MCP server bound to a random localhost port.
    ///
    /// All tool calls are forwarded to the GPUI foreground thread via the
    /// supplied dispatch channel, where they are resolved against `project`.
    /// `result_queue` is the in-process side-channel that lets the bridge
    /// publish each tool result so the connection's `AcpThread` can pick it
    /// up without going through the external agent.
    pub fn start(
        dispatch_tx: mpsc::UnboundedSender<crate::acp::ForegroundWork>,
        result_queue: BridgeResultQueue,
        cx: &mut App,
    ) -> Result<Self> {
        let listener_task = Tokio::spawn(cx, async move {
            TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await
        });

        // Block briefly on the bind so we can return the URL synchronously.
        // The bind itself is cheap; this is not the request-serving loop.
        let listener = futures::executor::block_on(listener_task)
            .map_err(|err| anyhow!("tokio join error binding LSP MCP listener: {err}"))?
            .context("failed to bind LSP MCP listener")?;
        let addr = listener.local_addr().context("local_addr")?;
        let url = format!("http://{addr}/");

        let state = BridgeState {
            dispatch_tx,
            result_queue,
        };
        let app = Router::new()
            .route("/", post(handle_request))
            .route("/", get(handle_get))
            .with_state(state);

        let server_task = Tokio::spawn(cx, async move {
            if let Err(err) = axum::serve(listener, app).await {
                log::warn!("LSP MCP bridge HTTP server exited: {err}");
            }
        });
        let server_task = cx.background_spawn(async move {
            server_task.await.ok();
        });

        Ok(Self {
            url,
            _server_task: server_task,
        })
    }
}

#[derive(Clone)]
struct BridgeState {
    dispatch_tx: mpsc::UnboundedSender<crate::acp::ForegroundWork>,
    result_queue: BridgeResultQueue,
}

async fn handle_get() -> impl IntoResponse {
    // Some MCP clients probe the URL with a GET before POSTing; respond
    // cheaply so they don't treat the endpoint as broken.
    (StatusCode::OK, "zed-lsp-bridge")
}

async fn handle_request(
    State(state): State<BridgeState>,
    Json(request): Json<Value>,
) -> impl IntoResponse {
    let response = process_message(&state, request).await;
    match response {
        Some(value) => (StatusCode::OK, Json(value)).into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    }
}

/// Returns `None` for notifications (no response body).
async fn process_message(state: &BridgeState, message: Value) -> Option<Value> {
    let id = message.get("id").cloned();
    let method = message.get("method").and_then(Value::as_str)?.to_string();
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let is_notification = id.is_none();

    let result: Result<Value, JsonRpcError> = match method.as_str() {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
        })),
        "notifications/initialized" => Ok(Value::Null),
        "tools/list" => Ok(json!({ "tools": tool_definitions() })),
        "tools/call" => handle_tools_call(state, params).await,
        "ping" => Ok(Value::Object(Default::default())),
        other => Err(JsonRpcError::method_not_found(format!(
            "method not supported: {other}"
        ))),
    };

    if is_notification {
        return None;
    }

    let id = id.unwrap_or(Value::Null);
    Some(match result {
        Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
        Err(err) => json!({ "jsonrpc": "2.0", "id": id, "error": err }),
    })
}

#[derive(Serialize)]
struct JsonRpcError {
    code: i32,
    message: String,
}

impl JsonRpcError {
    fn invalid_params(message: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: message.into(),
        }
    }
    fn internal(message: impl Into<String>) -> Self {
        Self {
            code: -32603,
            message: message.into(),
        }
    }
    fn method_not_found(message: impl Into<String>) -> Self {
        Self {
            code: -32601,
            message: message.into(),
        }
    }
}

fn tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "lsp_find_references",
            "description": "Find all references to a symbol across the project using the language \
                            server Zed is already running for this file (rust-analyzer, gopls, \
                            pyright, etc.). More accurate than grep — distinguishes same-named \
                            identifiers across scopes — and preferable to other LSP-style tools \
                            when Zed is the editor host: Zed's server is warm, applies the \
                            project's .zed/settings.json tuning, and reflects unsaved buffer edits.\n\
                            \n\
                            Provide the file path, 1-based line number, and the symbol's exact \
                            identifier as it appears on that line. Returns each reference's path, \
                            line range, and code snippet.\n\
                            \n\
                            When inspecting or modifying the code these results point at, prefer \
                            `read_buffer` / `apply_text_edit` over disk-based Read/Edit — only \
                            the former see the same in-memory buffer state this tool just queried.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file_path": { "type": "string", "description": "Relative project path of the file containing the symbol." },
                    "line": { "type": "integer", "description": "1-based line number of the symbol." },
                    "symbol_name": { "type": "string", "description": "The exact identifier text to locate on that line." },
                },
                "required": ["file_path", "line", "symbol_name"],
            },
        }),
        json!({
            "name": "lsp_diagnostics",
            "description": "Return errors and warnings produced by the language servers Zed is \
                            already running for this project. With no path argument returns a \
                            project-wide error/warning summary; with a path returns per-line \
                            diagnostics for that file. Preferable to other LSP-style tools when \
                            Zed is the editor host: results reflect Zed's configured language-\
                            server settings with no cold-start cost.\n\
                            \n\
                            When inspecting or fixing the code these diagnostics point at, prefer \
                            `read_buffer` / `apply_text_edit` over disk-based Read/Edit — only \
                            the former see the same in-memory buffer state this tool just queried.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Optional relative project path. Omit for a project-wide summary." },
                    "min_severity": { "type": "string", "enum": ["error", "warning", "information", "hint"], "description": "Minimum severity to report for a per-file query (default \"warning\"). Lower it to also surface information/hint-level lints, e.g. an unused argument in a pure internal helper. Ignored for the project-wide summary." },
                },
            },
        }),
        json!({
            "name": "lsp_rename_symbol",
            "description": "Rename a symbol across the entire project using the language server's \
                            semantic rename. Safer than text substitution: the server updates \
                            only the occurrences that semantically refer to this symbol, so \
                            unrelated same-named identifiers in other modules, comments, or \
                            strings are left untouched. All modified buffers are saved \
                            automatically, going through Zed's format-on-save pipeline. Returns \
                            the list of files that were changed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file_path": { "type": "string", "description": "Relative project path of the file containing the symbol." },
                    "line": { "type": "integer", "description": "1-based line number of the symbol." },
                    "symbol_name": { "type": "string", "description": "The exact identifier text to locate on that line." },
                    "new_name": { "type": "string", "description": "The new identifier to rename the symbol to." },
                },
                "required": ["file_path", "line", "symbol_name", "new_name"],
            },
        }),
        json!({
            "name": "lsp_format_document",
            "description": "Format a project file using the formatter Zed has configured for this \
                            language — typically the LSP server's own formatter (rust-analyzer → \
                            rustfmt, gopls → gofmt, etc.), but respects any project-specific \
                            override declared in .zed/settings.json. Preferable to running a \
                            formatter CLI yourself: the result matches what `format-on-save` \
                            would produce, including language-specific options.\n\
                            \n\
                            The formatted buffer is always saved to disk through Zed's \
                            format-on-save pipeline; any unsaved user edits in the buffer are \
                            saved together with the formatting result.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file_path": { "type": "string", "description": "Relative project path of the file to format." },
                },
                "required": ["file_path"],
            },
        }),
        json!({
            "name": "lsp_get_code_actions",
            "description": "List code actions (quick fixes, refactorings, source actions) the \
                            language server suggests at a symbol's location. Returns a numbered \
                            list — call `lsp_apply_code_action` with that 1-based number to apply \
                            one. The list is cached until the next `lsp_get_code_actions` call or \
                            a successful apply.\n\
                            \n\
                            Examples of actions the server might suggest: 'Add missing import', \
                            'Extract to function', 'Inline variable', 'Implement missing methods'.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file_path": { "type": "string", "description": "Relative project path of the file." },
                    "line": { "type": "integer", "description": "1-based line number of the symbol." },
                    "symbol_name": { "type": "string", "description": "The exact identifier text to locate on that line." },
                },
                "required": ["file_path", "line", "symbol_name"],
            },
        }),
        json!({
            "name": "lsp_apply_code_action",
            "description": "Apply a code action previously listed by `lsp_get_code_actions`. \
                            Reference the action by its 1-based index in the previous list. The \
                            cached list is cleared after a successful apply; call \
                            `lsp_get_code_actions` again to obtain a fresh list.\n\
                            \n\
                            All buffers modified by the action are saved automatically, going \
                            through Zed's format-on-save pipeline. Returns the list of files that \
                            were changed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "index": { "type": "integer", "description": "1-based index of the code action to apply." },
                },
                "required": ["index"],
            },
        }),
        json!({
            "name": "lsp_hover",
            "description": "Get hover information (type, signature, documentation) for a symbol \
                            using the language server. Preferable to other LSP-style tools when \
                            Zed is the editor host: Zed's server is warm, sees unsaved buffer \
                            edits, and applies the project's .zed/settings.json tuning (e.g. \
                            rust-analyzer's `hover.documentation.enable`).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file_path": { "type": "string", "description": "Relative project path of the file containing the symbol." },
                    "line": { "type": "integer", "description": "1-based line number of the symbol." },
                    "symbol_name": { "type": "string", "description": "The exact identifier text to locate on that line." },
                },
                "required": ["file_path", "line", "symbol_name"],
            },
        }),
        json!({
            "name": "lsp_workspace_symbol",
            "description": "Search for symbols (functions, types, variables) across the entire \
                            workspace using the language server's workspace/symbol request. \
                            Returns matching symbols with their kind, name, and source location. \
                            Results are client-filtered and capped to avoid excessive output from \
                            language servers that ignore the query. More accurate than grep for \
                            symbol lookup: the server understands scoping, language semantics, \
                            and aliasing.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Non-empty search query (typically a substring of the symbol name)." },
                },
                "required": ["query"],
            },
        }),
        json!({
            "name": "lsp_list_language_servers",
            "description": "List the language servers currently registered for this Zed project, \
                            including their name, worktree, and whether they are running or \
                            otherwise. Use this to discover which servers are available before \
                            issuing other LSP-backed calls, or to pick a name to pass to \
                            `lsp_restart_language_server` when results from a server look broken.",
            "inputSchema": { "type": "object", "properties": {} },
        }),
        json!({
            "name": "lsp_restart_language_server",
            "description": "Restart the language server(s) with the given name across the project. \
                            Use SPARINGLY — restart is expensive (rust-analyzer can take 30-60s \
                            to re-index a moderate project) and the user's IDE features for that \
                            language go cold during the restart. Reach for this only when:\n\
                              - LSP responses are clearly inconsistent across calls,\n\
                              - You explicitly changed Zed/LSP configuration mid-session,\n\
                              - Diagnostics have not refreshed despite save + wait.\n\
                            Do NOT use as a first-line response to stale-looking results — save \
                            the file via `apply_text_edit` and retry first. The `reason` arg is \
                            recorded for the user so they can see why their LSP restarted.\n\
                            \n\
                            Get the exact server name from `lsp_list_language_servers`. If \
                            multiple instances of that name exist (e.g. one per worktree), all of \
                            them are restarted.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Language server name as reported by lsp_list_language_servers (e.g. 'rust-analyzer')." },
                    "reason": { "type": "string", "description": "Short justification visible to the user (e.g. 'diagnostics stale after large refactor')." },
                },
                "required": ["name", "reason"],
            },
        }),
        json!({
            "name": "read_buffer",
            "description": "Read a file's content as Zed currently sees it, including any \
                            unsaved buffer edits made by the user. Prefer this over the disk-only \
                            Read tool for files that are likely open in Zed: it sees in-memory \
                            edits that disk reads miss, which matters when LSP-backed tools \
                            (diagnostics, find_references) report findings that reference the \
                            buffer state.\n\
                            \n\
                            Path accepts: project-relative (e.g. `crates/agent/src/lib.rs`), \
                            absolute (e.g. `/etc/hosts`), or tilde-prefixed home paths (e.g. \
                            `~/.zshrc`). For files outside the project, Zed opens an ephemeral \
                            single-file buffer just for the read — no long-lived editor tab is \
                            created.\n\
                            \n\
                            Optionally supply a 1-based [start_line, end_line] range to limit \
                            the returned slice. Omit for the whole file.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file_path": { "type": "string", "description": "Relative project path." },
                    "start_line": { "type": "integer", "description": "Optional 1-based line to start from." },
                    "end_line": { "type": "integer", "description": "Optional inclusive 1-based line to end at." },
                },
                "required": ["file_path"],
            },
        }),
        json!({
            "name": "apply_text_edit",
            "description": "Apply a text edit to Zed's in-memory buffer for a project file. \
                            Prefer this over the disk-only Edit tool for files that are open in \
                            Zed: writing to disk would not update the buffer, so LSP-backed \
                            tools (diagnostics, find_references) would see stale state until the \
                            user manually reloads or saves.\n\
                            \n\
                            Semantics: locate the unique occurrence of `old_string` in the buffer \
                            and replace it with `new_string`. Errors if the string is not found \
                            or appears multiple times (use a larger snippet to disambiguate).\n\
                            \n\
                            Save behaviour: the resulting buffer is always saved to disk through \
                            Zed's format-on-save pipeline. If the buffer had unsaved user edits, \
                            those edits are saved together with this edit (they are part of the \
                            buffer state that `old_string` was matched against, so this is also \
                            the only coherent outcome). Saving is required for language servers \
                            that refresh diagnostics on `didSave` (e.g. rust-analyzer's \
                            `checkOnSave`) to see your change.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file_path": { "type": "string", "description": "Relative project path." },
                    "old_string": { "type": "string", "description": "Exact text to replace. Must appear exactly once in the buffer." },
                    "new_string": { "type": "string", "description": "Replacement text." },
                },
                "required": ["file_path", "old_string", "new_string"],
            },
        }),
    ]
}

#[derive(Deserialize)]
struct ToolsCallParams {
    name: String,
    #[serde(default)]
    arguments: Value,
}

/// Output produced by a bridge tool. Most tools return plain text; edit-
/// producing tools also attach structured metadata (`_meta.zed_bridge`) so
/// that Zed's local AcpThread rewriter can present them like the built-in
/// Edit tool (diff renderer, default expanded, etc.).
pub(crate) enum ToolOutput {
    Text(String),
    Rich {
        text: String,
        meta: serde_json::Value,
    },
}

impl From<String> for ToolOutput {
    fn from(s: String) -> Self {
        ToolOutput::Text(s)
    }
}

/// A single before/after pair for one file modified by an edit-producing
/// tool. Travels through the in-process side-channel to the rewriter, where
/// `path` backs the diff view header and `abs_path` + `line` populate
/// `tool_call.locations` so the UI shows a clickable "Go to File" header
/// that lands on the edited line.
#[derive(Serialize, Clone)]
pub(crate) struct BridgeDiff {
    pub path: String,
    pub abs_path: String,
    /// 0-based row where the edit begins in `new_text`. `None` when the
    /// edit is whole-file (e.g. formatting), in which case the jump lands
    /// at the top of the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    pub old_text: String,
    pub new_text: String,
}

async fn handle_tools_call(state: &BridgeState, params: Value) -> Result<Value, JsonRpcError> {
    let ToolsCallParams { name, arguments } = serde_json::from_value(params)
        .map_err(|err| JsonRpcError::invalid_params(format!("invalid tools/call params: {err}")))?;

    let kind_hint = kind_for_tool(&name);
    let op = build_op(&name, arguments)?;
    let (reply_tx, reply_rx) = oneshot::channel::<Result<ToolOutput, String>>();

    state
        .dispatch_tx
        .unbounded_send(Box::new(LspBridgeForegroundWork {
            op,
            reply: Some(reply_tx),
        }))
        .map_err(|_| JsonRpcError::internal("dispatch queue closed"))?;

    let result = reply_rx
        .await
        .map_err(|_| JsonRpcError::internal("foreground dispatch dropped reply"))?;

    let output = match result {
        Ok(output) => output,
        Err(err) => return Ok(error_content(err)),
    };

    let (text, extra_meta) = match output {
        ToolOutput::Text(text) => (text, None),
        ToolOutput::Rich { text, meta } => (text, Some(meta)),
    };

    // Build the side-channel payload. Edit-producing tools merge in extra
    // fields (`saved`, `diffs`, ...) from `ToolOutput::Rich`. We do *not*
    // ship this inside the MCP response: external agents (claude-acp ...)
    // strip MCP `_meta` and truncate large response bodies, so the bridge
    // and the rewriter communicate directly in-process via
    // `BridgeResultQueue`.
    let mut payload = serde_json::Map::new();
    payload.insert("version".to_string(), json!(1));
    payload.insert("kind".to_string(), json!(kind_hint));
    payload.insert("tool".to_string(), json!(name));
    if name == "read_buffer" {
        payload.insert("text".to_string(), json!(text));
    }
    if let Some(extra) = extra_meta
        && let Some(extra_zb) = extra.get("zed_bridge").and_then(|v| v.as_object())
    {
        for (k, v) in extra_zb {
            payload.insert(k.clone(), v.clone());
        }
    }
    enqueue_bridge_result(
        &state.result_queue,
        BridgeResultEntry {
            tool: name.clone(),
            payload: serde_json::Value::Object(payload),
        },
    );

    Ok(json!({
        "content": [{ "type": "text", "text": text }],
        "isError": false,
    }))
}

/// Map a bridge tool name to the `acp::ToolKind` Zed should display it as.
/// This is consumed by the rewriter in `crates/acp_thread/`.
fn kind_for_tool(name: &str) -> &'static str {
    match name {
        "apply_text_edit"
        | "lsp_format_document"
        | "lsp_rename_symbol"
        | "lsp_apply_code_action" => "edit",
        "lsp_find_references" | "lsp_workspace_symbol" => "search",
        "lsp_diagnostics" | "lsp_hover" | "read_buffer" | "lsp_list_language_servers" => "read",
        _ => "other",
    }
}

fn error_content(err: String) -> Value {
    json!({
        "content": [{ "type": "text", "text": err }],
        "isError": true,
    })
}

/// Build a `ToolOutput::Rich` for an edit-producing tool.
///
/// The `text` body is the agent-facing summary; we keep it short on purpose
/// because the LLM does not need a full diff in its context, and large
/// response bodies are truncated by some agents anyway. The structured
/// `diffs` ride the in-process side-channel directly to the rewriter where
/// they back the real diff view; they never travel through the agent.
fn bridge_diffs_from_agent_lsp(diffs: Vec<agent_lsp::EditDiff>) -> Vec<BridgeDiff> {
    diffs
        .into_iter()
        .map(|diff| BridgeDiff {
            path: diff.path,
            abs_path: diff.abs_path,
            line: diff.line,
            old_text: diff.old_text,
            new_text: diff.new_text,
        })
        .collect()
}

fn build_edit_output(text: String, diffs: Vec<BridgeDiff>) -> ToolOutput {
    let meta = json!({
        "zed_bridge": {
            "version": 1,
            "kind": "edit",
            "diffs": diffs,
        }
    });

    ToolOutput::Rich { text, meta }
}

#[derive(Deserialize)]
struct SymbolArgs {
    file_path: String,
    line: u32,
    symbol_name: String,
}

#[derive(Deserialize)]
struct RenameArgs {
    file_path: String,
    line: u32,
    symbol_name: String,
    new_name: String,
}

#[derive(Deserialize)]
struct DiagnosticsArgs {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    min_severity: Option<String>,
}

#[derive(Deserialize)]
struct ReadBufferArgs {
    file_path: String,
    #[serde(default)]
    start_line: Option<u32>,
    #[serde(default)]
    end_line: Option<u32>,
}

#[derive(Deserialize)]
struct ApplyTextEditArgs {
    file_path: String,
    old_string: String,
    new_string: String,
}

#[derive(Deserialize)]
struct FormatDocumentArgs {
    file_path: String,
}

#[derive(Deserialize)]
struct GetCodeActionsArgs {
    file_path: String,
    line: u32,
    symbol_name: String,
}

#[derive(Deserialize)]
struct ApplyCodeActionArgs {
    index: u32,
}

#[derive(Deserialize)]
struct HoverArgs {
    file_path: String,
    line: u32,
    symbol_name: String,
}

#[derive(Deserialize)]
struct WorkspaceSymbolArgs {
    query: String,
}

#[derive(Deserialize)]
struct RestartLanguageServerArgs {
    name: String,
    reason: String,
}

pub(crate) enum LspBridgeOp {
    FindReferences {
        file_path: String,
        line: u32,
        symbol_name: String,
    },
    Diagnostics {
        path: Option<String>,
        min_severity: Option<String>,
    },
    RenameSymbol {
        file_path: String,
        line: u32,
        symbol_name: String,
        new_name: String,
    },
    ReadBuffer {
        file_path: String,
        start_line: Option<u32>,
        end_line: Option<u32>,
    },
    ApplyTextEdit {
        file_path: String,
        old_string: String,
        new_string: String,
    },
    FormatDocument {
        file_path: String,
    },
    GetCodeActions {
        file_path: String,
        line: u32,
        symbol_name: String,
    },
    ApplyCodeAction {
        index: u32,
    },
    Hover {
        file_path: String,
        line: u32,
        symbol_name: String,
    },
    WorkspaceSymbol {
        query: String,
    },
    ListLanguageServers,
    RestartLanguageServer {
        name: String,
        reason: String,
    },
}

fn build_op(name: &str, arguments: Value) -> Result<LspBridgeOp, JsonRpcError> {
    match name {
        "lsp_find_references" => {
            let args: SymbolArgs = serde_json::from_value(arguments)
                .map_err(|err| JsonRpcError::invalid_params(format!("bad arguments: {err}")))?;
            Ok(LspBridgeOp::FindReferences {
                file_path: args.file_path,
                line: args.line,
                symbol_name: args.symbol_name,
            })
        }
        "lsp_diagnostics" => {
            let args: DiagnosticsArgs = serde_json::from_value(arguments)
                .map_err(|err| JsonRpcError::invalid_params(format!("bad arguments: {err}")))?;
            Ok(LspBridgeOp::Diagnostics {
                path: args.path,
                min_severity: args.min_severity,
            })
        }
        "lsp_rename_symbol" => {
            let args: RenameArgs = serde_json::from_value(arguments)
                .map_err(|err| JsonRpcError::invalid_params(format!("bad arguments: {err}")))?;
            Ok(LspBridgeOp::RenameSymbol {
                file_path: args.file_path,
                line: args.line,
                symbol_name: args.symbol_name,
                new_name: args.new_name,
            })
        }
        "read_buffer" => {
            let args: ReadBufferArgs = serde_json::from_value(arguments)
                .map_err(|err| JsonRpcError::invalid_params(format!("bad arguments: {err}")))?;
            Ok(LspBridgeOp::ReadBuffer {
                file_path: args.file_path,
                start_line: args.start_line,
                end_line: args.end_line,
            })
        }
        "apply_text_edit" => {
            let args: ApplyTextEditArgs = serde_json::from_value(arguments)
                .map_err(|err| JsonRpcError::invalid_params(format!("bad arguments: {err}")))?;
            Ok(LspBridgeOp::ApplyTextEdit {
                file_path: args.file_path,
                old_string: args.old_string,
                new_string: args.new_string,
            })
        }
        "lsp_format_document" => {
            let args: FormatDocumentArgs = serde_json::from_value(arguments)
                .map_err(|err| JsonRpcError::invalid_params(format!("bad arguments: {err}")))?;
            Ok(LspBridgeOp::FormatDocument {
                file_path: args.file_path,
            })
        }
        "lsp_get_code_actions" => {
            let args: GetCodeActionsArgs = serde_json::from_value(arguments)
                .map_err(|err| JsonRpcError::invalid_params(format!("bad arguments: {err}")))?;
            Ok(LspBridgeOp::GetCodeActions {
                file_path: args.file_path,
                line: args.line,
                symbol_name: args.symbol_name,
            })
        }
        "lsp_apply_code_action" => {
            let args: ApplyCodeActionArgs = serde_json::from_value(arguments)
                .map_err(|err| JsonRpcError::invalid_params(format!("bad arguments: {err}")))?;
            Ok(LspBridgeOp::ApplyCodeAction { index: args.index })
        }
        "lsp_hover" => {
            let args: HoverArgs = serde_json::from_value(arguments)
                .map_err(|err| JsonRpcError::invalid_params(format!("bad arguments: {err}")))?;
            Ok(LspBridgeOp::Hover {
                file_path: args.file_path,
                line: args.line,
                symbol_name: args.symbol_name,
            })
        }
        "lsp_workspace_symbol" => {
            let args: WorkspaceSymbolArgs = serde_json::from_value(arguments)
                .map_err(|err| JsonRpcError::invalid_params(format!("bad arguments: {err}")))?;
            Ok(LspBridgeOp::WorkspaceSymbol { query: args.query })
        }
        "lsp_list_language_servers" => Ok(LspBridgeOp::ListLanguageServers),
        "lsp_restart_language_server" => {
            let args: RestartLanguageServerArgs = serde_json::from_value(arguments)
                .map_err(|err| JsonRpcError::invalid_params(format!("bad arguments: {err}")))?;
            Ok(LspBridgeOp::RestartLanguageServer {
                name: args.name,
                reason: args.reason,
            })
        }
        other => Err(JsonRpcError::method_not_found(format!(
            "unknown tool: {other}"
        ))),
    }
}

pub(crate) struct LspBridgeForegroundWork {
    op: LspBridgeOp,
    reply: Option<oneshot::Sender<Result<ToolOutput, String>>>,
}

impl crate::acp::ForegroundWorkItem for LspBridgeForegroundWork {
    fn run(mut self: Box<Self>, cx: &mut AsyncApp, ctx: &crate::acp::ClientContext) {
        let project = ctx.project.clone();
        let code_action_store = ctx.code_action_store.clone();
        let reply = self.reply.take();
        let op = self.op;
        cx.spawn(async move |cx| {
            let result = execute_op(project, code_action_store, op, cx).await;
            if let Some(reply) = reply {
                let _ = reply.send(result);
            }
        })
        .detach();
    }

    fn reject(mut self: Box<Self>) {
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(Err("foreground dispatch queue closed".to_string()));
        }
    }
}

async fn execute_op(
    project: WeakEntity<Project>,
    code_action_store: std::rc::Rc<std::cell::RefCell<Option<agent_lsp::PendingCodeActions>>>,
    op: LspBridgeOp,
    cx: &mut AsyncApp,
) -> Result<ToolOutput, String> {
    let project = project
        .upgrade()
        .ok_or_else(|| "project no longer available".to_string())?;

    match op {
        LspBridgeOp::FindReferences {
            file_path,
            line,
            symbol_name,
        } => find_references(project, file_path, line, symbol_name, cx)
            .await
            .map(Into::into),
        LspBridgeOp::Diagnostics { path, min_severity } => {
            diagnostics(project, path, min_severity, cx)
                .await
                .map(Into::into)
        }
        LspBridgeOp::RenameSymbol {
            file_path,
            line,
            symbol_name,
            new_name,
        } => rename_symbol(project, file_path, line, symbol_name, new_name, cx).await,
        LspBridgeOp::ReadBuffer {
            file_path,
            start_line,
            end_line,
        } => read_buffer(project, file_path, start_line, end_line, cx).await,
        LspBridgeOp::ApplyTextEdit {
            file_path,
            old_string,
            new_string,
        } => apply_text_edit(project, file_path, old_string, new_string, cx).await,
        LspBridgeOp::FormatDocument { file_path } => format_document(project, file_path, cx).await,
        LspBridgeOp::GetCodeActions {
            file_path,
            line,
            symbol_name,
        } => get_code_actions(project, file_path, line, symbol_name, code_action_store, cx)
            .await
            .map(Into::into),
        LspBridgeOp::ApplyCodeAction { index } => {
            apply_code_action(project, index, code_action_store, cx).await
        }
        LspBridgeOp::Hover {
            file_path,
            line,
            symbol_name,
        } => hover(project, file_path, line, symbol_name, cx)
            .await
            .map(Into::into),
        LspBridgeOp::WorkspaceSymbol { query } => {
            workspace_symbol(project, query, cx).await.map(Into::into)
        }
        LspBridgeOp::ListLanguageServers => {
            list_language_servers(project, cx).await.map(Into::into)
        }
        LspBridgeOp::RestartLanguageServer { name, reason } => {
            restart_language_server(project, name, reason, cx)
                .await
                .map(Into::into)
        }
    }
}

async fn find_references(
    project: Entity<Project>,
    file_path: String,
    line: u32,
    symbol_name: String,
    cx: &mut AsyncApp,
) -> Result<String, String> {
    let lease = agent_lsp::LspBufferLease::default();
    let result = agent_lsp::find_references(
        project,
        &lease,
        agent_lsp::SymbolLocator::new(file_path, line, symbol_name),
        cx,
    )
    .await;
    lease.release(cx);
    result
}

async fn diagnostics(
    project: Entity<Project>,
    path: Option<String>,
    min_severity: Option<String>,
    cx: &mut AsyncApp,
) -> Result<String, String> {
    let lease = agent_lsp::LspBufferLease::default();
    let result = agent_lsp::diagnostics(project, &lease, path, min_severity, cx).await;
    lease.release(cx);
    result
}

async fn rename_symbol(
    project: Entity<Project>,
    file_path: String,
    line: u32,
    symbol_name: String,
    new_name: String,
    cx: &mut AsyncApp,
) -> Result<ToolOutput, String> {
    let lease = agent_lsp::LspBufferLease::default();
    let result = agent_lsp::rename_symbol(
        project,
        &lease,
        agent_lsp::SymbolLocator::new(file_path, line, symbol_name),
        new_name,
        cx,
    )
    .await;
    lease.release(cx);
    match result? {
        agent_lsp::EditOperationOutput::Text(text) => Ok(ToolOutput::Text(text)),
        agent_lsp::EditOperationOutput::Edited(output) => Ok(build_edit_output(
            output.text,
            bridge_diffs_from_agent_lsp(output.diffs),
        )),
    }
}

async fn read_buffer(
    project: Entity<Project>,
    file_path: String,
    start_line: Option<u32>,
    end_line: Option<u32>,
    cx: &mut AsyncApp,
) -> Result<ToolOutput, String> {
    let buffer = open_buffer(&project, &file_path, cx)
        .await
        .map_err(|err| format!("Failed to open '{file_path}': {err}"))?;

    let (text, dirty, total_lines, abs_path, jump_row) = buffer.read_with(cx, |buffer, cx| {
        let snapshot = buffer.snapshot();
        let max_row = snapshot.max_point().row;
        let start_row = start_line
            .map(|l| l.saturating_sub(1))
            .unwrap_or(0)
            .min(max_row);
        let end_row = end_line
            .map(|l| l.saturating_sub(1))
            .unwrap_or(max_row)
            .min(max_row);
        let (start_row, end_row) = if start_row > end_row {
            (end_row, start_row)
        } else {
            (start_row, end_row)
        };
        let start = Point::new(start_row, 0);
        let end = Point::new(end_row, snapshot.line_len(end_row));
        let text = snapshot.text_for_range(start..end).collect::<String>();
        let abs_path = buffer
            .file()
            .and_then(|f| f.as_local().map(|local| local.abs_path(cx)))
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        (text, buffer.is_dirty(), max_row + 1, abs_path, start_row)
    });

    let mut output = String::new();
    if dirty {
        let _ = writeln!(
            output,
            "(buffer is dirty — shown content includes unsaved user edits)"
        );
    }
    let _ = writeln!(output, "{file_path} ({total_lines} lines)");
    // Tag the fence with the file path (not a bare ```), so Zed's markdown
    // renderer treats the info string as a path, resolves the language from
    // its extension, and applies syntax highlighting — matching the
    // built-in read_file tool. `MarkdownCodeBlock` also widens the fence if
    // the content itself contains backticks.
    output.push_str(
        &MarkdownCodeBlock {
            tag: &file_path,
            text: &text,
        }
        .to_string(),
    );

    // Carry the file location through the side-channel so the rewriter can
    // surface a clickable "Go to File" header (matching the built-in
    // read_file tool), landing on the first read line. `path` (relative)
    // becomes the tool-call label so the card reads the file path instead
    // of the raw `…/read_buffer` tool name.
    let meta = json!({
        "zed_bridge": {
            "path": file_path,
            "abs_path": abs_path,
            "line": jump_row,
        }
    });
    Ok(ToolOutput::Rich { text: output, meta })
}

async fn apply_text_edit(
    project: Entity<Project>,
    file_path: String,
    old_string: String,
    new_string: String,
    cx: &mut AsyncApp,
) -> Result<ToolOutput, String> {
    if old_string.is_empty() {
        return Err("old_string must not be empty".to_string());
    }

    let open_buffer_task = project.update(cx, |project, cx| {
        let Some(project_path) = project.find_project_path(&file_path, cx) else {
            return Err(format!("Could not find path '{file_path}' in project"));
        };
        Ok(project.open_buffer(project_path, cx))
    })?;

    let buffer = open_buffer_task
        .await
        .map_err(|err| format!("Failed to open '{file_path}': {err}"))?;

    let edit_range = buffer.read_with(cx, |buffer, _cx| {
        let text = buffer.text();
        let occurrences = text.matches(&old_string).count();
        match occurrences {
            0 => Err(format!(
                "`old_string` not found in `{file_path}`. The buffer may differ \
                 from what you expected — use `read_buffer` to inspect it."
            )),
            1 => {
                let start = text.find(&old_string).expect("verified by count above");
                let end = start + old_string.len();
                Ok(start..end)
            }
            n => Err(format!(
                "`old_string` appears {n} times in `{file_path}`. Provide a larger \
                 snippet so the target location is unique."
            )),
        }
    })?;

    let (old_full_text, abs_path) = buffer.read_with(cx, |buffer, cx| {
        let abs_path = buffer
            .file()
            .and_then(|f| f.as_local().map(|local| local.abs_path(cx)))
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        (buffer.text(), abs_path)
    });

    // 0-based row of the edit start in the pre-edit buffer. Used to make
    // the tool-call's "Go to File" header land on the edited line.
    let line = old_full_text[..edit_range.start].matches('\n').count() as u32;

    buffer.update(cx, |buffer, cx| {
        buffer.edit([(edit_range, new_string.as_str())], None, cx);
    });

    let buffers = HashSet::from_iter([buffer.clone()]);
    project
        .update(cx, |project, cx| project.save_buffers(buffers, cx))
        .await
        .map_err(|err| format!("Edit applied in-memory but saving `{file_path}` failed: {err}"))?;

    let new_full_text = buffer.read_with(cx, |buffer, _cx| buffer.text());

    let summary = format!(
        "Edit applied to `{file_path}` and saved to disk. Format-on-save \
         (if configured) has run."
    );

    Ok(build_edit_output(
        summary,
        vec![BridgeDiff {
            path: file_path.clone(),
            abs_path,
            line: Some(line),
            old_text: old_full_text,
            new_text: new_full_text,
        }],
    ))
}

async fn format_document(
    project: Entity<Project>,
    file_path: String,
    cx: &mut AsyncApp,
) -> Result<ToolOutput, String> {
    let lease = agent_lsp::LspBufferLease::default();
    let result = agent_lsp::format_document(project, &lease, file_path, cx).await;
    lease.release(cx);
    match result? {
        agent_lsp::EditOperationOutput::Text(text) => Ok(ToolOutput::Text(text)),
        agent_lsp::EditOperationOutput::Edited(output) => Ok(build_edit_output(
            output.text,
            bridge_diffs_from_agent_lsp(output.diffs),
        )),
    }
}

async fn get_code_actions(
    project: Entity<Project>,
    file_path: String,
    line: u32,
    symbol_name: String,
    code_action_store: std::rc::Rc<std::cell::RefCell<Option<agent_lsp::PendingCodeActions>>>,
    cx: &mut AsyncApp,
) -> Result<String, String> {
    let symbol = agent_lsp::SymbolLocator::new(file_path, line, symbol_name);
    let lease = agent_lsp::LspBufferLease::default();
    let output =
        agent_lsp::get_code_actions(project, &lease, symbol, "lsp_apply_code_action", cx).await;
    lease.release(cx);
    let output = output?;
    *code_action_store.borrow_mut() = output.pending;
    Ok(output.text)
}

async fn apply_code_action(
    project: Entity<Project>,
    index: u32,
    code_action_store: std::rc::Rc<std::cell::RefCell<Option<agent_lsp::PendingCodeActions>>>,
    cx: &mut AsyncApp,
) -> Result<ToolOutput, String> {
    let pending = code_action_store
        .borrow_mut()
        .take()
        .ok_or_else(|| "No code actions cached. Call `lsp_get_code_actions` first.".to_string())?;

    let lease = agent_lsp::LspBufferLease::default();
    let result = agent_lsp::apply_code_action(project, &lease, index, pending, cx).await;
    lease.release(cx);
    match result? {
        agent_lsp::EditOperationOutput::Text(text) => Ok(ToolOutput::Text(text)),
        agent_lsp::EditOperationOutput::Edited(output) => Ok(build_edit_output(
            output.text,
            bridge_diffs_from_agent_lsp(output.diffs),
        )),
    }
}

async fn hover(
    project: Entity<Project>,
    file_path: String,
    line: u32,
    symbol_name: String,
    cx: &mut AsyncApp,
) -> Result<String, String> {
    let lease = agent_lsp::LspBufferLease::default();
    let result = agent_lsp::hover(
        project,
        &lease,
        agent_lsp::SymbolLocator::new(file_path, line, symbol_name),
        cx,
    )
    .await;
    lease.release(cx);
    result
}

async fn workspace_symbol(
    project: Entity<Project>,
    query: String,
    cx: &mut AsyncApp,
) -> Result<String, String> {
    agent_lsp::workspace_symbol(project, query, cx).await
}

async fn list_language_servers(
    project: Entity<Project>,
    cx: &mut AsyncApp,
) -> Result<String, String> {
    agent_lsp::list_language_servers(project, cx).await
}

async fn restart_language_server(
    project: Entity<Project>,
    name: String,
    reason: String,
    cx: &mut AsyncApp,
) -> Result<String, String> {
    agent_lsp::restart_language_server(project, name, reason, cx).await
}

/// Open a buffer for `file_path`, accepting:
///   * project-relative paths (e.g. `crates/agent/src/lib.rs`)
///   * absolute paths (e.g. `/etc/hosts`)
///   * tilde-prefixed home paths (e.g. `~/.zshrc`)
///
/// Project-relative paths resolve via `find_project_path` so that the existing
/// buffer (with any unsaved user edits) is returned when the file is open in
/// Zed. Absolute / tilde paths fall through to `open_local_buffer`, which
/// creates an ephemeral single-file worktree if the path is outside every
/// existing worktree — effectively a buffer-aware disk read.
async fn open_buffer(
    project: &Entity<Project>,
    file_path: &str,
    cx: &mut AsyncApp,
) -> anyhow::Result<Entity<Buffer>> {
    use anyhow::anyhow;

    let expanded = expand_tilde(file_path);

    let task = project.update(cx, |project, cx| {
        if let Some(project_path) = project.find_project_path(&expanded, cx) {
            return Ok(project.open_buffer(project_path, cx));
        }
        if expanded.is_absolute() {
            return Ok(project.open_local_buffer(&expanded, cx));
        }
        Err(anyhow!(
            "'{}' is neither a project-relative path nor absolute. Pass an \
             absolute path or a path beginning with `~/` to read files outside \
             this project.",
            file_path
        ))
    })?;
    task.await
}

fn expand_tilde(file_path: &str) -> std::path::PathBuf {
    if let Some(rest) = file_path.strip_prefix("~/") {
        util::paths::home_dir().join(rest)
    } else if file_path == "~" {
        util::paths::home_dir().clone()
    } else {
        std::path::PathBuf::from(file_path)
    }
}
