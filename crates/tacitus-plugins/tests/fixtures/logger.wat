;; Emits one tacitus.log line, then returns a minimal ok envelope.
(module
  (import "tacitus" "log" (func $log (param i32 i32 i32)))
  (memory (export "memory") 1)
  (data (i32.const 100) "hello from guest")
  (data (i32.const 200) "{\"ok\":true,\"data\":null}")
  (func (export "tacitus_abi_version") (result i32) (i32.const 1))
  (func (export "tacitus_alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "tacitus_dealloc") (param i32) (param i32))
  (func (export "tacitus_run") (param i32) (param i32) (result i64)
    (call $log (i32.const 1) (i32.const 100) (i32.const 16))
    (i64.or (i64.shl (i64.const 200) (i64.const 32)) (i64.const 23))))
