//! hello-tacitus — the reference Tacitus WASM plugin.
//!
//! Implements ABI v1 by hand (no SDK — the point is that the ABI is small):
//! five exports, plus the two `tacitus` imports. Reads a query from its input,
//! calls the `search` tool through the sandbox, and returns a digest of the
//! top hits as a `{ ok, data }` envelope.

use std::alloc::{alloc as raw_alloc, dealloc as raw_dealloc, Layout};

use serde_json::{json, Value};

#[link(wasm_import_module = "tacitus")]
extern "C" {
    fn call(tool_ptr: i32, tool_len: i32, args_ptr: i32, args_len: i32) -> i64;
    fn log(level: i32, ptr: i32, len: i32);
}

#[no_mangle]
pub extern "C" fn tacitus_abi_version() -> i32 {
    1
}

#[no_mangle]
pub extern "C" fn tacitus_alloc(len: i32) -> i32 {
    let Ok(layout) = Layout::from_size_align(len.max(1) as usize, 1) else {
        return 0;
    };
    unsafe { raw_alloc(layout) as i32 }
}

#[no_mangle]
pub extern "C" fn tacitus_dealloc(ptr: i32, len: i32) {
    if ptr == 0 || len <= 0 {
        return;
    }
    let Ok(layout) = Layout::from_size_align(len as usize, 1) else {
        return;
    };
    unsafe { raw_dealloc(ptr as *mut u8, layout) }
}

/// Call a host tool and parse the `{ ok, data | error }` envelope it returns.
fn call_tool(tool: &str, args: &Value) -> Value {
    let args_text = args.to_string();
    let packed = unsafe {
        call(
            tool.as_ptr() as i32,
            tool.len() as i32,
            args_text.as_ptr() as i32,
            args_text.len() as i32,
        )
    };
    let ptr = (packed >> 32) as u32 as i32;
    let len = (packed & 0xFFFF_FFFF) as u32 as i32;
    let envelope = unsafe {
        let bytes = std::slice::from_raw_parts(ptr as *const u8, len as usize);
        serde_json::from_slice(bytes).unwrap_or(Value::Null)
    };
    // The host wrote into our allocator; the buffer is ours to free.
    tacitus_dealloc(ptr, len);
    envelope
}

fn log_line(level: i32, message: &str) {
    unsafe { log(level, message.as_ptr() as i32, message.len() as i32) }
}

/// Serialize a JSON value into a fresh guest buffer and pack its address.
fn return_value(value: &Value) -> i64 {
    let text = value.to_string();
    let ptr = tacitus_alloc(text.len() as i32);
    unsafe { std::ptr::copy_nonoverlapping(text.as_ptr(), ptr as *mut u8, text.len()) };
    ((ptr as i64) << 32) | (text.len() as i64)
}

#[no_mangle]
pub extern "C" fn tacitus_run(ptr: i32, len: i32) -> i64 {
    let input: Value = unsafe {
        let bytes = std::slice::from_raw_parts(ptr as *const u8, len as usize);
        serde_json::from_slice(bytes).unwrap_or(Value::Null)
    };
    tacitus_dealloc(ptr, len);

    let query = input["input"]["query"].as_str().unwrap_or("launch").to_string();
    log_line(1, &format!("hello-tacitus: searching for {query:?}"));

    let result = call_tool("search", &json!({ "query": query, "top_k": 3 }));
    let envelope = if result["ok"] == true {
        let top: Vec<Value> = result["data"]["hits"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|h| json!({ "note_id": h["note_id"], "title": h["title"], "score": h["score"] }))
            .collect();
        json!({ "ok": true, "data": { "query": query, "top": top } })
    } else {
        // Pass the host's structured error through untouched.
        result
    };
    return_value(&envelope)
}
