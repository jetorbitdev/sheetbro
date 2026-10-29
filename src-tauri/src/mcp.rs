//! MCP (Model Context Protocol) server: lets AI tools read and edit the open workbook.
//!
//! HTTP clients (Claude Code, Cursor, VS Code, ...) talk to `POST /mcp` on 127.0.0.1 with a
//! bearer token. Clients that only speak stdio (Claude Desktop, Codex, ...) launch this same
//! binary with `--mcp-stdio`, which relays every message to that endpoint. Both paths end in
//! `handle_message`, so the tools behave the same everywhere. The tools act on the workbook of
//! the window that last had focus. Every edit lands in the undo history like a user edit and is
//! pushed to that window through the `workbook-changed` event.

use std::{
    fs,
    io::{self, BufRead, Read, Write},
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
    ops::DerefMut,
    sync::{Arc, Mutex},
    thread::JoinHandle,
    time::Duration,
};

use ironcalc::base::{expressions::types::Area, types::CellType};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager, State};
use tiny_http::{Header, Method, Request, Response, Server};

use crate::{info, AppState, CmdResult, Doc, DocGuard, WorkbookInfo};

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
            *self.token.lock().unwrap() = self.config.token.clone();
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
        *self.token.lock().unwrap() = token;
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
    let mut rt = state.mcp.lock().unwrap();
    if rt.config.enabled {
        let port = rt.config.port;
        if let Err(e) = rt.start(Arc::new(TauriHost(app.clone())), port) {
            rt.error = Some(e);
        }
    }
}

#[tauri::command]
pub fn mcp_status(state: State<AppState>) -> Status {
    state.mcp.lock().unwrap().status()
}

#[tauri::command]
pub fn mcp_set_enabled(
    enabled: bool,
    port: Option<u16>,
    app: AppHandle,
    state: State<AppState>,
) -> CmdResult<Status> {
    let mut rt = state.mcp.lock().unwrap();
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
    let mut rt = state.mcp.lock().unwrap();
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
    let token = token.lock().unwrap().clone();
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
    let outcome = run_tool(&mut host.lock(), name, &args);
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

fn tool_result(text: String, is_error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

// --------------------------------------------------------------------- tools

const TOOL_NAMES: [&str; 10] = [
    "workbook_info",
    "read_range",
    "write_range",
    "clear_range",
    "format_range",
    "add_sheet",
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
             formulas=true to get the underlying formulas (\"=SUM(A1:A9)\") instead. Reads at most 20000 \
             cells per call; omit `range` to read the whole used area of the sheet.",
            json!({ "sheet": sheet, "range": range("Range to read"), "formulas": flag("Return formulas instead of displayed values.") }),
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
            "Empties the contents of a range (formatting stays). One undo step.",
            json!({ "sheet": sheet, "range": range("Range to clear") }),
            &["range"],
            Hint::Destructive,
        ),
        tool(
            "format_range",
            "Format cells",
            "Applies formatting to a range. Pass only what should change; at least one option is required. \
             One undo step.",
            json!({
                "sheet": sheet,
                "range": range("Range to format"),
                "bold": flag("Bold on or off."),
                "italic": flag("Italic on or off."),
                "underline": flag("Underline on or off."),
                "text_color": color("Text color"),
                "fill_color": color("Background color; use an empty string or \"none\" to remove it"),
                "align": { "type": "string", "enum": ["left", "center", "right"], "description": "Horizontal alignment." },
                "number_format": { "type": "string", "description": "Excel number format, e.g. \"$#,##0.00\", \"0%\", \"0.00\", \"yyyy-mm-dd\" or \"general\"." },
            }),
            &["range"],
            Hint::Safe,
        ),
        tool("add_sheet", "Add sheet", "Adds an empty sheet at the end. One undo step.", json!({}), &[], Hint::Safe),
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
        "add_sheet" => add_sheet(doc),
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
                    if raw {
                        doc.model.get_cell_content(sheet, r, c)
                    } else {
                        doc.model.get_formatted_cell_value(sheet, r, c)
                    }
                })
                .collect::<Result<Vec<_>, _>>()
        })
        .collect::<Result<Vec<_>, _>>()?;
    data["range"] = json!(rect.label());
    data["values"] = json!(values);
    Ok(Outcome::read(data))
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
    let wanted = parse_range(req_str(args, "range")?, rows, cols)?;
    let mut data = json!({ "sheet": sheet_name(doc, sheet), "range": wanted.label() });
    // Nothing exists past the used area, and clearing a million empty rows would only be slow
    let Some(rect) = wanted.clip(rows, cols) else {
        data["note"] = json!("This range is outside the used area; nothing to clear.");
        return Ok(Outcome::read(data));
    };
    doc.model.range_clear_contents(&rect.area(sheet))?;
    doc.record(1);
    data["range"] = json!(rect.label());
    data["cleared"] = json!(true);
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
    let rect = parse_range(req_str(args, "range")?, rows, cols)?;
    if rect.cells() > MAX_FORMAT_CELLS {
        return Err(format!("{} cells is too many to format at once (limit {MAX_FORMAT_CELLS}).", rect.cells()));
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
    if changes.is_empty() {
        return Err("Nothing to change: pass bold, italic, underline, text_color, fill_color, align or number_format".into());
    }
    let area = rect.area(sheet);
    let mut applied = 0;
    let mut result = Ok(());
    for (path, value) in &changes {
        result = doc.model.update_range_style(&area, path, value);
        if result.is_err() {
            break;
        }
        applied += 1;
    }
    // what was applied before a failure is still one undo step
    doc.record(applied);
    result?;
    Ok(Outcome::edit(json!({
        "sheet": sheet_name(doc, sheet),
        "range": rect.label(),
        "applied": changes.len(),
    })))
}

fn add_sheet(doc: &mut Doc) -> Result<Outcome, String> {
    doc.model.new_sheet()?;
    doc.record(1);
    let names: Vec<String> = doc.model.get_worksheets_properties().into_iter().map(|p| p.name).collect();
    Ok(Outcome::edit(json!({ "added": names.last(), "sheets": names })))
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
