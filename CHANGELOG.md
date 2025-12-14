# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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

