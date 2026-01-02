# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.8] - 2025-01-03

### Added

- **Lock-stealing cancellation support** (duroxide 0.1.8 API):
  - `ack_orchestration_item` now accepts `cancelled_activities: Vec<ScheduledActivityIdentifier>`
  - Cancelled activities are deleted from worker queue atomically during orchestration ack
  - Enables immediate activity cancellation without waiting for lock expiry

- 5 new provider validation tests for lock-stealing cancellation:
  - `test_cancelled_activities_deleted_from_worker_queue`
  - `test_ack_work_item_fails_when_entry_deleted`
  - `test_renew_fails_when_entry_deleted`
  - `test_cancelling_nonexistent_activities_is_idempotent`
  - `test_batch_cancellation_deletes_multiple_activities`

### Changed

- Updated to duroxide 0.1.8 from crates.io
- `ack_orchestration_item` stored procedure now handles `p_cancelled_activities` JSONB parameter
- Simplified migration approach: single `0001_initial_schema.sql` as source of truth

### Removed

- Migration delta scripts (`0002_*.sql`, `*_diff.md`) - not needed for single-schema approach
- `generate_migration_diff.sh` script

### Notes

- Total validation tests: 82 (77 + 5 lock-stealing)
- Requires duroxide 0.1.8+ for lock-stealing cancellation support

## [0.1.7] - 2024-12-29

### Added

- **Cooperative cancellation support** (duroxide 0.1.7 API):
  - `fetch_work_item` now returns `ExecutionState` (4th tuple element) indicating orchestration status
  - `renew_work_item_lock` now returns `ExecutionState` instead of `()`:
    - Returns `Running` when lock is successfully extended
    - Returns `Terminal { status }` when orchestration completed/failed (lock NOT extended)
    - Returns `Missing` when execution record doesn't exist (lock NOT extended)
  - `ack_work_item` now accepts `Option<WorkItem>`:
    - `Some(completion)` - enqueue completion to orchestrator queue
    - `None` - just delete worker item (for terminal/missing orchestrations)

- 9 new provider validation tests for cancellation support:
  - `test_fetch_returns_running_state_for_active_orchestration`
  - `test_fetch_returns_terminal_state_when_orchestration_completed`
  - `test_fetch_returns_terminal_state_when_orchestration_failed`
  - `test_fetch_returns_terminal_state_when_orchestration_continued_as_new`
  - `test_fetch_returns_missing_state_when_instance_deleted`
  - `test_renew_returns_running_when_orchestration_active`
  - `test_renew_returns_terminal_when_orchestration_completed`
  - `test_renew_returns_missing_when_instance_deleted`
  - `test_ack_work_item_none_deletes_without_enqueue`

- 5 new long-polling validation tests

### Changed

- Updated to duroxide 0.1.7 from crates.io
- `renew_work_item_lock` stored procedure now checks execution status BEFORE extending lock
  - Per provider contract: lock only extended when orchestration is Running
  - Terminal/Missing states return status without extending lock
- `ack_worker` stored procedure now accepts NULL completion (no enqueue, just delete)

### Removed

- Connection pre-warming workaround (duroxide #32 fixed in v0.1.7)
- STOPGAP comments for resolved upstream issues (#31, #32, #34)

### Notes

- Total validation tests: 86 (77 + 9 cancellation)
- Requires duroxide 0.1.7+ for cooperative cancellation support

## [0.1.6] - 2025-12-22

### Changed

- Code cleanup: removed unused `#[allow(dead_code)]` and feature gates from tests

### Fixed

- Fixed clippy warnings (empty line after doc comment, type complexity, manual Range::contains)
- Applied cargo fmt formatting fixes

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

