//! `tacitus-mcp plugin …` — headless WASM plugins (cron agents, scripts).
//!
//!   tacitus-mcp plugin list [--vault <path>]
//!   tacitus-mcp plugin run <name> [--vault <path>] [--input '<json>'] [--timeout-ms <n>]
//!
//! Consent model: invoking a plugin from the terminal IS the consent — the
//! same as running any script — and the manifest's scope still gates writes.
//! The desktop app's approval state (`state.json`) is a desktop concept,
//! where plugins can also run automatically on hooks.
//!
//! `run` prints the plugin's `{ ok, data | error }` envelope on stdout and
//! the guest's `tacitus.log` lines on stderr; exit code 0 only when
//! `ok: true` (cron-friendly).

use std::path::{Path, PathBuf};

use serde_json::Value;
use tacitus_core::vault::NoteWriter;
use tacitus_plugins::{err_envelope, HostConfig, PluginHost, PluginManifest, ToolRegistry};

const DEFAULT_TIMEOUT_MS: u64 = 30_000;

#[derive(Debug, PartialEq, Eq)]
pub enum PluginCmd {
    List {
        vault: PathBuf,
    },
    Run {
        vault: PathBuf,
        name: String,
        input: String,
        timeout_ms: u64,
    },
}

/// Hand-rolled like the rest of the binary — no clap.
pub fn parse_plugin_args(args: &[String]) -> Result<PluginCmd, String> {
    let sub = args.first().map(String::as_str).unwrap_or("");
    let mut vault = PathBuf::from(".");
    let mut input = String::from("{}");
    let mut timeout_ms = DEFAULT_TIMEOUT_MS;
    let mut name: Option<String> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--vault" => {
                vault = PathBuf::from(args.get(i + 1).ok_or("--vault needs a path")?);
                i += 2;
            }
            "--input" => {
                input = args
                    .get(i + 1)
                    .ok_or("--input needs a JSON string")?
                    .clone();
                i += 2;
            }
            "--timeout-ms" => {
                timeout_ms = args
                    .get(i + 1)
                    .ok_or("--timeout-ms needs a number")?
                    .parse()
                    .map_err(|_| "--timeout-ms must be a number of milliseconds")?;
                i += 2;
            }
            other if !other.starts_with("--") && name.is_none() => {
                name = Some(other.to_string());
                i += 1;
            }
            other => return Err(format!("unknown plugin flag: {other}")),
        }
    }

    match sub {
        "list" => Ok(PluginCmd::List { vault }),
        "run" => Ok(PluginCmd::Run {
            vault,
            name: name.ok_or("plugin run needs a plugin name")?,
            input,
            timeout_ms,
        }),
        other => Err(format!(
            "unknown plugin subcommand {other:?} — use list | run"
        )),
    }
}

/// One line per installed plugin — valid ones with their permission surface,
/// broken ones with the reason (shown, not hidden).
pub fn list_lines(vault: &Path) -> Vec<String> {
    let dir = vault.join(".tacitus").join("plugins");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return vec![format!("no plugins directory at {}", dir.display())];
    };
    let mut names: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
        .collect();
    names.sort();
    if names.is_empty() {
        return vec![format!("no plugins installed in {}", dir.display())];
    }
    names
        .into_iter()
        .map(|(name, path)| {
            match PluginManifest::load(&path)
                .and_then(|m| m.validate(ToolRegistry::descriptors()).map(|()| m))
            {
                Ok(m) => {
                    let scope = serde_json::to_value(m.permissions.scope)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_owned))
                        .unwrap_or_default();
                    let hooks = if m.hooks.is_empty() {
                        String::new()
                    } else {
                        format!(" hooks=[{}]", m.hooks.join(", "))
                    };
                    format!(
                        "{} {} {} tools=[{}]{}",
                        m.name,
                        m.version,
                        scope,
                        m.permissions.tools.join(", "),
                        hooks
                    )
                }
                Err(e) => format!("{name} INVALID ({}: {})", e.code, e.reason),
            }
        })
        .collect()
}

/// Load + run one plugin. Returns the envelope (host failures become error
/// envelopes so the caller always gets JSON) plus the guest's log lines.
pub fn run_plugin(
    vault: &Path,
    name: &str,
    input: &Value,
    timeout_ms: u64,
) -> (Value, Vec<(u8, String)>) {
    let plugin_dir = vault.join(".tacitus").join("plugins").join(name);
    let host = match PluginHost::new(HostConfig {
        epoch_deadline_ms: Some(timeout_ms),
        ..HostConfig::default()
    }) {
        Ok(host) => host,
        Err(e) => return (err_envelope(&e), Vec::new()),
    };
    let vault_for_writer = vault.to_path_buf();
    let loaded = host.load_with_registry(&plugin_dir, move |m| {
        // Same attribution as the desktop: every write lands in the audit
        // log as plugin:<name>, versioned and revertible.
        let mut writer = NoteWriter::new(&vault_for_writer, m.permissions.scope);
        writer.set_origin(format!("plugin:{}", m.name));
        ToolRegistry::standard(&vault_for_writer, m.permissions.scope)
            .with_writer(writer)
            .with_identity("tacitus-cli", env!("CARGO_PKG_VERSION"))
    });
    let mut instance = match loaded {
        Ok(instance) => instance,
        Err(e) => return (err_envelope(&e), Vec::new()),
    };
    let envelope = match instance.run(input) {
        Ok(envelope) => envelope,
        Err(e) => err_envelope(&e),
    };
    (envelope, instance.drain_logs())
}

