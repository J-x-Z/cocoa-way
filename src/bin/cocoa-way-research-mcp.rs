//! Optional developer launcher. Never imported by Cocoa-Way's GUI/startup path.
#[path = "../research_mcp/mod.rs"]
mod research_mcp;

fn main() {
    if let Err(error) = research_mcp::run(std::env::args_os().skip(1)) {
        eprintln!("cocoa-way-research-mcp: {error}");
        std::process::exit(1);
    }
}
