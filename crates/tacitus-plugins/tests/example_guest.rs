//! E2E over the compiled Rust example guest. Ignored by default so `cargo
//! test` stays green on machines without the wasm32 target — CI builds the
//! guest first, then runs this with `--ignored`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;
use tacitus_plugins::{HostConfig, PluginHost};

fn temp(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tacitus-plugins-e2e-{tag}-{nanos}"));
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
#[ignore = "needs the built example guest: cargo build --release --target wasm32-unknown-unknown --manifest-path examples/plugins/hello-tacitus/Cargo.toml"]
fn example_guest_end_to_end() {
    let example =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/plugins/hello-tacitus");
    let wasm = std::env::var("TACITUS_EXAMPLE_WASM")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            example.join("target/wasm32-unknown-unknown/release/hello_tacitus.wasm")
        });

    // "Install" the plugin: a dir named after its manifest, holding both files.
    let install = temp("install").join("hello-tacitus");
    fs::create_dir_all(&install).unwrap();
    fs::copy(
        example.join("tacitus-plugin.toml"),
        install.join("tacitus-plugin.toml"),
    )
    .unwrap();
    fs::copy(&wasm, install.join("hello_tacitus.wasm"))
        .unwrap_or_else(|e| panic!("build the example guest first ({}): {e}", wasm.display()));

    let vault = temp("vault");
    fs::create_dir_all(vault.join("notes")).unwrap();
    fs::write(
        vault.join("notes/alpha.md"),
        "# Alpha\n\nLaunch checklist for the alpha release.\n",
    )
    .unwrap();

    let host = PluginHost::new(HostConfig::default()).unwrap();
    let mut plugin = host.load(&install, &vault).unwrap();
    assert_eq!(plugin.manifest().name, "hello-tacitus");

    let out = plugin.run(&json!({ "query": "launch" })).unwrap();
    assert_eq!(out["ok"], true, "digest envelope: {out}");
    assert_eq!(out["data"]["query"], "launch");
    assert_eq!(out["data"]["top"][0]["note_id"], "notes/alpha");

    let logs = plugin.drain_logs();
    assert!(
        logs.iter().any(|(_, line)| line.contains("hello-tacitus")),
        "guest logged through tacitus.log: {logs:?}"
    );
}
