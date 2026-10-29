//! MCP (Model Context Protocol) server: lets AI tools read and edit the open workbook.
//!
//! HTTP clients (Claude Code, Cursor, VS Code, ...) talk to `POST /mcp` on 127.0.0.1 with a
//! bearer token. Clients that only speak stdio (Claude Desktop, Codex, ...) launch this same
//! binary with `--mcp-stdio`, which relays every message to that endpoint. Both paths end in
//! `handle_message`, so the tools behave the same everywhere. The tools act on the workbook of
//! the window that last had focus. Every edit lands in the undo history like a user edit and is
//! pushed to that window through the `workbook-changed` event.

use std::{
    collections::HashMap,
    fs,
    io::{self, BufRead, Read, Write},
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
    ops::DerefMut,
    panic::{self, AssertUnwindSafe},
    sync::{Arc, Mutex},
    thread::JoinHandle,
    time::Duration,
};

use ironcalc::base::{
    cell::CellValue,
    expressions::types::Area,
    formatter::lexer::is_likely_date_number_format,
    types::{BorderItem, BorderStyle, CellType, HorizontalAlignment, Style, VerticalAlignment},
    BorderArea,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager, State};
use tiny_http::{Header, Method, Request, Response, Server};

use crate::{info, AppState, CmdResult, Doc, DocGuard, LockExt, WorkbookInfo};

pub const DEFAULT_PORT: u16 = 7424;
const MAX_BODY: usize = 8 * 1024 * 1024;
const MAX_READ_CELLS: i64 = 20_000;
const MAX_WRITE_CELLS: i64 = 50_000;
const MAX_FORMAT_CELLS: i64 = 250_000;
const MAX_ROW: i32 = 1_048_576;
const MAX_COL: i32 = 16_384;

/// Protocol revisions we can speak: the tools-only subset is identical in all of them.
const PROTOCOLS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// Same as `identifier` in tauri.conf.json. The stdio bridge runs without Tauri, so it finds
/// the config at the path Tauri's `app_config_dir()` would give.
const APP_ID: &str = "com.jetorbit.spritsit";

// -------------------------------------------------------------------- config

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
struct Config {
    enabled: bool,
    port: u16,
    token: String,
}

impl Default for Config {
    fn default() -> Self {
        Config { enabled: false, port: DEFAULT_PORT, token: String::new() }
    }
}

fn config_path() -> CmdResult<PathBuf> {
    dirs::config_dir()
        .map(|d| d.join(APP_ID).join("mcp.json"))
        .ok_or_else(|| "No config directory".to_string())
}

