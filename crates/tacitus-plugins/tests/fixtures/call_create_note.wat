;; Calls tacitus.call("create_note", {...}) — a WRITE tool — and returns the
;; host's envelope. Only succeeds under a read-write manifest.
(module
  (import "tacitus" "call" (func $call (param i32 i32 i32 i32) (result i64)))
  (memory (export "memory") 2)
  (data (i32.const 100) "create_note")
  (data (i32.const 120) "{\"note_id\":\"notes/from-guest\",\"content\":\"hello\"}")
  (global $heap (mut i32) (i32.const 65536))
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
  (func (export "tacitus_run") (param i32) (param i32) (result i64)
    (call $call (i32.const 100) (i32.const 11) (i32.const 120) (i32.const 48))))
