# TLA+ model of orchestration leases

This folder contains a TLA+ model of the PostgreSQL provider's orchestration
lease procedures, and TLC configurations that check it. It was written for
migration `0026`.

The model checks one question: can concurrent fetch, acknowledge, renew and
abandon calls, from workers whose clocks disagree, lose ownership of an
instance, process a message twice, or deadlock?

TLC explores every reachable state within fixed bounds. That is strong evidence
for those bounds. It is not a proof for every worker count, message count or
duration, and it does not prove that the Rust and SQL code implement the model.

## Run it

You need Java 17 or newer.

```sh
cd docs/tla/orchestration-leases
./run.sh                                   # all 25 configurations
./run.sh MAIN-consumption FIXED-safety     # selected configurations
```

`run.sh` does the following:

1. It downloads `tla2tools.jar` v1.7.4 (TLC 2.19) and checks its SHA-256. To
   use a local copy, set `TLA2TOOLS_JAR`.
2. It runs TLC with one worker for each configuration.
3. It compares each result with `expected.tsv`.

The script exits non-zero if any result differs. Raw TLC output, including
counterexample traces, goes to `logs/<config>.log`.

The seven largest configurations each take 3 to 6 minutes and need a 4 GiB
heap. All the others finish in seconds. A full run takes about 25 minutes on
one core.

## What is modelled

| Part | In the model |
|---|---|
| `fetch_orchestration_item` | Migration `0025`: candidate query, advisory lock, `FOR UPDATE SKIP LOCKED`, lease computed as `max(caller + elapsed, database) + timeout`, takeover by time, history read |
| `ack_orchestration_item` | Migration `0020` (MAIN, PR43) or `0026` (FIXED): token check, advisory lock and token recheck (FIXED only), history insert, outputs, queue delete, instance-lock delete, and the row-count check on that delete (FIXED only) |
| `renew_orchestration_item_lock` | Migration `0004` (MAIN) or `0026` (PR43, FIXED) |
| `abandon_orchestration_item` | Migration `0006` |
| PostgreSQL | Each statement sees the latest committed state plus its own writes (READ COMMITTED). Row and advisory locks are held until commit or rollback. A blocked update re-evaluates its token predicate. Deadlock detection aborts any one member of a cycle. The history primary key is enforced |
| Callers | Each call is one autocommit statement. The caller reads its clock before waiting for a connection. A call can be cancelled before it commits. A worker's renewal can run at the same time as its own acknowledge |
| Clocks | Each worker has a fixed skew from the database clock, chosen from `-MaxSkew..MaxSkew`. Every combination is checked. Time can advance between any two statements |

Three variants, set by the `Variant` constant, are pinned to commits:

| Variant | Source | Description |
|---|---|---|
| `MAIN` | `3beba92` | Before migration `0026` |
| `PR43` | `b2ff296` | Migration `0026` with the renewal fix only; acknowledge as on `main` |
| `FIXED` | `ea598d9` | Migration `0026` with the renewal and acknowledge fixes. The executable SQL is byte-identical in later commits of the same change |

The comments in the modules cite the SQL line numbers at those commits.
`SOURCE-MAP.md` maps each TLA+ action to its SQL.

## Invariants

| Invariant | Meaning | Source of the requirement |
|---|---|---|
| `ExactlyOnceConsumption` | No committed turn leaves its input messages in the queue | Provider contract: an acknowledge is atomic (`duroxide` `src/providers/mod.rs`, `ack_orchestration_item`) |
| `NoDuplicateConsumption` | No message is reflected in more than one committed turn | Duroxide does not de-duplicate redelivered queue messages or external events (`src/runtime/replay_engine.rs`) |
| `SingleWriter` | No two committed turns start from the same history position | History primary key |
| `AtomicOutputs` | History and new work commit together | Provider contract |
| `NoMessageLoss` | Every message is either still queued or consumed by a committed turn | Provider contract |
| `QueueLeaseAgreement` | Queue rows carry the instance lock's token and deadline | Provider design |
| `NoStaleAck`, `NoStaleRenewal` | An acknowledge or renewal commits only while its token still owns the instance | Provider contract: lock tokens fence ownership |
| `NeverShorten` | A renewal never moves a deadline earlier | Design rule of migration `0026` |
| `NoDeadlock` | No cycle among the calls' row and advisory locks | Operational requirement |

