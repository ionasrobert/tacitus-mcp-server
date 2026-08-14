//! `tacitus-mcp sync …` — the CLI face of Tacitus Sync.
//!
//!   tacitus-mcp sync init    [--vault <path>] [--relay <url>] [--code <code>]
//!   tacitus-mcp sync once    [--vault <path>]
//!   tacitus-mcp sync run     [--vault <path>] [--interval <secs>] [--live]
//!   tacitus-mcp sync status  [--vault <path>]
//!   tacitus-mcp sync compact [--vault <path>]
//!   tacitus-mcp sync upgrade [--vault <path>] [--plan monthly|yearly] [--open]
//!
//! `run --live` holds one persistent relay connection: remote edits land
//! within ~a second, local edits are picked up by the scan tick
//! (`--interval`, default 30s — no file watcher by design).
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

/// The relay we host — the only one with anything to sell. A self-hosted
/// relay never gets an upgrade pitch out of this binary.
const HOSTED_RELAY_HOST: &str = "sync.tacitus.md";
const HOSTED_BILLING_BASE: &str = "https://sync.tacitus.md";

/// How full a vault must be before `sync status` mentions Pro. Below this,
/// silence: nobody wants their tools upselling them at 12%.
const UPGRADE_HINT_PERCENT: u64 = 80;

/// `sync status` asks the relay for the vault's size; being offline must not
/// turn a local status read into a failure, so the wait is short.
const STATUS_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

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
        live: bool,
    },
    Status {
        vault: PathBuf,
    },
    Compact {
        vault: PathBuf,
    },
    /// Print (or open) the checkout link for this vault's Pro quota.
    Upgrade {
        vault: PathBuf,
        plan: String,
        open: bool,
    },
}

/// Hand-rolled like the rest of the binary — no clap.
pub fn parse_sync_args(args: &[String]) -> Result<SyncCmd, String> {
    let sub = args.first().map(String::as_str).unwrap_or("");
    let mut vault = PathBuf::from(".");
    let mut relay = DEFAULT_RELAY.to_string();
    let mut code = None;
    let mut interval_secs = 30u64;
    let mut live = false;
    let mut plan = "monthly".to_string();
    let mut open = false;

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
            "--live" => {
                live = true;
                i += 1;
            }
            "--plan" => {
                plan = args
                    .get(i + 1)
                    .ok_or("--plan needs monthly or yearly")?
                    .clone();
                if plan != "monthly" && plan != "yearly" {
                    return Err(format!("--plan must be monthly or yearly, not {plan:?}"));
                }
                i += 2;
            }
            "--open" => {
                open = true;
                i += 1;
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
            live,
        }),
        "status" => Ok(SyncCmd::Status { vault }),
        "compact" => Ok(SyncCmd::Compact { vault }),
        "upgrade" => Ok(SyncCmd::Upgrade { vault, plan, open }),
        other => Err(format!(
            "unknown sync subcommand {other:?} — use init | once | run | status | compact | upgrade"
        )),
    }
}

/// Host of a `ws(s)://` URL — no scheme, port, userinfo or path.
fn relay_host(relay_url: &str) -> &str {
    let rest = relay_url.split_once("://").map_or(relay_url, |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    if authority.starts_with('[') {
        // IPv6 literal: [::1]:9099 → ::1
        return authority
            .split(']')
            .next()
            .map_or(authority, |bracketed| &bracketed[1..]);
    }
    authority.split(':').next().unwrap_or(authority)
}

/// Where Pro is sold for a given relay, if anywhere. Pure so the policy is
/// testable; `billing_base` supplies the environment.
///
/// Self-hosted relays deliberately return None: they have no quota to lift
/// and their users are not our customers. An operator running their own
/// sidecar points clients at it with `TACITUS_BILLING_URL`.
fn billing_base_from(relay_url: &str, env_override: Option<&str>) -> Option<String> {
    if let Some(base) = env_override {
        let base = base.trim().trim_end_matches('/');
        if !base.is_empty() {
            return Some(base.to_string());
        }
    }
    if relay_host(relay_url) == HOSTED_RELAY_HOST {
        return Some(HOSTED_BILLING_BASE.to_string());
    }
    None
}

fn billing_base(relay_url: &str) -> Option<String> {
    billing_base_from(
        relay_url,
        std::env::var("TACITUS_BILLING_URL").ok().as_deref(),
    )
}

/// The checkout link for one vault. The vault id IS the account — there is
/// nothing else to identify, which is the whole point.
fn checkout_url(relay_url: &str, vault_id: &str, plan: &str) -> Option<String> {
    billing_base(relay_url)
        .map(|base| format!("{base}/billing/checkout?vault_id={vault_id}&plan={plan}"))
}

/// Sizes the way the quotas are quoted (binary units, MB/GB labels — a
/// "100 MB" quota is 104857600 bytes on both the relay and the price list).
fn human_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let n = bytes as f64;
    if n >= GB {
        format!("{:.1} GB", n / GB)
    } else if n >= MB {
        format!("{:.1} MB", n / MB)
    } else if n >= KB {
        format!("{:.1} KB", n / KB)
    } else {
        format!("{bytes} B")
    }
}