fn new_token() -> CmdResult<String> {
    let mut bytes = [0u8; 24];
    getrandom::fill(&mut bytes).map_err(|e| format!("No secure random source: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

impl Config {
    fn load() -> Config {
        config_path()
            .ok()
            .and_then(|p| fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn save(&self) -> CmdResult<()> {
        self.save_to(&config_path()?)
    }

    /// The token is a credential, so the file is private to the user.
    fn save_to(&self, path: &Path) -> CmdResult<()> {
        fs::create_dir_all(path.parent().unwrap_or(Path::new("."))).map_err(|e| e.to_string())?;
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        opts.open(path)
            .and_then(|mut f| f.write_all(json.as_bytes()))
            .map_err(|e| format!("Could not save MCP settings: {e}"))
    }
}

// ------------------------------------------------------------------- runtime

/// What the app needs from the outside world, so the server can run against a test double.
pub(crate) trait Host: Send + Sync + 'static {
    type Guard<'a>: DerefMut<Target = Doc>
    where
        Self: 'a;
    fn lock(&self) -> Self::Guard<'_>;
    /// Called after a tool changed the workbook, with the lock released.
    fn changed(&self, change: Change);
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Change {
    /// cells, formats or sheets changed
    Edited,
    /// a different workbook was loaded
    Replaced,
}

pub struct TauriHost(pub AppHandle);

#[derive(Serialize, Clone)]
struct ChangeEvent {
    replaced: bool,
    info: WorkbookInfo,
}

impl Host for TauriHost {
    type Guard<'a> = DocGuard<'a>;

    fn lock(&self) -> DocGuard<'_> {
        self.0.state::<AppState>().inner().active_doc()
    }

    fn changed(&self, change: Change) {
        let state = self.0.state::<AppState>();
        let label = state.active_label();
        // every window was closed: show the workbook the tool just changed
        if self.0.get_webview_window(&label).is_none() {
            let _ = crate::build_window(&self.0, label, None);
            return;
        }
        let info = info(&state.doc(&label));
        let _ = self.0.emit_to(
            label.as_str(),
            "workbook-changed",
            ChangeEvent { replaced: change == Change::Replaced, info },
        );
    }
}

struct Running {
    server: Arc<Server>,
    thread: Option<JoinHandle<()>>,
}

impl Running {
    fn start<H: Host>(host: Arc<H>, port: u16, token: Arc<Mutex<String>>) -> CmdResult<(Running, u16)> {
        let server = Server::http(("127.0.0.1", port)).map_err(|e| {
            let in_use = e
                .downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::AddrInUse);
            if in_use {
                format!("Port {port} is already in use. Choose another port.")
            } else {
                format!("Could not listen on port {port}: {e}")
            }
        })?;
        let bound = server
            .server_addr()
            .to_ip()
            .map_or(port, |a| a.port());
        let server = Arc::new(server);
        let accept = server.clone();
        let thread = std::thread::spawn(move || {
            for req in accept.incoming_requests() {
                let (host, token) = (host.clone(), token.clone());
                // one thread per request: a client stalling mid-body must not block the others
                std::thread::spawn(move || handle_http(&*host, &token, bound, req));
            }
        });
        Ok((Running { server, thread: Some(thread) }, bound))
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.server.unblock();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Settings plus the live server; kept in `AppState`.
pub struct Runtime {
    config: Config,
    token: Arc<Mutex<String>>,
    running: Option<Running>,
    error: Option<String>,
}

#[derive(Serialize)]
pub struct Status {
    enabled: bool,
    running: bool,
    port: u16,
    url: String,
    token: String,
    /// this executable; stdio clients run it with `--mcp-stdio`
    command: String,
    error: Option<String>,
}

impl Runtime {
    pub fn load() -> Runtime {
        let mut config = Config::load();
        if config.token.is_empty() {
            config.token = new_token().unwrap_or_default();
        }
        Runtime {
            token: Arc::new(Mutex::new(config.token.clone())),
            config,
            running: None,
            error: None,
        }
    }

    fn start<H: Host>(&mut self, host: Arc<H>, port: u16) -> CmdResult<()> {
        if self.config.token.is_empty() {
            self.config.token = new_token()?;
            *self.token.lock_recovering() = self.config.token.clone();
        }
        // let go of the old listener first, or restarting on the same port would collide with it
        self.running = None;
        let (running, bound) = Running::start(host, port, self.token.clone())?;
        let previous = self.config.clone();
        self.config.enabled = true;
        self.config.port = bound;
        if let Err(e) = self.config.save() {
            self.config = previous;
            return Err(e);
        }
        self.running = Some(running);
        self.error = None;
        Ok(())
    }

    fn disable(&mut self) -> CmdResult<()> {
        self.running = None;
        self.error = None;
        self.config.enabled = false;
        self.config.save()
    }

    fn regenerate_token(&mut self) -> CmdResult<()> {
        let token = new_token()?;
        self.config.token = token.clone();
        self.config.save()?;
        // requests already carrying the old token are refused from now on
        *self.token.lock_recovering() = token;
        Ok(())
    }

    fn status(&self) -> Status {
        let port = self.config.port;
        Status {
            enabled: self.config.enabled,
            running: self.running.is_some(),
            port,
            url: format!("http://127.0.0.1:{port}/mcp"),
            token: self.config.token.clone(),
            command: std::env::current_exe()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            error: self.error.clone(),
        }
    }
}

/// Starts the server at launch when the user left it on.
pub fn autostart(app: &AppHandle) {
    let state = app.state::<AppState>();
    let mut rt = state.mcp.lock_recovering();
    if rt.config.enabled {
        let port = rt.config.port;
        if let Err(e) = rt.start(Arc::new(TauriHost(app.clone())), port) {
            rt.error = Some(e);
        }
    }
}

#[tauri::command]
pub fn mcp_status(state: State<AppState>) -> Status {
    state.mcp.lock_recovering().status()
}

#[tauri::command]
pub fn mcp_set_enabled(
    enabled: bool,
    port: Option<u16>,
    app: AppHandle,
    state: State<AppState>,
) -> CmdResult<Status> {
    let mut rt = state.mcp.lock_recovering();
    if enabled {
        let port = port.unwrap_or(rt.config.port);
        if port < 1024 {
            return Err("Choose a port between 1024 and 65535".into());
        }
        rt.start(Arc::new(TauriHost(app)), port)?;
    } else {
        rt.disable()?;
    }
    Ok(rt.status())
}

#[tauri::command]
pub fn mcp_regenerate_token(state: State<AppState>) -> CmdResult<Status> {
    let mut rt = state.mcp.lock_recovering();
    rt.regenerate_token()?;
    Ok(rt.status())
}

// ---------------------------------------------------------------------- HTTP

fn header(req: &Request, name: &'static str) -> Option<String> {
    req.headers()
        .iter()
        .find(|h| h.field.equiv(name))
        .map(|h| h.value.as_str().to_string())
}

fn respond(req: Request, status: u16, body: String, headers: &[(&str, &str)]) {
    let mut res = Response::from_string(body)
        .with_status_code(status)
        // every body is in memory; chunked encoding would only complicate the stdio bridge
        .with_chunked_threshold(usize::MAX);
    let fixed = [("Content-Type", "application/json"), ("Cache-Control", "no-store")];
    for (k, v) in fixed.iter().chain(headers) {
        if let Ok(h) = Header::from_bytes(k.as_bytes(), v.as_bytes()) {
            res.add_header(h);
        }
    }
    let _ = req.respond(res);
}

fn fail(req: Request, status: u16, message: &str, headers: &[(&str, &str)]) {
    respond(req, status, rpc_error(Value::Null, -32000, message).to_string(), headers);
}

/// DNS-rebinding guard: only requests addressed to this very listener are served.
fn host_ok(host: Option<&str>, port: u16) -> bool {
    let Some(host) = host else { return false };
    let host = host.to_ascii_lowercase();
    [format!("127.0.0.1:{port}"), format!("localhost:{port}"), format!("[::1]:{port}")]
        .contains(&host)
}

fn bearer_ok(auth: Option<&str>, token: &str) -> bool {
    let Some(auth) = auth else { return false };
    let Some(given) = auth.get(7..).filter(|_| auth[..7].eq_ignore_ascii_case("bearer ")) else {
        return false;
    };
    let (a, b) = (given.trim().as_bytes(), token.as_bytes());
    // compare in constant time so the token cannot be guessed byte by byte
    !b.is_empty() && a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

fn handle_http<H: Host>(host: &H, token: &Mutex<String>, port: u16, mut req: Request) {
    if !host_ok(header(&req, "Host").as_deref(), port) {
        return fail(req, 403, "Forbidden host", &[]);
    }
    // Browsers always send Origin on cross-site requests; MCP clients never do.
    // Refusing it keeps web pages from driving the spreadsheet.
    if header(&req, "Origin").is_some() {
        return fail(req, 403, "Forbidden origin", &[]);
    }
    let token = token.lock_recovering().clone();
    if !bearer_ok(header(&req, "Authorization").as_deref(), &token) {
        let msg = "Missing or wrong access token. Copy the connection settings from the MCP panel in SheetBro.";
        return fail(req, 401, msg, &[("WWW-Authenticate", "Bearer realm=\"SheetBro\"")]);
    }
    if req.url().split('?').next() != Some("/mcp") {
        return fail(req, 404, "Not found. The MCP endpoint is /mcp", &[]);
    }
    // Streamable HTTP: no server-initiated stream (GET) and no sessions (DELETE)
    if *req.method() != Method::Post {
        return fail(req, 405, "Use POST", &[("Allow", "POST")]);
    }

    let mut body = Vec::new();
    let read = req.as_reader().take(MAX_BODY as u64 + 1).read_to_end(&mut body);
    if read.is_err() {
        return fail(req, 400, "Could not read the request body", &[]);
    }
    if body.len() > MAX_BODY {
        return fail(req, 413, "Request too large", &[]);
    }
    let reply = match serde_json::from_slice::<Value>(&body) {
        Err(_) => Some(rpc_error(Value::Null, -32700, "Parse error")),
        Ok(Value::Array(batch)) if batch.is_empty() => {
            Some(rpc_error(Value::Null, -32600, "Invalid Request"))
        }
        Ok(Value::Array(batch)) => {
            let replies: Vec<Value> = batch.iter().filter_map(|m| handle_message(host, m)).collect();
            (!replies.is_empty()).then(|| Value::Array(replies))
        }
        Ok(msg) => handle_message(host, &msg),
    };
    match reply {
        Some(v) => respond(req, 200, v.to_string(), &[]),
        // notifications and responses are acknowledged without a body
        None => respond(req, 202, String::new(), &[]),
    }
}

// ------------------------------------------------------------------ JSON-RPC

fn rpc_error(id: Value, code: i32, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// Answers one JSON-RPC message; `None` for notifications and client responses.
fn handle_message<H: Host>(host: &H, msg: &Value) -> Option<Value> {
    let Some(obj) = msg.as_object() else {
        return Some(rpc_error(Value::Null, -32600, "Invalid Request"));
    };
    let id = obj.get("id").cloned();
    let Some(method) = obj.get("method").and_then(Value::as_str) else {
        // an answer to a request we never sent
        if obj.contains_key("result") || obj.contains_key("error") {
            return None;
        }
        return Some(rpc_error(id.unwrap_or(Value::Null), -32600, "Invalid Request"));
    };
    // no id: a notification such as notifications/initialized
    let id = id?;
    let params = obj.get("params").unwrap_or(&Value::Null);
    let result = match method {
        "initialize" => Ok(initialize(params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_defs() })),
        "tools/call" => call_tool(host, params),
        _ => Err((-32601, format!("Method not found: {method}"))),
    };
    Some(match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => rpc_error(id, code, &message),
    })
}

fn initialize(params: &Value) -> Value {
    let wanted = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("");
    let version = PROTOCOLS.iter().find(|v| **v == wanted).unwrap_or(&PROTOCOLS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "sheetbro", "title": "SheetBro", "version": env!("CARGO_PKG_VERSION") },
        "instructions": "SheetBro is a spreadsheet app. These tools read and edit the workbook the user \
            has open, live in their window (the one in front when several are open). Cells use A1 \
            notation; a value starting with '=' is a formula. \
            Call workbook_info first to see the sheets and how far the data extends. Every edit is one \
            undo step (the user can press Cmd+Z, or you can call undo)."
    })
}

fn call_tool<H: Host>(host: &H, params: &Value) -> Result<Value, (i32, String)> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or((-32602, "Missing tool name".to_string()))?;
    if !TOOL_NAMES.contains(&name) {
        return Err((-32602, format!("Unknown tool: {name}")));
    }
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
    let outcome = guarded(|| {
        let mut doc = host.lock();
        let mut out = run_tool(&mut doc, name, &args)?;
        // so the AI can tell the user, or save, before it walks away from an edited workbook
        if out.change == Some(Change::Edited) {
            if let Some(data) = out.data.as_object_mut() {
                data.entry("unsaved_changes").or_insert(json!(doc.dirty));
            }
        }
        Ok(out)
    });
    // Tool failures are results, not protocol errors, so the model sees them and can retry
    Ok(match outcome {
        Ok(out) => {
            if let Some(change) = out.change {
                host.changed(change);
            }
            tool_result(out.data.to_string(), false)
        }
        Err(message) => tool_result(message, true),
    })
}

/// Runs a tool and turns a panic inside it into an error result. A failing tool must not take
/// the whole app down, and with it the workbook the user has not saved.
fn guarded<T>(f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    panic::catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| {
        Err("Internal error while running this tool. The workbook may be in an unexpected state: check it \
             with read_range, or use undo."
            .to_string())
    })
}

fn tool_result(text: String, is_error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

// --------------------------------------------------------------------- tools

const TOOL_NAMES: [&str; 17] = [
    "workbook_info",
    "read_range",
    "write_range",
    "clear_range",
    "format_range",
    "set_layout",
    "insert_lines",
    "delete_lines",
    "add_sheet",
    "rename_sheet",
    "move_sheet",
    "delete_sheet",
    "batch",
    "undo",
    "redo",
    "open_workbook",
    "save_workbook",
];

enum Hint {
    Read,
    /// changes the workbook but loses nothing that undo cannot bring back
    Safe,
    Destructive,
}

fn tool(name: &str, title: &str, description: &str, props: Value, required: &[&str], hint: Hint) -> Value {
    let (read_only, destructive) = match hint {
        Hint::Read => (true, false),
        Hint::Safe => (false, false),
        Hint::Destructive => (false, true),
    };
    json!({
        "name": name,
        "title": title,
        "description": description,
        "inputSchema": { "type": "object", "properties": props, "required": required },
        "annotations": {
            "title": title,
            "readOnlyHint": read_only,
            "destructiveHint": destructive,
            "openWorldHint": false,
        },
    })
}

fn tool_defs() -> Vec<Value> {
    let sheet = json!({ "type": "string", "description": "Sheet name. Defaults to the first sheet." });
    let range = |what: &str| {
        json!({
            "type": "string",
            "description": format!("{what} in A1 notation: B2, B2:D10, A:C (whole columns) or 1:5 (whole rows)."),
        })
    };
    // several ranges in one value save a round trip per range
    let ranges = |what: &str| {
        json!({
            "type": "string",
            "description": format!(
                "{what} in A1 notation: B2, B2:D10, A:C (whole columns) or 1:5 (whole rows). Several ranges \
                 can be given at once, separated by commas: \"A1:H1,A9:H9\"."
            ),
        })
    };
    let flag = |what: &str| json!({ "type": "boolean", "description": what });
    let color = |what: &str| json!({ "type": "string", "description": format!("{what} as #RRGGBB.") });
    vec![
        tool(
            "workbook_info",
            "Workbook info",
            "Name, file path, unsaved-changes flag and the sheets of the workbook open in SheetBro, with \
             how many rows and columns of each are in use. Call this first.",
            json!({}),
            &[],
            Hint::Read,
        ),
        tool(
            "read_range",
            "Read cells",
            "Reads a range as a grid of text, as displayed in the app (e.g. \"$1,200.00\", \"15%\"). Set \
             formulas=true to get the underlying formulas (\"=SUM(A1:A9)\") instead. Set typed=true to get \
             real JSON types instead of text: numbers, strings, booleans, null for empty cells, \
             {\"date\": \"2026-10-29\"} for dates and {\"error\": \"#NAME?\"} for formula errors. Set \
             include_formatting=true to also get the formatting and layout of the range (see `formatting` in \
             the result). Reads at most 20000 cells per call; omit `range` to read the whole used area of \
             the sheet.",
            json!({
                "sheet": sheet,
                "range": range("Range to read"),
                "formulas": flag("Return formulas instead of displayed values. With typed, only formula cells give their formula."),
                "typed": flag("Return numbers, booleans, dates and errors as real JSON values instead of text."),
                "include_formatting": flag("Also return, in `formatting`: `styles` (each distinct cell style once, the first being the default), `cells` (a grid with the index into `styles` of every cell), `column_widths` and `row_heights` in pixels (rows of the default height are left out), `merged` ranges and `frozen` panes."),
            }),
            &[],
            Hint::Read,
        ),
        tool(
            "write_range",
            "Write cells",
            "Writes a grid of values with its top-left corner at `start`, replacing what is there. Strings \
             starting with '=' are formulas; numbers and booleans are stored as such; null leaves the cell \
             empty. Reports any cell that ended up as a formula error (e.g. #NAME?). One undo step.",
            json!({
                "sheet": sheet,
                "start": { "type": "string", "description": "Top-left cell, e.g. B2." },
                "values": {
                    "type": "array",
                    "description": "Rows of cells, e.g. [[\"Item\",\"Qty\"],[\"Pen\",3],[\"Total\",\"=SUM(B2:B2)\"]].",
                    "items": { "type": "array", "items": { "type": ["string", "number", "boolean", "null"] } },
                },
            }),
            &["start", "values"],
            Hint::Destructive,
        ),
        tool(
            "clear_range",
            "Clear cells",
            "Clears a range. By default only the contents go and the formatting stays; set `what` to \
             \"formats\" to reset the formatting (fill, bold, number format, ...) and keep the contents, or \
             to \"all\" for both. One undo step.",
            json!({
                "sheet": sheet,
                "range": ranges("Range to clear"),
                "what": { "type": "string", "enum": ["contents", "formats", "all"], "description": "What to clear. Defaults to contents." },
            }),
            &["range"],
            Hint::Destructive,
        ),
        tool(
            "format_range",
            "Format cells",
            "Applies formatting to a range, or to several comma-separated ranges at once. Pass only what \
             should change; at least one option is required. One undo step.",
            json!({
                "sheet": sheet,
                "range": ranges("Range to format"),
                "bold": flag("Bold on or off."),
                "italic": flag("Italic on or off."),
                "underline": flag("Underline on or off."),
                "text_color": color("Text color"),
                "fill_color": color("Background color; use an empty string or \"none\" to remove it"),
                "align": { "type": "string", "enum": ["left", "center", "right"], "description": "Horizontal alignment." },
                "number_format": { "type": "string", "description": "Excel number format, e.g. \"$#,##0.00\", \"0%\", \"0.00\", \"yyyy-mm-dd\" or \"general\"." },
                "font_size": { "type": "integer", "description": "Font size in points (1-409). Default is 12." },
                "strikethrough": flag("Strikethrough on or off."),
                "wrap_text": flag("Wrap long text onto several lines inside the cell."),
                "vertical_align": { "type": "string", "enum": ["top", "center", "bottom"], "description": "Vertical alignment." },
                "borders": {
                    "type": "object",
                    "description": "Cell borders. `sides` picks where (default all), `style` how (default thin) and `color` what color (default #000000). sides \"none\" removes the borders.",
                    "properties": {
                        "sides": { "type": "string", "enum": ["all", "outer", "inner", "top", "bottom", "left", "right", "none"] },
                        "style": { "type": "string", "enum": ["thin", "medium", "thick", "double", "dotted", "dashed"] },
                        "color": color("Border color"),
                    },
                },
            }),
            &["range"],
            Hint::Safe,
        ),
        tool(
            "set_layout",
            "Set layout",
            "Sets column widths, row heights, frozen panes and merged cells; pass any combination. Sizes are \
             in pixels (a column is 90 and a row 25 by default). A column width of \"auto\" fits the column \
             to its longest text, so long text is not clipped. One undo step.",
            json!({
                "sheet": sheet,
                "columns": {
                    "type": "array",
                    "description": "Column widths, e.g. [{\"range\":\"A\",\"width\":\"auto\"},{\"range\":\"B:D\",\"width\":120}].",
                    "items": {
                        "type": "object",
                        "properties": {
                            "range": { "type": "string", "description": "A column (B) or columns (A:C)." },
                            "width": { "type": ["number", "string"], "description": "Pixels (1-1000), or \"auto\"." },
                        },
                        "required": ["range", "width"],
                    },
                },
                "rows": {
                    "type": "array",
                    "description": "Row heights, e.g. [{\"range\":\"1\",\"height\":32}].",
                    "items": {
                        "type": "object",
                        "properties": {
                            "range": { "type": "string", "description": "A row (3) or rows (1:5)." },
                            "height": { "type": "number", "description": "Pixels (1-1000)." },
                        },
                        "required": ["range", "height"],
                    },
                },
                "freeze": {
                    "type": "object",
                    "description": "Rows and columns to keep in view while scrolling, from the top-left. 0 unfreezes. E.g. {\"rows\":1} freezes the header row.",
                    "properties": {
                        "rows": { "type": "integer" },
                        "columns": { "type": "integer" },
                    },
                },
                "merge": { "type": "array", "items": { "type": "string" }, "description": "Ranges to merge into one cell, e.g. [\"A1:D1\"]. The top-left value is kept." },
                "unmerge": { "type": "array", "items": { "type": "string" }, "description": "Ranges whose merged cells are split again." },
                "discard_values": flag("Allow merging cells that hold values besides the top-left one, which are then cleared."),
            }),
            &[],
            Hint::Safe,
        ),
        tool(
            "insert_lines",
            "Insert rows or columns",
            "Inserts empty rows or columns before position `at`, pushing the existing ones down or to the \
             right. Formulas that point at moved cells follow them. One undo step.",
            json!({
                "sheet": sheet,
                "axis": { "type": "string", "enum": ["rows", "columns"] },
                "at": { "type": ["integer", "string"], "description": "Row number, or column letter or number, that the new ones go before." },
                "count": { "type": "integer", "description": "How many to insert. Defaults to 1." },
            }),
            &["axis", "at"],
            Hint::Safe,
        ),
        tool(
            "delete_lines",
            "Delete rows or columns",
            "Deletes rows or columns starting at `at`, pulling the ones after them up or to the left. \
             Formulas that pointed at deleted cells show #REF!. One undo step.",
            json!({
                "sheet": sheet,
                "axis": { "type": "string", "enum": ["rows", "columns"] },
                "at": { "type": ["integer", "string"], "description": "First row number, or column letter or number, to delete." },
                "count": { "type": "integer", "description": "How many to delete. Defaults to 1." },
            }),
            &["axis", "at"],
            Hint::Destructive,
        ),
        tool(
            "add_sheet",
            "Add sheet",
            "Adds an empty sheet at the end, named `name` or else Sheet2, Sheet3, ... One undo step.",
            json!({ "name": { "type": "string", "description": "Name of the new sheet (up to 31 characters, none of []:*?/\\)." } }),
            &[],
            Hint::Safe,
        ),
        tool(
            "rename_sheet",
            "Rename sheet",
            "Renames a sheet. Formulas that refer to it follow the new name. One undo step.",
            json!({ "sheet": { "type": "string", "description": "Current sheet name." }, "name": { "type": "string", "description": "New name." } }),
            &["sheet", "name"],
            Hint::Safe,
        ),
        tool(
            "move_sheet",
            "Move sheet",
            "Moves a sheet to another position in the tab bar. One undo step.",
            json!({
                "sheet": { "type": "string", "description": "Sheet name." },
                "position": { "type": "integer", "description": "New position, counting from 1 (the first tab)." },
            }),
            &["sheet", "position"],
            Hint::Safe,
        ),
        tool(
            "delete_sheet",
            "Delete sheet",
            "Deletes a sheet and everything on it. The last remaining sheet cannot be deleted. One undo step.",
            json!({ "sheet": { "type": "string", "description": "Sheet name." } }),
            &["sheet"],
            Hint::Destructive,
        ),
        tool(
            "batch",
            "Run several edits at once",
            "Runs a list of edits in order as ONE call and ONE undo step, which is much faster than calling \
             the tools one by one. Allowed tools: write_range, clear_range, format_range, set_layout, \
             insert_lines, delete_lines, add_sheet, rename_sheet, move_sheet and delete_sheet; `args` are \
             the arguments the tool takes on its own. It is all or nothing: if an operation fails, \
             the ones before it are rolled back and the error says which one failed.",
            json!({
                "operations": {
                    "type": "array",
                    "description": "Operations to run in order, e.g. [{\"tool\":\"write_range\",\"args\":{\"start\":\"A1\",\"values\":[[\"x\"]]}},{\"tool\":\"format_range\",\"args\":{\"range\":\"A1\",\"bold\":true}}].",
                    "items": {
                        "type": "object",
                        "properties": {
                            "tool": { "type": "string", "enum": BATCHABLE },
                            "args": { "type": "object", "description": "Arguments of that tool." },
                        },
                        "required": ["tool"],
                    },
                },
            }),
            &["operations"],
            Hint::Destructive,
        ),
        tool("undo", "Undo", "Undoes the last edit (yours or the user's).", json!({}), &[], Hint::Safe),
        tool("redo", "Redo", "Redoes the last undone edit.", json!({}), &[], Hint::Safe),
        tool(
            "open_workbook",
            "Open file",
            "Opens an .xlsx, .xlsm, .csv or .tsv file in SheetBro, replacing the current workbook. Refuses \
             when the current workbook has unsaved changes unless discard_changes is true, so ask the user \
             before setting that.",
            json!({
                "path": { "type": "string", "description": "Absolute path (a leading ~/ is allowed)." },
                "discard_changes": flag("Throw away unsaved changes in the current workbook."),
            }),
            &["path"],
            Hint::Destructive,
        ),
        tool(
            "save_workbook",
            "Save file",
            "Saves the workbook. Without `path` it saves to the file it was opened from. With `path` (.xlsx, \
             or .csv to export one sheet) it saves there and refuses to replace an existing file unless \
             overwrite is true.",
            json!({
                "path": { "type": "string", "description": "Absolute target path ending in .xlsx or .csv." },
                "sheet": { "type": "string", "description": "Sheet to export when saving .csv. Defaults to the first sheet." },
                "overwrite": flag("Allow replacing an existing file at `path`."),
            }),
            &[],
            Hint::Safe,
        ),
    ]
}

struct Outcome {
    data: Value,
    change: Option<Change>,
}

impl Outcome {
    fn read(data: Value) -> Outcome {
        Outcome { data, change: None }
    }
    fn edit(data: Value) -> Outcome {
        Outcome { data, change: Some(Change::Edited) }
    }
}

fn run_tool(doc: &mut Doc, name: &str, args: &Value) -> Result<Outcome, String> {
    match name {
        "workbook_info" => Ok(Outcome::read(workbook_info(doc)?)),
        "read_range" => read_range(doc, args),
        "write_range" => write_range(doc, args),
        "clear_range" => clear_range(doc, args),
        "format_range" => format_range(doc, args),
        "set_layout" => set_layout(doc, args),
        "insert_lines" => insert_lines(doc, args),
        "delete_lines" => delete_lines(doc, args),
        "add_sheet" => add_sheet(doc, args),
        "rename_sheet" => rename_sheet(doc, args),
        "move_sheet" => move_sheet(doc, args),
        "delete_sheet" => delete_sheet(doc, args),
        "batch" => batch(doc, args),
        "undo" => replay(doc, false),
        "redo" => replay(doc, true),
        "open_workbook" => open_workbook(doc, args),
        "save_workbook" => save_workbook(doc, args),
        _ => Err(format!("Unknown tool: {name}")),
    }
}

fn workbook_info(doc: &Doc) -> Result<Value, String> {
    serde_json::to_value(info(doc)).map_err(|e| e.to_string())
}

fn read_range(doc: &Doc, args: &Value) -> Result<Outcome, String> {
    let sheet = sheet_index(doc, args.get("sheet"))?;
    let (rows, cols) = used_range(doc, sheet)?;
    let used = Rect { row: 1, col: 1, height: rows, width: cols };
    let wanted = match str_arg(args, "range")? {
        Some(text) => parse_range(text, rows, cols)?,
        None => used,
    };
    let raw = bool_arg(args, "formulas")?.unwrap_or(false);
    let typed = bool_arg(args, "typed")?.unwrap_or(false);
    let formatting = bool_arg(args, "include_formatting")?.unwrap_or(false);
    // Past the used area there is only emptiness
    let mut data = json!({ "sheet": sheet_name(doc, sheet), "used_range": used.label() });
    let Some(rect) = wanted.clip(rows, cols) else {
        data["range"] = json!(wanted.label());
        data["values"] = json!([]);
        data["note"] = json!("This range is outside the used area, so it is empty.");
        return Ok(Outcome::read(data));
    };
    if rect.cells() > MAX_READ_CELLS {
        return Err(format!(
            "{} has {} cells; at most {MAX_READ_CELLS} can be read per call. Read it in smaller ranges, e.g. {}.",
            rect.label(),
            rect.cells(),
            Rect { height: (MAX_READ_CELLS / rect.width as i64).max(1) as i32, ..rect }.label(),
        ));
    }
    let values = (rect.row..rect.row + rect.height)
        .map(|r| {
            (rect.col..rect.col + rect.width)
                .map(|c| {
                    if typed {
                        typed_value(doc, sheet, r, c, raw)
                    } else if raw {
                        doc.model.get_cell_content(sheet, r, c).map(Value::from)
                    } else {
                        doc.model.get_formatted_cell_value(sheet, r, c).map(Value::from)
                    }
                })
                .collect::<Result<Vec<_>, _>>()
        })
        .collect::<Result<Vec<_>, _>>()?;
    data["range"] = json!(rect.label());
    data["values"] = json!(values);
    if formatting {
        data["formatting"] = formatting_json(doc, sheet, rect)?;
    }
    Ok(Outcome::read(data))
}

/// The value of a cell as a JSON number, string or boolean; null when empty. Dates come as
/// {"date": "2026-10-29"} and formula errors as {"error": "#NAME?"}. With `formulas`, a
/// formula cell gives its formula instead.
fn typed_value(doc: &Doc, sheet: u32, row: i32, col: i32, formulas: bool) -> Result<Value, String> {
    if formulas {
        let content = doc.model.get_cell_content(sheet, row, col)?;
        if content.starts_with('=') {
            return Ok(json!(content));
        }
    }
    if matches!(doc.model.get_cell_type(sheet, row, col)?, CellType::ErrorValue) {
        return Ok(json!({ "error": doc.model.get_formatted_cell_value(sheet, row, col)? }));
    }
    Ok(match doc.model.get_model().get_cell_value_by_index(sheet, row, col)? {
        CellValue::None => Value::Null,
        CellValue::String(s) => json!(s),
        CellValue::Boolean(b) => json!(b),
        CellValue::Number(n) if !n.is_finite() => Value::Null,
        CellValue::Number(n) => {
            if is_likely_date_number_format(&doc.model.get_cell_style(sheet, row, col)?.num_fmt) {
                json!({ "date": excel_date(n) })
            } else if n.fract() == 0.0 && n.abs() < 9e15 {
                // 100000, not 100000.0
                json!(n as i64)
            } else {
                json!(n)
            }
        }
    })
}

/// "2026-10-29", "2026-10-29T12:00:00" or, for a bare time of day, "06:00:00".
fn excel_date(serial: f64) -> String {
    let seconds = (serial * 86_400.0).round() as i64;
    let (days, secs) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));
    let time = format!("{:02}:{:02}:{:02}", secs / 3600, secs % 3600 / 60, secs % 60);
    if days == 0 {
        return time;
    }
    // Serial 25569 is 1970-01-01. Spreadsheets also count a 29 Feb 1900 that never was, which
    // shifts every serial before 1 March 1900 by a day.
    let (year, month, day) = civil_from_days(days - if days >= 61 { 25_569 } else { 25_568 });
    let date = format!("{year:04}-{month:02}-{day:02}");
    if secs == 0 {
        date
    } else {
        format!("{date}T{time}")
    }
}

