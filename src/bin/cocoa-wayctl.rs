use std::path::PathBuf;

#[path = "../control_client.rs"]
mod control_client;
#[allow(dead_code)]
#[path = "../control_protocol.rs"]
mod control_protocol;

use control_client::send_request;
use control_protocol::{ControlRequest, ControlResponse};

fn main() {
    match run() {
        Ok(response) => {
            let json = if std::env::args().any(|argument| argument == "--json") {
                serde_json::to_string(&response)
            } else {
                serde_json::to_string_pretty(&response)
            };
            match json {
                Ok(json) => println!("{json}"),
                Err(error) => fail(&format!("failed to encode response: {error}")),
            }
            if !response.ok {
                std::process::exit(1);
            }
        }
        Err(error) => fail(&error),
    }
}

fn run() -> Result<ControlResponse, String> {
    let mut arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if arguments
        .iter()
        .any(|argument| argument == "--help" || argument == "-h")
    {
        print_help();
        std::process::exit(0);
    }
    arguments.retain(|argument| argument != "--json");

    let mut socket = control_client::default_socket_path();
    let mut limit = 100usize;
    let mut positional = Vec::new();
    let mut index = 0usize;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--socket" => {
                index += 1;
                socket = arguments
                    .get(index)
                    .map(PathBuf::from)
                    .ok_or_else(|| "--socket requires a path".to_string())?;
            }
            "--limit" => {
                index += 1;
                limit = arguments
                    .get(index)
                    .ok_or_else(|| "--limit requires a number".to_string())?
                    .parse::<usize>()
                    .map_err(|_| "--limit must be a positive integer".to_string())?;
            }
            option if option.starts_with('-') => {
                return Err(format!("unknown option: {option}"));
            }
            value => positional.push(value.to_string()),
        }
        index += 1;
    }

    let command = positional
        .first()
        .cloned()
        .ok_or_else(|| "missing command; run cocoa-wayctl --help".to_string())?;
    let session = positional.get(1).cloned();
    if positional.len() > 2 {
        return Err("too many positional arguments".into());
    }

    let request = ControlRequest {
        command,
        session,
        limit,
    };
    send_request(&socket, &request)
}

fn print_help() {
    println!(
        "cocoa-wayctl [--json] [--socket PATH] [--limit N] COMMAND [SESSION]\n\n\
         Commands:\n  status\n  applications\n  sessions (compatibility alias)\n  running\n  displays\n  images\n  volumes\n  runtimes\n  tasks\n  environment\n  features\n  diagnostics [APPLICATION]\n  logs APPLICATION\n  check APPLICATION\n  launch APPLICATION\n  stop APPLICATION\n  display-create [NAME]\n  display-close NAME\n\n\
         APPLICATION is an exact profile name or the zero-based index shown by `applications`. Display names are normalized to stable lowercase slots."
    );
}

fn fail(message: &str) -> ! {
    eprintln!("cocoa-wayctl: {message}");
    std::process::exit(2);
}
