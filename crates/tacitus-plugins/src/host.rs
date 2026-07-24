//! The wasmtime host: loads a plugin directory (manifest + wasm), wires the
//! `tacitus.call` / `tacitus.log` imports, and runs the guest under fuel and
//! memory limits. Guest failures never panic the host — they surface as
//! structured `PLUGIN_ABI` (protocol broken) or `PLUGIN_TRAP` (crashed or
//! exceeded limits) errors.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use serde_json::{json, Value};
use tacitus_core::error::TacitusError;
use wasmtime::{
    Caller, Config, Engine, Instance, Linker, Memory, Module, Store, StoreLimits,
    StoreLimitsBuilder, Trap, TypedFunc,
};

use crate::abi;
use crate::manifest::PluginManifest;
use crate::registry::ToolRegistry;

/// Host policy — a manifest can never raise these.
pub struct HostConfig {
    /// Instruction budget per [`PluginInstance::run`] call (refueled each run).
    pub fuel_per_run: u64,
    /// Linear-memory ceiling for the guest.
    pub max_memory_bytes: usize,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            fuel_per_run: 1_000_000_000,
            max_memory_bytes: 64 * 1024 * 1024,
        }
    }
}

pub struct PluginHost {
    engine: Engine,
    config: HostConfig,
}

struct PluginState {
    registry: Arc<ToolRegistry>,
    allowlist: HashSet<String>,
    logs: Vec<(u8, String)>,
    limits: StoreLimits,
}

fn internal(e: impl std::fmt::Display) -> TacitusError {
    TacitusError::new(
        "INTERNAL",
        e.to_string(),
        "Report this as a tacitus-plugins bug.",
    )
}

fn plugin_abi(reason: String, suggestion: &str) -> TacitusError {
    TacitusError::new("PLUGIN_ABI", reason, suggestion)
}

fn missing_export(name: &str) -> TacitusError {
    plugin_abi(
        format!("Guest is missing the required export {name:?}."),
        "Implement ABI v1: memory, tacitus_abi_version, tacitus_alloc, tacitus_dealloc, tacitus_run.",
    )
}

fn plugin_trap(e: &wasmtime::Error) -> TacitusError {
    let reason = match e.downcast_ref::<Trap>() {
        Some(trap) => format!("Guest trapped: {trap}."),
        None => format!("Guest failed: {e}."),
    };
    TacitusError::new(
        "PLUGIN_TRAP",
        reason,
        "The plugin crashed or exceeded its fuel/memory limits. Fix the plugin, or raise HostConfig if the limits are too tight.",
    )
}

impl PluginHost {
    pub fn new(config: HostConfig) -> Result<Self, TacitusError> {
        let mut wt = Config::new();
        wt.consume_fuel(true);
        let engine = Engine::new(&wt).map_err(internal)?;
        Ok(Self { engine, config })
    }

    /// Load a plugin: manifest → validate → compile → instantiate → ABI check.
    /// The registry is scoped by the manifest (scope constructs the
    /// `NoteWriter`, allowlist gates every `tacitus.call`).
    pub fn load(&self, plugin_dir: &Path, vault: &Path) -> Result<PluginInstance, TacitusError> {
        let manifest = PluginManifest::load(plugin_dir)?;
        manifest.validate(ToolRegistry::descriptors())?;
        let wasm_path = manifest.wasm_path(plugin_dir)?;
        let bytes = std::fs::read(&wasm_path).map_err(|e| {
            TacitusError::new(
                "IO_ERROR",
                format!("Cannot read wasm module {}: {e}.", wasm_path.display()),
                "Build the plugin first so the manifest's `entry` .wasm exists.",
            )
        })?;
        let module = Module::new(&self.engine, &bytes).map_err(|e| {
            plugin_abi(
                format!("Entry is not a valid wasm module: {e}."),
                "Compile the plugin for wasm32-unknown-unknown and point `entry` at the .wasm.",
            )
        })?;

        let state = PluginState {
            registry: Arc::new(ToolRegistry::standard(vault, manifest.permissions.scope)),
            allowlist: manifest.permissions.tools.iter().cloned().collect(),
            logs: Vec::new(),
            limits: StoreLimitsBuilder::new()
                .memory_size(self.config.max_memory_bytes)
                .build(),
        };
        let mut store = Store::new(&self.engine, state);
        store.limiter(|s| &mut s.limits);
        store.set_fuel(self.config.fuel_per_run).map_err(internal)?;

        let mut linker: Linker<PluginState> = Linker::new(&self.engine);
        linker
            .func_wrap(abi::IMPORT_MODULE, abi::IMPORT_CALL, host_call)
            .map_err(internal)?;
        linker
            .func_wrap(abi::IMPORT_MODULE, abi::IMPORT_LOG, host_log)
            .map_err(internal)?;

        let instance = linker.instantiate(&mut store, &module).map_err(|e| {
            plugin_abi(
                format!("Cannot instantiate plugin: {e}."),
                "The only host imports are tacitus.call and tacitus.log (module \"tacitus\").",
            )
        })?;

        let memory = instance
            .get_memory(&mut store, abi::EXPORT_MEMORY)
            .ok_or_else(|| missing_export(abi::EXPORT_MEMORY))?;
        let abi_version = typed_export::<(), i32>(&instance, &mut store, abi::EXPORT_ABI_VERSION)?;
        let alloc = typed_export::<i32, i32>(&instance, &mut store, abi::EXPORT_ALLOC)?;
        let dealloc = typed_export::<(i32, i32), ()>(&instance, &mut store, abi::EXPORT_DEALLOC)?;
        let run = typed_export::<(i32, i32), i64>(&instance, &mut store, abi::EXPORT_RUN)?;

        let version = abi_version
            .call(&mut store, ())
            .map_err(|e| plugin_trap(&e))?;
        if version != abi::ABI_VERSION {
            return Err(plugin_abi(
                format!(
                    "Plugin speaks ABI v{version}; this host speaks v{}.",
                    abi::ABI_VERSION
                ),
                "Rebuild the plugin against this host's ABI (docs/PLUGINS.md).",
            ));
        }

        Ok(PluginInstance {
            manifest,
            store,
            memory,
            alloc,
            dealloc,
            run,
            fuel_per_run: self.config.fuel_per_run,
        })
    }
}