/// Year, month and day of the day that is `days` after 1970-01-01.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (year_of_era + era * 400 + i64::from(month <= 2), month, day)
}

const DEFAULT_FONT_SIZE: i32 = 12;
const DEFAULT_ROW_HEIGHT: f64 = 25.0;

fn border_text(side: &Option<BorderItem>) -> Option<String> {
    let item = side.as_ref()?;
    let style = match item.style {
        BorderStyle::Thin => "thin",
        BorderStyle::Medium => "medium",
        BorderStyle::Thick => "thick",
        BorderStyle::Double => "double",
        BorderStyle::Dotted => "dotted",
        BorderStyle::MediumDashed => "dashed",
        BorderStyle::SlantDashDot => "slantdashdot",
        BorderStyle::MediumDashDot => "mediumdashdot",
        BorderStyle::MediumDashDotDot => "mediumdashdotdot",
    };
    Some(match crate::rgb(&item.color) {
        Some(color) => format!("{style} {color}"),
        None => style.to_string(),
    })
}

/// What differs from the default style, with the same names `format_range` takes.
fn style_json(style: &Style) -> Value {
    let mut out = serde_json::Map::new();
    let mut put = |key: &str, value: Value| {
        out.insert(key.to_string(), value);
    };
    for (key, on) in [
        ("bold", style.font.b),
        ("italic", style.font.i),
        ("underline", style.font.u),
        ("strikethrough", style.font.strike),
    ] {
        if on {
            put(key, json!(true));
        }
    }
    if style.font.sz != DEFAULT_FONT_SIZE {
        put("font_size", json!(style.font.sz));
    }
    if let Some(color) = crate::rgb(&style.font.color) {
        put("text_color", json!(color));
    }
    if let Some(color) = crate::rgb(&style.fill.color) {
        put("fill_color", json!(color));
    }
    if style.num_fmt != "general" {
        put("number_format", json!(style.num_fmt));
    }
    if let Some(a) = &style.alignment {
        let horizontal = match a.horizontal {
            HorizontalAlignment::General => None,
            HorizontalAlignment::Left => Some("left"),
            HorizontalAlignment::Center | HorizontalAlignment::CenterContinuous => Some("center"),
            HorizontalAlignment::Right => Some("right"),
            HorizontalAlignment::Justify => Some("justify"),
            HorizontalAlignment::Fill => Some("fill"),
            HorizontalAlignment::Distributed => Some("distributed"),
        };
        if let Some(h) = horizontal {
            put("align", json!(h));
        }
        let vertical = match a.vertical {
            VerticalAlignment::Bottom => None,
            VerticalAlignment::Top => Some("top"),
            VerticalAlignment::Center => Some("center"),
            VerticalAlignment::Justify => Some("justify"),
            VerticalAlignment::Distributed => Some("distributed"),
        };
        if let Some(v) = vertical {
            put("vertical_align", json!(v));
        }
        if a.wrap_text {
            put("wrap_text", json!(true));
        }
    }
    let mut borders = serde_json::Map::new();
    for (side, border) in [
        ("top", &style.border.top),
        ("right", &style.border.right),
        ("bottom", &style.border.bottom),
        ("left", &style.border.left),
    ] {
        if let Some(text) = border_text(border) {
            borders.insert(side.to_string(), json!(text));
        }
    }
    if !borders.is_empty() {
        put("borders", Value::Object(borders));
    }
    Value::Object(out)
}

/// The formatting and layout of `rect`. Each distinct style is listed once in `styles`
/// (the first is the default, with nothing set) and `cells` says which one every cell has.
fn formatting_json(doc: &Doc, sheet: u32, rect: Rect) -> Result<Value, String> {
    let mut styles = vec![json!({})];
    let mut index_of: HashMap<String, usize> = HashMap::from([("{}".to_string(), 0)]);
    let mut cells = Vec::new();
    for row in rect.row..rect.row + rect.height {
        let mut line = Vec::new();
        for col in rect.col..rect.col + rect.width {
            let style = style_json(&doc.model.get_cell_style(sheet, row, col)?);
            let next = styles.len();
            let index = *index_of.entry(style.to_string()).or_insert(next);
            if index == next {
                styles.push(style);
            }
            line.push(index);
        }
        cells.push(line);
    }
    let mut column_widths = serde_json::Map::new();
    for col in rect.col..rect.col + rect.width {
        column_widths.insert(col_name(col), json!(doc.model.get_column_width(sheet, col)?));
    }
    let mut row_heights = serde_json::Map::new();
    for row in rect.row..rect.row + rect.height {
        let height = doc.model.get_row_height(sheet, row)?;
        if (height - DEFAULT_ROW_HEIGHT).abs() > 0.5 {
            row_heights.insert(row.to_string(), json!(height));
        }
    }
    let merged: Vec<String> = doc
        .model
        .get_merged_cells(sheet)?
        .into_iter()
        .map(|m| Rect { row: m.row, col: m.column, height: m.height, width: m.width })
        .filter(|m| {
            m.row < rect.row + rect.height && rect.row < m.row + m.height && m.col < rect.col + rect.width && rect.col < m.col + m.width
        })
        .map(|m| m.label())
        .collect();
    Ok(json!({
        "styles": styles,
        "cells": cells,
        "column_widths": column_widths,
        "row_heights": row_heights,
        "merged": merged,
        "frozen": {
            "rows": doc.model.get_frozen_rows_count(sheet)?,
            "columns": doc.model.get_frozen_columns_count(sheet)?,
        },
    }))
}

fn cell_text(v: &Value) -> Result<String, String> {
    match v {
        Value::Null => Ok(String::new()),
        Value::String(s) => Ok(s.clone()),
        Value::Bool(b) => Ok(if *b { "TRUE" } else { "FALSE" }.into()),
        Value::Number(n) => Ok(n.to_string()),
        _ => Err("each cell must be a string, number, boolean or null".into()),
    }
}

fn write_range(doc: &mut Doc, args: &Value) -> Result<Outcome, String> {
    let sheet = sheet_index(doc, args.get("sheet"))?;
    let Ref::Cell(row, col) = parse_ref(req_str(args, "start")?)? else {
        return Err("`start` must be a single cell such as B2".into());
    };
    let rows = args
        .get("values")
        .and_then(Value::as_array)
        .ok_or("`values` must be an array of rows, e.g. [[\"Item\",\"Qty\"],[\"Pen\",3]]")?;
    let grid = rows
        .iter()
        .map(|r| {
            r.as_array()
                .ok_or_else(|| "`values` must be an array of rows (arrays)".to_string())?
                .iter()
                .map(cell_text)
                .collect::<Result<Vec<_>, _>>()
        })
        .collect::<Result<Vec<Vec<String>>, _>>()?;
    let height = grid.len() as i32;
    let width = grid.iter().map(Vec::len).max().unwrap_or(0) as i32;
    if height == 0 || width == 0 {
        return Err("`values` is empty".into());
    }
    let rect = Rect { row, col, height, width };
    if rect.cells() > MAX_WRITE_CELLS {
        return Err(format!("{} cells is too many for one call (limit {MAX_WRITE_CELLS}); write it in parts.", rect.cells()));
    }
    if row + height - 1 > MAX_ROW || col + width - 1 > MAX_COL {
        return Err(format!("{} runs past the end of the sheet", rect.label()));
    }
    crate::write_cells(doc, sheet, row, col, &grid)?;
    let mut data = json!({
        "sheet": sheet_name(doc, sheet),
        "range": rect.label(),
        "cells_written": grid.iter().map(Vec::len).sum::<usize>(),
    });
    let errors = error_cells(doc, sheet, rect);
    if !errors.is_empty() {
        data["formula_errors"] = json!(errors);
    }
    Ok(Outcome::edit(data))
}

/// Cells in `rect` showing an error such as #NAME?, so a bad formula does not go unnoticed.
fn error_cells(doc: &Doc, sheet: u32, rect: Rect) -> Vec<Value> {
    let mut out = Vec::new();
    for r in rect.row..rect.row + rect.height {
        for c in rect.col..rect.col + rect.width {
            if matches!(doc.model.get_cell_type(sheet, r, c), Ok(CellType::ErrorValue)) {
                let value = doc.model.get_formatted_cell_value(sheet, r, c).unwrap_or_default();
                out.push(json!({ "cell": cell_label(r, c), "value": value }));
                if out.len() >= 20 {
                    return out;
                }
            }
        }
    }
    out
}

fn clear_range(doc: &mut Doc, args: &Value) -> Result<Outcome, String> {
    let sheet = sheet_index(doc, args.get("sheet"))?;
    let (rows, cols) = used_range(doc, sheet)?;
    let wanted = parse_ranges(req_str(args, "range")?, rows, cols)?;
    let what = str_arg(args, "what")?.unwrap_or("contents");
    if !["contents", "formats", "all"].contains(&what) {
        return Err("`what` must be contents, formats or all".into());
    }
    let mut data = json!({ "sheet": sheet_name(doc, sheet), "range": ranges_label(&wanted) });
    // Nothing exists past the used area, and clearing a million empty rows would only be slow
    let inside: Vec<Rect> = wanted.iter().filter_map(|r| r.clip(rows, cols)).collect();
    if inside.is_empty() {
        data["note"] = json!("This range is outside the used area; nothing to clear.");
        return Ok(Outcome::read(data));
    }
    let mut cleared = 0;
    let mut result = Ok(());
    for rect in &inside {
        let area = rect.area(sheet);
        result = match what {
            "formats" => doc.model.range_clear_formatting(&area),
            "all" => doc.model.range_clear_all(&area),
            _ => doc.model.range_clear_contents(&area),
        };
        if result.is_err() {
            break;
        }
        cleared += 1;
    }
    doc.record(cleared);
    result?;
    data["range"] = json!(ranges_label(&inside));
    data["cleared"] = json!(true);
    data["what"] = json!(what);
    Ok(Outcome::edit(data))
}

fn hex_color(text: &str, key: &str) -> Result<String, String> {
    let hex = text.strip_prefix('#').unwrap_or(text);
    if hex.len() == 6 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(format!("#{}", hex.to_ascii_uppercase()))
    } else {
        Err(format!("`{key}` must be a color like #1F5A43"))
    }
}

fn format_range(doc: &mut Doc, args: &Value) -> Result<Outcome, String> {
    let sheet = sheet_index(doc, args.get("sheet"))?;
    let (rows, cols) = used_range(doc, sheet)?;
    let rects = parse_ranges(req_str(args, "range")?, rows, cols)?;
    let cells: i64 = rects.iter().map(Rect::cells).sum();
    if cells > MAX_FORMAT_CELLS {
        return Err(format!("{cells} cells is too many to format at once (limit {MAX_FORMAT_CELLS})."));
    }
    // (style path understood by IronCalc, value) — the same paths the toolbar uses
    let mut changes: Vec<(&str, String)> = Vec::new();
    for (key, path) in [("bold", "font.b"), ("italic", "font.i"), ("underline", "font.u")] {
        if let Some(on) = bool_arg(args, key)? {
            changes.push((path, on.to_string()));
        }
    }
    if let Some(c) = str_arg(args, "text_color")? {
        changes.push(("font.color", hex_color(c, "text_color")?));
    }
    if let Some(c) = str_arg(args, "fill_color")? {
        let value = if c.is_empty() || c.eq_ignore_ascii_case("none") {
            String::new()
        } else {
            hex_color(c, "fill_color")?
        };
        changes.push(("fill.color", value));
    }
    if let Some(a) = str_arg(args, "align")? {
        let a = a.to_ascii_lowercase();
        if !["left", "center", "right"].contains(&a.as_str()) {
            return Err("`align` must be left, center or right".into());
        }
        changes.push(("alignment.horizontal", a));
    }
    if let Some(f) = str_arg(args, "number_format")? {
        changes.push(("num_fmt", f.to_string()));
    }
    if let Some(size) = int_arg(args, "font_size")? {
        if !(1..=409).contains(&size) {
            return Err("`font_size` must be between 1 and 409".into());
        }
        changes.push(("font.size", size.to_string()));
    }
    for (key, path) in [("strikethrough", "font.strike"), ("wrap_text", "alignment.wrap_text")] {
        if let Some(on) = bool_arg(args, key)? {
            changes.push((path, on.to_string()));
        }
    }
    if let Some(a) = str_arg(args, "vertical_align")? {
        let a = a.to_ascii_lowercase();
        if !["top", "center", "bottom"].contains(&a.as_str()) {
            return Err("`vertical_align` must be top, center or bottom".into());
        }
        changes.push(("alignment.vertical", a));
    }
    let borders = border_json(args)?;
    if changes.is_empty() && borders.is_none() {
        return Err("Nothing to change: pass bold, italic, underline, strikethrough, text_color, fill_color, \
                    align, vertical_align, wrap_text, font_size, borders or number_format"
            .into());
    }
    let mut applied = 0;
    let mut result = Ok(());
    'apply: for rect in &rects {
        let area = rect.area(sheet);
        for (path, value) in &changes {
            result = doc.model.update_range_style(&area, path, value);
            if result.is_err() {
                break 'apply;
            }
            applied += 1;
        }
        if let Some(border) = &borders {
            result = serde_json::from_value::<BorderArea>(border.clone())
                .map_err(|e| e.to_string())
                .and_then(|b| doc.model.set_area_with_border(&area, &b));
            if result.is_err() {
                break 'apply;
            }
            applied += 1;
        }
    }
    // what was applied before a failure is still one undo step
    doc.record(applied);
    result?;
    let mut data = json!({
        "sheet": sheet_name(doc, sheet),
        "range": ranges_label(&rects),
        "applied": changes.len() + usize::from(borders.is_some()),
    });
    let undrawn = ["font_size", "strikethrough", "wrap_text", "vertical_align", "borders"];
    if undrawn.iter().any(|k| args.get(*k).is_some_and(|v| !v.is_null())) {
        data["note"] = json!(
            "Saved in the workbook and in the .xlsx file, but SheetBro's own grid does not draw font size, \
             strikethrough, wrapping, vertical alignment or borders yet, so they will not show in its window."
        );
    }
    Ok(Outcome::edit(data))
}