Four stricter clock properties are checked separately. They are not part of the
safety claim:

- `NoExpiredRenewalAtCheck`
- `NoExpiredRenewalAtWrite`
- `RenewalDeadlineLiveAtCommit`
- `FetchDeadlineLiveAtCommit`

## Results

TLC 2.19, one worker. Bounds for the main profiles:

- 2 workers and 1 instance.
- 2 messages in one batch.
- Lease `L = 2` ticks and renewal interval `R = 1` tick.
- `MaxSkew = 2`, so a clock can be off by a whole lease.
- A horizon of 3 ticks.

The exact bounds of each configuration are in `configs/`.

| Variant | Exactly-once consumption | Renewal fenced by token | Acknowledge fenced by token | No deadlock (one instance) | Never shorten |
|---|---|---|---|---|---|
| `MAIN` | violated | holds | not checked ¹ | violated: renewal with fetch, acknowledge and abandon | violated |
| `PR43` | violated | holds | not checked ¹ | holds | holds |
| `FIXED` | **holds** | **holds** | **holds** | **holds** | **holds** |

¹ The consumption counterexample is itself an acknowledge that commits after its
token was replaced.

Exhaustive passes for `FIXED`:

| Configuration | Calls checked | Distinct states | Depth |
|---|---|---:|---:|
| `FIXED-safety` | acknowledge, fetch then acknowledge, renewal | 3,854,292 | 39 |
| `FIXED-abandon` | abandon, fetch then acknowledge, renewal | 4,227,695 | 40 |
| `FIXED-redelivery` | acknowledge, then two later fetches with acknowledge | 286,344 | 45 |
| `FIXED-release-race` | acknowledge and abandon on the same token | 1,894 | 18 |

Shortest counterexamples on the earlier variants:

| Configuration | Invariant | Trace |
|---|---|---|
| `MAIN-consumption`, `PR43-consumption` | `ExactlyOnceConsumption` | 20 states: the acknowledge passes its token check, a fetch whose clock runs ahead takes over and commits, then the acknowledge's deletes match no row and it commits |
| `MAIN-zero-skew-consumption` | `ExactlyOnceConsumption` | 22 states, no skew: the lease expires during the acknowledge |
| `MAIN-redelivery`, `PR43-redelivery` | `NoDuplicateConsumption` | 45 states: a later fetch consumes the same messages a second time |
| `MAIN-release-race`, `PR43-release-race` | `ExactlyOnceConsumption` | 15 states: an abandon with the same token commits in the middle of the acknowledge |
| `MAIN-deadlock-fetch`, `-ack`, `-abandon` | `NoDeadlock` | 9 to 11 states: renewal locks the instance row, then the queue rows |
| `MAIN-shortening` | `NeverShorten` | 7 states |
| `MAIN-revival` | `NoExpiredRenewalAtCheck` | 8 states: renewal accepts an expired lease using a stale caller time |

The stricter clock properties fail in every variant, including `FIXED` with zero
skew. A renewal reads the clock, then stalls before it writes. It can then
commit a deadline that has already passed. No fetch can take the instance
during that stall, because renewal holds the advisory lock. Afterwards the
expired lease can be taken over normally, and the old owner's acknowledge then
fails on its token. This is a liveness cost, not an ownership failure.

## Limits

- Bounded: up to 3 concurrent calls, 2 workers, 1 instance, 2 messages and a
  few ticks. TLC's fingerprint-collision estimate is in each log.
- Fetch's candidate loop is reduced to one attempt for one instance. So the
  advisory-lock deadlock between fetches for different instances is not
  covered.
- Not modelled:
  - the worker queue and activity leases;
  - sessions;
  - `ContinueAsNew`;
  - administrative deletes;
  - the runtime's replay logic, which the invariants stand in for.
- An acknowledge always adds one history position and consumes the whole batch.
- The model was written with the known failures in view. A version written
  independently from the provider contract alone would be a stronger check.
