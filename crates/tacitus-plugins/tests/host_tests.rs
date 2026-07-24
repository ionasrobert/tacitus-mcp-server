//! Host/ABI integration tests over WAT fixtures — compiled to wasm at test
//! time with the pure-Rust `wat` crate, so the fixtures stay reviewable text
//! and the suite needs no wasm toolchain.

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;
use tacitus_plugins::{HostConfig, PluginHost};

fn temp(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tacitus-plugins-host-{tag}-{nanos}"));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn vault(tag: &str) -> PathBuf {
    let dir = temp(&format!("vault-{tag}"));
    fs::create_dir_all(dir.join("notes")).unwrap();
    fs::write(
        dir.join("notes/alpha.md"),
        "# Alpha\n\nLaunch checklist for the alpha release.\n",
    )
    .unwrap();
    dir
}

/// Write a plugin dir (dir name == manifest name, as `load` requires) with the
/// given WAT compiled to plugin.wasm, tool allowlist and scope.
fn plugin_dir_scoped(tag: &str, wat_src: &str, tools: &[&str], scope: &str) -> PathBuf {
    let dir = temp(&format!("plugin-{tag}")).join("fixture");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("plugin.wasm"), wat::parse_str(wat_src).unwrap()).unwrap();
    let tools_toml: Vec<String> = tools.iter().map(|t| format!("{t:?}")).collect();
    fs::write(
        dir.join("tacitus-plugin.toml"),
        format!(
            "name = \"fixture\"\nversion = \"0.0.1\"\nentry = \"plugin.wasm\"\n\n[permissions]\nscope = \"{scope}\"\ntools = [{}]\n",
            tools_toml.join(", ")
        ),
    )
    .unwrap();
    dir
}

fn plugin_dir(tag: &str, wat_src: &str, tools: &[&str]) -> PathBuf {
    plugin_dir_scoped(tag, wat_src, tools, "read-only")
}

const ECHO: &str = include_str!("fixtures/echo.wat");
const CALL_SEARCH: &str = include_str!("fixtures/call_search.wat");
const CALL_CREATE_NOTE: &str = include_str!("fixtures/call_create_note.wat");
const INFINITE_LOOP: &str = include_str!("fixtures/infinite_loop.wat");
const GROW_MEMORY: &str = include_str!("fixtures/grow_memory.wat");
const BAD_JSON: &str = include_str!("fixtures/bad_json.wat");
const NO_EXPORTS: &str = include_str!("fixtures/no_exports.wat");
const WRONG_ABI: &str = include_str!("fixtures/wrong_abi.wat");
const TRAP: &str = include_str!("fixtures/trap.wat");
const LOGGER: &str = include_str!("fixtures/logger.wat");

#[test]
fn host_runs_echo_guest() {
    let host = PluginHost::new(HostConfig::default()).unwrap();
    let mut plugin = host
        .load(&plugin_dir("echo", ECHO, &[]), &vault("echo"))
        .unwrap();
    let out = plugin.run(&json!({ "query": "roundtrip" })).unwrap();
    // The echo guest returns the exact input payload the host wrote.
    assert_eq!(out["input"]["query"], "roundtrip");
    assert_eq!(out["plugin"]["name"], "fixture");
    assert_eq!(out["plugin"]["version"], "0.0.1");
}

#[test]
fn host_rejects_missing_exports() {
    let host = PluginHost::new(HostConfig::default()).unwrap();
    let e = host
        .load(
            &plugin_dir("noexports", NO_EXPORTS, &[]),
            &vault("noexports"),
        )
        .unwrap_err();
    assert_eq!(e.code, "PLUGIN_ABI");
    assert!(e.reason.contains("memory"), "names the missing export: {e}");
}

#[test]
fn host_rejects_wrong_abi_version() {
    let host = PluginHost::new(HostConfig::default()).unwrap();
    let e = host
        .load(&plugin_dir("wrongabi", WRONG_ABI, &[]), &vault("wrongabi"))
        .unwrap_err();
    assert_eq!(e.code, "PLUGIN_ABI");
    assert!(e.reason.contains("ABI v2"), "names the version: {e}");
}

#[test]
fn guest_call_search_flows_through() {
    let host = PluginHost::new(HostConfig::default()).unwrap();
    let mut plugin = host
        .load(
            &plugin_dir("search", CALL_SEARCH, &["search"]),
            &vault("search"),
        )
        .unwrap();
    let out = plugin.run(&json!({})).unwrap();
    assert_eq!(out["ok"], true, "search envelope: {out}");
    let hits = out["data"]["hits"].as_array().unwrap();
    assert!(!hits.is_empty(), "real hits from the temp vault");
    assert_eq!(hits[0]["note_id"], "notes/alpha");
}

