# Source map

Each TLA+ action in `LeaseProtocol.tla`, mapped to the SQL or Rust it models.
Paths without a prefix are in `microsoft/duroxide-pg`. "Core" means
`microsoft/duroxide` v0.1.32 (`585c89c`).

Line numbers are pinned to these commits:

| Variant | Commit |
|---|---|
| `MAIN` | `3beba92048b9fd6c9e68262a3e3fa192441c1e31` |
| `PR43` | `b2ff29633a5ea437f8f1e737429d7ba9170ba491` |
| `FIXED` | `ea598d91839b6d656917a47a3b4e772fa515b9aa` |

The executable `DO $$` block of migration `0026` is byte-identical in later
commits of the same change. Only the header comment grew, by one line.

| TLA+ action | SQL/Rust source at that SHA |
| --- | --- |
| Begin / Arrival | PR43 `src/provider.rs:1000-1024,1320-1334,1937-1947`; Rust samples raw time before pool acquisition; function entry captures PostgreSQL statement time. |
| FetchCandidate | `migrations/0025_fetch_orchestration_lease_at_claim.sql:88-96`; visible row and raw-clock lease-expiry predicate. |
| Advisory (fetch) | Same `0025:103-104`. |
| FetchQueueSelect | Same `0025:127-139`; repeated availability check and queue `FOR UPDATE SKIP LOCKED`. |
| FetchComputeLease | Same `0025:146-151`; max(caller + elapsed, DB) + lease. |
| FetchInstance | Same `0025:153-165`; insert/upsert token, conditional on raw-clock expiry. |
| FetchQueueUpdate / FetchMessages | Same `0025:168-181`; queue takeover and own-write-visible input read. |
| FetchHistory | Same `0025:189-201`; new committed snapshot, no history row lock. |
| Lookup (old ack) | `migrations/0020_add_kv_delta.sql:73-80`; plain token/expiry SELECT. |
| AckHistory | Same `0020:91-126`; metadata row serialization and history unique-key INSERT. |
| AckOutputs | Same `0020:221-291`; durable work enqueues and other atomic output changes. |
| ReleaseQueue / ReleaseInstance (old ack) | Same `0020:294-299`; token-matching DELETEs with no count check. |
| Lookup / ReleaseQueue / ReleaseInstance (abandon) | `migrations/0006_use_rust_timestamps.sql:101-107,141-149`; token-only SELECT, queue clear then lock delete. |
| MainRenewInstance / RenewQueue | MAIN `migrations/0004_update_stored_procedures_for_attempt_count.sql:349-364`; raw-time deadline, instance-before-queue order. |
| Lookup / Advisory (new renew) | PR43 `migrations/0026_serialize_orchestration_lock_mutations.sql:53-62`. |
| RenewSampleNow / RenewRecheck | Same `0026:64-78`; clock assignment and subsequent token/expiry SELECT are distinct steps. |
| RenewQueue / RenewInstance (new renew) | Same `0026:80-93`; queue then instance, zero instance UPDATE count raises. |
| Lookup / Advisory / AckRecheck (fixed ack) | FIXED `migrations/0026_serialize_orchestration_lock_mutations.sql:153-178`; initial expiry check, advisory lock, token-only recheck. |
| AckHistory / AckOutputs (fixed ack) | Same `0026:189-224,319-389`. |
| ReleaseQueue / ReleaseInstance (fixed ack) | Same `0026:392-405`; queue delete is unchecked; instance delete is checked. |
| Commit / Rollback | Each provider call is one autocommit SQL statement. Procedure exceptions roll back its writes and release transaction locks. |
| SingleWriter | `migrations/0001_initial_schema.sql:29-37`; primary key on instance/execution/event. |
| Core atomicity contract | Core `src/providers/mod.rs:1278-1328`. |
| Core cancellation | Core `docs/provider-implementation-guide.md:342-361`; a cancelled await is not proof of rollback. |
| Renewal start guard tick >= R | Core `src/runtime/dispatchers/orchestration.rs:262-321`; default 5 s lease yields ceil(2.5) = 3 s renewal interval. Core `src/runtime/mod.rs:359-362`; activity default 30 s / 25 s renewal. |
| Ack failures / retries | Core `src/runtime/dispatchers/orchestration.rs:989-1056,1200-1249`; storage failure path is not a message deduplicator. PR43 `src/provider.rs:589-621,1353-1384`; 23505 is permanent, 40P01 retryable. |
| NoDuplicateConsumption | Core `src/runtime/replay_engine.rs:131-183,464-486`; ExternalRaised and QueueMessage are not deduplicated. |
