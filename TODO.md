# TODO

- fix up perf testing prompt and tests
- Connection pool pre-warming in `provider.rs` is still needed - the `test_multi_threaded_lock_expiration_recovery` validation test requires pre-warmed connections to avoid connection-establishment latency affecting lock ordering. This is not a duroxide bug.
- fetch_orchestration_item review as listed below
- remove all dead code masked by allow dead code
- Add `visible_at` to worker queue - see [proposal](docs/WORKER_VISIBLE_AT_PROPOSAL.md)

# DONE

- verify FI tests, verify perf tests, figure out counters


