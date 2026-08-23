# Verify: parse_record lane — VERDICT: CONFIRMED

Tree: /Users/martin/Workspaces/bigdata/turso-parse-record (uncommitted `@`).
STEP 0 identity hit: `core/types.rs:3630: pub fn parse_record(&self) -> Result<Vec<Value>>`.

## Evidence produced in this session

**1. Diff scope — 5 files, no extras.**
`jj diff --stat`: `core/types.rs` (+40/-...), `tests/integration/query_processing/test_cdc_row_image_width.rs`,
`test_ivm_chained_recursive_cte_cdc.rs`, `test_ivm_json_set.rs`, `test_ivm_recursive_cte_update_cdc.rs`.
5 files, 48 insertions / 23 deletions. No stray files.

**2. Caller sweep complete, no silent ignore left.**
`grep -rn 'parse_record' --include='*.rs' . | grep -v /target/` yields exactly 9 hits:
1 definition, 4 in-crate test hits (`.unwrap()` / `.is_err()`), and 4 production-test call
sites, all `.expect("CDC record must decode")`:
- `tests/integration/query_processing/test_ivm_json_set.rs:140`
- `tests/integration/query_processing/test_cdc_row_image_width.rs:38`
- `tests/integration/query_processing/test_ivm_chained_recursive_cte_cdc.rs:181`
- `tests/integration/query_processing/test_ivm_recursive_cte_update_cdc.rs:41`
No `.ok()`, `.unwrap_or_default()`, or `if let Ok(..)`-and-skip anywhere. The two prior
silent-ignore idioms (`if let Some(values)` and `.unwrap_or_default()`) are both gone.

**3. No indirect exposure breaks.** `grep -rn 'DatabaseChange'` shows the only out-of-core
consumer is a type re-export, `sdk-kit/src/rsapi.rs:69` (`pub use turso_core::types::{DatabaseChange, ...}`).
No binding (python/js/java/go/dotnet), sync, extension, simulator, or cli code calls
`parse_record` or wraps it, so the signature change has no FFI surface.

**4. Doc comment matches the impl.** New comment: "Insert, Update and Delete all carry a row
image and parse the same way." The impl's single match arm binds `bin_record` for all three
variants and decodes identically — accurate. The old comment ("None for Delete changes") was
wrong about behavior; that is what was fixed.

**5. Unit tests: 2 new, both pass, nonzero.**
`cargo test -p turso_core parse_record` →
`test result: ok. 2 passed; 0 failed; 0 ignored; 2475 filtered out`
- `types::tests::parse_record_reads_the_row_image_of_a_delete` ... ok
- `types::tests::parse_record_fails_on_a_corrupt_row_image` ... ok

**6. Corrupt-record test has teeth (traced by hand through the decode path).**
Input `vec![0x02, 0x04]`. `values_owned` (core/types.rs:1387) calls `ValueIterator::new`
(core/types.rs:1969): `read_varint` → header_size = 2, header_varint_len = 1. The guard
`header_size > payload.len() || header_varint_len > payload.len() || header_varint_len > header_size`
is `2 > 2 || 1 > 2 || 1 > 2` = false, so construction SUCCEEDS. header_section = `[0x04]`
(serial type 4 = 4-byte integer), data_section = `payload[2..]` = empty. The `Err` therefore
comes from the iterator failing to read 4 data bytes out of 0 — a genuine truncated-payload
decode error, not an empty-record shortcut and not an early header-guard rejection.
The test fails for the right reason.

**7. Integration gate.**
`cd tests && cargo test -p core_tester --test integration_tests -- cdc_row_image_width ivm_json_set ivm_chained_recursive_cte_cdc ivm_recursive_cte_update_cdc`
→ `test result: ok. 26 passed; 0 failed; 0 ignored; 0 measured; 1434 filtered out; finished in 1.13s`
Log: `/private/tmp/claude-501/-Users-martin-Workspaces-bigdata-turso/1b331b13-8d42-46b4-93f9-5b9d74b5a9b2/scratchpad/pr-int.log`

**8. Whole-workspace compile.**
`PYO3_PYTHON=$(which python3.12) cargo check --workspace --all-targets --exclude memory-benchmark`
→ EXIT=0, zero errors.
Log: `.../scratchpad/pr-check3.log`
Two pre-existing environment failures were isolated and are unrelated to this diff:
- without `PYO3_PYTHON`, `pyo3-ffi` build script rejects the default python3.9 (known env blocker).
- `memory-benchmark` fails with `the #[global_allocator] in this crate conflicts with global
  allocator in: turso` — a crate-level allocator clash, no mention of `parse_record`.

**9. Formatting.** `cargo fmt --check` → exit 0, no diff.

## Non-refuting observation (pre-existing, out of scope)

`values_owned` at core/types.rs:1388 is `iter(payload).expect("Failed to create payload iterator")`.
A row image whose header guard fails (e.g. `bin_record = vec![0x09]`, header_size 9 > len 1)
therefore PANICS inside `ValueIterator::new` instead of surfacing as `Err` from
`parse_record`. So `Result` does not cover every corruption shape. This predates the change,
is loud rather than silent, and does not contradict any claim under test — but the "propagates
the decode error" description is only true for corruption detected after iterator construction.

## Verdict

CONFIRMED. Every clause of the claim was checked against evidence produced in this session.