pub fn plugin_main(args: &[String]) -> Result<bool, String> {
    match parse_plugin_args(args)? {
        PluginCmd::List { vault } => {
            for line in list_lines(&vault) {
                println!("{line}");
            }
            Ok(true)
        }
        PluginCmd::Run {
            vault,
            name,
            input,
            timeout_ms,
        } => {
            let input: Value = serde_json::from_str(&input)
                .map_err(|e| format!("--input is not valid JSON: {e}"))?;
            let (envelope, logs) = run_plugin(&vault, &name, &input, timeout_ms);
            let tags = ["debug", "info", "warn", "error"];
            for (level, line) in logs {
                eprintln!("[{}] {line}", tags[usize::from(level.min(3))]);
            }
            println!("{envelope:#}");
            Ok(envelope["ok"] == Value::Bool(true))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn s(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|p| p.to_string()).collect()
    }

    #[test]
    fn plugin_args_parse_list_and_run() {
        assert_eq!(
            parse_plugin_args(&s(&["list", "--vault", "/tmp/v"])).unwrap(),
            PluginCmd::List {
                vault: PathBuf::from("/tmp/v"),
            }
        );
        assert_eq!(
            parse_plugin_args(&s(&[
                "run",
                "digest",
                "--input",
                "{\"q\":1}",
                "--timeout-ms",
                "500"
            ]))
            .unwrap(),
            PluginCmd::Run {
                vault: PathBuf::from("."),
                name: "digest".into(),
                input: "{\"q\":1}".into(),
                timeout_ms: 500,
            }
        );
        assert!(parse_plugin_args(&s(&["run"])).is_err(), "run needs a name");
        assert!(parse_plugin_args(&s(&["frobnicate"])).is_err());
        assert!(parse_plugin_args(&s(&["run", "x", "--timeout-ms", "soon"])).is_err());
    }

    fn temp_vault(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("tacitus-plugincli-{tag}-{nanos}"));
        fs::create_dir_all(dir.join("notes")).unwrap();
        fs::write(dir.join("notes/alpha.md"), "# Alpha\n\nSeed.\n").unwrap();
        dir
    }

    const CREATE_NOTE_WAT: &str = r#"
(module
  (import "tacitus" "call" (func $call (param i32 i32 i32 i32) (result i64)))
  (memory (export "memory") 2)
  (data (i32.const 100) "create_note")
  (data (i32.const 120) "{\"note_id\":\"notes/from-guest\",\"content\":\"hello\"}")
  (global $heap (mut i32) (i32.const 65536))
  (func (export "tacitus_abi_version") (result i32) (i32.const 1))
  (func (export "tacitus_alloc") (param $len i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $len)))
    (block $done
      (loop $grow
        (br_if $done (i32.le_u (global.get $heap)
                               (i32.mul (memory.size) (i32.const 65536))))
        (drop (memory.grow (i32.const 1)))
        (br $grow)))
    (local.get $ptr))
  (func (export "tacitus_dealloc") (param i32) (param i32))
  (func (export "tacitus_run") (param i32) (param i32) (result i64)
    (call $call (i32.const 100) (i32.const 11) (i32.const 120) (i32.const 48))))
"#;

    #[test]
    fn list_lines_show_valid_and_invalid() {
        let vault = temp_vault("list");
        let good = vault.join(".tacitus/plugins/good-one");
        fs::create_dir_all(&good).unwrap();
        fs::write(good.join("plugin.wasm"), b"stub").unwrap();
        fs::write(
            good.join("tacitus-plugin.toml"),
            "name = \"good-one\"\nversion = \"0.1.0\"\nentry = \"plugin.wasm\"\nhooks = [\"note_saved\"]\n\n[permissions]\nscope = \"read-only\"\ntools = [\"search\"]\n",
        )
        .unwrap();
        let broken = vault.join(".tacitus/plugins/broken");
        fs::create_dir_all(&broken).unwrap();
        fs::write(broken.join("tacitus-plugin.toml"), "not = valid = toml").unwrap();

        let lines = list_lines(&vault);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("broken INVALID (INVALID_MANIFEST"));
        assert_eq!(
            lines[1],
            "good-one 0.1.0 read-only tools=[search] hooks=[note_saved]"
        );
        fs::remove_dir_all(&vault).ok();
    }

    #[test]
    fn run_plugin_writes_and_attributes() {
        let vault = temp_vault("run");
        let dir = vault.join(".tacitus/plugins/writer");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("plugin.wasm"),
            wat::parse_str(CREATE_NOTE_WAT).unwrap(),
        )
        .unwrap();
        fs::write(
            dir.join("tacitus-plugin.toml"),
            "name = \"writer\"\nversion = \"0.1.0\"\nentry = \"plugin.wasm\"\n\n[permissions]\nscope = \"read-write\"\ntools = [\"create_note\"]\n",
        )
        .unwrap();

        let (envelope, _logs) = run_plugin(&vault, "writer", &serde_json::json!({}), 5_000);
        assert_eq!(envelope["ok"], true, "envelope: {envelope}");
        assert!(vault.join("notes/from-guest.md").exists());
        let audit = fs::read_to_string(vault.join(".tacitus/audit.log")).unwrap();
        assert!(audit.contains("\"origin\":\"plugin:writer\""), "{audit}");
        fs::remove_dir_all(&vault).ok();
    }
}
