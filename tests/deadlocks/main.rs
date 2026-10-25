// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Deadlock regression suite for the PostgreSQL provider.
//!
//! Each test here reproduces one way two provider calls can lock the same rows
//! in opposite orders, or interleave where they must serialize, and fails if
//! PostgreSQL detects a deadlock or the calls do not serialize. Run the
//! suite alone with `cargo test --test deadlocks`. It also runs in every
//! default `cargo test`, which is what CI runs.
//!
//! Rules for this suite:
//!
//! - Every test runs by default. No test carries the ignore attribute, and
//!   `suite_has_no_ignored_tests` fails the build's test run if one does.
//! - Every wait is bounded (`STEP_TIMEOUT`, `WAIT_BUDGET`, statement
//!   timeouts), so a regression fails instead of hanging.
//! - Assert SQLSTATE 40P01 directly. Send the statement the provider sends,
//!   once, so a deadlock fails the test as a deadlock; do not rely on a retry
//!   or on eventual success, which can hide one.
//! - Race tests run 200 rounds by default. Set `RACE_ROUNDS` for a soak, for
//!   example `RACE_ROUNDS=10000 cargo test --test deadlocks race_ -- --nocapture`.
//!   Each prints a `RACEJSON` line with its tally.
//! - Every lock-ordering or deadlock fix adds its test here.
//!
//! Modules:
//!
//! - `ack_fetch`: acknowledge against a fetch that sees the lease as expired
//!   (migration 0026).
//! - `renew_fetch`: renewal against fetch, forced and raced (migration 0026).
//! - `renew_ack_abandon`: renewal against acknowledge and abandon.
//! - `victim_retry`: the provider retries a renewal aborted as a deadlock
//!   victim.
//! - `harness`: shared helpers, also used by
//!   `tests/orchestration_lock_renewal_tests.rs` and
//!   `tests/orchestration_ack_lease_tests.rs`.

mod ack_fetch;
mod harness;
mod renew_ack_abandon;
mod renew_fetch;
mod victim_retry;

/// Every source file of the suite, checked by `suite_has_no_ignored_tests`.
const SOURCES: &[(&str, &str)] = &[
    ("main.rs", include_str!("main.rs")),
    ("ack_fetch.rs", include_str!("ack_fetch.rs")),
    ("harness.rs", include_str!("harness.rs")),
    ("renew_ack_abandon.rs", include_str!("renew_ack_abandon.rs")),
    ("renew_fetch.rs", include_str!("renew_fetch.rs")),
    ("victim_retry.rs", include_str!("victim_retry.rs")),
];

/// Deadlock tests must run in every default `cargo test`. Fails if any file of
/// the suite ignores a test, or if a file was added without being checked.
#[test]
fn suite_has_no_ignored_tests() {
    // Built from two parts so this file does not match itself.
    let marker = concat!("#[", "ignore");
    for (file, source) in SOURCES {
        assert!(
            !source.contains(marker),
            "tests/deadlocks/{file} ignores a test; deadlock tests must run in every cargo test"
        );
    }

    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/deadlocks");
    for entry in std::fs::read_dir(&dir).expect("failed to list tests/deadlocks") {
        let name = entry
            .expect("failed to read a tests/deadlocks entry")
            .file_name()
            .into_string()
            .expect("non-UTF-8 file name in tests/deadlocks");
        if name.ends_with(".rs") {
            assert!(
                SOURCES.iter().any(|(file, _)| *file == name),
                "tests/deadlocks/{name} is not listed in SOURCES in tests/deadlocks/main.rs"
            );
        }
    }
}
