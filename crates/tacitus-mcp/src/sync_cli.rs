//! `tacitus-mcp sync …` — the CLI face of Tacitus Sync.
//!
//!   tacitus-mcp sync init   [--vault <path>] [--relay <url>] [--code <code>]
//!   tacitus-mcp sync once   [--vault <path>]
//!   tacitus-mcp sync run    [--vault <path>] [--interval <secs>]
//!   tacitus-mcp sync status [--vault <path>]
//!
//! Config lives in `<vault>/.tacitus/sync/config.json` (owner-only): the
//! relay URL and the vault code. The code IS the encryption key material —
//! losing it makes the relay's copy permanently undecryptable (the local
//! vault stays plain files, unaffected).

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tacitus_sync::client;
use tacitus_sync::{SyncEngine, VaultCode};

pub const DEFAULT_RELAY: &str = "wss://sync.tacitus.md";

#[derive(Debug, PartialEq, Eq)]
pub enum SyncCmd {
    Init {
        vault: PathBuf,
        relay: String,
        code: Option<String>,
    },
    Once {
        vault: PathBuf,
    },
    Run {
        vault: PathBuf,
        interval_secs: u64,
    },
    Status {
        vault: PathBuf,
    },
}

/// Hand-rolled like the rest of the binary — no clap.
pub fn parse_sync_args(args: &[String]) -> Result<SyncCmd, String> {
    let sub = args.first().map(String::as_str).unwrap_or("");
    let mut vault = PathBuf::from(".");
    let mut relay = DEFAULT_RELAY.to_string();
    let mut code = None;
    let mut interval_secs = 30u64;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--vault" => {
                vault = PathBuf::from(args.get(i + 1).ok_or("--vault needs a path")?);
                i += 2;
            }
            "--relay" => {
                relay = args.get(i + 1).ok_or("--relay needs a URL")?.clone();
                i += 2;
            }
            "--code" => {
                code = Some(args.get(i + 1).ok_or("--code needs a vault code")?.clone());
                i += 2;
            }
            "--interval" => {
                interval_secs = args
                    .get(i + 1)
                    .ok_or("--interval needs seconds")?
                    .parse()
                    .map_err(|_| "--interval must be a number of seconds")?;
                i += 2;
            }
            other => return Err(format!("unknown sync flag: {other}")),
        }
    }

    match sub {
        "init" => Ok(SyncCmd::Init { vault, relay, code }),
        "once" => Ok(SyncCmd::Once { vault }),
        "run" => Ok(SyncCmd::Run {
            vault,
            interval_secs,
        }),
        "status" => Ok(SyncCmd::Status { vault }),
        other => Err(format!(
            "unknown sync subcommand {other:?} — use init | once | run | status"
        )),
    }
}

#[derive(Serialize, Deserialize)]
struct SyncConfig {
    relay_url: String,
    vault_code: String,
}

fn config_path(vault: &Path) -> PathBuf {
    vault.join(".tacitus").join("sync").join("config.json")
}

fn load_config(vault: &Path) -> Result<SyncConfig, String> {
    let raw = fs::read_to_string(config_path(vault))
        .map_err(|_| "sync is not set up for this vault — run `tacitus-mcp sync init` first")?;
    serde_json::from_str(&raw).map_err(|e| format!("corrupt sync config: {e}"))
}

