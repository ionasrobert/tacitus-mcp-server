//! `tacitus-plugin.toml` — the plugin's declared identity and permissions.
//!
//! Everything here is validated *before* any wasm is compiled: unknown tools,
//! write tools under a read-only scope, and entry paths that escape the plugin
//! directory are all `INVALID_MANIFEST` at load time, not surprises at call
//! time. Least privilege is the default posture: a plugin can only ever call
//! the tools it declared.

use std::path::{Component, Path, PathBuf};

use serde::Deserialize;
use tacitus_core::error::TacitusError;
use tacitus_core::vault::PermissionScope;

use crate::registry::ToolDescriptor;

pub const MANIFEST_FILE: &str = "tacitus-plugin.toml";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginManifest {
    /// `^[a-z0-9][a-z0-9-]*$`, and must equal the plugin directory's name.
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: Option<String>,
    /// Path of the `.wasm` module, relative to the manifest. Absolute paths
    /// and `..` are rejected — the sandbox cannot be pointed outside its dir.
    pub entry: String,
    pub permissions: Permissions,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Permissions {
    /// Reuses the engine's scope (kebab-case: "read-only" | "read-write").
    /// Constructs the registry's `NoteWriter`, so the core seam enforces it.
    pub scope: PermissionScope,
    /// Exact allowlist of tools the plugin may call. Anything else gets a
    /// `PERMISSION_DENIED` envelope at call time.
    pub tools: Vec<String>,
}

fn invalid(reason: String, suggestion: impl Into<String>) -> TacitusError {
    TacitusError::new("INVALID_MANIFEST", reason, suggestion)
}

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

impl PluginManifest {
    pub fn parse(toml_src: &str) -> Result<Self, TacitusError> {
        let manifest: PluginManifest = toml::from_str(toml_src).map_err(|e| {
            invalid(
                format!("Manifest does not parse: {e}"),
                "Fix tacitus-plugin.toml — see docs/PLUGINS.md for the schema.",
            )
        })?;
        if !valid_name(&manifest.name) {
            return Err(invalid(
                format!(
                    "Plugin name {:?} is invalid (want ^[a-z0-9][a-z0-9-]*$).",
                    manifest.name
                ),
                "Use lowercase letters, digits and dashes, e.g. \"vault-digest\".",
            ));
        }
        Ok(manifest)
    }

    /// Read + parse `<plugin_dir>/tacitus-plugin.toml` and pin the manifest
    /// name to the directory name (installs stay addressable by path).
    pub fn load(plugin_dir: &Path) -> Result<Self, TacitusError> {
        let path = plugin_dir.join(MANIFEST_FILE);
        let src = std::fs::read_to_string(&path).map_err(|e| {
            invalid(
                format!("Cannot read {}: {e}.", path.display()),
                "Every plugin directory needs a tacitus-plugin.toml manifest.",
            )
        })?;
        let manifest = Self::parse(&src)?;
        if let Some(dir_name) = plugin_dir.file_name().and_then(|n| n.to_str()) {
            if dir_name != manifest.name {
                return Err(invalid(
                    format!(
                        "Manifest name {:?} does not match its directory {dir_name:?}.",
                        manifest.name
                    ),
                    "Rename the directory or the manifest's `name` so they match.",
                ));
            }
        }
        Ok(manifest)
    }

    /// Check the allowlist against the registry: every tool must exist, and
    /// write tools require scope = "read-write".
    pub fn validate(&self, tools: &[ToolDescriptor]) -> Result<(), TacitusError> {
        for wanted in &self.permissions.tools {
            let Some(desc) = tools.iter().find(|d| d.name == wanted.as_str()) else {
                let names: Vec<&str> = tools.iter().map(|d| d.name).collect();
                return Err(invalid(
                    format!("Unknown tool {wanted:?} in [permissions].tools."),
                    format!("Valid tools: {}.", names.join(", ")),
                ));
            };
            if desc.writes && self.permissions.scope == PermissionScope::ReadOnly {
                return Err(invalid(
                    format!("Tool {wanted:?} writes to the vault but scope is \"read-only\"."),
                    "Drop the tool from [permissions].tools or set scope = \"read-write\".",
                ));
            }
        }
        Ok(())
    }

    /// Resolve `entry` inside the plugin directory. `..` and absolute paths
    /// are `INVALID_MANIFEST` — never a path escape.
    pub fn wasm_path(&self, plugin_dir: &Path) -> Result<PathBuf, TacitusError> {
        let entry = Path::new(&self.entry);
        let escapes = entry.is_absolute()
            || entry
                .components()
                .any(|c| matches!(c, Component::ParentDir | Component::RootDir));
        if escapes {
            return Err(invalid(
                format!("entry {:?} escapes the plugin directory.", self.entry),
                "Use a path relative to tacitus-plugin.toml, e.g. \"plugin.wasm\".",
            ));
        }
        Ok(plugin_dir.join(entry))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::ToolRegistry;

    const VALID: &str = r#"
name = "hello-tacitus"
version = "0.1.0"
description = "Read-only vault digest."
entry = "hello.wasm"

[permissions]
scope = "read-only"
tools = ["capabilities", "search", "get_note"]
"#;

    #[test]
    fn manifest_parses_valid_toml() {
        let m = PluginManifest::parse(VALID).unwrap();
        assert_eq!(m.name, "hello-tacitus");
        assert_eq!(m.version, "0.1.0");
        assert_eq!(m.entry, "hello.wasm");
        assert_eq!(m.permissions.scope, PermissionScope::ReadOnly);
        assert_eq!(m.permissions.tools, ["capabilities", "search", "get_note"]);
        m.validate(ToolRegistry::descriptors()).unwrap();
    }

    #[test]
    fn manifest_rejects_unknown_tool() {
        let src = VALID.replace("\"search\"", "\"serach\"");
        let m = PluginManifest::parse(&src).unwrap();
        let e = m.validate(ToolRegistry::descriptors()).unwrap_err();
        assert_eq!(e.code, "INVALID_MANIFEST");
        assert!(e.reason.contains("serach"), "reason names the bad tool");
        assert!(
            e.suggestion.contains("search"),
            "suggestion lists valid tools"
        );
    }

    #[test]
    fn manifest_rejects_write_tool_with_readonly_scope() {
        let src = VALID.replace("\"search\"", "\"create_note\"");
        let m = PluginManifest::parse(&src).unwrap();
        let e = m.validate(ToolRegistry::descriptors()).unwrap_err();
        assert_eq!(e.code, "INVALID_MANIFEST");
        assert!(e.reason.contains("create_note"));
        assert!(e.reason.contains("read-only"));
    }

    #[test]
    fn manifest_rejects_entry_escape() {
        for entry in ["../../evil.wasm", "/tmp/evil.wasm"] {
            let src = VALID.replace("hello.wasm", entry);
            let m = PluginManifest::parse(&src).unwrap();
            let e = m
                .wasm_path(Path::new("/vault/.tacitus/plugins/x"))
                .unwrap_err();
            assert_eq!(
                e.code, "INVALID_MANIFEST",
                "entry {entry:?} must be rejected"
            );
        }
        // The happy path stays inside the plugin dir.
        let m = PluginManifest::parse(VALID).unwrap();
        let p = m.wasm_path(Path::new("/vault/.tacitus/plugins/x")).unwrap();
        assert_eq!(p, Path::new("/vault/.tacitus/plugins/x/hello.wasm"));
    }

    #[test]
    fn manifest_rejects_bad_name_and_unknown_keys() {
        let bad_name = VALID.replace("hello-tacitus", "Hello Tacitus!");
        let e = PluginManifest::parse(&bad_name).unwrap_err();
        assert_eq!(e.code, "INVALID_MANIFEST");

        let unknown_key = format!("{VALID}\nnetwork = true\n");
        let e = PluginManifest::parse(&unknown_key).unwrap_err();
        assert_eq!(e.code, "INVALID_MANIFEST", "deny_unknown_fields fires");
        assert!(e.reason.contains("network"));
    }

    #[test]
    fn manifest_load_missing_file_is_structured() {
        let dir = std::env::temp_dir().join("tacitus-plugins-no-manifest");
        std::fs::create_dir_all(&dir).unwrap();
        let e = PluginManifest::load(&dir).unwrap_err();
        assert_eq!(e.code, "INVALID_MANIFEST");
        assert!(e.suggestion.contains("tacitus-plugin.toml"));
    }
}