/// The `storage:` block of `sync status`, plus an upgrade nudge once the
/// vault is genuinely close to full. Pure — the caller does the printing.
fn storage_lines(relay_url: &str, vault_id: &str, status: &client::RelayStatus) -> Vec<String> {
    let used = human_bytes(status.log_bytes);
    // A pre-0.24 relay reports no quota; say so rather than invent one.
    let Some(percent) = (status.quota_bytes > 0)
        .then(|| (status.log_bytes.saturating_mul(100) / status.quota_bytes).min(100))
    else {
        return vec![format!(
            "storage:  {used} used (this relay reports no quota)"
        )];
    };
    let mut lines = vec![format!(
        "storage:  {used} of {} ({percent}%)",
        human_bytes(status.quota_bytes)
    )];
    if percent >= UPGRADE_HINT_PERCENT {
        if let Some(url) = checkout_url(relay_url, vault_id, "monthly") {
            lines.push(String::new());
            lines.push(format!(
                "This vault is at {percent}% of its quota. Pro raises it — see"
            ));
            lines.push(format!("  {url}"));
            lines.push("or run `tacitus-mcp sync upgrade` (add --plan yearly).".into());
        }
    }
    lines
}

/// What to say when the relay refuses a push for size. Includes the exact
/// link, because "or upgrade" without one is not an answer.
fn quota_exceeded_lines(
    relay_url: &str,
    vault_id: &str,
    quota_bytes: u64,
    log_bytes: u64,
) -> Vec<String> {
    if quota_bytes == 0 {
        return vec!["relay log full — compacting".into()];
    }
    let mut lines = vec![format!(
        "relay storage quota reached ({} of {}) — compacting",
        human_bytes(log_bytes),
        human_bytes(quota_bytes)
    )];
    if let Some(url) = checkout_url(relay_url, vault_id, "monthly") {
        lines.push(format!(
            "if this repeats, shrink the vault or go Pro: {url}"
        ));
    } else {
        lines.push("if this repeats, shrink the vault or raise the relay's quota".into());
    }
    lines
}

/// The platform's "open this URL" command — pure, so the mapping is
/// testable; spawning is the caller's business.
fn open_command(url: &str) -> (&'static str, Vec<String>) {
    #[cfg(target_os = "macos")]
    let cmd = ("open", vec![url.to_string()]);
    #[cfg(target_os = "windows")]
    // The empty string is `start`'s title argument — without it a quoted URL
    // becomes the window title and nothing opens.
    let cmd = (
        "cmd",
        vec![
            "/C".to_string(),
            "start".to_string(),
            String::new(),
            url.to_string(),
        ],
    );
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let cmd = ("xdg-open", vec![url.to_string()]);
    cmd
}

#[derive(Serialize, Deserialize)]
struct SyncConfig {
    relay_url: String,
    vault_code: String,
}

fn config_path(vault: &Path) -> PathBuf {
    vault.join(".tacitus").join("sync").join("config.json")
}

