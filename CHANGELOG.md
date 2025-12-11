# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.2] - 2024-12-10

### Changed

- Updated to duroxide 0.1.1 API
- `fetch_orchestration_item` now accepts `poll_timeout: Duration` parameter (for long-polling support)
- `fetch_work_item` now accepts `poll_timeout: Duration` parameter (for long-polling support)
- Updated test configurations to use `dispatcher_min_poll_interval` (renamed from `dispatcher_idle_sleep`)

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

