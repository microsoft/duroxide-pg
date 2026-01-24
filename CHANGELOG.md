# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.14] - 2026-01-24

### Changed

- Update to duroxide 0.1.13
- `utcnow()` renamed to `utc_now()` in test files (breaking API change in duroxide)

### Notes

- Total validation tests: 135 (unchanged)
- duroxide 0.1.13 reimplements system calls as regular activities, simplifying replay

## [0.1.13] - 2026-01-24

### Changed

- **BREAKING:** Update to duroxide 0.1.12 API with simplified future handling
- `DurableFuture` now implements `Future` directly - no more `.into_activity()`, `.into_timer()`, etc.
- `Runtime::start_with_store` and `start_with_options` now take `ActivityRegistry` directly (not `Arc<ActivityRegistry>`)
- `ctx.select(vec![...])` replaced with `ctx.select2(f1, f2)` returning `Either2<T1, T2>`
- `ctx.join(...)` now returns `Vec<T>` directly instead of `Vec<DurableOutput>`

### Notes

- Total validation tests: 99 (unchanged)
- All 25 e2e sample tests passing
- All 2 regression tests passing

## [0.1.12] - 2026-01-06

### Fixed

- Fix migration system to handle function signature changes
  - Add `DROP FUNCTION IF EXISTS` before all `CREATE OR REPLACE FUNCTION` statements
  - PostgreSQL cannot replace functions when return type changes without explicit DROP
  - Affected migrations: 0002 (13 functions), 0010 (4 functions)

### Notes

- Total validation tests: 135 (unchanged)
- This fix resolves test failures when running against existing public schema

## [0.1.11] - 2026-01-05

### Changed

- **BREAKING:** Update to duroxide 0.1.9
- `get_instance_info` now returns `parent_instance_id` field for sub-orchestration hierarchy

### Added

- New migration `0010_add_deletion_and_pruning_support.sql`:
  - Adds `parent_instance_id` column to `instances` table
  - 5 new stored procedures: `list_children`, `get_parent_id`, `delete_instances_atomic`, `prune_executions`, `get_instance_info` (updated)
- 6 new ProviderAdmin methods for Management API:
  - `list_children(instance_id)` - List direct child sub-orchestrations
  - `get_parent_id(instance_id)` - Get parent instance ID
  - `delete_instances_atomic(ids, force)` - Atomic batch deletion with cascade
  - `delete_instance_bulk(filter)` - Bulk delete with filters
  - `prune_executions(instance_id, options)` - Prune old executions
  - `prune_executions_bulk(filter, options)` - Bulk prune across instances
- 19 new provider validation tests:
  - 12 deletion tests (cascade delete, hierarchy, atomic operations)
  - 3 prune tests (options, safety, bulk)
  - 4 bulk deletion tests (filters, limits, cascading)

### Notes

- Total validation tests: 99 (up from 80)
- All pruning operations now raise error for non-existent instances (matches duroxide semantics)
- Parent/child relationships tracked via `parent_instance_id` column

## [0.1.10] - 2026-01-02

### Changed

- **BREAKING:** Update to duroxide 0.1.8 (crates.io release)
- Remove `ExecutionState` from provider API (was experimental in 0.1.7, removed in 0.1.8)
  - `fetch_work_item` now returns `(WorkItem, String, u32)` (removed 4th column)
  - `renew_work_item_lock` now returns `()` instead of `ExecutionState`
- Add activity cancellation support via lock stealing
  - `ack_orchestration_item` now accepts `cancelled_activities` parameter
  - Worker queue items now track `instance_id`, `execution_id`, `activity_id` for cancellation lookup

### Added

- New migration `0008_remove_execution_state.sql` - removes ExecutionState from stored procedures
- New migration `0009_add_activity_cancellation_support.sql` - adds lock stealing support
- New script `scripts/generate_migration_diff.sh` - auto-generates migration diff markdown files
- 5 new lock-stealing validation tests:
  - `test_cancelled_activities_deleted_from_worker_queue`
  - `test_ack_work_item_fails_when_entry_deleted`
  - `test_renew_fails_when_entry_deleted`
  - `test_cancelling_nonexistent_activities_is_idempotent`
  - `test_batch_cancellation_deletes_multiple_activities`
- 3 new long-polling validation tests

### Fixed

- All timestamps in migration 0009 now use Rust-provided time (`v_now_ts`) instead of database `NOW()`

### Notes

- Total validation tests: 80 (up from 72)
- Switched duroxide dependency from git to crates.io
- Migration diff files are now required for all schema changes

## [0.1.9] - 2025-12-28

### Added

- Activity cancellation support (duroxide 0.1.7)
  - `fetch_work_item` now returns `ExecutionState` as fourth column
  - `renew_work_item_lock` now returns `ExecutionState` (was `void`)
  - `ack_work_item` now accepts `Option<WorkItem>` to support cancellation without enqueue
- New migration `0007_add_execution_state_support.sql`
- 9 new provider validation tests for cancellation:
  - `test_fetch_returns_running_state_for_active_orchestration`
  - `test_fetch_returns_terminal_state_when_orchestration_completed`
  - `test_fetch_returns_terminal_state_when_orchestration_failed`
  - `test_fetch_returns_terminal_state_when_orchestration_continued_as_new`
  - `test_fetch_returns_missing_state_when_instance_deleted`
  - `test_renew_returns_running_when_orchestration_active`
  - `test_renew_returns_terminal_when_orchestration_completed`
  - `test_renew_returns_missing_when_instance_deleted`
  - `test_ack_work_item_none_deletes_without_enqueue`

### Changed

- Updated to duroxide 0.1.7
- `parse_execution_state` helper added to convert database strings to `ExecutionState` enum

### Notes

