;; Grows linear memory 1MiB at a time until the host's limiter denies the
;; grow (memory.grow returns -1), then traps. Proves the memory cap holds.
(module
  (memory (export "memory") 1)
  (func (export "tacitus_abi_version") (result i32) (i32.const 1))
  (func (export "tacitus_alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "tacitus_dealloc") (param i32) (param i32))
  (func (export "tacitus_run") (param i32) (param i32) (result i64)
    (block $denied
      (loop $grow
        (br_if $denied (i32.eq (memory.grow (i32.const 16)) (i32.const -1)))
        (br $grow)))
    unreachable))