/// The writer sync applies remote batches through: read-write, and every
/// commit marked origin "sync" in the audit log.
fn sync_writer(vault: &Path) -> tacitus_core::vault::NoteWriter {
    let mut writer = tacitus_core::vault::NoteWriter::new(
        vault,
        tacitus_core::vault::PermissionScope::ReadWrite,
    );
    writer.set_origin("sync");
    writer
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
            let mut writer = sync_writer(&vault);
            let report = client::sync_pass(&mut engine, &mut writer, &config.relay_url)
                .await
                .map_err(|e| e.to_string())?;
            eprintln!(
                "synced: pushed {}, wrote {} note(s) + {} memory(ies){}, cursor at {}",
                report.run.pushed,
                report.apply.notes.len(),
                report.apply.memories.len(),
                report
                    .apply
                    .version_id
                    .as_deref()
                    .map(|v| format!(" (revertible: {v})"))
                    .unwrap_or_default(),
                engine.last_seq()
            );
            Ok(())
        }
        SyncCmd::Run {
            vault,
            interval_secs,
            live,
        } => {
            let config = load_config(&vault)?;
            let code = VaultCode::parse(&config.vault_code).map_err(|e| e.to_string())?;
            let mut engine = SyncEngine::open(&vault, &code).map_err(|e| e.to_string())?;
            let mut writer = sync_writer(&vault);
            if live {
                use tacitus_sync::{LiveConfig, LiveEvent};
                // Cloned up front: the event closure runs while `engine` is
                // mutably borrowed by the session.
                let relay_url = config.relay_url.clone();
                let vault_id = engine.vault_id().to_string();
                let mut live_config = LiveConfig::new(config.relay_url.clone());
                live_config.scan_interval = std::time::Duration::from_secs(interval_secs);
                eprintln!(
                    "live sync on {} — scan every {interval_secs}s (Ctrl-C to stop)",
                    vault.display()
                );
                // Ctrl-C drops the only cmd sender → the session says
                // goodbye, flushes pending applies and returns Ok — a clean
                // shutdown, not an abort mid-write.
                let (tx, mut rx) = tokio::sync::mpsc::channel::<tacitus_sync::LiveCmd>(4);
                tokio::spawn(async move {
                    let _ = tokio::signal::ctrl_c().await;
                    drop(tx);
                });
                return tacitus_sync::run_live(
                    &mut engine,
                    &mut writer,
                    &live_config,
                    &mut rx,
                    |event| match event {
                        LiveEvent::Connected { latest_seq } => {
                            eprintln!("connected (relay at seq {latest_seq})");
                        }
                        LiveEvent::CaughtUp => eprintln!("caught up — streaming"),
                        LiveEvent::Pushed { count } => eprintln!("pushed {count} update(s)"),
                        LiveEvent::Applied(report) => {
                            if !report.notes.is_empty() || !report.memories.is_empty() {
                                eprintln!(
                                    "applied {} note(s) + {} memory(ies)",
                                    report.notes.len(),
                                    report.memories.len()
                                );
                            }
                        }
                        LiveEvent::Disconnected { reason, retry_in } => {
                            eprintln!("disconnected: {reason} — retrying in {retry_in:?}");
                        }
                        // Rooms are a desktop concept — the CLI never opens
                        // one, so these can't fire; ignore for totality.
                        LiveEvent::RoomState { .. }
                        | LiveEvent::CoeditUpdate { .. }
                        | LiveEvent::CoeditAwareness { .. } => {}
                        LiveEvent::Compacted { upto_seq } => {
                            eprintln!("compacted relay log up to seq {upto_seq}");
                        }
                        LiveEvent::QuotaExceeded {
                            quota_bytes,
                            log_bytes,
                        } => {
                            for line in
                                quota_exceeded_lines(&relay_url, &vault_id, quota_bytes, log_bytes)
                            {
                                eprintln!("{line}");
                            }
                        }
                        LiveEvent::Peers(peers) => {
                            if peers.is_empty() {
                                eprintln!("no other devices online");
                            } else {
                                let list: Vec<String> = peers
                                    .iter()
                                    .map(|p| {
                                        let device = &p.device[..p.device.len().min(12)];
                                        match &p.note_id {
                                            Some(n) if p.editing => format!("{device} ✎ {n}"),
                                            Some(n) => format!("{device} → {n}"),
                                            None => device.to_string(),
                                        }
                                    })
                                    .collect();
                                eprintln!("{} device(s) online: {}", peers.len(), list.join(", "));
                            }
                        }
                    },
                )
                .await
                .map_err(|e| e.to_string());
            }
            eprintln!(
                "syncing {} every {interval_secs}s (Ctrl-C to stop)",
                vault.display()
            );
            client::run_forever(
                &mut engine,
                &mut writer,
                &config.relay_url,
                std::time::Duration::from_secs(interval_secs),
                |report| {
                    if report.run.pushed > 0 || !report.apply.notes.is_empty() {
                        eprintln!(
                            "synced: pushed {}, wrote {} note(s)",
                            report.run.pushed,
                            report.apply.notes.len()
                        );
                    }
                },
            )
            .await
            .map_err(|e| e.to_string())
        }
        SyncCmd::Compact { vault } => {
            let config = load_config(&vault)?;
            let code = VaultCode::parse(&config.vault_code).map_err(|e| e.to_string())?;
            let mut engine = SyncEngine::open(&vault, &code).map_err(|e| e.to_string())?;
            let upto = client::run_compact(&mut engine, &config.relay_url)
                .await
                .map_err(|e| e.to_string())?;
            // Materialize whatever the catch-up pulled in on the way.
            let mut writer = sync_writer(&vault);
            let dirty = engine.pending_apply();
            engine
                .apply_dirty(&mut writer, &dirty)
                .map_err(|e| e.to_string())?;
            if upto == 0 {
                eprintln!("nothing to compact — the relay log is empty");
            } else {
                eprintln!("compacted: relay log truncated up to seq {upto}");
            }
            Ok(())
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
            // One short round-trip for the numbers only the relay knows.
            // Being offline is not a status failure — everything above is
            // local truth and still printed.
            match tokio::time::timeout(
                STATUS_PROBE_TIMEOUT,
                client::probe(&engine, &config.relay_url),
            )
            .await
            {
                Ok(Ok(status)) => {
                    for line in storage_lines(&config.relay_url, engine.vault_id(), &status) {
                        eprintln!("{line}");
                    }
                }
                Ok(Err(e)) => eprintln!("storage:  unknown — relay unreachable ({e})"),
                Err(_) => eprintln!("storage:  unknown — relay did not answer in time"),
            }
            Ok(())
        }
        SyncCmd::Upgrade { vault, plan, open } => {
            let config = load_config(&vault)?;
            let code = VaultCode::parse(&config.vault_code).map_err(|e| e.to_string())?;
            let engine = SyncEngine::open(&vault, &code).map_err(|e| e.to_string())?;
            let Some(url) = checkout_url(&config.relay_url, engine.vault_id(), &plan) else {
                return Err(format!(
                    "this vault syncs through {}, which is not the hosted relay — Pro is a \
                     quota on sync.tacitus.md, and a self-hosted relay has no quota to lift \
                     (set TACITUS_BILLING_URL if you run your own billing sidecar)",
                    relay_host(&config.relay_url)
                ));
            };
            eprintln!("vault id: {}", engine.vault_id());
            eprintln!("plan:     {plan}");
            eprintln!();
            eprintln!("Open this to subscribe — Stripe handles the payment, and the only");
            eprintln!("thing it tells us about you is the vault id above:");
            eprintln!();
            eprintln!("  {url}");
            if open {
                let (program, args) = open_command(&url);
                match std::process::Command::new(program).args(&args).spawn() {
                    Ok(_) => eprintln!("\n(opening in your browser…)"),
                    Err(e) => eprintln!("\n(could not launch {program}: {e} — open it yourself)"),
                }
            }
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
                live: false,
            }
        );
        assert_eq!(
            parse_sync_args(&s(&["run", "--live", "--interval", "2"])).unwrap(),
            SyncCmd::Run {
                vault: PathBuf::from("."),
                interval_secs: 2,
                live: true,
            },
            "--live keeps a persistent session; --interval becomes the scan tick"
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
        assert_eq!(
            parse_sync_args(&s(&["compact", "--vault", "/tmp/v"])).unwrap(),
            SyncCmd::Compact {
                vault: PathBuf::from("/tmp/v"),
            }
        );
        assert!(parse_sync_args(&s(&["frobnicate"])).is_err());
        assert!(parse_sync_args(&s(&["run", "--interval", "soon"])).is_err());
        assert!(parse_sync_args(&s(&["init", "--vault"])).is_err());
    }

    #[test]
    fn sync_args_parse_upgrade() {
        assert_eq!(
            parse_sync_args(&s(&["upgrade"])).unwrap(),
            SyncCmd::Upgrade {
                vault: PathBuf::from("."),
                plan: "monthly".into(),
                open: false,
            },
            "monthly is the default plan"
        );
        assert_eq!(
            parse_sync_args(&s(&["upgrade", "--plan", "yearly", "--open"])).unwrap(),
            SyncCmd::Upgrade {
                vault: PathBuf::from("."),
                plan: "yearly".into(),
                open: true,
            }
        );
        // A typo must not reach Stripe as a price lookup that 400s in a
        // browser — reject it here.
        assert!(parse_sync_args(&s(&["upgrade", "--plan", "montly"])).is_err());
        assert!(parse_sync_args(&s(&["upgrade", "--plan"])).is_err());
    }

    #[test]
    fn relay_host_strips_scheme_port_path_and_userinfo() {
        assert_eq!(relay_host("wss://sync.tacitus.md"), "sync.tacitus.md");
        assert_eq!(relay_host("wss://sync.tacitus.md/ws"), "sync.tacitus.md");
        assert_eq!(relay_host("ws://127.0.0.1:9099/ws"), "127.0.0.1");
        assert_eq!(
            relay_host("ws://user:pw@relay.example:8080"),
            "relay.example"
        );
        assert_eq!(relay_host("ws://[::1]:9099/ws"), "::1");
    }

    #[test]
    fn billing_base_is_hosted_relay_or_explicit_override() {
        assert_eq!(
            billing_base_from("wss://sync.tacitus.md", None).as_deref(),
            Some(HOSTED_BILLING_BASE)
        );
        // A self-hosted relay is nobody's customer: no pitch, no link.
        assert_eq!(billing_base_from("ws://127.0.0.1:9099", None), None);
        assert_eq!(
            billing_base_from("ws://relay.internal", Some("https://pay.internal/")).as_deref(),
            Some("https://pay.internal"),
            "the override wins and its trailing slash is dropped"
        );
        assert_eq!(
            billing_base_from("ws://relay.internal", Some("  ")),
            None,
            "an empty override is not a billing endpoint"
        );
        // A lookalike host must not be mistaken for ours.
        assert_eq!(
            billing_base_from("wss://sync.tacitus.md.evil.example", None),
            None
        );
    }

    #[test]
    fn checkout_url_carries_vault_and_plan() {
        let url = checkout_url("wss://sync.tacitus.md", "a".repeat(32).as_str(), "yearly").unwrap();
        assert_eq!(
            url,
            format!(
                "{HOSTED_BILLING_BASE}/billing/checkout?vault_id={}&plan=yearly",
                "a".repeat(32)
            )
        );
        assert_eq!(checkout_url("ws://localhost:9099", "abc", "monthly"), None);
    }

    #[test]
    fn human_bytes_reads_like_the_price_list() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(900), "900 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
        // The free quota and the Pro quota, exactly as they are advertised.
        assert_eq!(human_bytes(104_857_600), "100.0 MB");
        assert_eq!(human_bytes(1_073_741_824), "1.0 GB");
    }

    #[test]
    fn storage_lines_stay_quiet_until_the_vault_is_nearly_full() {
        let quiet = storage_lines(
            "wss://sync.tacitus.md",
            "v",
            &client::RelayStatus {
                latest_seq: 4,
                log_bytes: 12 * 1024 * 1024,
                quota_bytes: 104_857_600,
            },
        );
        assert_eq!(quiet, vec!["storage:  12.0 MB of 100.0 MB (12%)"]);

        let nudged = storage_lines(
            "wss://sync.tacitus.md",
            "v",
            &client::RelayStatus {
                latest_seq: 4,
                log_bytes: 90 * 1024 * 1024,
                quota_bytes: 104_857_600,
            },
        );
        assert!(nudged[0].contains("(90%)"));
        assert!(
            nudged
                .iter()
                .any(|l| l.contains("/billing/checkout?vault_id=v&plan=monthly")),
            "at 90% the link is the whole point: {nudged:?}"
        );

        let self_hosted = storage_lines(
            "ws://127.0.0.1:9099",
            "v",
            &client::RelayStatus {
                latest_seq: 4,
                log_bytes: 90 * 1024 * 1024,
                quota_bytes: 104_857_600,
            },
        );
        assert_eq!(
            self_hosted.len(),
            1,
            "self-hosters get the number and no sales pitch: {self_hosted:?}"
        );
    }

    #[test]
    fn storage_lines_admit_when_the_relay_reports_no_quota() {
        let lines = storage_lines(
            "wss://sync.tacitus.md",
            "v",
            &client::RelayStatus {
                latest_seq: 1,
                log_bytes: 2048,
                quota_bytes: 0,
            },
        );
        assert_eq!(
            lines,
            vec!["storage:  2.0 KB used (this relay reports no quota)"]
        );
    }

    #[test]
    fn quota_exceeded_lines_name_the_way_out() {
        let hosted = quota_exceeded_lines("wss://sync.tacitus.md", "v", 104_857_600, 104_900_000);
        assert!(hosted[0].contains("100.0 MB"));
        assert!(hosted[1].contains("/billing/checkout?vault_id=v&plan=monthly"));

        let self_hosted =
            quota_exceeded_lines("ws://127.0.0.1:9099", "v", 104_857_600, 104_900_000);
        assert!(self_hosted[1].contains("raise the relay's quota"));

        // Pre-0.24 relays say log_full and know nothing about quotas.
        assert_eq!(
            quota_exceeded_lines("wss://sync.tacitus.md", "v", 0, 0),
            vec!["relay log full — compacting"]
        );
    }

    #[test]
    fn open_command_passes_the_url_through() {
        let (program, args) = open_command("https://example.test/x?a=b");
        assert!(!program.is_empty());
        assert!(args.iter().any(|a| a == "https://example.test/x?a=b"));
    }
}
