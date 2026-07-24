//! Dispatch-level tests for the shared tool registry. The first eight moved
//! here verbatim from tacitus-plugins (plugins-m1) when the registry became
//! shared; the rest cover the tools that joined the shared surface in
//! plugins-m2.

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use super::*;

fn temp_vault(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tacitus-tools-{tag}-{nanos}"));
    fs::create_dir_all(dir.join("notes")).unwrap();
    fs::write(
        dir.join("notes/alpha.md"),
        "# Alpha\n\nLaunch checklist for the alpha release.\n",
    )
    .unwrap();
    fs::write(
        dir.join("notes/beta.md"),
        "# Beta\n\nNotes about the beta program. See [[notes/alpha]].\n",
    )
    .unwrap();
    dir
}

fn allow(names: &[&str]) -> HashSet<String> {
    names.iter().map(|s| s.to_string()).collect()
}

fn rw(vault: &std::path::Path) -> ToolRegistry {
    ToolRegistry::standard(vault, PermissionScope::ReadWrite)
}

// ---- moved from tacitus-plugins (plugins-m1) ----

#[test]
fn dispatch_search_returns_hits_envelope() {
    let vault = temp_vault("search");
    let reg = ToolRegistry::standard(&vault, PermissionScope::ReadOnly);
    let out = reg.dispatch("search", &json!({ "query": "alpha launch" }), None);
    assert_eq!(out["ok"], true);
    let hits = out["data"]["hits"].as_array().unwrap();
    assert!(!hits.is_empty());
    let hit = &hits[0];
    for key in ["note_id", "title", "score", "snippet", "token_count"] {
        assert!(hit.get(key).is_some(), "hit has {key}");
    }
}

#[test]
fn dispatch_unknown_tool_is_invalid_input() {
    let vault = temp_vault("unknown");
    let reg = ToolRegistry::standard(&vault, PermissionScope::ReadOnly);
    let out = reg.dispatch("no_such_tool", &json!({}), None);
    assert_eq!(out["ok"], false);
    assert_eq!(out["error"]["code"], "INVALID_INPUT");
    assert!(out["error"]["suggestion"]
        .as_str()
        .unwrap()
        .contains("search"));
}

#[test]
fn dispatch_denies_tool_missing_from_allowlist() {
    let vault = temp_vault("denied");
    let reg = ToolRegistry::standard(&vault, PermissionScope::ReadOnly);
    let allowed = allow(&["get_note"]);
    let out = reg.dispatch("search", &json!({ "query": "alpha" }), Some(&allowed));
    assert_eq!(out["ok"], false);
    assert_eq!(out["error"]["code"], "PERMISSION_DENIED");
    assert!(out["error"]["suggestion"]
        .as_str()
        .unwrap()
        .contains("tacitus-plugin.toml"));
}

#[test]
fn dispatch_create_note_readonly_scope_denied() {
    let vault = temp_vault("readonly");
    let reg = ToolRegistry::standard(&vault, PermissionScope::ReadOnly);
    let out = reg.dispatch(
        "create_note",
        &json!({ "note_id": "notes/x", "content": "hi" }),
        None,
    );
    assert_eq!(out["ok"], false);
    assert_eq!(out["error"]["code"], "PERMISSION_DENIED");
}

#[test]
fn dispatch_create_then_get_note_roundtrip() {
    let vault = temp_vault("roundtrip");
    let reg = rw(&vault);
    let out = reg.dispatch(
        "create_note",
        &json!({ "note_id": "notes/fresh", "content": "hello from a plugin" }),
        None,
    );
    assert_eq!(out["ok"], true, "create failed: {out}");
    assert!(out["data"]["version_id"].is_string());

    let out = reg.dispatch(
        "get_note",
        &json!({ "note_id": "notes/fresh", "format": "full" }),
        None,
    );
    assert_eq!(out["ok"], true);
    assert!(out["data"]["content"]
        .as_str()
        .unwrap()
        .contains("hello from a plugin"));
}

#[test]
fn dispatch_remember_then_recall_roundtrip() {
    let vault = temp_vault("memory");
    let reg = rw(&vault);
    let out = reg.dispatch(
        "remember",
        &json!({
            "content": "The plugin marketplace launches in autumn.",
            "type": "project",
            "source": { "origin": "test", "author": "agent" }
        }),
        None,
    );
    assert_eq!(out["ok"], true, "remember failed: {out}");
    assert!(out["data"]["memory_id"].is_string());

    let out = reg.dispatch("recall", &json!({ "query": "marketplace launch" }), None);
    assert_eq!(out["ok"], true);
    assert!(!out["data"]["items"].as_array().unwrap().is_empty());
}

