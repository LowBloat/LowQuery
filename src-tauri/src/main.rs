// Prevents the extra console window on Windows in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // `lowquery mcp`: MCP server for AI agents over stdio, without a window.
    if std::env::args().nth(1).as_deref() == Some("mcp") {
        lowquery_lib::mcp::run();
        return;
    }
    lowquery_lib::run()
}
