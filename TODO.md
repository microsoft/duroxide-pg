# TODO

- AFFANDAR : TODO : verify FI tests, verify perf tests, figure out counters
    - pull in new duroxide create, add visible_at
    - update duroxide-pg as well

- fetch_orchestration_item review as listed below
- remove all dead code masked by allow dead code
- Add `visible_at` to worker queue - see [proposal](docs/WORKER_VISIBLE_AT_PROPOSAL.md)
- Remove connection pool pre-warming from `provider.rs` once duroxide fixes the timing of the `test_multi_threaded_lock_expiration_recovery` validation test