#[test]
fn dispatch_capabilities_reflects_allowlist_only() {
    let vault = temp_vault("caps");
    let reg = ToolRegistry::standard(&vault, PermissionScope::ReadOnly);
    let allowed = allow(&["capabilities", "search"]);
    let out = reg.dispatch("capabilities", &json!({}), Some(&allowed));
    assert_eq!(out["ok"], true);
    let names: Vec<&str> = out["data"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["capabilities", "search"],
        "allowlist only, not all tools"
    );
    assert_eq!(out["data"]["permissions"]["scope"], "read-only");
}

#[test]
fn dispatch_never_panics_on_malformed_args() {
    let vault = temp_vault("malformed");
    let reg = ToolRegistry::standard(&vault, PermissionScope::ReadOnly);
    let out = reg.dispatch("search", &json!({ "query": 42 }), None);
    assert_eq!(out["ok"], false);
    assert_eq!(out["error"]["code"], "INVALID_INPUT");
    let out = reg.dispatch("get_note", &json!("not an object"), None);
    assert_eq!(out["ok"], false);
    assert_eq!(out["error"]["code"], "INVALID_INPUT");
}

// ---- plugins-m2: the newly shared tools, through dispatch ----

#[test]
fn dispatch_propose_commit_revert_roundtrip() {
    let vault = temp_vault("txn");
    let reg = rw(&vault);
    let out = reg.dispatch(
        "propose_changes",
        &json!({ "ops": [{ "op": "update", "note_id": "notes/alpha", "content": "changed body" }] }),
        None,
    );
    assert_eq!(out["ok"], true, "propose failed: {out}");
    let change_id = out["data"]["change_id"].as_str().unwrap().to_string();
    let on_disk = fs::read_to_string(vault.join("notes/alpha.md")).unwrap();
    assert!(on_disk.contains("Launch checklist"), "propose is a dry-run");

    let out = reg.dispatch("commit_changes", &json!({ "change_id": change_id }), None);
    assert_eq!(out["ok"], true, "commit failed: {out}");
    let version_id = out["data"]["version_id"].as_str().unwrap().to_string();
    let on_disk = fs::read_to_string(vault.join("notes/alpha.md")).unwrap();
    assert!(on_disk.contains("changed body"), "commit writes");

    let out = reg.dispatch("revert", &json!({ "version_id": version_id }), None);
    assert_eq!(out["ok"], true, "revert failed: {out}");
    let on_disk = fs::read_to_string(vault.join("notes/alpha.md")).unwrap();
    assert!(on_disk.contains("Launch checklist"), "revert restores");
}

#[test]
fn dispatch_propose_is_dry_run_under_readonly() {
    let vault = temp_vault("dryrun-ro");
    let reg = ToolRegistry::standard(&vault, PermissionScope::ReadOnly);
    let out = reg.dispatch(
        "propose_changes",
        &json!({ "ops": [{ "op": "update", "note_id": "notes/alpha", "content": "nope" }] }),
        None,
    );
    assert_eq!(out["ok"], true, "propose allowed under read-only: {out}");
    let change_id = out["data"]["change_id"].as_str().unwrap().to_string();

    let out = reg.dispatch("commit_changes", &json!({ "change_id": change_id }), None);
    assert_eq!(out["ok"], false);
    assert_eq!(out["error"]["code"], "PERMISSION_DENIED");
}

#[test]
fn dispatch_link_is_idempotent() {
    let vault = temp_vault("link");
    let reg = rw(&vault);
    for _ in 0..2 {
        let out = reg.dispatch(
            "link",
            &json!({ "from": "notes/alpha", "to": "notes/beta" }),
            None,
        );
        assert_eq!(out["ok"], true, "link failed: {out}");
    }
    let content = fs::read_to_string(vault.join("notes/alpha.md")).unwrap();
    assert_eq!(content.matches("[[notes/beta]]").count(), 1);
}

#[test]
fn dispatch_tag_dedupes() {
    let vault = temp_vault("tag");
    let reg = rw(&vault);
    for _ in 0..2 {
        let out = reg.dispatch(
            "tag",
            &json!({ "note_id": "notes/alpha", "tag": "milestone" }),
            None,
        );
        assert_eq!(out["ok"], true, "tag failed: {out}");
    }
    let content = fs::read_to_string(vault.join("notes/alpha.md")).unwrap();
    assert_eq!(content.matches("milestone").count(), 1);
}

