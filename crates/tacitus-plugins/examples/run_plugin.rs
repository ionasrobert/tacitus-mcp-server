//! Dev runner for sandboxed plugins — the m1 way to try one:
//!
//! ```sh
//! cargo run -p tacitus-plugins --example run_plugin -- <vault> <plugin-dir> [input-json]
//! ```
//!
//! Prints the plugin's `{ ok, data | error }` envelope on stdout and its
//! `tacitus.log` lines on stderr; exits non-zero on a structured error.

use std::path::PathBuf;

use serde_json::{json, Value};
use tacitus_plugins::{err_envelope, HostConfig, PluginHost};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("usage: run_plugin <vault> <plugin-dir> [input-json]");
        std::process::exit(2);
    }
    let vault = PathBuf::from(&args[0]);
    let plugin_dir = PathBuf::from(&args[1]);
    let input: Value = match args.get(2) {
        Some(raw) => match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("input is not valid JSON: {e}");
                std::process::exit(2);
            }
        },
        None => json!({}),
    };

    let result = PluginHost::new(HostConfig::default())
        .and_then(|host| host.load(&plugin_dir, &vault))
        .and_then(|mut plugin| {
            let out = plugin.run(&input);
            for (level, line) in plugin.drain_logs() {
                let tag = ["debug", "info", "warn", "error"][usize::from(level.min(3))];
                eprintln!("[{tag}] {line}");
            }
            out
        });
    match result {
        Ok(envelope) => println!("{envelope:#}"),
        Err(e) => {
            println!("{:#}", err_envelope(&e));
            std::process::exit(1);
        }
    }
}
