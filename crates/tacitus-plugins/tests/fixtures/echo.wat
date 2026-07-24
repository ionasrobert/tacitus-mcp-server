;; Well-behaved ABI v1 guest: bump allocator, echoes its input buffer back as
;; its result — proves the memory protocol in both directions.
(module
  (memory (export "memory") 1)
  (global $heap (mut i32) (i32.const 1024))
  (func (export "tacitus_abi_version") (result i32) (i32.const 1))
  (func (export "tacitus_alloc") (param $len i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $len)))
    (block $done
      (loop $grow
        (br_if $done (i32.le_u (global.get $heap)
                               (i32.mul (memory.size) (i32.const 65536))))
        (drop (memory.grow (i32.const 1)))
        (br $grow)))
    (local.get $ptr))
  (func (export "tacitus_dealloc") (param i32) (param i32))
  (func (export "tacitus_run") (param $ptr i32) (param $len i32) (result i64)
    (i64.or
      (i64.shl (i64.extend_i32_u (local.get $ptr)) (i64.const 32))
      (i64.extend_i32_u (local.get $len)))))