pub struct PluginInstance {
    manifest: PluginManifest,
    store: Store<PluginState>,
    memory: Memory,
    alloc: TypedFunc<i32, i32>,
    dealloc: TypedFunc<(i32, i32), ()>,
    run: TypedFunc<(i32, i32), i64>,
    fuel_per_run: u64,
}

impl std::fmt::Debug for PluginInstance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginInstance")
            .field("name", &self.manifest.name)
            .field("version", &self.manifest.version)
            .finish_non_exhaustive()
    }
}

impl PluginInstance {
    pub fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    /// One plugin invocation: refuel, hand the input JSON to the guest, read
    /// its `{ ok, data | error }` envelope back.
    pub fn run(&mut self, input: &Value) -> Result<Value, TacitusError> {
        self.store.set_fuel(self.fuel_per_run).map_err(internal)?;
        let payload = json!({
            "input": input,
            "plugin": { "name": self.manifest.name, "version": self.manifest.version },
        })
        .to_string();
        let len = i32::try_from(payload.len()).map_err(|_| {
            plugin_abi(
                "Input too large for the ABI.".into(),
                "Pass smaller inputs.",
            )
        })?;
        let ptr = self
            .alloc
            .call(&mut self.store, len)
            .map_err(|e| plugin_trap(&e))?;
        write_guest(&self.memory, &mut self.store, ptr, payload.as_bytes())?;

        let packed = self
            .run
            .call(&mut self.store, (ptr, len))
            .map_err(|e| plugin_trap(&e))?;
        if packed == 0 {
            return Err(plugin_abi(
                "tacitus_run returned 0 instead of a packed ptr/len.".into(),
                "Return pack(ptr, len) of a JSON envelope from tacitus_run.",
            ));
        }
        let (out_ptr, out_len) = abi::unpack(packed as u64);
        let bytes = read_guest(&self.memory, &self.store, out_ptr as i32, out_len as i32)?;
        // Best-effort free; the guest may no-op it.
        let _ = self
            .dealloc
            .call(&mut self.store, (out_ptr as i32, out_len as i32));

        let text = String::from_utf8(bytes).map_err(|_| {
            plugin_abi(
                "Guest returned non-UTF-8 output.".into(),
                "Return UTF-8 JSON from tacitus_run.",
            )
        })?;
        serde_json::from_str(&text).map_err(|e| {
            plugin_abi(
                format!("Guest returned invalid JSON: {e}."),
                "Return a { ok, data | error } JSON envelope from tacitus_run.",
            )
        })
    }

    /// Take the log lines the guest emitted via `tacitus.log` since the last
    /// drain: `(level, message)` with 0=debug 1=info 2=warn 3=error.
    pub fn drain_logs(&mut self) -> Vec<(u8, String)> {
        std::mem::take(&mut self.store.data_mut().logs)
    }
}

fn typed_export<P, R>(
    instance: &Instance,
    store: &mut Store<PluginState>,
    name: &str,
) -> Result<TypedFunc<P, R>, TacitusError>
where
    P: wasmtime::WasmParams,
    R: wasmtime::WasmResults,
{
    let func = instance
        .get_func(&mut *store, name)
        .ok_or_else(|| missing_export(name))?;
    func.typed::<P, R>(&*store).map_err(|e| {
        plugin_abi(
            format!("Export {name:?} has the wrong signature: {e}."),
            "Match the ABI v1 signatures in docs/PLUGINS.md.",
        )
    })
}