- Total validation tests: 72 (up from 63)
- ExecutionState values: `Running`, `Terminal:<status>`, `Missing`
- Enables runtime to detect orchestration state changes during long-running activities

## [0.1.8] - 2025-12-19

### Fixed

- Fix timestamp consistency issue causing intermittent test failures
  - All stored procedures now receive timestamps from Rust (`p_now_ms`) instead of using database `NOW()`
  - Eliminates clock skew and precision mismatch between application and database time
  - Affected procedures: `enqueue_worker_work`, `abandon_work_item`, `abandon_orchestration_item`, `ack_worker`, `ack_orchestration`, `enqueue_orchestration_work`, `enqueue_completion`, `enqueue_message`

### Added

- New migration `0006_use_rust_timestamps.sql` - updates all stored procedures to use Rust-provided timestamps

### Notes

- Total validation tests: 93 (25 basic + 63 provider + 2 regression + 3 doc tests)
- This fix resolves timing-related failures that varied by database latency

## [0.1.7] - 2025-12-19

### Added

- Worker queue visibility control via `visible_at` column (duroxide 0.1.5)
  - Added `visible_at` column to `worker_queue` table
  - `fetch_work_item` now checks `visible_at <= now` in addition to lock status
  - `abandon_work_item` with delay now sets `visible_at` instead of keeping `locked_until`
  - Cleaner semantics: `visible_at` controls visibility, `locked_until` only for lock expiry
- New migration `0005_add_visible_at_to_worker_queue.sql`
- 2 new provider validation tests:
  - `test_worker_item_immediate_visibility` - Verify newly enqueued items are immediately visible
  - `test_worker_delayed_visibility_skips_future_items` - Verify items with future visible_at are skipped

### Changed

- Updated to duroxide 0.1.5

### Notes

- Total validation tests: 64 (up from 62)

## [0.1.6] - 2024-12-13

### Added

- `name()` method on Provider trait - returns "duroxide-pg"
- `version()` method on Provider trait - returns crate version
- Long-polling design document (`docs/LONG_POLLING_DESIGN.md`)

### Changed

- Updated to duroxide 0.1.4

### Notes

- Long-polling is a design document only; implementation pending

## [0.1.5] - 2024-12-14

### Fixed

- Added migration 0004 to update stored procedures for existing databases
  - 0.1.4 only updated migration 0002 which doesn't run on existing databases
  - This migration recreates procedures with attempt_count support

## [0.1.4] - 2024-12-14

### Added

- 3 new provider validation tests from duroxide 0.1.3:
  - `test_abandon_work_item_releases_lock` - Verify abandon_work_item releases lock immediately
  - `test_abandon_work_item_with_delay` - Verify abandon_work_item with delay defers refetch
  - `max_attempt_count_across_message_batch` - Verify MAX attempt_count returned for batched messages

### Fixed

- `abandon_work_item` with delay now correctly keeps lock_token to prevent immediate refetch
  (matches SQLite provider behavior from duroxide 0.1.3)
- `abandon_orchestration_item` without delay no longer updates visible_at
  (was causing timing issues where messages became temporarily invisible)

### Notes

- Total validation tests: 61 passing
- Updated to duroxide 0.1.3 from crates.io

## [0.1.3] - 2024-12-14

### Changed

- **BREAKING:** Updated to duroxide 0.1.2 API with poison message handling
- `fetch_orchestration_item` now returns `(OrchestrationItem, String, u32)` tuple (lock_token and attempt_count moved to tuple)
- `fetch_work_item` now returns `(WorkItem, String, u32)` tuple (added attempt_count)
- `abandon_orchestration_item` now requires `ignore_attempt: bool` parameter
- `OrchestrationItem` no longer contains `lock_token` field (moved to return tuple)

### Added

- New migration `0003_add_attempt_count.sql` - adds `attempt_count` column to queue tables
- `abandon_work_item()` method - explicit work item lock release with delay and ignore_attempt support
- `renew_orchestration_item_lock()` method - extends orchestration lock timeout for long-running turns
- 8 new poison message validation tests:
  - `orchestration_attempt_count_starts_at_one`
  - `orchestration_attempt_count_increments_on_refetch`
  - `worker_attempt_count_starts_at_one`
  - `worker_attempt_count_increments_on_lock_expiry`
  - `attempt_count_is_per_message`
  - `abandon_work_item_ignore_attempt_decrements`
  - `abandon_orchestration_item_ignore_attempt_decrements`
  - `ignore_attempt_never_goes_negative`

### Notes

- Total validation tests: 58 (up from 50)
- Poison message detection is automatic in duroxide runtime when `attempt_count` exceeds `max_attempts` (default: 10)

## [0.1.2] - 2024-12-10

### Changed

- Updated to duroxide 0.1.1 API
- `fetch_orchestration_item` now accepts `poll_timeout: Duration` parameter (for long-polling support)
- `fetch_work_item` now accepts `poll_timeout: Duration` parameter (for long-polling support)
- Updated test configurations to use `dispatcher_min_poll_interval` (renamed from `dispatcher_idle_sleep`)
- Updated tests to use new `continue_as_new()` awaitable API (`return ctx.continue_as_new(input).await`)

### Notes

- The `test_worker_lock_renewal_extends_timeout` validation test may fail with high-latency database connections (>200ms round-trip). This is a timing-sensitive test from the duroxide validation suite, not a provider bug.

## [0.1.1] - 2024-12-09

### Added

- Initial release on crates.io
- Full implementation of `Provider` trait for PostgreSQL
- Full implementation of `ProviderAdmin` trait for management/observability
- Atomic stored procedures for all provider operations
- Instance-level locking with advisory locks
- Worker queue with lock renewal support
- Multi-execution support for continue-as-new
- Comprehensive test suite with 50+ validation tests