#[test]
fn guest_write_tool_flows_through_readwrite_manifest() {
    let vault_dir = vault("write");
    let host = PluginHost::new(HostConfig::default()).unwrap();
    let mut plugin = host
        .load(
            &plugin_dir_scoped("write", CALL_CREATE_NOTE, &["create_note"], "read-write"),
            &vault_dir,
        )
        .unwrap();
    let out = plugin.run(&json!({})).unwrap();
    assert_eq!(out["ok"], true, "create_note envelope: {out}");
    assert!(out["data"]["version_id"].is_string(), "versioned write");
    assert!(
        vault_dir.join("notes/from-guest.md").exists(),
        "the note landed on disk"
    );
}

#[test]
fn guest_call_undeclared_tool_gets_envelope_not_trap() {
    let host = PluginHost::new(HostConfig::default()).unwrap();
    let mut plugin = host
        .load(&plugin_dir("denied", CALL_SEARCH, &[]), &vault("denied"))
        .unwrap();
    // The guest survives — denial is data, not a trap.
    let out = plugin.run(&json!({})).unwrap();
    assert_eq!(out["ok"], false);
    assert_eq!(out["error"]["code"], "PERMISSION_DENIED");
}

#[test]
fn runaway_guest_exhausts_fuel() {
    let host = PluginHost::new(HostConfig {
        fuel_per_run: 1_000_000,
        ..HostConfig::default()
    })
    .unwrap();
    let mut plugin = host
        .load(&plugin_dir("fuel", INFINITE_LOOP, &[]), &vault("fuel"))
        .unwrap();
    let e = plugin.run(&json!({})).unwrap_err();
    assert_eq!(e.code, "PLUGIN_TRAP");
    assert!(e.reason.contains("fuel"), "mentions fuel: {e}");
}

#[test]
fn run_refuels_between_calls() {
    let host = PluginHost::new(HostConfig {
        fuel_per_run: 5_000_000,
        ..HostConfig::default()
    })
    .unwrap();
    let mut plugin = host
        .load(&plugin_dir("refuel", ECHO, &[]), &vault("refuel"))
        .unwrap();
    plugin.run(&json!({ "n": 1 })).unwrap();
    plugin.run(&json!({ "n": 2 })).unwrap();
}

#[test]
fn guest_memory_growth_capped() {
    let host = PluginHost::new(HostConfig::default()).unwrap();
    let mut plugin = host
        .load(&plugin_dir("grow", GROW_MEMORY, &[]), &vault("grow"))
        .unwrap();
    let e = plugin.run(&json!({})).unwrap_err();
    assert_eq!(
        e.code, "PLUGIN_TRAP",
        "limiter denied the grow, guest trapped"
    );
}

#[test]
fn guest_invalid_json_is_plugin_abi() {
    let host = PluginHost::new(HostConfig::default()).unwrap();
    let mut plugin = host
        .load(&plugin_dir("badjson", BAD_JSON, &[]), &vault("badjson"))
        .unwrap();
    let e = plugin.run(&json!({})).unwrap_err();
    assert_eq!(e.code, "PLUGIN_ABI");
    assert!(e.reason.contains("JSON"), "mentions JSON: {e}");
}

#[test]
fn guest_trap_maps_to_structured_error() {
    let host = PluginHost::new(HostConfig::default()).unwrap();
    let mut plugin = host
        .load(&plugin_dir("trap", TRAP, &[]), &vault("trap"))
        .unwrap();
    let e = plugin.run(&json!({})).unwrap_err();
    assert_eq!(e.code, "PLUGIN_TRAP");
    assert!(e.reason.contains("unreachable"), "names the trap: {e}");
}

#[test]
fn tacitus_log_captured() {
    let host = PluginHost::new(HostConfig::default()).unwrap();
    let mut plugin = host
        .load(&plugin_dir("logger", LOGGER, &[]), &vault("logger"))
        .unwrap();
    let out = plugin.run(&json!({})).unwrap();
    assert_eq!(out["ok"], true);
    assert_eq!(
        plugin.drain_logs(),
        vec![(1u8, "hello from guest".to_string())]
    );
    assert!(plugin.drain_logs().is_empty(), "drain drains");
}