fn read_guest(
    memory: &Memory,
    store: impl wasmtime::AsContext<Data = PluginState>,
    ptr: i32,
    len: i32,
) -> Result<Vec<u8>, TacitusError> {
    let start = usize::try_from(ptr).ok();
    let count = usize::try_from(len).ok();
    let slice = match (start, count) {
        (Some(s), Some(c)) => memory.data(&store).get(s..s + c),
        _ => None,
    };
    slice.map(<[u8]>::to_vec).ok_or_else(|| {
        plugin_abi(
            format!("Guest pointer out of bounds (ptr={ptr}, len={len})."),
            "Return pointers inside the exported linear memory.",
        )
    })
}

fn write_guest(
    memory: &Memory,
    store: &mut Store<PluginState>,
    ptr: i32,
    bytes: &[u8],
) -> Result<(), TacitusError> {
    let start = usize::try_from(ptr).ok();
    let dest = start.and_then(|s| memory.data_mut(&mut *store).get_mut(s..s + bytes.len()));
    match dest {
        Some(dest) => {
            dest.copy_from_slice(bytes);
            Ok(())
        }
        None => Err(plugin_abi(
            format!(
                "tacitus_alloc returned an out-of-bounds pointer (ptr={ptr}, len={}).",
                bytes.len()
            ),
            "Make tacitus_alloc grow memory to fit the requested length.",
        )),
    }
}

/// `tacitus.call` — the one door back into the vault. Reads tool + args from
/// guest memory, dispatches under the manifest allowlist, and writes the
/// envelope back via the guest's own allocator. Errors returned here trap the
/// guest — reserved for broken memory protocol; tool failures are envelopes.
fn host_call(
    mut caller: Caller<'_, PluginState>,
    tool_ptr: i32,
    tool_len: i32,
    args_ptr: i32,
    args_len: i32,
) -> Result<i64, wasmtime::Error> {
    let memory = caller
        .get_export(abi::EXPORT_MEMORY)
        .and_then(|e| e.into_memory())
        .ok_or_else(|| wasmtime::Error::msg("guest has no exported memory"))?;

    let read = |caller: &Caller<'_, PluginState>,
                ptr: i32,
                len: i32|
     -> Result<String, wasmtime::Error> {
        let start = usize::try_from(ptr).map_err(|_| wasmtime::Error::msg("negative pointer"))?;
        let count = usize::try_from(len).map_err(|_| wasmtime::Error::msg("negative length"))?;
        let bytes = memory
            .data(caller)
            .get(start..start + count)
            .ok_or_else(|| wasmtime::Error::msg("pointer out of bounds"))?;
        String::from_utf8(bytes.to_vec()).map_err(|_| wasmtime::Error::msg("non-UTF-8 string"))
    };
    let tool = read(&caller, tool_ptr, tool_len)?;
    let args_text = read(&caller, args_ptr, args_len)?;

    let envelope = match serde_json::from_str::<Value>(&args_text) {
        Ok(args) => {
            let state = caller.data();
            state
                .registry
                .dispatch(&tool, &args, Some(&state.allowlist))
        }
        Err(e) => crate::err_envelope(&TacitusError::new(
            "INVALID_INPUT",
            format!("tacitus.call args are not valid JSON: {e}."),
            "Pass UTF-8 JSON args to tacitus.call.",
        )),
    };
    let bytes = envelope.to_string().into_bytes();

    let alloc = caller
        .get_export(abi::EXPORT_ALLOC)
        .and_then(|e| e.into_func())
        .ok_or_else(|| wasmtime::Error::msg("guest has no tacitus_alloc"))?
        .typed::<i32, i32>(&caller)?;
    let len = i32::try_from(bytes.len()).map_err(|_| wasmtime::Error::msg("envelope too large"))?;
    let ptr = alloc.call(&mut caller, len)?;
    let start = usize::try_from(ptr).map_err(|_| wasmtime::Error::msg("negative alloc pointer"))?;
    memory
        .data_mut(&mut caller)
        .get_mut(start..start + bytes.len())
        .ok_or_else(|| wasmtime::Error::msg("tacitus_alloc returned out-of-bounds pointer"))?
        .copy_from_slice(&bytes);
    Ok(abi::pack(ptr as u32, len as u32) as i64)
}

/// `tacitus.log` — captured into the store, surfaced via `drain_logs`.
fn host_log(
    mut caller: Caller<'_, PluginState>,
    level: i32,
    ptr: i32,
    len: i32,
) -> Result<(), wasmtime::Error> {
    let memory = caller
        .get_export(abi::EXPORT_MEMORY)
        .and_then(|e| e.into_memory())
        .ok_or_else(|| wasmtime::Error::msg("guest has no exported memory"))?;
    let start = usize::try_from(ptr).map_err(|_| wasmtime::Error::msg("negative pointer"))?;
    let count = usize::try_from(len).map_err(|_| wasmtime::Error::msg("negative length"))?;
    let bytes = memory
        .data(&caller)
        .get(start..start + count)
        .ok_or_else(|| wasmtime::Error::msg("pointer out of bounds"))?;
    let message = String::from_utf8_lossy(bytes).into_owned();
    let level = u8::try_from(level.clamp(0, 3)).unwrap_or(3);
    caller.data_mut().logs.push((level, message));
    Ok(())
}