fn save_config(vault: &Path, config: &SyncConfig) -> Result<(), String> {
    let path = config_path(vault);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(config).map_err(|e| e.to_string())?;
    fs::write(&path, json).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

pub async fn sync_main(args: &[String]) -> Result<(), String> {
    match parse_sync_args(args)? {
        SyncCmd::Init { vault, relay, code } => {
            let code = match code {
                Some(raw) => VaultCode::parse(&raw).map_err(|e| e.to_string())?,
                None => VaultCode::generate(),
            };
            save_config(
                &vault,
                &SyncConfig {
                    relay_url: relay.clone(),
                    vault_code: code.as_str().to_string(),
                },
            )?;
            let engine = SyncEngine::open(&vault, &code).map_err(|e| e.to_string())?;
            eprintln!("Sync is set up.");
            eprintln!("  relay:    {relay}");
            eprintln!("  vault id: {}", engine.vault_id());
            eprintln!();
            eprintln!("Your vault code (paste it on your other devices):");
            eprintln!();
            eprintln!("  {}", code.as_str());
            eprintln!();
            eprintln!("KEEP IT SAFE. It is the encryption key: anyone with the code can");
            eprintln!("read this vault, and if you lose it the relay's copy can never be");
            eprintln!("decrypted again (your local files stay untouched).");
            Ok(())
        }
        SyncCmd::Once { vault } => {
            let config = load_config(&vault)?;
            let code = VaultCode::parse(&config.vault_code).map_err(|e| e.to_string())?;
            let mut engine = SyncEngine::open(&vault, &code).map_err(|e| e.to_string())?;
            let report = client::run_once(&mut engine, &config.relay_url)
                .await
                .map_err(|e| e.to_string())?;
            eprintln!(
                "synced: pushed {} change set(s), applied {} remote update(s), cursor at {}",
                report.pushed,
                report.applied,
                engine.last_seq()
            );
            Ok(())
        }
        SyncCmd::Run {
            vault,
            interval_secs,
        } => {
            let config = load_config(&vault)?;
            let code = VaultCode::parse(&config.vault_code).map_err(|e| e.to_string())?;
            let mut engine = SyncEngine::open(&vault, &code).map_err(|e| e.to_string())?;
            eprintln!(
                "syncing {} every {interval_secs}s (Ctrl-C to stop)",
                vault.display()
            );
            client::run_forever(
                &mut engine,
                &config.relay_url,
                std::time::Duration::from_secs(interval_secs),
                |report| {
                    if report.pushed > 0 || report.applied > 0 {
                        eprintln!(
                            "synced: pushed {}, applied {}",
                            report.pushed, report.applied
                        );
                    }
                },
            )
            .await
            .map_err(|e| e.to_string())
        }
        SyncCmd::Status { vault } => {
            let config = load_config(&vault)?;
            let code = VaultCode::parse(&config.vault_code).map_err(|e| e.to_string())?;
            let engine = SyncEngine::open(&vault, &code).map_err(|e| e.to_string())?;
            eprintln!("relay:    {}", config.relay_url);
            eprintln!("vault id: {}", engine.vault_id());
            eprintln!("device:   {}", engine.device_id());
            eprintln!("cursor:   seq {}", engine.last_seq());
            eprintln!(
                "outbox:   {} pending push(es)",
                engine.pending_pushes().len()
            );
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|p| p.to_string()).collect()
    }

    #[test]
    fn sync_args_parse_init_run_status() {
        assert_eq!(
            parse_sync_args(&s(&["init", "--vault", "/tmp/v", "--relay", "ws://x"])).unwrap(),
            SyncCmd::Init {
                vault: PathBuf::from("/tmp/v"),
                relay: "ws://x".into(),
                code: None,
            }
        );
        assert_eq!(
            parse_sync_args(&s(&["run", "--interval", "5"])).unwrap(),
            SyncCmd::Run {
                vault: PathBuf::from("."),
                interval_secs: 5,
            }
        );
        assert_eq!(
            parse_sync_args(&s(&["status"])).unwrap(),
            SyncCmd::Status {
                vault: PathBuf::from("."),
            }
        );
        assert_eq!(
            parse_sync_args(&s(&["once"])).unwrap(),
            SyncCmd::Once {
                vault: PathBuf::from("."),
            }
        );
        assert!(parse_sync_args(&s(&["frobnicate"])).is_err());
        assert!(parse_sync_args(&s(&["run", "--interval", "soon"])).is_err());
        assert!(parse_sync_args(&s(&["init", "--vault"])).is_err());
    }
}