/// The `borders` option as the JSON IronCalc's `BorderArea` reads: which sides, and the line to draw.
fn border_json(args: &Value) -> Result<Option<Value>, String> {
    let given = match args.get("borders") {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Object(o)) => o,
        Some(_) => return Err("`borders` must be an object like {\"sides\":\"outer\",\"style\":\"thin\",\"color\":\"#000000\"}".into()),
    };
    if let Some(unknown) = given.keys().find(|k| !["sides", "style", "color"].contains(&k.as_str())) {
        return Err(format!("`borders` has no option \"{unknown}\"; use sides, style and color"));
    }
    let word = |key: &str, default: &str| -> Result<String, String> {
        match given.get(key) {
            None | Some(Value::Null) => Ok(default.to_string()),
            Some(Value::String(s)) => Ok(s.to_ascii_lowercase()),
            Some(_) => Err(format!("`borders.{key}` must be a string")),
        }
    };
    let sides = match word("sides", "all")?.as_str() {
        "all" => "All",
        "outer" => "Outer",
        "inner" => "Inner",
        "top" => "Top",
        "bottom" => "Bottom",
        "left" => "Left",
        "right" => "Right",
        "none" => "None",
        _ => return Err("`borders.sides` must be all, outer, inner, top, bottom, left, right or none".into()),
    };
    let style = match word("style", "thin")?.as_str() {
        s @ ("thin" | "medium" | "thick" | "double" | "dotted") => s.to_string(),
        "dashed" => "mediumdashed".to_string(),
        _ => return Err("`borders.style` must be thin, medium, thick, double, dotted or dashed".into()),
    };
    let color = match given.get("color") {
        None | Some(Value::Null) => "#000000".to_string(),
        Some(Value::String(c)) => hex_color(c, "borders.color")?,
        Some(_) => return Err("`borders.color` must be a color like #1F5A43".into()),
    };
    Ok(Some(json!({ "item": { "style": style, "color": color }, "type": sides })))
}

/// Tools that `batch` may run: the ones that only edit the open workbook.
const BATCHABLE: [&str; 10] = [
    "write_range",
    "clear_range",
    "format_range",
    "set_layout",
    "insert_lines",
    "delete_lines",
    "add_sheet",
    "rename_sheet",
    "move_sheet",
    "delete_sheet",
];
const MAX_BATCH: usize = 500;

/// Runs `f` and turns everything it recorded in the undo history into a single step.
fn as_one_step<T>(doc: &mut Doc, f: impl FnOnce(&mut Doc) -> Result<T, String>) -> Result<T, String> {
    let first = doc.undo_groups.len();
    let result = f(doc);
    let entries: usize = doc.undo_groups.drain(first..).sum();
    if entries > 0 {
        doc.undo_groups.push(entries);
    }
    result
}

/// Runs the operations in order as one undo step; on the first failure it undoes them all.
fn batch(doc: &mut Doc, args: &Value) -> Result<Outcome, String> {
    let ops = args
        .get("operations")
        .and_then(Value::as_array)
        .filter(|ops| !ops.is_empty())
        .ok_or("`operations` must be a non-empty array of {tool, args}")?;
    if ops.len() > MAX_BATCH {
        return Err(format!("{} operations is too many for one batch (limit {MAX_BATCH}).", ops.len()));
    }
    let (was_dirty, first_group) = (doc.dirty, doc.undo_groups.len());
    let no_args = json!({});
    let mut results = Vec::new();
    let outcome = as_one_step(doc, |doc| {
        for (i, op) in ops.iter().enumerate() {
            let name = op.get("tool").and_then(Value::as_str).unwrap_or("");
            let step = if BATCHABLE.contains(&name) {
                run_tool(doc, name, op.get("args").unwrap_or(&no_args)).map(|out| {
                    let mut data = out.data;
                    data["tool"] = json!(name);
                    data
                })
            } else if name.is_empty() {
                Err("it needs a `tool` name".to_string())
            } else {
                Err(format!("`{name}` cannot be used in a batch. Allowed: {}", BATCHABLE.join(", ")))
            };
            match step {
                Ok(data) => results.push(data),
                Err(e) => {
                    let name = if name.is_empty() { "?" } else { name };
                    return Err(format!("Operation {} ({name}) failed: {e}", i + 1));
                }
            }
        }
        Ok(())
    });
    if let Err(message) = outcome {
        if doc.undo_groups.len() > first_group {
            doc.replay(false).map_err(|e| format!("{message}. Rolling back failed too ({e}); use undo to check."))?;
            // a rolled back batch must not leave a redo step behind
            doc.redo_groups.clear();
            doc.dirty = was_dirty;
        }
        return Err(format!("{message}. Rolled back, so nothing was changed."));
    }
    Ok(Outcome::edit(json!({ "applied": results.len(), "results": results })))
}

// -------------------------------------------------------------------- layout

/// First and last row (or column) of "3", "1:5", "B" or "A:C".
fn parse_lines(text: &str, axis: crate::Axis) -> Result<(i32, i32), String> {
    let bad = || match axis {
        crate::Axis::Rows => format!("\"{text}\" is not a row or rows (use 3 or 1:5)"),
        crate::Axis::Columns => format!("\"{text}\" is not a column or columns (use B or A:C)"),
    };
    let line = |part: &str| match (parse_ref(part), axis) {
        (Ok(Ref::Row(r)), crate::Axis::Rows) if r >= 1 => Ok(r),
        (Ok(Ref::Col(c)), crate::Axis::Columns) => Ok(c),
        _ => Err(bad()),
    };
    let (first, last) = match text.split_once(':') {
        Some((a, b)) => (line(a)?, line(b)?),
        None => (line(text)?, line(text)?),
    };
    Ok((first.min(last), first.max(last)))
}

/// Rough width in pixels of `text` in the grid's 13px font, for fitting a column to its contents.
fn text_width(text: &str, bold: bool, mono: bool) -> f64 {
    let em: f64 = text
        .chars()
        .map(|ch| match ch {
            _ if mono => 0.6,
            'i' | 'l' | 'j' | '.' | ',' | ':' | ';' | '|' | '!' | '\'' | ' ' => 0.3,
            'f' | 't' | 'r' | 'I' | '(' | ')' | '[' | ']' | '-' | '/' => 0.4,
            'm' | 'w' | 'M' | 'W' | '@' | '%' => 0.9,
            c if c.is_ascii_uppercase() => 0.66,
            c if c.is_ascii_digit() => 0.6,
            c if c.is_ascii() => 0.55,
            // wide scripts and emoji
            _ => 1.0,
        })
        .sum();
    em * 13.0 * if bold { 1.07 } else { 1.0 }
}

/// What a column without a width of its own measures, as in the app.
const DEFAULT_COLUMN_WIDTH: f64 = 90.0;
const MAX_LINES: i32 = 10_000;

/// The width that shows every text of the column, like double-clicking its edge in the app.
fn autofit_width(doc: &Doc, sheet: u32, col: i32, rows: i32) -> Result<f64, String> {
    let mut widest: f64 = 0.0;
    for row in 1..=rows {
        let text = doc.model.get_formatted_cell_value(sheet, row, col)?;
        if text.is_empty() {
            continue;
        }
        let bold = doc.model.get_cell_style(sheet, row, col)?.font.b;
        let mono = matches!(doc.model.get_cell_type(sheet, row, col)?, CellType::Number);
        widest = widest.max(text_width(&text, bold, mono));
    }
    // a little to spare, since the real font is only measured roughly; 18px is the cell padding
    Ok(if widest == 0.0 { DEFAULT_COLUMN_WIDTH } else { (widest * 1.05 + 18.0).ceil().clamp(24.0, 1000.0) })
}

fn array_arg<'a>(args: &'a Value, key: &str) -> Result<&'a [Value], String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(&[]),
        Some(Value::Array(a)) => Ok(a),
        Some(_) => Err(format!("`{key}` must be an array")),
    }
}

fn size_arg(item: &Value, key: &str) -> Result<f64, String> {
    item.get(key)
        .and_then(Value::as_f64)
        .filter(|n| (1.0..=1000.0).contains(n))
        .ok_or_else(|| format!("`{key}` must be a number of pixels between 1 and 1000"))
}

fn set_layout(doc: &mut Doc, args: &Value) -> Result<Outcome, String> {
    enum Width {
        Fixed(f64),
        Auto,
    }
    let sheet = sheet_index(doc, args.get("sheet"))?;
    let (rows, cols) = used_range(doc, sheet)?;
    let discard = bool_arg(args, "discard_values")?.unwrap_or(false);

    // check everything first, so a mistake in the last item does not leave the others applied
    let mut widths = Vec::new();
    for item in array_arg(args, "columns")? {
        let range = item.get("range").and_then(Value::as_str).ok_or("each item of `columns` needs a `range`")?;
        let (first, last) = parse_lines(range, crate::Axis::Columns)?;
        let width = match item.get("width") {
            Some(Value::String(s)) if s.eq_ignore_ascii_case("auto") => Width::Auto,
            Some(_) => Width::Fixed(size_arg(item, "width").map_err(|e| format!("{range}: {e}, or \"auto\""))?),
            None => return Err(format!("{range}: `width` is required (pixels, or \"auto\")")),
        };
        widths.push((range, first, last, width));
    }
    let mut heights = Vec::new();
    for item in array_arg(args, "rows")? {
        let range = item.get("range").and_then(Value::as_str).ok_or("each item of `rows` needs a `range`")?;
        let (first, last) = parse_lines(range, crate::Axis::Rows)?;
        if last - first >= MAX_LINES {
            return Err(format!("{range}: at most {MAX_LINES} rows can be resized at once"));
        }
        heights.push((range, first, last, size_arg(item, "height").map_err(|e| format!("{range}: {e}"))?));
    }
    let freeze = match args.get("freeze") {
        None | Some(Value::Null) => None,
        Some(f @ Value::Object(_)) => {
            let count = |key: &str, max: i32| -> Result<Option<i32>, String> {
                Ok(match int_arg(f, key)? {
                    Some(n) if (0..=max as i64).contains(&n) => Some(n as i32),
                    Some(_) => return Err(format!("`freeze.{key}` must be between 0 and {max}")),
                    None => None,
                })
            };
            let (frozen_rows, frozen_cols) = (count("rows", MAX_ROW)?, count("columns", MAX_COL)?);
            if frozen_rows.is_none() && frozen_cols.is_none() {
                return Err("`freeze` needs `rows` and/or `columns`".into());
            }
            Some((frozen_rows, frozen_cols))
        }
        Some(_) => return Err("`freeze` must be an object like {\"rows\": 1, \"columns\": 0}".into()),
    };
    let mut merges = Vec::new();
    for item in array_arg(args, "merge")? {
        let text = item.as_str().ok_or("`merge` must be an array of ranges like \"A1:D1\"")?;
        let rect = parse_range(text, rows, cols)?;
        if rect.cells() < 2 {
            return Err(format!("{text}: merging needs at least two cells"));
        }
        let area = rect.area(sheet);
        if !discard && crate::count_content(&doc.model, &area) > 1 {
            return Err(format!(
                "Merging {} would clear the values in all its cells except the top-left one. Pass \
                 discard_values=true if that is fine.",
                rect.label()
            ));
        }
        merges.push((rect, area));
    }
    let mut unmerges = Vec::new();
    for item in array_arg(args, "unmerge")? {
        let text = item.as_str().ok_or("`unmerge` must be an array of ranges like \"A1:D1\"")?;
        unmerges.push(parse_range(text, rows, cols)?);
    }
    if widths.is_empty() && heights.is_empty() && freeze.is_none() && merges.is_empty() && unmerges.is_empty() {
        return Err("Nothing to change: pass columns, rows, freeze, merge or unmerge".into());
    }

    let mut data = json!({ "sheet": sheet_name(doc, sheet) });
    as_one_step(doc, |doc| {
        let mut done = Vec::new();
        for (range, first, last, width) in widths {
            match width {
                Width::Fixed(px) => {
                    doc.model.set_columns_width(sheet, first, last, px)?;
                    doc.record(1);
                    done.push(json!({ "range": range, "width": px }));
                }
                // each column of the range fits its own text
                Width::Auto => {
                    for col in first..=last {
                        let px = autofit_width(doc, sheet, col, rows)?;
                        doc.model.set_columns_width(sheet, col, col, px)?;
                        doc.record(1);
                        done.push(json!({ "range": col_name(col), "width": px }));
                    }
                }
            }
        }
        if !done.is_empty() {
            data["columns"] = json!(done);
        }
        let mut done = Vec::new();
        for (range, first, last, height) in heights {
            doc.model.set_rows_height(sheet, first, last, height)?;
            doc.record(1);
            done.push(json!({ "range": range, "height": height }));
        }
        if !done.is_empty() {
            data["rows"] = json!(done);
        }
        if let Some((frozen_rows, frozen_cols)) = freeze {
            if let Some(n) = frozen_rows {
                doc.model.set_frozen_rows_count(sheet, n)?;
                doc.record(1);
            }
            if let Some(n) = frozen_cols {
                doc.model.set_frozen_columns_count(sheet, n)?;
                doc.record(1);
            }
            data["freeze"] = json!({
                "rows": doc.model.get_frozen_rows_count(sheet)?,
                "columns": doc.model.get_frozen_columns_count(sheet)?,
            });
        }
        for rect in &unmerges {
            crate::unmerge_in(doc, &rect.area(sheet))?;
        }
        for (_, area) in &merges {
            crate::merge_in(doc, area, crate::MergeMode::Merge, discard)?;
        }
        if !merges.is_empty() {
            data["merged"] = json!(merges.iter().map(|(r, _)| r.label()).collect::<Vec<_>>());
        }
        if !unmerges.is_empty() {
            data["unmerged"] = json!(unmerges.iter().map(Rect::label).collect::<Vec<_>>());
        }
        Ok(())
    })?;
    Ok(Outcome::edit(data))
}

// ------------------------------------------------------------- rows, columns

