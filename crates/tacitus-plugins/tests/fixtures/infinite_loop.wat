;; Runaway guest — spins forever; the host's fuel budget must stop it.
(module
  (memory (export "memory") 1)
  (func (export "tacitus_abi_version") (result i32) (i32.const 1))
  (func (export "tacitus_alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "tacitus_dealloc") (param i32) (param i32))
  (func (export "tacitus_run") (param i32) (param i32) (result i64)
    (loop $l (br $l))
    (i64.const 0)))