#[test]
fn dispatch_toggle_task_conflict_guard() {
    let vault = temp_vault("toggle");
    fs::write(
        vault.join("notes/todo.md"),
        "# Todo\n\n- [ ] write the report due:2030-01-01\n",
    )
    .unwrap();
    let reg = rw(&vault);
    let out = reg.dispatch("list_tasks", &json!({}), None);
    assert_eq!(out["ok"], true);
    let task = &out["data"]["tasks"][0];
    let (line, text) = (task["line"].clone(), task["text"].as_str().unwrap());

    let out = reg.dispatch(
        "toggle_task",
        &json!({ "note_id": "notes/todo", "line": line, "expect_text": "something stale" }),
        None,
    );
    assert_eq!(out["error"]["code"], "CONFLICT", "stale guard fires: {out}");

    let out = reg.dispatch(
        "toggle_task",
        &json!({ "note_id": "notes/todo", "line": line, "expect_text": text }),
        None,
    );
    assert_eq!(out["ok"], true, "toggle failed: {out}");
    let content = fs::read_to_string(vault.join("notes/todo.md")).unwrap();
    assert!(content.contains("- [x]"), "checkbox flipped: {content}");
}

#[test]
fn dispatch_rename_note_retargets_links() {
    let vault = temp_vault("rename");
    let reg = rw(&vault);
    let out = reg.dispatch(
        "rename_note",
        &json!({ "from": "notes/alpha", "to": "notes/gamma" }),
        None,
    );
    assert_eq!(out["ok"], true, "rename failed: {out}");
    assert_eq!(out["data"]["links_updated_in"], 1);
    let beta = fs::read_to_string(vault.join("notes/beta.md")).unwrap();
    assert!(
        beta.contains("[[notes/gamma]]"),
        "backlink retargeted: {beta}"
    );
    assert!(!vault.join("notes/alpha.md").exists());
    assert!(vault.join("notes/gamma.md").exists());
}

#[test]
fn dispatch_delete_note_then_revert_restores() {
    let vault = temp_vault("delete");
    let reg = rw(&vault);
    let out = reg.dispatch("delete_note", &json!({ "note_id": "notes/alpha" }), None);
    assert_eq!(out["ok"], true, "delete failed: {out}");
    assert!(!vault.join("notes/alpha.md").exists());

    let version_id = out["data"]["version_id"].as_str().unwrap();
    let out = reg.dispatch("revert", &json!({ "version_id": version_id }), None);
    assert_eq!(out["ok"], true, "revert failed: {out}");
    assert!(vault.join("notes/alpha.md").exists());
}

#[test]
fn dispatch_get_version_include_content_clips() {
    let vault = temp_vault("version");
    let reg = rw(&vault);
    let out = reg.dispatch(
        "update_note",
        &json!({ "note_id": "notes/alpha", "content": "a much longer body than four characters" }),
        None,
    );
    let version_id = out["data"]["version_id"].as_str().unwrap().to_string();

    let out = reg.dispatch("get_version", &json!({ "version_id": version_id }), None);
    assert_eq!(out["ok"], true);
    assert!(
        out["data"]["notes"][0]["after"].is_null(),
        "default: ops only"
    );

    let out = reg.dispatch(
        "get_version",
        &json!({ "version_id": version_id, "include_content": true, "max_tokens": 1 }),
        None,
    );
    let after = &out["data"]["notes"][0]["after"];
    assert_eq!(after["truncated"], true);
    assert!(after["content"].as_str().unwrap().chars().count() <= 4);
}