fn is_columns(axis: crate::Axis) -> bool {
    matches!(axis, crate::Axis::Columns)
}

/// Row number, or column letter or number, given as `at`.
fn line_arg(args: &Value, axis: crate::Axis) -> Result<i32, String> {
    let max = if is_columns(axis) { MAX_COL } else { MAX_ROW };
    let bad = || {
        let what = if is_columns(axis) { "column letter or number" } else { "row number" };
        format!("`at` must be a {what} between 1 and {max}")
    };
    let at = match args.get("at") {
        Some(Value::Number(n)) => n.as_i64().ok_or_else(bad)?,
        Some(Value::String(s)) if is_columns(axis) && s.bytes().all(|b| b.is_ascii_alphabetic()) => match parse_ref(s) {
            Ok(Ref::Col(c)) => c as i64,
            _ => return Err(bad()),
        },
        Some(Value::String(s)) => s.trim().parse::<i64>().map_err(|_| bad())?,
        _ => return Err(bad()),
    };
    if !(1..=max as i64).contains(&at) {
        return Err(bad());
    }
    Ok(at as i32)
}

/// "row 5", "rows 2:3", "column B", "columns B:D"
fn lines_label(axis: crate::Axis, first: i32, last: i32) -> String {
    let name = |n: i32| if is_columns(axis) { col_name(n) } else { n.to_string() };
    let noun = if is_columns(axis) { "column" } else { "row" };
    if first == last {
        format!("{noun} {}", name(first))
    } else {
        format!("{noun}s {}:{}", name(first), name(last))
    }
}

fn lines_args(doc: &Doc, args: &Value) -> Result<(u32, crate::Axis, i32, i32), String> {
    let sheet = sheet_index(doc, args.get("sheet"))?;
    let axis = match str_arg(args, "axis")? {
        Some("rows") => crate::Axis::Rows,
        Some("columns") => crate::Axis::Columns,
        _ => return Err("`axis` must be \"rows\" or \"columns\"".into()),
    };
    let at = line_arg(args, axis)?;
    let count = int_arg(args, "count")?.unwrap_or(1);
    if !(1..=MAX_LINES as i64).contains(&count) {
        return Err(format!("`count` must be between 1 and {MAX_LINES}"));
    }
    let limit = if is_columns(axis) { MAX_COL } else { MAX_ROW };
    if at as i64 + count - 1 > limit as i64 {
        return Err("That runs past the end of the sheet".into());
    }
    Ok((sheet, axis, at, count as i32))
}

fn insert_lines(doc: &mut Doc, args: &Value) -> Result<Outcome, String> {
    let (sheet, axis, at, count) = lines_args(doc, args)?;
    match axis {
        crate::Axis::Rows => doc.model.insert_rows(sheet, at, count)?,
        crate::Axis::Columns => doc.model.insert_columns(sheet, at, count)?,
    }
    doc.record(1);
    Ok(Outcome::edit(json!({
        "sheet": sheet_name(doc, sheet),
        "inserted": lines_label(axis, at, at + count - 1),
    })))
}

fn delete_lines(doc: &mut Doc, args: &Value) -> Result<Outcome, String> {
    let (sheet, axis, at, count) = lines_args(doc, args)?;
    crate::delete_lines_in(doc, sheet, axis, at, count)?;
    Ok(Outcome::edit(json!({
        "sheet": sheet_name(doc, sheet),
        "deleted": lines_label(axis, at, at + count - 1),
    })))
}

// -------------------------------------------------------------------- sheets

/// Excel's rules: 1-31 characters, none of []:*?/\ , and unique whatever the letter case.
fn check_sheet_name(doc: &Doc, name: &str, renaming: Option<u32>) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("A sheet name cannot be empty".into());
    }
    if name.chars().count() > 31 {
        return Err(format!("\"{name}\" is longer than 31 characters"));
    }
    if let Some(c) = name.chars().find(|c| "[]:*?/\\".contains(*c)) {
        return Err(format!("A sheet name cannot contain {c}"));
    }
    let taken = sheet_names(doc)
        .iter()
        .enumerate()
        .any(|(i, n)| Some(i as u32) != renaming && n.to_lowercase() == name.to_lowercase());
    if taken {
        return Err(format!("There is already a sheet named \"{name}\". Sheets: {}", sheet_names(doc).join(", ")));
    }
    Ok(())
}

/// Like `sheet_index`, but the sheet has to be named.
fn required_sheet(doc: &Doc, args: &Value) -> Result<u32, String> {
    match args.get("sheet") {
        None | Some(Value::Null) => Err("`sheet` is required".into()),
        some => sheet_index(doc, some),
    }
}

fn add_sheet(doc: &mut Doc, args: &Value) -> Result<Outcome, String> {
    let name = str_arg(args, "name")?;
    if let Some(name) = name {
        check_sheet_name(doc, name, None)?;
    }
    as_one_step(doc, |doc| {
        doc.model.new_sheet()?;
        doc.record(1);
        if let Some(name) = name {
            doc.model.rename_sheet(sheet_names(doc).len() as u32 - 1, name)?;
            doc.record(1);
        }
        Ok(())
    })?;
    let names = sheet_names(doc);
    Ok(Outcome::edit(json!({ "added": names.last(), "sheets": names })))
}

fn rename_sheet(doc: &mut Doc, args: &Value) -> Result<Outcome, String> {
    let sheet = required_sheet(doc, args)?;
    let name = req_str(args, "name")?;
    check_sheet_name(doc, name, Some(sheet))?;
    doc.model.rename_sheet(sheet, name)?;
    doc.record(1);
    Ok(Outcome::edit(json!({ "sheets": sheet_names(doc) })))
}

fn move_sheet(doc: &mut Doc, args: &Value) -> Result<Outcome, String> {
    let sheet = required_sheet(doc, args)?;
    let count = sheet_names(doc).len() as i64;
    let position = int_arg(args, "position")?
        .filter(|p| (1..=count).contains(p))
        .ok_or_else(|| format!("`position` must be a whole number between 1 and {count}"))?;
    doc.model.move_sheet(sheet, position as u32 - 1)?;
    doc.record(1);
    Ok(Outcome::edit(json!({ "sheets": sheet_names(doc) })))
}

fn delete_sheet(doc: &mut Doc, args: &Value) -> Result<Outcome, String> {
    let sheet = required_sheet(doc, args)?;
    if sheet_names(doc).len() < 2 {
        return Err("A workbook needs at least one sheet, so the last one cannot be deleted.".into());
    }
    doc.model.delete_sheet(sheet)?;
    doc.record(1);
    Ok(Outcome::edit(json!({ "sheets": sheet_names(doc) })))
}

fn replay(doc: &mut Doc, redo: bool) -> Result<Outcome, String> {
    let available = if redo { &doc.redo_groups } else { &doc.undo_groups };
    if available.is_empty() {
        let what = if redo { "redo" } else { "undo" };
        return Ok(Outcome::read(json!({ "performed": false, "reason": format!("Nothing to {what}") })));
    }
    doc.replay(redo)?;
    Ok(Outcome::edit(json!({
        "performed": true,
        "can_undo": !doc.undo_groups.is_empty(),
        "can_redo": !doc.redo_groups.is_empty(),
    })))
}

fn abs_path(text: &str) -> Result<PathBuf, String> {
    let path = match text.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().ok_or("No home directory")?.join(rest),
        None => PathBuf::from(text),
    };
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(format!("\"{text}\" is not an absolute path"))
    }
}

fn open_workbook(doc: &mut Doc, args: &Value) -> Result<Outcome, String> {
    let path = abs_path(req_str(args, "path")?)?;
    if doc.dirty && !bool_arg(args, "discard_changes")?.unwrap_or(false) {
        return Err("The open workbook has unsaved changes. Save it first (save_workbook), or ask the user \
                    and retry with discard_changes=true."
            .into());
    }
    *doc = crate::load_doc(&path.to_string_lossy())?;
    Ok(Outcome { data: workbook_info(doc)?, change: Some(Change::Replaced) })
}

fn save_workbook(doc: &mut Doc, args: &Value) -> Result<Outcome, String> {
    let sheet = sheet_index(doc, args.get("sheet"))?;
    let given = str_arg(args, "path")?.map(abs_path).transpose()?;
    let target = given
        .clone()
        .or_else(|| doc.path.clone())
        .ok_or("This workbook has no file yet. Pass `path` ending in .xlsx or .csv.")?;
    let ext = target
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if ext != "xlsx" && ext != "csv" {
        return Err("Only .xlsx and .csv files can be saved".into());
    }
    if let Some(path) = &given {
        let replaces_other = path.exists() && doc.path.as_deref() != Some(path.as_path());
        if replaces_other && !bool_arg(args, "overwrite")?.unwrap_or(false) {
            return Err(format!("{} already exists. Pass overwrite=true to replace it.", path.display()));
        }
    }
    crate::save_doc(doc, Some(target.clone()), sheet)?;
    Ok(Outcome::edit(json!({
        "saved_to": target.to_string_lossy(),
        "format": ext,
        "unsaved_changes": doc.dirty,
    })))
}

// ---------------------------------------------------------- argument parsing

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<Option<&'a str>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(format!("`{key}` must be a string")),
    }
}

fn req_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    str_arg(args, key)?.ok_or_else(|| format!("`{key}` is required"))
}

fn int_arg(args: &Value, key: &str) -> Result<Option<i64>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v.as_i64().map(Some).ok_or_else(|| format!("`{key}` must be a whole number")),
    }
}

fn bool_arg(args: &Value, key: &str) -> Result<Option<bool>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(format!("`{key}` must be true or false")),
    }
}

fn sheet_names(doc: &Doc) -> Vec<String> {
    doc.model.get_worksheets_properties().into_iter().map(|p| p.name).collect()
}

fn sheet_name(doc: &Doc, sheet: u32) -> String {
    sheet_names(doc).into_iter().nth(sheet as usize).unwrap_or_default()
}

/// A sheet by name (case-insensitive); a bare number is taken as a 0-based index.
fn sheet_index(doc: &Doc, arg: Option<&Value>) -> Result<u32, String> {
    let names = sheet_names(doc);
    let unknown = |what: String| format!("No sheet {what}. Sheets: {}", names.join(", "));
    match arg {
        None | Some(Value::Null) => Ok(0),
        Some(Value::String(s)) => names
            .iter()
            .position(|n| n.eq_ignore_ascii_case(s))
            .map(|i| i as u32)
            .ok_or_else(|| unknown(format!("named \"{s}\""))),
        Some(Value::Number(n)) => n
            .as_u64()
            .filter(|i| (*i as usize) < names.len())
            .map(|i| i as u32)
            .ok_or_else(|| unknown(format!("at index {n}"))),
        Some(_) => Err("`sheet` must be a sheet name".into()),
    }
}

fn used_range(doc: &Doc, sheet: u32) -> Result<(i32, i32), String> {
    let dim = doc.model.get_model().workbook.worksheet(sheet)?.dimension();
    Ok((dim.max_row, dim.max_column))
}

// ---------------------------------------------------------- A1 references

fn col_name(col: i32) -> String {
    let mut name = String::new();
    let mut n = col;
    while n > 0 {
        name.insert(0, (b'A' + ((n - 1) % 26) as u8) as char);
        n = (n - 1) / 26;
    }
    name
}

fn cell_label(row: i32, col: i32) -> String {
    format!("{}{row}", col_name(col))
}

/// 1-based, inclusive rectangle
#[derive(Clone, Copy, Debug, PartialEq)]
struct Rect {
    row: i32,
    col: i32,
    height: i32,
    width: i32,
}

impl Rect {
    fn label(&self) -> String {
        let (last_row, last_col) = (self.row + self.height - 1, self.col + self.width - 1);
        if (last_row, last_col) == (self.row, self.col) {
            cell_label(self.row, self.col)
        } else {
            format!("{}:{}", cell_label(self.row, self.col), cell_label(last_row, last_col))
        }
    }

    fn area(&self, sheet: u32) -> Area {
        Area { sheet, row: self.row, column: self.col, width: self.width, height: self.height }
    }

    fn cells(&self) -> i64 {
        self.height as i64 * self.width as i64
    }

    /// The part inside rows 1..=`rows` and columns 1..=`cols`.
    fn clip(&self, rows: i32, cols: i32) -> Option<Rect> {
        let last_row = (self.row + self.height - 1).min(rows);
        let last_col = (self.col + self.width - 1).min(cols);
        (self.row <= last_row && self.col <= last_col).then_some(Rect {
            height: last_row - self.row + 1,
            width: last_col - self.col + 1,
            ..*self
        })
    }
}

enum Ref {
    Cell(i32, i32),
    Col(i32),
    Row(i32),
}

fn parse_ref(text: &str) -> Result<Ref, String> {
    let bad = || format!("\"{text}\" is not a valid cell reference (use A1 notation, e.g. B2 or B2:D10)");
    let t = text.trim().replace('$', "").to_ascii_uppercase();
    let split = t.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(t.len());
    let (letters, digits) = t.split_at(split);
    if letters.len() > 3
        || !digits.bytes().all(|b| b.is_ascii_digit())
        || (letters.is_empty() && digits.is_empty())
    {
        return Err(bad());
    }
    let col = letters.bytes().fold(0i32, |n, b| n * 26 + (b - b'A') as i32 + 1);
    let row = if digits.is_empty() { 0 } else { digits.parse::<i32>().map_err(|_| bad())? };
    if col > MAX_COL || row > MAX_ROW || (!digits.is_empty() && row < 1) {
        return Err(bad());
    }
    Ok(match (letters.is_empty(), digits.is_empty()) {
        (false, false) => Ref::Cell(row, col),
        (false, true) => Ref::Col(col),
        _ => Ref::Row(row),
    })
}

/// Several ranges separated by commas, e.g. "A1:H1, A9:H9".
fn parse_ranges(text: &str, used_rows: i32, used_cols: i32) -> Result<Vec<Rect>, String> {
    text.split(',').map(|part| parse_range(part.trim(), used_rows, used_cols)).collect()
}

fn ranges_label(rects: &[Rect]) -> String {
    rects.iter().map(Rect::label).collect::<Vec<_>>().join(",")
}

/// Whole-column (`A:C`) and whole-row (`1:5`) ranges stretch over the used area only.
fn parse_range(text: &str, used_rows: i32, used_cols: i32) -> Result<Rect, String> {
    if text.contains('!') {
        return Err("Put the sheet name in `sheet`, not in the range".into());
    }
    let span = |r1: i32, c1: i32, r2: i32, c2: i32| Rect {
        row: r1.min(r2),
        col: c1.min(c2),
        height: (r1 - r2).abs() + 1,
        width: (c1 - c2).abs() + 1,
    };
    let (from, to) = match text.split_once(':') {
        Some((a, b)) => (parse_ref(a)?, Some(parse_ref(b)?)),
        None => (parse_ref(text)?, None),
    };
    match (from, to) {
        (Ref::Cell(r, c), None) => Ok(span(r, c, r, c)),
        (Ref::Cell(r1, c1), Some(Ref::Cell(r2, c2))) => Ok(span(r1, c1, r2, c2)),
        (Ref::Col(c1), Some(Ref::Col(c2))) => Ok(span(1, c1, used_rows.max(1), c2)),
        (Ref::Row(r1), Some(Ref::Row(r2))) => Ok(span(r1, 1, r2, used_cols.max(1))),
        _ => Err(format!("\"{text}\" is not a valid range (use B2, B2:D10, A:C or 1:5)")),
    }
}

// -------------------------------------------------------------- stdio bridge

const OFF_MESSAGE: &str = "SheetBro's MCP server is off. Open SheetBro, click the MCP button \
    in the toolbar and turn it on.";

