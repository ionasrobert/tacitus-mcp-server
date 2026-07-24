# hello-tacitus

The reference sandboxed Tacitus plugin: reads `{ "query": "..." }` from its
input, calls the `search` tool through `tacitus.call`, and returns a digest of
the top hits. Read-only scope, one tool in its allowlist — least privilege by
default.

It implements [ABI v1](../../../docs/PLUGINS.md) by hand on purpose: five
exports, two imports, JSON both ways. No SDK required.

## Build

```sh
rustup target add wasm32-unknown-unknown
cargo build --release --target wasm32-unknown-unknown
```

The module lands at `target/wasm32-unknown-unknown/release/hello_tacitus.wasm`.

## Install into a vault

A plugin is a directory under `.tacitus/plugins/` whose name matches its
manifest:

```sh
mkdir -p /path/to/vault/.tacitus/plugins/hello-tacitus
cp tacitus-plugin.toml \
   target/wasm32-unknown-unknown/release/hello_tacitus.wasm \
   /path/to/vault/.tacitus/plugins/hello-tacitus/
```

## Run (dev runner)

```sh
cargo run -p tacitus-plugins --example run_plugin -- \
  /path/to/vault /path/to/vault/.tacitus/plugins/hello-tacitus '{"query":"launch"}'
```