#[test]
fn dispatch_audit_log_lists_recent_first() {
    let vault = temp_vault("audit");
    let reg = rw(&vault);
    reg.dispatch(
        "create_note",
        &json!({ "note_id": "notes/first", "content": "1" }),
        None,
    );
    reg.dispatch(
        "create_note",
        &json!({ "note_id": "notes/second", "content": "2" }),
        None,
    );
    let out = reg.dispatch("audit_log", &json!({ "limit": 1 }), None);
    assert_eq!(out["ok"], true);
    let entries = out["data"]["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["notes"][0], "notes/second", "most recent first");
}

#[test]
fn dispatch_forget_removes_memory() {
    let vault = temp_vault("forget");
    let reg = rw(&vault);
    let out = reg.dispatch(
        "remember",
        &json!({
            "content": "Ephemeral fact.", "type": "project",
            "source": { "origin": "test", "author": "agent" }
        }),
        None,
    );
    let memory_id = out["data"]["memory_id"].as_str().unwrap().to_string();

    let out = reg.dispatch("forget", &json!({ "memory_id": memory_id }), None);
    assert_eq!(out["data"]["removed"], true);
    let out = reg.dispatch("recall", &json!({ "query": "ephemeral fact" }), None);
    assert!(out["data"]["items"].as_array().unwrap().is_empty());
    let out = reg.dispatch("forget", &json!({ "memory_id": "mem-nope" }), None);
    assert_eq!(out["data"]["removed"], false);
}

#[test]
fn dispatch_suggest_links_returns_ranked_candidates() {
    let vault = temp_vault("suggest");
    fs::write(
        vault.join("notes/draft.md"),
        "# Draft\n\nWe should fold the Alpha checklist into this plan.\n",
    )
    .unwrap();
    let reg = ToolRegistry::standard(&vault, PermissionScope::ReadOnly);
    let out = reg.dispatch("suggest_links", &json!({ "note_id": "notes/draft" }), None);
    assert_eq!(out["ok"], true, "suggest failed: {out}");
    let suggestions = out["data"]["suggestions"].as_array().unwrap();
    let alpha = suggestions
        .iter()
        .find(|s| s["note_id"] == "notes/alpha")
        .expect("alpha suggested");
    assert!(!alpha["reasons"].as_array().unwrap().is_empty());
}

#[test]
fn dispatch_template_missing_vars_is_structured() {
    let vault = temp_vault("template");
    fs::create_dir_all(vault.join(".tacitus/templates")).unwrap();
    fs::write(
        vault.join(".tacitus/templates/meeting.md"),
        "---\ntitle: \"{{title}}\"\n---\n\n# {{title}}\n",
    )
    .unwrap();
    let reg = rw(&vault);
    let out = reg.dispatch("list_templates", &json!({}), None);
    assert_eq!(out["data"]["templates"][0]["name"], "meeting");
    assert!(out["data"]["templates"][0]["vars"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == "title"));

    let out = reg.dispatch(
        "create_from_template",
        &json!({ "template": "meeting", "note_id": "notes/standup" }),
        None,
    );
    assert_eq!(out["ok"], false);
    assert_eq!(out["error"]["code"], "MISSING_VARS");
}

#[test]
fn dispatch_capabilities_reports_configured_identity() {
    let vault = temp_vault("identity");
    let reg = ToolRegistry::standard(&vault, PermissionScope::ReadWrite)
        .with_identity("tacitus-plugins", "9.9.9");
    let out = reg.dispatch("capabilities", &json!({}), None);
    assert_eq!(out["data"]["server"], "tacitus-plugins");
    assert_eq!(out["data"]["version"], "9.9.9");
    let names: Vec<&str> = out["data"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names.len(), 25, "all tools when unrestricted");
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted, "sorted by name (matches rmcp list_all)");
}

#[test]
fn descriptor_table_is_complete_and_classified() {
    let descriptors = ToolRegistry::descriptors();
    assert_eq!(descriptors.len(), 25);
    let writes: HashSet<&str> = descriptors
        .iter()
        .filter(|d| d.writes)
        .map(|d| d.name)
        .collect();
    let expected: HashSet<&str> = [
        "remember",
        "forget",
        "commit_changes",
        "revert",
        "create_note",
        "update_note",
        "link",
        "tag",
        "create_from_template",
        "toggle_task",
        "rename_note",
        "delete_note",
    ]
    .into();
    assert_eq!(writes, expected, "write-tool classification");

    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ToolRegistry>();
}

#[test]
fn dispatch_no_arg_tools_accept_empty_args() {
    let vault = temp_vault("noargs");
    let reg = ToolRegistry::standard(&vault, PermissionScope::ReadOnly);
    let out = reg.dispatch("list_notes", &json!({}), None);
    assert_eq!(out["ok"], true);
    assert_eq!(out["data"]["notes"].as_array().unwrap().len(), 2);
    let out = reg.dispatch("list_templates", &json!({}), None);
    assert_eq!(out["ok"], true);
    let _unused: Value = out;
}

#[test]
fn with_writer_reflects_into_live_index_and_attributes_origin() {
    let vault = temp_vault("livewriter");
    let index = std::sync::Arc::new(std::sync::Mutex::new(VaultIndex::build(&vault).unwrap()));
    let mut writer = NoteWriter::with_index(&vault, PermissionScope::ReadWrite, index.clone());
    writer.set_origin("plugin:fixture");
    let reg = ToolRegistry::standard(&vault, PermissionScope::ReadWrite).with_writer(writer);
    assert_eq!(reg.scope(), PermissionScope::ReadWrite);

    let out = reg.dispatch(
        "create_note",
        &json!({ "note_id": "notes/live", "content": "written by a plugin" }),
        None,
    );
    assert_eq!(out["ok"], true, "create failed: {out}");
    // The SHARED index sees the note without any rebuild.
    assert!(
        index.lock().unwrap().get("notes/live").is_some(),
        "live index refreshed by the injected writer"
    );
    // And the audit log attributes the write.
    let audit = fs::read_to_string(vault.join(".tacitus/audit.log")).unwrap();
    assert!(
        audit.contains("\"origin\":\"plugin:fixture\""),
        "origin attributed: {audit}"
    );
}