/// Relays newline-delimited JSON-RPC from stdin to the running app and prints the answers.
/// Port and token come from the config file, so client configs hold no secrets.
pub fn stdio_bridge() -> i32 {
    let mut out = io::stdout();
    for line in io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match forward(&line) {
            Ok(reply) => reply,
            Err(message) => failure_reply(&line, &message),
        };
        if let Some(reply) = reply {
            if writeln!(out, "{reply}").and_then(|_| out.flush()).is_err() {
                break;
            }
        }
    }
    0
}

fn forward(message: &str) -> Result<Option<String>, String> {
    let config = Config::load();
    if !config.enabled || config.token.is_empty() {
        return Err(OFF_MESSAGE.into());
    }
    let (status, body) = post(config.port, &config.token, message)?;
    match status {
        200 => Ok(Some(body)),
        202 => Ok(None),
        401 => Err("SheetBro rejected the access token. Restart this AI tool so it picks up the new one.".into()),
        _ => {
            let detail = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
                .unwrap_or(body);
            Err(format!("SheetBro answered HTTP {status}: {detail}"))
        }
    }
}

/// An error answer for a request that could not be relayed; notifications get none.
fn failure_reply(message: &str, error: &str) -> Option<String> {
    let id = serde_json::from_str::<Value>(message).ok()?.get("id")?.clone();
    Some(rpc_error(id, -32000, error).to_string())
}

/// One plain HTTP/1.1 POST over a fresh connection; returns status and body.
fn post(port: u16, token: &str, body: &str) -> Result<(u16, String), String> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(3)).map_err(|_| {
        format!("Cannot reach SheetBro on port {port}. Is the app running with its MCP server turned on?")
    })?;
    let io_err = |e: io::Error| format!("Lost the connection to SheetBro: {e}");
    stream.set_read_timeout(Some(Duration::from_secs(120))).map_err(io_err)?;
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\n\
         Content-Type: application/json\r\nAccept: application/json, text/event-stream\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).map_err(io_err)?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).map_err(io_err)?;
    let text = String::from_utf8_lossy(&raw);
    let (head, body) = text.split_once("\r\n\r\n").ok_or("Malformed answer from SheetBro")?;
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or("Malformed answer from SheetBro")?;
    Ok((status, body.trim().to_string()))
}

