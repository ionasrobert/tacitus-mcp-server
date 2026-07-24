;; Returns bytes that are not JSON — the host must answer PLUGIN_ABI, not panic.
(module
  (memory (export "memory") 1)
  (data (i32.const 100) "definitely not json")
  (func (export "tacitus_abi_version") (result i32) (i32.const 1))
  (func (export "tacitus_alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "tacitus_dealloc") (param i32) (param i32))
  (func (export "tacitus_run") (param i32) (param i32) (result i64)
    (i64.or (i64.shl (i64.const 100) (i64.const 32)) (i64.const 19))))
