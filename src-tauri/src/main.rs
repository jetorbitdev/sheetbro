// Prevents an extra console window on Windows in release
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // MCP clients that only speak stdio (Claude Desktop, Codex, ...) launch this
    // binary like a plain command; no window, just a relay to the running app.
    // (The windows_subsystem above has no console, so this mode is for macOS/Linux builds.)
    if std::env::args().any(|a| a == "--mcp-stdio") {
        std::process::exit(spritsit_lib::mcp_stdio_bridge());
    }
    spritsit_lib::run()
}
