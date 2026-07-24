# Comparison: Turso Simulator vs proptest-state-machine

This document compares the custom property-testing framework in `testing/simulator/` with [proptest-state-machine](https://proptest-rs.github.io/proptest/proptest/state-machine.html).

## Core Architecture Differences

| Aspect | Turso Simulator | proptest-state-machine |
|--------|-----------------|------------------------|
| **Focus** | Database-specific deterministic simulation | General-purpose state machine testing |
| **Execution Model** | Sequential + multi-connection (pseudo-concurrent) | Sequential only (concurrent planned) |
| **Oracle Strategy** | Triple oracle (shadow state, differential, doublecheck) | Single reference model |
| **Shrinking** | Domain-aware heuristic + brute-force | Generic transition deletion/shrinking |

## Unique Turso Simulator Capabilities

These features would be non-trivial to implement on top of proptest-state-machine.

### 1. I/O Fault Injection with Deterministic Replay

The simulator has a sophisticated I/O abstraction layer (`runner/io.rs`) that can:
- Inject faults on read/write/fsync operations with configurable probability
- Simulate latency with deterministic timing via simulated clock
- Swap between real filesystem and in-memory backends

```rust
pub struct FaultProfile {
    pub read: bool,     // Fault on reads
    pub write: bool,    // Fault on writes
    pub sync: bool,     // Fault on fsync
}
```

This is critical for testing database durability guarantees. proptest-state-machine has no concept of I/O injection—you'd need to build an entire deterministic I/O layer.

### 2. Differential Testing Oracle

The simulator runs the same test plan against both Turso and SQLite (via rusqlite), comparing results row-by-row. This catches semantic deviations that a reference model might miss because it uses the *actual production SQLite* as ground truth.

proptest-state-machine only supports a hand-written reference model. Integrating a second implementation as oracle would require significant custom plumbing.

### 3. Domain-Aware Shrinking

The shrinking strategy understands SQL semantics:
- Identifies tables used in failing queries
- Traces table renames backward through the plan
- Removes only properties dependent on affected tables
- Preserves causal dependencies between DDL/DML

proptest shrinking is generic (delete transitions, shrink values). Adding SQL-aware shrinking would require implementing the dependency analysis from scratch.

### 4. Shadow State with Transaction Snapshots

The simulator maintains per-connection transaction snapshots:

```rust
enum TransactionTables {
    Read(HashMap<String, ShadowTable>),
    Concurrent(HashMap<String, ShadowTable>),
    Write(HashMap<String, ShadowTable>),
}
```

This enables verification of isolation levels, rollback behavior, and MVCC semantics. proptest-state-machine's `ReferenceStateMachine` is a single state—modeling concurrent transactions with isolation would require complex manual bookkeeping.

### 5. Property-Guided Generation with Filters

Properties can constrain intermediate interactions:

```rust
// InsertValuesSelect property filters out mutations to the target table
fn filter(interaction: &Interaction) -> bool {
    !interaction.mutates_table(&target_table)
}
```

This ensures generated plans don't violate property assumptions. proptest-state-machine's `preconditions()` can reject transitions, but there's no mechanism for properties to dynamically filter generation mid-plan.

### 6. Bug Database with Seed Reproduction

The simulator persists failing seeds in a bug database (`runner/bugbase.rs`), enabling:
- Regression testing via `--test` flag
- Automatic deduplication of equivalent failures
- Minimal reproduction storage

proptest saves failure seeds in files, but has no centralized bug tracking or deduplication.

## What proptest-state-machine Does Better

- **Simpler API**: Two traits vs. an entire framework
- **General Purpose**: Works for any stateful system, not just databases
- **Mature Shrinking**: Well-tested generic shrinking with `complicate` for undoing failed shrinks
- **Integration**: Part of the proptest ecosystem with good documentation

## Potential Adaptations from proptest-state-machine

Features worth adopting from proptest-state-machine:

### 1. Formalized Preconditions Trait

proptest-state-machine has explicit `preconditions(state, transition) -> bool` that runs before each transition attempt. The Turso simulator currently handles this ad-hoc through property filters and scattered transaction state checks.

**Proposed adaptation**:

```rust
trait Precondition {
    fn is_valid(&self, env: &SimulatorEnv) -> bool;
}

impl Precondition for InteractionType {
    fn is_valid(&self, env: &SimulatorEnv) -> bool {
        match self {
            Query(q) if q.is_write() => !env.has_pending_read_txn(),
            Query(q) if q.references_table(t) => env.table_exists(t),
            // ...
        }
    }
}
```

This would reduce invalid plan generation and make constraints explicit/testable.

### 2. Shrinking with Complicate (Undo)

proptest's shrinking can *undo* failed shrink attempts via `complicate()`. The Turso simulator's shrinking is one-directional—once an interaction is removed, there's no mechanism to restore it if the shrink was overly aggressive.

**Proposed adaptation**:

```rust
struct ShrinkState {
    current: InteractionPlan,
    history: Vec<ShrinkStep>,  // Enable undo
}

enum ShrinkStep {
    Removed(usize, Interaction),
    Truncated(usize),
}
```

This could find smaller reproductions when the current heuristic over-shrinks.

### 3. Configurable Transition Counts

proptest-state-machine's test macro accepts ranges: `sequential 1..20`. The Turso simulator uses `--maximum-size` but doesn't have built-in support for *minimum* sizes or range-based generation.

**Proposed adaptation**: Add `--minimum-size` flag:

```rust
struct PlanConfig {
    min_interactions: usize,  // Currently missing
    max_interactions: usize,
}
```

### 4. Explicit Invariant Checks

proptest-state-machine separates `apply()` (execute + post-condition) from `check_invariants()` (properties that must hold in *every* state). The Turso simulator mixes these—assertions are part of properties, and integrity checks only run at the end.

**Proposed adaptation**:

```rust
trait Invariant {
    fn check(&self, env: &SimulatorEnv) -> Result<(), String>;
}

// Run after each interaction
fn execute_with_invariants(interaction: &Interaction, env: &mut SimulatorEnv) {
    execute(interaction, env)?;
    for invariant in &env.invariants {
        invariant.check(env)?;  // Fail fast on any violation
    }
}
```

Currently, `PRAGMA integrity_check` only runs at completion. Invariants could catch corruption earlier.

### 5. Strategy-Based Initial State Generation

proptest-state-machine uses `init_state() -> impl Strategy<Value = State>` to generate diverse initial states. The Turso simulator always starts with an empty database and generates the first table.

**Proposed adaptation**:

```rust
enum InitialState {
    Empty,
    Snapshot(PathBuf),           // Load from file
    Generated { tables: usize }, // Generate N tables with data
}
```

This could find bugs that only manifest with specific schema shapes or data distributions.

### 6. Teardown Hook

proptest-state-machine has `teardown(state)` for cleanup. The simulator doesn't have explicit teardown. An explicit hook could:
- Verify final state consistency
- Collect metrics
- Archive artifacts for debugging

## Summary: Adaptation Priority

| Feature | Effort | Value | Priority |
|---------|--------|-------|----------|
| Formalized preconditions | Medium | High | **High** |
| Per-interaction invariants | Low | High | **High** |
| Shrink undo (complicate) | Medium | Medium | Medium |
| Strategy-based init states | Medium | Medium | Medium |
| Min/max plan size range | Low | Low | Low |
| Teardown hook | Low | Low | Low |

## Conclusion

The Turso simulator is a **domain-specific deterministic simulation framework** optimized for database testing. Its I/O fault injection, differential testing, and SQL-aware shrinking are capabilities that would take substantial effort to build on top of proptest-state-machine.

However, proptest-state-machine's formalized preconditions and per-interaction invariants would improve the simulator's rigor with modest implementation effort and should be considered for adoption.