// --------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::new_model;
    use ironcalc::base::types::Color;
    use std::sync::MutexGuard;

    struct TestHost {
        doc: Mutex<Doc>,
        changes: Mutex<Vec<Change>>,
    }

    impl TestHost {
        fn new() -> Arc<TestHost> {
            Arc::new(TestHost {
                doc: Mutex::new(Doc::new(new_model("t").unwrap(), None)),
                changes: Mutex::new(Vec::new()),
            })
        }
    }

    impl Host for TestHost {
        type Guard<'a> = MutexGuard<'a, Doc>;

        fn lock(&self) -> MutexGuard<'_, Doc> {
            self.doc.lock().unwrap()
        }
        fn changed(&self, change: Change) {
            self.changes.lock().unwrap().push(change);
        }
    }

    /// Calls a tool through the JSON-RPC layer; returns (is_error, parsed text).
    fn call(host: &TestHost, name: &str, args: Value) -> (bool, Value) {
        let msg = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": name, "arguments": args } });
        let reply = handle_message(host, &msg).unwrap();
        let result = &reply["result"];
        let text = result["content"][0]["text"].as_str().unwrap();
        let value = serde_json::from_str(text).unwrap_or_else(|_| json!(text));
        (result["isError"].as_bool().unwrap(), value)
    }

    fn ok(host: &TestHost, name: &str, args: Value) -> Value {
        let (is_error, value) = call(host, name, args);
        assert!(!is_error, "{name} failed: {value}");
        value
    }

    #[test]
    fn advertises_every_tool() {
        let defs = tool_defs();
        let names: Vec<&str> = defs.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, TOOL_NAMES);
        for t in &defs {
            assert_eq!(t["inputSchema"]["type"], "object", "{}", t["name"]);
        }
    }

    #[test]
    fn handshake_negotiates_protocol_version() {
        let host = TestHost::new();
        let init = |v: &str| {
            handle_message(&*host, &json!({ "jsonrpc": "2.0", "id": 7, "method": "initialize", "params": { "protocolVersion": v } }))
                .unwrap()
        };
        let reply = init("2025-03-26");
        assert_eq!(reply["id"], 7);
        assert_eq!(reply["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(reply["result"]["serverInfo"]["name"], "sheetbro");
        assert!(reply["result"]["capabilities"]["tools"].is_object());
        // an unknown revision gets our newest instead of an error
        assert_eq!(init("1999-01-01")["result"]["protocolVersion"], PROTOCOLS[0]);

        // notifications and stray responses get no answer; unknown methods do
        assert!(handle_message(&*host, &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).is_none());
        assert!(handle_message(&*host, &json!({ "jsonrpc": "2.0", "id": 1, "result": {} })).is_none());
        let missing = handle_message(&*host, &json!({ "jsonrpc": "2.0", "id": 2, "method": "resources/list" })).unwrap();
        assert_eq!(missing["error"]["code"], -32601);
        let ping = handle_message(&*host, &json!({ "jsonrpc": "2.0", "id": 3, "method": "ping" })).unwrap();
        assert_eq!(ping["result"], json!({}));
        let unknown = handle_message(&*host, &json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": { "name": "nope" } })).unwrap();
        assert_eq!(unknown["error"]["code"], -32602);
    }

    #[test]
    fn parses_ranges() {
        let r = |t: &str| parse_range(t, 10, 4);
        assert_eq!(r("B2").unwrap(), Rect { row: 2, col: 2, height: 1, width: 1 });
        assert_eq!(r("$B$2:d5").unwrap(), Rect { row: 2, col: 2, height: 4, width: 3 });
        // corners given backwards are normalised
        assert_eq!(r("D5:B2").unwrap(), r("B2:D5").unwrap());
        // whole columns / rows stretch over the used area
        assert_eq!(r("A:C").unwrap(), Rect { row: 1, col: 1, height: 10, width: 3 });
        assert_eq!(r("2:3").unwrap(), Rect { row: 2, col: 1, height: 2, width: 4 });
        assert_eq!(r("AA1").unwrap().col, 27);
        assert_eq!(r("A1:C3").unwrap().label(), "A1:C3");
        assert_eq!(r("C3").unwrap().label(), "C3");
        for bad in ["", "A", "1", "A1:", "A:3", "A0", "ZZZZ1", "A1:B2:C3", "Sheet1!A1", "A1048577", "XFE1", "99999999999"] {
            assert!(r(bad).is_err(), "{bad} should be rejected");
        }
        assert_eq!(col_name(16_384), "XFD");
    }

    #[test]
    fn writes_reads_and_recalculates() {
        let host = TestHost::new();
        let wrote = ok(&host, "write_range", json!({
            "start": "A1",
            "values": [["Item", "Qty", "Price"], ["Pen", 3, 1.5], ["Ink", 2, "=B3*10"], ["Total", "=SUM(B2:B3)", null]],
        }));
        assert_eq!(wrote["range"], "A1:C4");
        assert_eq!(wrote["cells_written"], 12);
        assert!(wrote.get("formula_errors").is_none());
        assert_eq!(*host.changes.lock().unwrap(), vec![Change::Edited]);

        let read = ok(&host, "read_range", json!({ "range": "A1:C4" }));
        assert_eq!(read["values"][1], json!(["Pen", "3", "1.5"]));
        assert_eq!(read["values"][2][2], "20");
        assert_eq!(read["values"][3][1], "5");
        assert_eq!(read["values"][3][2], "");
        assert_eq!(read["sheet"], "Sheet1");

        let formulas = ok(&host, "read_range", json!({ "range": "B4", "formulas": true }));
        assert_eq!(formulas["values"], json!([["=SUM(B2:B3)"]]));

        // no range = the whole used area; ranges past the data are empty, not an error
        assert_eq!(ok(&host, "read_range", json!({}))["range"], "A1:C4");
        assert_eq!(ok(&host, "read_range", json!({ "range": "A:A" }))["values"][3], json!(["Total"]));
        assert_eq!(ok(&host, "read_range", json!({ "range": "H20:J30" }))["values"], json!([]));

        // editing a cell recalculates dependants
        ok(&host, "write_range", json!({ "start": "B2", "values": [[10]] }));
        assert_eq!(ok(&host, "read_range", json!({ "range": "B4" }))["values"][0][0], "12");
    }

    #[test]
    fn reports_formula_errors_and_bad_input() {
        let host = TestHost::new();
        let wrote = ok(&host, "write_range", json!({ "start": "B2", "values": [["=NOPE(1)", "fine"]] }));
        assert_eq!(wrote["formula_errors"][0]["cell"], "B2");

        // failures come back as tool errors the model can read, not protocol errors
        for (tool, args) in [
            ("write_range", json!({ "start": "A1", "values": "x" })),
            ("write_range", json!({ "start": "A1:B2", "values": [[1]] })),
            ("write_range", json!({ "start": "A1", "values": [[{ "a": 1 }]] })),
            ("write_range", json!({ "start": "A1", "values": [] })),
            ("write_range", json!({ "start": "XFD1", "values": [[1, 2]] })),
            ("read_range", json!({ "sheet": "Nope" })),
            ("read_range", json!({ "range": "??" })),
            ("format_range", json!({ "range": "A1" })),
            ("format_range", json!({ "range": "A1", "fill_color": "red" })),
            ("format_range", json!({ "range": "A1", "align": "justify" })),
            ("clear_range", json!({})),
        ] {
            let (is_error, message) = call(&host, tool, args.clone());
            assert!(is_error, "{tool} {args} should fail, got {message}");
        }
        let (_, message) = call(&host, "read_range", json!({ "sheet": "Nope" }));
        assert!(message.as_str().unwrap().contains("Sheet1"), "lists the real sheets: {message}");
    }

    #[test]
    fn refuses_oversized_reads() {
        let host = TestHost::new();
        let row: Vec<Value> = (0..200).map(|i| json!(i)).collect();
        let rows: Vec<Value> = (0..150).map(|_| json!(row)).collect();
        ok(&host, "write_range", json!({ "start": "A1", "values": rows }));
        let (is_error, message) = call(&host, "read_range", json!({}));
        assert!(is_error);
        assert!(message.as_str().unwrap().contains("30000 cells"), "{message}");
        assert_eq!(ok(&host, "read_range", json!({ "range": "A1:T100" }))["values"].as_array().unwrap().len(), 100);
    }

    #[test]
    fn formats_clears_and_undoes_each_call_as_one_step() {
        let host = TestHost::new();
        ok(&host, "write_range", json!({ "start": "A1", "values": [["a", "b"], ["c", "d"]] }));
        ok(&host, "format_range", json!({
            "range": "A1:B1", "bold": true, "italic": true, "text_color": "#1f5a43", "fill_color": "#fdf3c8",
            "align": "center", "number_format": "0.00",
        }));
        {
            let doc = host.lock();
            let style = doc.model.get_cell_style(0, 1, 2).unwrap();
            assert!(style.font.b && style.font.i);
            assert!(!doc.model.get_cell_style(0, 2, 1).unwrap().font.b);
            assert_eq!(style.num_fmt, "0.00");
        }
        // removing the fill, then an undo brings it back
        ok(&host, "format_range", json!({ "range": "A1", "fill_color": "none" }));
        assert_eq!(ok(&host, "undo", json!({}))["performed"], true);
        {
            let doc = host.lock();
            assert!(crate::rgb(&doc.model.get_cell_style(0, 1, 1).unwrap().fill.color).is_some());
        }
        // the whole 5-property format is a single undo step
        ok(&host, "undo", json!({}));
        assert!(!host.lock().model.get_cell_style(0, 1, 1).unwrap().font.b);
        ok(&host, "redo", json!({}));
        assert!(host.lock().model.get_cell_style(0, 1, 1).unwrap().font.b);

        let cleared = ok(&host, "clear_range", json!({ "range": "A1:B2" }));
        assert_eq!(cleared["cleared"], true);
        assert_eq!(ok(&host, "read_range", json!({ "range": "A1:B2", "formulas": true }))["values"], json!([["", ""], ["", ""]]));
        // formatting survives a clear
        assert!(host.lock().model.get_cell_style(0, 1, 1).unwrap().font.b);
        // clearing the far-away empty space is a no-op instead of a million-row sweep
        let far = ok(&host, "clear_range", json!({ "range": "A100:Z1000000" }));
        assert!(far.get("cleared").is_none());
        ok(&host, "undo", json!({}));
        assert_eq!(ok(&host, "read_range", json!({ "range": "B2" }))["values"][0][0], "d");

        // drain the history: further undos say so instead of failing
        while ok(&host, "undo", json!({}))["performed"] == true {}
        assert_eq!(ok(&host, "undo", json!({}))["reason"], "Nothing to undo");
        assert_eq!(ok(&host, "redo", json!({}))["performed"], true);
    }

    #[test]
    fn adds_and_addresses_sheets() {
        let host = TestHost::new();
        let added = ok(&host, "add_sheet", json!({}));
        assert_eq!(added["added"], "Sheet2");
        ok(&host, "write_range", json!({ "sheet": "sheet2", "start": "A1", "values": [["x"]] }));
        ok(&host, "write_range", json!({ "sheet": 0, "start": "A1", "values": [["y"]] }));
        assert_eq!(ok(&host, "read_range", json!({ "sheet": "Sheet2" }))["values"][0][0], "x");
        assert_eq!(ok(&host, "read_range", json!({}))["values"][0][0], "y");
        let info = ok(&host, "workbook_info", json!({}));
        assert_eq!(info["sheets"].as_array().unwrap().len(), 2);
        assert_eq!(info["dirty"], true);
        assert_eq!(info["can_undo"], true);
    }

    fn is_bold(host: &TestHost, row: i32, col: i32) -> bool {
        host.lock().model.get_cell_style(0, row, col).unwrap().font.b
    }

    #[test]
    fn formats_and_clears_several_ranges_in_one_call() {
        let host = TestHost::new();
        ok(&host, "write_range", json!({ "start": "A1", "values": [["a", "b"], ["c", "d"], ["e", "f"]] }));
        let done = ok(&host, "format_range", json!({ "range": "A1:B1, A3", "bold": true }));
        assert_eq!(done["range"], "A1:B1,A3");
        assert!(is_bold(&host, 1, 2) && is_bold(&host, 3, 1));
        assert!(!is_bold(&host, 2, 1));
        // both ranges are one undo step
        ok(&host, "undo", json!({}));
        assert!(!is_bold(&host, 1, 1) && !is_bold(&host, 3, 1));

        ok(&host, "clear_range", json!({ "range": "A1,B3" }));
        assert_eq!(ok(&host, "read_range", json!({}))["values"], json!([["", "b"], ["c", "d"], ["e", ""]]));
        ok(&host, "undo", json!({}));
        assert_eq!(ok(&host, "read_range", json!({}))["values"][0][0], "a");
        assert!(call(&host, "format_range", json!({ "range": "A1,Q", "bold": true })).0);
    }

    #[test]
    fn formats_text_size_wrapping_alignment_and_borders() {
        let host = TestHost::new();
        ok(&host, "write_range", json!({ "start": "A1", "values": [["a", "b"], ["c", "d"]] }));
        let done = ok(&host, "format_range", json!({
            "range": "A1:B1", "font_size": 14, "strikethrough": true, "wrap_text": true, "vertical_align": "top",
        }));
        assert!(done["note"].as_str().unwrap().contains("does not draw"), "{done}");
        {
            let doc = host.lock();
            let style = doc.model.get_cell_style(0, 1, 2).unwrap();
            assert_eq!(style.font.sz, 14);
            assert!(style.font.strike);
            let alignment = style.alignment.unwrap();
            assert!(alignment.wrap_text);
            assert_eq!(alignment.vertical, VerticalAlignment::Top);
            assert_eq!(doc.model.get_cell_style(0, 2, 1).unwrap().font.sz, 12);
        }
        // plain formatting says nothing about the grid
        assert!(ok(&host, "format_range", json!({ "range": "A1", "bold": true })).get("note").is_none());

        ok(&host, "format_range", json!({ "range": "A1:B2", "borders": { "sides": "outer", "style": "medium", "color": "#ff0000" } }));
        {
            let doc = host.lock();
            let top_left = doc.model.get_cell_style(0, 1, 1).unwrap().border;
            let medium = |b: &Option<BorderItem>| b.as_ref().map(|i| i.style.clone());
            assert_eq!(medium(&top_left.top), Some(BorderStyle::Medium));
            assert_eq!(medium(&top_left.left), Some(BorderStyle::Medium));
            assert_eq!(top_left.top.unwrap().color, Color::Rgb("#FF0000".into()));
            // only the outline: the cell edges inside stay bare
            assert!(top_left.right.is_none() && top_left.bottom.is_none());
        }
        // a call with borders is one undo step, and "none" takes them off again
        ok(&host, "undo", json!({}));
        assert!(host.lock().model.get_cell_style(0, 1, 1).unwrap().border.top.is_none());
        ok(&host, "format_range", json!({ "range": "A1:B2", "borders": { "style": "thin" } }));
        assert!(host.lock().model.get_cell_style(0, 1, 2).unwrap().border.bottom.is_some());
        ok(&host, "format_range", json!({ "range": "A1:B2", "borders": { "sides": "none" } }));
        assert!(host.lock().model.get_cell_style(0, 1, 2).unwrap().border.bottom.is_none());

        for bad in [
            json!({ "font_size": 0 }),
            json!({ "font_size": 2.5 }),
            json!({ "vertical_align": "middle" }),
            json!({ "borders": { "sides": "diagonal" } }),
            json!({ "borders": { "style": "wavy" } }),
            json!({ "borders": { "color": "red" } }),
            json!({ "borders": "all" }),
        ] {
            let mut args = bad.clone();
            args["range"] = json!("A1");
            assert!(call(&host, "format_range", args).0, "{bad} should be rejected");
        }
    }

    #[test]
    fn sets_widths_heights_freeze_panes_and_merges_in_one_step() {
        let host = TestHost::new();
        ok(&host, "write_range", json!({ "start": "A1", "values": [["2 x Platinum 8249C server", 1], ["x", 2]] }));
        let (default_width, default_height) = {
            let doc = host.lock();
            (doc.model.get_column_width(0, 2).unwrap(), doc.model.get_row_height(0, 2).unwrap())
        };
        let done = ok(&host, "set_layout", json!({
            "columns": [{ "range": "A", "width": "auto" }, { "range": "B:C", "width": 40 }],
            "rows": [{ "range": "1:2", "height": 32 }],
            "freeze": { "rows": 1, "columns": 1 },
            "merge": ["D1:E1"],
        }));
        assert_eq!(done["columns"][0]["range"], "A");
        let fitted = done["columns"][0]["width"].as_f64().unwrap();
        // 26 characters of text: clearly wider than the 90px default, nowhere near the limit
        assert!((130.0..300.0).contains(&fitted), "{fitted}");
        {
            let doc = host.lock();
            assert_eq!(doc.model.get_column_width(0, 1).unwrap(), fitted);
            assert_eq!(doc.model.get_column_width(0, 2).unwrap(), 40.0);
            assert_eq!(doc.model.get_column_width(0, 3).unwrap(), 40.0);
            assert_eq!(doc.model.get_row_height(0, 2).unwrap(), 32.0);
            assert_eq!((doc.model.get_frozen_rows_count(0).unwrap(), doc.model.get_frozen_columns_count(0).unwrap()), (1, 1));
            assert_eq!(doc.model.get_merged_cells(0).unwrap().len(), 1);
        }
        assert_eq!(*host.changes.lock().unwrap(), vec![Change::Edited, Change::Edited]);
        // everything above was one undo step
        ok(&host, "undo", json!({}));
        {
            let doc = host.lock();
            assert_eq!(doc.model.get_column_width(0, 2).unwrap(), default_width);
            assert_eq!(doc.model.get_row_height(0, 2).unwrap(), default_height);
            assert_eq!(doc.model.get_frozen_rows_count(0).unwrap(), 0);
            assert!(doc.model.get_merged_cells(0).unwrap().is_empty());
        }
        assert_eq!(host.lock().model.get_cell_content(0, 1, 1).unwrap(), "2 x Platinum 8249C server");

        // an empty column fits to the default width; numbers are measured too
        let empty = ok(&host, "set_layout", json!({ "columns": [{ "range": "H", "width": "auto" }] }));
        assert_eq!(empty["columns"][0]["width"], default_width);

        // merging over other values needs the caller's say-so, like in the app
        ok(&host, "write_range", json!({ "start": "F1", "values": [["one", "two"]] }));
        let (failed, message) = call(&host, "set_layout", json!({ "merge": ["F1:G1"] }));
        assert!(failed && message.as_str().unwrap().contains("discard_values"), "{message}");
        ok(&host, "set_layout", json!({ "merge": ["F1:G1"], "discard_values": true }));
        assert_eq!(host.lock().model.get_cell_content(0, 1, 7).unwrap(), "");
        ok(&host, "set_layout", json!({ "unmerge": ["F1"] }));
        assert!(host.lock().model.get_merged_cells(0).unwrap().is_empty());

        for bad in [
            json!({}),
            json!({ "columns": [{ "range": "A:B" }] }),
            json!({ "columns": [{ "range": "A", "width": 0 }] }),
            json!({ "columns": [{ "range": "A", "width": 5000 }] }),
            json!({ "columns": [{ "range": "1", "width": 50 }] }),
            json!({ "rows": [{ "range": "A", "height": 20 }] }),
            json!({ "rows": [{ "range": "1", "height": -1 }] }),
            json!({ "freeze": { "rows": -1 } }),
            json!({ "freeze": { "rows": 2_000_000 } }),
            json!({ "freeze": {} }),
            json!({ "merge": ["A1"] }),
            json!({ "merge": "A1:B2" }),
        ] {
            assert!(call(&host, "set_layout", bad.clone()).0, "{bad} should be rejected");
        }
    }

    #[test]
    fn names_renames_moves_and_deletes_sheets() {
        let host = TestHost::new();
        assert_eq!(ok(&host, "add_sheet", json!({ "name": "Prices" }))["added"], "Prices");
        ok(&host, "write_range", json!({ "sheet": "Prices", "start": "A1", "values": [[5]] }));
        ok(&host, "write_range", json!({ "start": "A1", "values": [["=Prices!A1*2"]] }));
        assert_eq!(ok(&host, "read_range", json!({}))["values"][0][0], "10");

        let renamed = ok(&host, "rename_sheet", json!({ "sheet": "prices", "name": "Price list" }));
        assert_eq!(renamed["sheets"], json!(["Sheet1", "Price list"]));
        // formulas follow the new name
        assert_eq!(ok(&host, "read_range", json!({ "formulas": true }))["values"][0][0], "='Price list'!A1*2");
        assert_eq!(ok(&host, "read_range", json!({}))["values"][0][0], "10");

        let moved = ok(&host, "move_sheet", json!({ "sheet": "Price list", "position": 1 }));
        assert_eq!(moved["sheets"], json!(["Price list", "Sheet1"]));
        ok(&host, "undo", json!({}));
        assert_eq!(ok(&host, "workbook_info", json!({}))["sheets"][0]["name"], "Sheet1");

        let deleted = ok(&host, "delete_sheet", json!({ "sheet": "Sheet1" }));
        assert_eq!(deleted["sheets"], json!(["Price list"]));
        ok(&host, "undo", json!({}));
        assert_eq!(ok(&host, "workbook_info", json!({}))["sheets"].as_array().unwrap().len(), 2);

        let rejected = [
            ("add_sheet", json!({ "name": "price list" })),
            ("add_sheet", json!({ "name": "" })),
            ("add_sheet", json!({ "name": "a/b" })),
            ("add_sheet", json!({ "name": "x".repeat(32) })),
            ("rename_sheet", json!({ "sheet": "Sheet1" })),
            ("rename_sheet", json!({ "sheet": "Sheet1", "name": "Price list" })),
            ("rename_sheet", json!({ "sheet": "Nope", "name": "Z" })),
            ("move_sheet", json!({ "sheet": "Sheet1", "position": 0 })),
            ("move_sheet", json!({ "sheet": "Sheet1", "position": 3 })),
            ("delete_sheet", json!({})),
        ];
        for (tool, args) in rejected {
            assert!(call(&host, tool, args.clone()).0, "{tool} {args} should be rejected");
        }
        // renaming to a different case of its own name is fine
        ok(&host, "rename_sheet", json!({ "sheet": "Sheet1", "name": "SHEET1" }));
        // the last sheet stays
        ok(&host, "delete_sheet", json!({ "sheet": "Price list" }));
        assert!(call(&host, "delete_sheet", json!({ "sheet": "SHEET1" })).0);
    }

    #[test]
    fn inserts_and_deletes_rows_and_columns() {
        let host = TestHost::new();
        ok(&host, "write_range", json!({ "start": "A1", "values": [["a", "b"], ["c", "d"], ["=A1&A2", "=SUM(1,2)"]] }));
        let inserted = ok(&host, "insert_lines", json!({ "axis": "rows", "at": 2, "count": 2 }));
        assert_eq!(inserted["inserted"], "rows 2:3");
        assert_eq!(ok(&host, "read_range", json!({ "range": "A1:A5" }))["values"], json!([["a"], [""], [""], ["c"], ["ac"]]));
        ok(&host, "insert_lines", json!({ "axis": "columns", "at": "A" }));
        assert_eq!(ok(&host, "read_range", json!({ "range": "B1" }))["values"][0][0], "a");
        ok(&host, "undo", json!({}));
        assert_eq!(ok(&host, "read_range", json!({ "range": "A1" }))["values"][0][0], "a");

        let deleted = ok(&host, "delete_lines", json!({ "axis": "rows", "at": 2, "count": 2 }));
        assert_eq!(deleted["deleted"], "rows 2:3");
        assert_eq!(ok(&host, "read_range", json!({ "range": "A1:A3" }))["values"], json!([["a"], ["c"], ["ac"]]));
        // formulas keep working after a delete, and undo brings the rows back
        ok(&host, "undo", json!({}));
        assert_eq!(ok(&host, "read_range", json!({ "range": "A4:A5" }))["values"], json!([["c"], ["ac"]]));
        ok(&host, "delete_lines", json!({ "axis": "columns", "at": "B", "count": 1 }));
        assert_eq!(ok(&host, "read_range", json!({ "range": "A1:B1" }))["values"], json!([["a"]]));

        for (tool, args) in [
            ("insert_lines", json!({ "axis": "rows" })),
            ("insert_lines", json!({ "at": 1 })),
            ("insert_lines", json!({ "axis": "cells", "at": 1 })),
            ("insert_lines", json!({ "axis": "rows", "at": "A" })),
            ("insert_lines", json!({ "axis": "columns", "at": 0 })),
            ("insert_lines", json!({ "axis": "rows", "at": 0 })),
            ("insert_lines", json!({ "axis": "rows", "at": 1, "count": 0 })),
            ("insert_lines", json!({ "axis": "rows", "at": 1, "count": 100_000 })),
            ("delete_lines", json!({ "axis": "rows", "at": 1, "count": 2_000_000 })),
        ] {
            assert!(call(&host, tool, args.clone()).0, "{tool} {args} should be rejected");
        }
    }

    #[test]
    fn a_panicking_tool_becomes_an_error_and_the_next_call_still_works() {
        let failed: Result<(), String> = guarded(|| panic!("boom"));
        assert!(failed.unwrap_err().contains("Internal error"));
        assert_eq!(guarded(|| Ok::<_, String>(5)), Ok(5));

        // a lock that a panicking thread held is still usable afterwards
        let lock = Arc::new(Mutex::new(1));
        let held = lock.clone();
        let _ = std::thread::spawn(move || {
            let _guard = held.lock().unwrap();
            panic!("poison it");
        })
        .join();
        assert!(lock.lock().is_err());
        assert_eq!(*lock.lock_recovering(), 1);
    }

    #[test]
    fn edits_say_whether_the_workbook_has_unsaved_changes() {
        let host = TestHost::new();
        assert_eq!(ok(&host, "write_range", json!({ "start": "A1", "values": [["a"]] }))["unsaved_changes"], true);
        assert_eq!(ok(&host, "format_range", json!({ "range": "A1", "bold": true }))["unsaved_changes"], true);
        assert!(ok(&host, "read_range", json!({})).get("unsaved_changes").is_none());
        let path = tmp("unsaved.xlsx");
        let _ = fs::remove_file(&path);
        let saved = ok(&host, "save_workbook", json!({ "path": path.to_string_lossy() }));
        assert_eq!(saved["unsaved_changes"], false);
        assert_eq!(ok(&host, "clear_range", json!({ "range": "A1" }))["unsaved_changes"], true);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn converts_spreadsheet_serials_to_dates() {
        assert_eq!(excel_date(46324.0), "2026-10-29");
        assert_eq!(excel_date(25569.0), "1970-01-01");
        assert_eq!(excel_date(61.0), "1900-03-01");
        assert_eq!(excel_date(1.0), "1900-01-01");
        assert_eq!(excel_date(46324.5), "2026-10-29T12:00:00");
        assert_eq!(excel_date(46324.75), "2026-10-29T18:00:00");
        // a bare time of day
        assert_eq!(excel_date(0.25), "06:00:00");
        // float noise must not turn 07:12:00 into 07:11:59
        assert_eq!(excel_date(46324.3), "2026-10-29T07:12:00");
    }

    #[test]
    fn reads_typed_values() {
        let host = TestHost::new();
        ok(&host, "write_range", json!({
            "start": "A1",
            "values": [[100000, "Pen", true, null, "=A1*2", "=NOPE(1)", 46324, 1.5, "=A1&\"x\"", "=1>0"]],
        }));
        ok(&host, "format_range", json!({ "range": "G1", "number_format": "yyyy-mm-dd" }));

        let typed = ok(&host, "read_range", json!({ "typed": true }));
        assert_eq!(
            typed["values"][0],
            json!([100000, "Pen", true, null, 200000, { "error": "#NAME?" }, { "date": "2026-10-29" }, 1.5, "100000x", true])
        );
        // the number and its display text are different things now
        assert_eq!(ok(&host, "read_range", json!({}))["values"][0][0], "100000");
        assert!(typed["values"][0][0].is_number());

        // with formulas=true the formula cells give their formula and the rest stay typed
        let both = ok(&host, "read_range", json!({ "typed": true, "formulas": true, "range": "A1:E1" }));
        assert_eq!(both["values"][0], json!([100000, "Pen", true, null, "=A1*2"]));
        assert!(call(&host, "read_range", json!({ "typed": "yes" })).0);
    }

    #[test]
    fn reads_formatting_and_layout() {
        let host = TestHost::new();
        ok(&host, "write_range", json!({ "start": "A1", "values": [["a", "b"], ["c", "d"]] }));
        ok(&host, "format_range", json!({
            "range": "A1:B1", "bold": true, "fill_color": "#fdf3c8", "number_format": "0.00", "align": "center",
            "font_size": 14, "wrap_text": true, "vertical_align": "top",
        }));
        ok(&host, "format_range", json!({ "range": "A2", "borders": { "sides": "bottom", "style": "thick", "color": "#1f5a43" } }));
        ok(&host, "set_layout", json!({
            "columns": [{ "range": "A", "width": 144 }],
            "rows": [{ "range": "1", "height": 32 }],
            "freeze": { "rows": 1 },
            "merge": ["A1:B1"],
            "discard_values": true,
        }));

        let plain = ok(&host, "read_range", json!({}));
        assert!(plain.get("formatting").is_none());
        let read = ok(&host, "read_range", json!({ "include_formatting": true }));
        let f = &read["formatting"];
        assert_eq!(f["styles"][0], json!({}));
        let grid = f["cells"].as_array().unwrap();
        assert_eq!((grid.len(), grid[0].as_array().unwrap().len()), (2, 2));
        let style = |r: usize, c: usize| f["styles"][grid[r][c].as_u64().unwrap() as usize].clone();
        assert_eq!(grid[0][0], grid[0][1]);
        assert_eq!(
            style(0, 0),
            json!({
                "bold": true, "font_size": 14, "fill_color": "#FDF3C8", "number_format": "0.00",
                "align": "center", "vertical_align": "top", "wrap_text": true,
            })
        );
        assert_eq!(style(1, 0), json!({ "borders": { "bottom": "thick #1F5A43" } }));
        assert_eq!(style(1, 1), json!({}));
        // every distinct style is listed once
        assert_eq!(f["styles"].as_array().unwrap().len(), 3);

        assert_eq!(f["column_widths"], json!({ "A": 144.0, "B": 90.0 }));
        assert_eq!(f["row_heights"], json!({ "1": 32.0 }));
        assert_eq!(f["merged"], json!(["A1:B1"]));
        assert_eq!(f["frozen"], json!({ "rows": 1, "columns": 0 }));

        // only what touches the range is reported
        let part = ok(&host, "read_range", json!({ "range": "B2", "include_formatting": true }));
        let f = &part["formatting"];
        assert_eq!(f["column_widths"], json!({ "B": 90.0 }));
        assert_eq!(f["row_heights"], json!({}));
        assert_eq!(f["merged"], json!([]));
        assert_eq!(f["cells"], json!([[0]]));
    }

    #[test]
    fn clears_contents_formats_or_both() {
        let host = TestHost::new();
        ok(&host, "write_range", json!({ "start": "A1", "values": [["a", "b"]] }));
        ok(&host, "format_range", json!({ "range": "A1:B1", "bold": true, "fill_color": "#fdf3c8", "number_format": "0.00" }));

        let cleared = ok(&host, "clear_range", json!({ "range": "A1", "what": "formats" }));
        assert_eq!((&cleared["cleared"], &cleared["what"]), (&json!(true), &json!("formats")));
        assert_eq!(ok(&host, "read_range", json!({ "range": "A1" }))["values"][0][0], "a");
        {
            let doc = host.lock();
            let style = doc.model.get_cell_style(0, 1, 1).unwrap();
            assert!(!style.font.b);
            assert_eq!(style.num_fmt, "general");
            assert!(crate::rgb(&style.fill.color).is_none());
        }
        // contents is the default and keeps the formatting, as it always did
        ok(&host, "clear_range", json!({ "range": "B1" }));
        assert!(is_bold(&host, 1, 2));
        ok(&host, "clear_range", json!({ "range": "B1", "what": "all" }));
        assert!(!is_bold(&host, 1, 2));
        assert_eq!(host.lock().model.get_cell_content(0, 1, 2).unwrap(), "");
        // "all" is one undo step; the contents were already gone before it
        ok(&host, "undo", json!({}));
        assert_eq!(host.lock().model.get_cell_content(0, 1, 2).unwrap(), "");
        assert!(is_bold(&host, 1, 2));
        assert!(call(&host, "clear_range", json!({ "range": "A1", "what": "everything" })).0);
    }

    #[test]
    fn batch_applies_many_operations_as_one_undo_step() {
        let host = TestHost::new();
        let done = ok(&host, "batch", json!({ "operations": [
            { "tool": "write_range", "args": { "start": "A1", "values": [["a", "b"], ["c", "d"]] } },
            { "tool": "format_range", "args": { "range": "A1:B1", "bold": true } },
            { "tool": "format_range", "args": { "range": "A2:B2", "italic": true } },
            { "tool": "add_sheet", "args": {} },
        ] }));
        assert_eq!(done["applied"], 4);
        assert_eq!(done["results"][0]["cells_written"], 4);
        assert_eq!(done["results"][3]["added"], "Sheet2");
        // the window refreshes once, not once per operation
        assert_eq!(*host.changes.lock().unwrap(), vec![Change::Edited]);
        assert!(is_bold(&host, 1, 1));

        assert_eq!(ok(&host, "undo", json!({}))["performed"], true);
        assert!(!is_bold(&host, 1, 1));
        assert_eq!(host.lock().model.get_cell_content(0, 1, 1).unwrap(), "");
        assert_eq!(ok(&host, "workbook_info", json!({}))["sheets"].as_array().unwrap().len(), 1);
        assert_eq!(ok(&host, "undo", json!({}))["performed"], false);
        ok(&host, "redo", json!({}));
        assert!(is_bold(&host, 1, 1));
        assert_eq!(ok(&host, "workbook_info", json!({}))["sheets"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn batch_is_all_or_nothing() {
        let host = TestHost::new();
        ok(&host, "write_range", json!({ "start": "A1", "values": [["keep"]] }));
        let (failed, message) = call(&host, "batch", json!({ "operations": [
            { "tool": "write_range", "args": { "start": "A1", "values": [["lost"]] } },
            { "tool": "format_range", "args": { "range": "A1", "bold": true } },
            { "tool": "format_range", "args": { "range": "A1", "text_color": "red" } },
        ] }));
        assert!(failed);
        let message = message.as_str().unwrap();
        assert!(message.contains("Operation 3") && message.contains("format_range"), "{message}");
        assert!(message.contains("nothing was changed"), "{message}");
        assert_eq!(ok(&host, "read_range", json!({}))["values"][0][0], "keep");
        assert!(!is_bold(&host, 1, 1));
        // the rolled-back work leaves neither a redo step nor extra undo steps behind
        assert_eq!(ok(&host, "redo", json!({}))["performed"], false);
        assert_eq!(ok(&host, "undo", json!({}))["performed"], true);
        assert_eq!(ok(&host, "undo", json!({}))["performed"], false);
        // an untouched workbook stays clean
        let fresh = TestHost::new();
        assert!(call(&fresh, "batch", json!({ "operations": [{ "tool": "clear_range", "args": {} }] })).0);
        assert_eq!(ok(&fresh, "workbook_info", json!({}))["dirty"], false);
    }

    #[test]
    fn batch_only_takes_editing_tools() {
        let host = TestHost::new();
        for tool in ["read_range", "undo", "save_workbook", "open_workbook", "batch", "nope"] {
            let (failed, message) = call(&host, "batch", json!({ "operations": [{ "tool": tool, "args": {} }] }));
            assert!(failed, "{tool}");
            assert!(message.as_str().unwrap().contains("Operation 1"), "{message}");
        }
        assert!(call(&host, "batch", json!({ "operations": [] })).0);
        assert!(call(&host, "batch", json!({})).0);
        assert!(call(&host, "batch", json!({ "operations": [{ "args": {} }] })).0);
    }

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("spritsit-mcp-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn opens_and_saves_with_guards() {
        let host = TestHost::new();
        let csv = tmp("in.csv");
        fs::write(&csv, "a,b\n1,2\n").unwrap();
        let csv_arg = csv.to_string_lossy().to_string();

        // a clean workbook is replaced and the window told to reset
        let opened = ok(&host, "open_workbook", json!({ "path": csv_arg }));
        assert_eq!(opened["sheets"][0]["rows"], 2);
        assert_eq!(*host.changes.lock().unwrap(), vec![Change::Replaced]);
        assert_eq!(ok(&host, "read_range", json!({ "range": "B2" }))["values"][0][0], "2");

        // unsaved edits are protected
        ok(&host, "write_range", json!({ "start": "A5", "values": [["keep me"]] }));
        let (refused, message) = call(&host, "open_workbook", json!({ "path": csv_arg }));
        assert!(refused && message.as_str().unwrap().contains("unsaved"), "{message}");
        assert_eq!(ok(&host, "read_range", json!({ "range": "A5" }))["values"][0][0], "keep me");

        // no file yet, relative paths and odd extensions are all refused
        for args in [json!({}), json!({ "path": "rel.xlsx" }), json!({ "path": tmp("x.txt").to_string_lossy() })] {
            assert!(call(&host, "save_workbook", args).0);
        }

        let out = tmp("out.xlsx");
        let _ = fs::remove_file(&out);
        let saved = ok(&host, "save_workbook", json!({ "path": out.to_string_lossy() }));
        assert_eq!(saved["unsaved_changes"], false);
        assert!(out.exists());
        // an existing file is only replaced on request ...
        let (refused, message) = call(&host, "save_workbook", json!({ "path": csv_arg }));
        assert!(refused && message.as_str().unwrap().contains("already exists"), "{message}");
        // ... but the workbook's own file can simply be saved again
        ok(&host, "write_range", json!({ "start": "A6", "values": [["more"]] }));
        ok(&host, "save_workbook", json!({}));
        let reopened = ok(&host, "open_workbook", json!({ "path": out.to_string_lossy() }));
        assert_eq!(reopened["dirty"], false);
        assert_eq!(ok(&host, "read_range", json!({ "range": "A6" }))["values"][0][0], "more");

        let exported = tmp("export.csv");
        let _ = fs::remove_file(&exported);
        ok(&host, "save_workbook", json!({ "path": exported.to_string_lossy() }));
        assert!(fs::read_to_string(&exported).unwrap().starts_with("a,b\n1,2\n"));
    }

    // -- HTTP -------------------------------------------------------------

    const TOKEN: &str = "t0ken";

    fn start(host: &Arc<TestHost>) -> (Running, u16) {
        Running::start(host.clone(), 0, Arc::new(Mutex::new(TOKEN.into()))).unwrap()
    }

    /// Sends a hand-written request and returns (status, body).
    fn raw(port: u16, request: &str) -> (u16, String) {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        s.write_all(request.as_bytes()).unwrap();
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).unwrap();
        let text = String::from_utf8_lossy(&buf).to_string();
        let (head, body) = text.split_once("\r\n\r\n").unwrap();
        (head.split_whitespace().nth(1).unwrap().parse().unwrap(), body.to_string())
    }

    fn request(method: &str, path: &str, port: u16, extra: &str, body: &str) -> String {
        format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[test]
    fn http_endpoint_is_locked_down() {
        let host = TestHost::new();
        let (_server, port) = start(&host);
        let auth = format!("Authorization: Bearer {TOKEN}\r\n");
        let ping = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;

        let (status, body) = raw(port, &request("POST", "/mcp", port, &auth, ping));
        assert_eq!(status, 200);
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap(), json!({ "jsonrpc": "2.0", "id": 1, "result": {} }));

        // credentials: none, wrong, and a prefix of the real one
        for extra in ["".to_string(), "Authorization: Bearer nope\r\n".to_string(), "Authorization: Bearer t0k\r\n".to_string(), format!("Authorization: Basic {TOKEN}\r\n")] {
            assert_eq!(raw(port, &request("POST", "/mcp", port, &extra, ping)).0, 401, "{extra:?}");
        }
        // lowercase scheme is fine (RFC 7235)
        assert_eq!(raw(port, &request("POST", "/mcp", port, &format!("Authorization: bearer {TOKEN}\r\n"), ping)).0, 200);
        // a browser page (Origin) or a rebinding host name never gets in, even with the token
        assert_eq!(raw(port, &request("POST", "/mcp", port, &format!("{auth}Origin: https://evil.example\r\n"), ping)).0, 403);
        let rebound = format!("POST /mcp HTTP/1.1\r\nHost: evil.example:{port}\r\n{auth}Content-Length: {}\r\nConnection: close\r\n\r\n{ping}", ping.len());
        assert_eq!(raw(port, &rebound).0, 403);

        assert_eq!(raw(port, &request("GET", "/mcp", port, &auth, "")).0, 405);
        assert_eq!(raw(port, &request("DELETE", "/mcp", port, &auth, "")).0, 405);
        assert_eq!(raw(port, &request("POST", "/elsewhere", port, &auth, ping)).0, 404);

        // notifications are acknowledged with an empty 202
        let note = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        assert_eq!(raw(port, &request("POST", "/mcp", port, &auth, note)), (202, String::new()));
        // garbage in, JSON-RPC error out
        let (status, body) = raw(port, &request("POST", "/mcp", port, &auth, "{nope"));
        assert_eq!(status, 200);
        assert!(body.contains("-32700"), "{body}");
        // a batch is answered as a batch, minus the notifications
        let batch = format!("[{ping},{note},{ping}]");
        let (_, body) = raw(port, &request("POST", "/mcp", port, &auth, &batch));
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap().as_array().unwrap().len(), 2);
        // nothing of the above touched the workbook
        assert!(host.changes.lock().unwrap().is_empty());
    }

    #[test]
    fn stdio_bridge_round_trip_and_big_answers() {
        let host = TestHost::new();
        let (server, port) = start(&host);
        let call = |name: &str, args: Value| {
            let msg = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": name, "arguments": args } });
            let (status, body) = post(port, TOKEN, &msg.to_string()).unwrap();
            assert_eq!(status, 200);
            serde_json::from_str::<Value>(&body).unwrap()
        };
        // ~120 KB answer: would be chunked by default, the bridge must still read it whole
        let cell = "x".repeat(60);
        let rows: Vec<Value> = (0..20).map(|_| json!(vec![cell.as_str(); 100])).collect();
        call("write_range", json!({ "start": "A1", "values": rows }));
        let reply = call("read_range", json!({}));
        let text = reply["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.len() > 100_000);
        let data: Value = serde_json::from_str(text).unwrap();
        assert_eq!(data["values"][19][99], cell.as_str());

        assert_eq!(post(port, "wrong", "{}").unwrap().0, 401);
        let (status, none) = post(port, TOKEN, r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).unwrap();
        assert_eq!((status, none.as_str()), (202, ""));

        // requests that cannot be relayed still get an answer; notifications do not
        let answer = failure_reply(r#"{"jsonrpc":"2.0","id":"abc","method":"ping"}"#, "app is off").unwrap();
        assert_eq!(serde_json::from_str::<Value>(&answer).unwrap()["id"], "abc");
        assert!(failure_reply(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#, "x").is_none());

        // stopping really closes the port (tiny_http releases the socket a moment after drop)
        drop(server);
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let closed = (0..40).any(|_| {
            std::thread::sleep(Duration::from_millis(50));
            TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_err()
        });
        assert!(closed, "port {port} still accepts connections after stop");
    }

    #[test]
    fn rejects_a_port_that_is_taken() {
        let host = TestHost::new();
        let (_server, port) = start(&host);
        let err = Running::start(host.clone(), port, Arc::new(Mutex::new(TOKEN.into())))
            .err()
            .unwrap();
        assert!(err.contains("already in use"), "{err}");
    }

    #[test]
    fn config_file_is_private_and_round_trips() {
        let path = tmp("cfg-dir/nested/mcp.json");
        let _ = fs::remove_file(&path);
        let cfg = Config { enabled: true, port: 7431, token: "secret".into() };
        cfg.save_to(&path).unwrap();
        let back: Config = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!((back.enabled, back.port, back.token.as_str()), (true, 7431, "secret"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        // a config written by an older version (missing keys) still loads with defaults
        let old: Config = serde_json::from_str(r#"{"enabled":true}"#).unwrap();
        assert_eq!((old.port, old.token.as_str()), (DEFAULT_PORT, ""));
    }

    #[test]
    fn tokens_are_long_and_unique() {
        let (a, b) = (new_token().unwrap(), new_token().unwrap());
        assert_eq!(a.len(), 48);
        assert_ne!(a, b);
        assert!(bearer_ok(Some(&format!("Bearer {a}")), &a));
        assert!(!bearer_ok(Some("Bearer "), ""), "an empty token never matches");
    }
}
