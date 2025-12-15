# Long Polling Design (Simplified)

> Push-based work detection using PostgreSQL LISTEN/NOTIFY with application-level timer tracking.

## Problem Statement

The current PostgreSQL provider uses a polling-based approach to detect new work:

```
Dispatcher Loop:
1. Call fetch_orchestration_item() → Query DB
2. If None, sleep(dispatcher_idle_sleep)
3. Repeat
```

With N dispatchers polling every X ms, this generates:
- `N × (1000/X) × 2` queries/second (orchestrator + worker queues)
- Example: 4 dispatchers at 50ms = **160 queries/second even when idle**

This creates unnecessary database load and costs, especially in cloud-hosted PostgreSQL.

## Goals

1. **Reduce idle query load by 99%+** (from ~160 q/s to ~1 q/s)
2. **Maintain work detection latency** (<10ms for immediate work)
3. **Zero changes to duroxide core** - all changes in provider only
4. **Support multi-dispatcher and multi-node deployments**
5. **Graceful degradation** - fall back to polling if LISTEN fails

## Design Overview

### Core Mechanism

1. **Triggers** fire `pg_notify()` on INSERT to queue tables
2. **Listener Task** receives NOTIFY and routes wake tokens
3. **Timer Task** watches pending timers and sends wake tokens when due
4. **Competing consumer channel** ensures only ONE dispatcher wakes per event
5. **Fallback timeout** in timer task ensures work is never missed

### Architecture

```
┌─────────────────────────────────────────────────────────────────────────┐
│                              PostgreSQL                                  │
│                                                                         │
│  ┌─────────────────────┐      ┌─────────────────────┐                   │
│  │  orchestrator_queue │      │    worker_queue     │                   │
│  │                     │      │                     │                   │
│  │  AFTER INSERT ──────┼──────┼─► pg_notify()       │                   │
│  │  trigger            │      │   trigger           │                   │
│  └─────────────────────┘      └─────────────────────┘                   │
│                                                                         │
│  Channels: {schema}_orch_work, {schema}_worker_work                     │
└─────────────────────────────────────────────────────────────────────────┘
                                    │
                                    │ NOTIFY
                                    ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                         PostgresProvider                                 │
│                                                                         │
│  ┌─────────────────────────────────────────────────────────────────┐    │
│  │                    Listener Task                                 │    │
│  │                                                                 │    │
│  │  PgListener (dedicated connection)                              │    │
│  │  ├─ LISTEN {schema}_orch_work                                   │    │
│  │  └─ LISTEN {schema}_worker_work                                 │    │
│  │                                                                 │    │
│  │  On NOTIFY:                                                     │    │
│  │  ├─ Parse visible_at_ms from payload                            │    │
│  │  ├─ If future → update next_timer_ms, notify timer task         │    │
│  │  └─ If immediate → send token to wake channel                   │    │
│  └─────────────────────────────────────────────────────────────────┘    │
│                              │                                          │
│  ┌─────────────────────────────────────────────────────────────────┐    │
│  │                    Timer Task                                    │    │
│  │                                                                 │    │
│  │  Watches: next_orch_timer_ms, next_worker_timer_ms              │    │
│  │                                                                 │    │
│  │  Loop:                                                          │    │
│  │  ├─ Sleep until min(next_timer, fallback_interval)              │    │
│  │  ├─ If timer due → send token to wake channel                   │    │
│  │  └─ If fallback due → send token to wake channel                │    │
│  └─────────────────────────────────────────────────────────────────┘    │
│                              │                                          │
│                              ▼                                          │
│  ┌─────────────────────────────────────────────────────────────────┐    │
│  │           async_channel (bounded, multi-consumer)                │    │
│  │                                                                 │    │
│  │   Only ONE dispatcher receives each wake token                  │    │
│  │   Sources: NOTIFY (immediate), Timer (scheduled), Fallback      │    │
│  └───────┬───────────────────┬───────────────────┬─────────────────┘    │
│          │                   │                   │                      │
│          ▼                   ▼                   ▼                      │
│   ┌─────────────┐     ┌─────────────┐     ┌─────────────┐              │
│   │ Dispatcher 1│     │ Dispatcher 2│     │ Dispatcher 3│              │
│   │             │     │             │     │             │              │
│   │ recv()      │     │ recv()      │     │ recv()      │              │
│   │ (blocking)  │     │ (blocking)  │     │ (blocking)  │              │
│   └─────────────┘     └─────────────┘     └─────────────┘              │
│                                                                         │
│  Shared State:                                                          │
│  ├─ next_orch_timer_ms: AtomicI64 (earliest pending timer)             │
│  ├─ next_worker_timer_ms: AtomicI64                                    │
│  └─ timer_notify: tokio::sync::Notify (wake timer task on new timer)   │
└─────────────────────────────────────────────────────────────────────────┘
```

### Key Insight: Wake Chain for Parallel Draining

Dispatchers send a wake token **only after waking from recv()**, not during tight loops:

```rust
// Runtime calls provider.fetch() in a loop
// Provider logic:

async fn fetch_orchestration_item(&self, ...) {
    // 1. Immediate fetch (tight loop path - NO wake token)
    if let Some(item) = try_fetch().await? {
        return Ok(Some(item));
    }
    
    // 2. Queue empty - wait for wake
    wake_rx.recv().await;
    
    // 3. Just woke - fetch and send ONE wake token if found work
    if let Some(item) = try_fetch().await? {
        wake_tx.try_send(());  // Wake chain: help drain queue
        return Ok(Some(item));
    }
    Ok(None)
}
```

**Wake sources:**
- **Listener Task**: Sends token on NOTIFY (immediate work)
- **Timer Task**: Sends token when timer expires or fallback
- **Dispatchers**: Send ONE token after waking and finding work

**Why this is efficient:**
- Tight loop (step 1): No tokens sent - dispatcher already active
- Wake path (step 3): ONE token per wake cycle, not per item
- 100 items = ~5 tokens (one per dispatcher), not 100 tokens

**Self-regulating properties:**
- Single item: D1 wakes, finds work, wakes D2. D2 finds nothing, waits.
- Burst of 100: D1→D2→D3→D4 chain (4 tokens). All in tight loops draining.
- Sustained load: All dispatchers in tight loops, no tokens needed.
- Queue empties: ~4 wasted fetches (one per dispatcher), acceptable overhead.

The bounded channel (`async_channel::bounded(4)`) matches dispatcher count.

## Detailed Design

### 1. Database Schema (Migration 0005)

```sql
-- Migration 0005: LISTEN/NOTIFY triggers for long-polling

DO $$
DECLARE
    v_schema_name TEXT := current_schema();
BEGIN
    -- ========================================================================
    -- Notification Functions
    -- ========================================================================

    -- Orchestrator queue notification
    -- Payload: visible_at timestamp in milliseconds (for timer tracking)
    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.notify_orch_work()
        RETURNS trigger AS $notify$
        BEGIN
            PERFORM pg_notify(
                %L,
                (EXTRACT(EPOCH FROM NEW.visible_at) * 1000)::BIGINT::TEXT
            );
            RETURN NEW;
        END;
        $notify$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name || '_orch_work');

    -- Worker queue notification (no payload needed - always immediate)
    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.notify_worker_work()
        RETURNS trigger AS $notify$
        BEGIN
            PERFORM pg_notify(%L, '''');
            RETURN NEW;
        END;
        $notify$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name || '_worker_work');

    -- ========================================================================
    -- Triggers
    -- ========================================================================

    EXECUTE format('DROP TRIGGER IF EXISTS trg_orch_notify ON %I.orchestrator_queue', v_schema_name);
    EXECUTE format('DROP TRIGGER IF EXISTS trg_worker_notify ON %I.worker_queue', v_schema_name);

    EXECUTE format('
        CREATE TRIGGER trg_orch_notify
            AFTER INSERT ON %I.orchestrator_queue
            FOR EACH ROW EXECUTE FUNCTION %I.notify_orch_work();
    ', v_schema_name, v_schema_name);

    EXECUTE format('
        CREATE TRIGGER trg_worker_notify
            AFTER INSERT ON %I.worker_queue
            FOR EACH ROW EXECUTE FUNCTION %I.notify_worker_work();
    ', v_schema_name, v_schema_name);

END $$;
```

### 2. Configuration

```rust
/// Configuration for long-polling behavior
#[derive(Clone, Debug)]
pub struct LongPollConfig {
    /// Enable LISTEN/NOTIFY based long-polling.
    /// When false, fetch methods return immediately (backward compatible).
    pub enabled: bool,
    
    /// Fallback poll interval when no timers are pending.
    /// Safety net to catch any missed notifications.
    /// Default: 60 seconds
    pub fallback_interval: Duration,
    
    /// Grace period before timer expiry to account for clock skew.
    /// Wake slightly early to ensure timely processing.
    /// Default: 50ms
    pub timer_grace_ms: u64,
}

impl Default for LongPollConfig {
    fn default() -> Self {
        Self {
            enabled: false, // Backward compatible default
            fallback_interval: Duration::from_secs(60),
            timer_grace_ms: 50,
        }
    }
}
```

### 3. Provider Structure

```rust
use async_channel::{Receiver, Sender};
use std::sync::atomic::{AtomicI64, Ordering};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

pub struct PostgresProvider {
    pool: Arc<PgPool>,
    schema_name: String,
    
    // Long-poll infrastructure (None if disabled)
    long_poll: Option<LongPollState>,
}

struct LongPollState {
    config: LongPollConfig,
    
    // Wake channels (competing consumer)
    orch_wake_tx: Sender<()>,
    orch_wake_rx: Receiver<()>,
    worker_wake_tx: Sender<()>,
    worker_wake_rx: Receiver<()>,
    
    // Timer tracking
    next_orch_timer_ms: Arc<AtomicI64>,  // 0 = no pending timer
    next_worker_timer_ms: Arc<AtomicI64>,
    
    // Signal to timer task when new timer is scheduled
    orch_timer_notify: Arc<Notify>,
    worker_timer_notify: Arc<Notify>,
    
    // Background task handles
    listener_handle: JoinHandle<()>,
    orch_timer_handle: JoinHandle<()>,
    worker_timer_handle: JoinHandle<()>,
}
```

### 4. Background Tasks

#### Listener Task

```rust
async fn listener_task(
    pool: PgPool,
    schema: String,
    orch_wake_tx: Sender<()>,
    worker_wake_tx: Sender<()>,
    next_orch_timer_ms: Arc<AtomicI64>,
    orch_timer_notify: Arc<Notify>,
) {
    let orch_channel = format!("{}_orch_work", schema);
    let worker_channel = format!("{}_worker_work", schema);
    
    loop {
        match run_listener(
            &pool, &orch_channel, &worker_channel,
            &orch_wake_tx, &worker_wake_tx,
            &next_orch_timer_ms, &orch_timer_notify,
        ).await {
            Ok(_) => break, // Clean shutdown
            Err(e) => {
                warn!("Listener disconnected: {}, reconnecting in 1s", e);
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

async fn run_listener(
    pool: &PgPool,
    orch_channel: &str,
    worker_channel: &str,
    orch_wake_tx: &Sender<()>,
    worker_wake_tx: &Sender<()>,
    next_orch_timer_ms: &AtomicI64,
    orch_timer_notify: &Notify,
) -> Result<()> {
    let mut listener = PgListener::connect_with(pool).await?;
    listener.listen(orch_channel).await?;
    listener.listen(worker_channel).await?;
    
    loop {
        let notification = listener.recv().await?;
        let channel = notification.channel();
        
        if channel == orch_channel {
            let visible_at_ms: i64 = notification.payload().parse().unwrap_or(0);
            let now_ms = now_millis();
            
            if visible_at_ms > now_ms {
                // Future timer - update tracking and wake timer task
                update_next_timer(next_orch_timer_ms, visible_at_ms);
                orch_timer_notify.notify_one();
            } else {
                // Immediate work - wake ONE dispatcher
                let _ = orch_wake_tx.try_send(());
            }
        } else if channel == worker_channel {
            let _ = worker_wake_tx.try_send(());
        }
    }
}

fn update_next_timer(atomic: &AtomicI64, new_time_ms: i64) {
    let _ = atomic.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
        if current == 0 || new_time_ms < current {
            Some(new_time_ms)
        } else {
            None
        }
    });
}
```

#### Timer Task

```rust
async fn timer_task(
    wake_tx: Sender<()>,
    next_timer_ms: Arc<AtomicI64>,
    timer_notify: Arc<Notify>,
    config: LongPollConfig,
) {
    loop {
        let sleep_duration = compute_sleep_duration(&next_timer_ms, &config);
        
        tokio::select! {
            // Sleep until next timer or fallback
            _ = tokio::time::sleep(sleep_duration) => {
                // Timer or fallback expired - wake ONE dispatcher
                let _ = wake_tx.try_send(());
                
                // Clear timer if it was a timer wake (not fallback)
                let next = next_timer_ms.load(Ordering::SeqCst);
                if next > 0 && next <= now_millis() {
                    next_timer_ms.store(0, Ordering::SeqCst);
                }
            }
            
            // New timer scheduled - recalculate sleep
            _ = timer_notify.notified() => {
                // Loop will recalculate sleep duration
                continue;
            }
        }
    }
}

fn compute_sleep_duration(next_timer_ms: &AtomicI64, config: &LongPollConfig) -> Duration {
    let next = next_timer_ms.load(Ordering::SeqCst);
    let now = now_millis();
    
    if next > 0 && next > now {
        // Sleep until timer (minus grace period)
        let until_timer_ms = (next - now) as u64;
        let until_timer = Duration::from_millis(
            until_timer_ms.saturating_sub(config.timer_grace_ms)
        );
        until_timer.min(config.fallback_interval)
    } else if next > 0 {
        // Timer already due
        Duration::ZERO
    } else {
        // No pending timer - use fallback
        config.fallback_interval
    }
}
```

### 5. Fetch Methods

The key insight: **only send wake token after waking from recv()**, not during tight fetch loops.

```rust
#[async_trait::async_trait]
impl Provider for PostgresProvider {
    async fn fetch_orchestration_item(
        &self,
        lock_timeout: Duration,
        poll_timeout: Duration,
    ) -> Result<Option<(OrchestrationItem, String, u32)>, ProviderError> {
        // 1. Try immediate fetch (tight loop - no wake token!)
        //    If runtime is calling us repeatedly, we're already awake
        if let Some(item) = self.try_fetch_orchestration_immediate(lock_timeout).await? {
            self.clear_orch_timer();
            return Ok(Some(item));
        }
        
        // 2. If long-poll disabled, return immediately
        let Some(ref lp) = self.long_poll else {
            return Ok(None);
        };
        
        // 3. Queue empty - wait for wake signal
        let _ = tokio::time::timeout(poll_timeout, lp.orch_wake_rx.recv()).await;
        
        // 4. Just woke up - fetch and propagate wake chain if found work
        let result = self.try_fetch_orchestration_immediate(lock_timeout).await?;
        if result.is_some() {
            // Found work after waking - wake ONE more dispatcher to help
            let _ = lp.orch_wake_tx.try_send(());
            self.clear_orch_timer();
        }
        Ok(result)
    }
    
    async fn fetch_work_item(
        &self,
        lock_timeout: Duration,
        poll_timeout: Duration,
    ) -> Result<Option<(WorkItem, String, u32)>, ProviderError> {
        // 1. Try immediate fetch (tight loop - no wake token!)
        if let Some(item) = self.try_fetch_work_immediate(lock_timeout).await? {
            return Ok(Some(item));
        }
        
        // 2. If long-poll disabled, return immediately
        let Some(ref lp) = self.long_poll else {
            return Ok(None);
        };
        
        // 3. Queue empty - wait for wake signal
        let _ = tokio::time::timeout(poll_timeout, lp.worker_wake_rx.recv()).await;
        
        // 4. Just woke up - fetch and propagate wake chain if found work
        let result = self.try_fetch_work_immediate(lock_timeout).await?;
        if result.is_some() {
            let _ = lp.worker_wake_tx.try_send(());
        }
        Ok(result)
    }
}

impl PostgresProvider {
    fn clear_orch_timer(&self) {
        if let Some(ref lp) = self.long_poll {
            lp.next_orch_timer_ms.store(0, Ordering::SeqCst);
        }
    }
}
```

### 6. Clone Implementation

```rust
impl Clone for PostgresProvider {
    fn clone(&self) -> Self {
        Self {
            pool: self.pool.clone(),
            schema_name: self.schema_name.clone(),
            long_poll: self.long_poll.as_ref().map(|lp| LongPollState {
                config: lp.config.clone(),
                orch_wake_tx: lp.orch_wake_tx.clone(),
                orch_wake_rx: lp.orch_wake_rx.clone(),  // async_channel Receiver is Clone
                worker_wake_tx: lp.worker_wake_tx.clone(),
                worker_wake_rx: lp.worker_wake_rx.clone(),
                next_orch_timer_ms: lp.next_orch_timer_ms.clone(),
                next_worker_timer_ms: lp.next_worker_timer_ms.clone(),
                orch_timer_notify: lp.orch_timer_notify.clone(),
                worker_timer_notify: lp.worker_timer_notify.clone(),
                // Handles are not cloned - only original owns them
                listener_handle: /* dummy or Arc-wrapped */,
                orch_timer_handle: /* dummy or Arc-wrapped */,
                worker_timer_handle: /* dummy or Arc-wrapped */,
            }),
        }
    }
}
```

## Why Not Fetch-in-Trigger?

One might ask: can the trigger call `fetch_orchestration_item` and send the locked item via NOTIFY payload?

**No**, because:

| Issue | Problem |
|-------|---------|
| **NOTIFY is broadcast** | All nodes receive every NOTIFY. Pre-locking sends token to everyone, but only one can use it. |
| **Transaction timing** | Trigger runs inside INSERT transaction. Lock isn't visible until COMMIT. |
| **Payload size limit** | pg_notify limited to ~8KB. OrchestrationItem with history can exceed this. |
| **Lock ownership** | Trigger doesn't know which dispatcher will win. Can't pre-assign. |

The current design is optimal:
- NOTIFY wakes ONE dispatcher (via channel)
- That dispatcher acquires lock via `FOR UPDATE SKIP LOCKED`
- Clean transaction boundaries

## Wake Chain Dynamics

### Single Node - Burst of Work (100 items)

```
T+0    Queue: [1,2,3...100], All dispatchers waiting
       NOTIFY arrives → Listener sends 1 token

T+1    D1 wakes (recv), fetches item1, sends 1 token
       D1 returns item1 to runtime
       D2 wakes (recv), fetches item2, sends 1 token

T+2    D1: runtime calls fetch() → immediate path → item3 (no token!)
       D2 returns item2 to runtime
       D3 wakes (recv), fetches item4, sends 1 token

T+3    D1, D2: tight loop fetching (no tokens)
       D3 returns item4
       D4 wakes (recv), fetches item5, sends 1 token (to channel)

T+4    All 4 dispatchers in tight fetch loops
       Channel has 1 token
       No more tokens sent!

T+5+   D1→item6, D2→item7, D3→item8, D4→item9 (parallel, tight loops)
       ... continues until queue empty ...

T+100  Queue empty
       D1 immediate fetch → nothing → wait on recv()
       D1 gets token from channel, fetches → nothing, sends 1 token
       D2 immediate fetch → nothing → wait on recv()
       D2 gets token, fetches → nothing, sends 1 token
       ... chain winds down, all waiting ...

Total tokens: ~8 (4 initial wake chain + ~4 wind-down)
NOT 100!
```

### Sustained Load

Under sustained load, all dispatchers stay in tight fetch loops:
- Immediate fetch path succeeds → no tokens sent
- All dispatchers continuously fetching and processing
- **Zero token overhead** - system is self-sustaining
- Only NOTIFY needed at start, then pure tight loops

### Returning to Idle

When queue empties:
- Immediate fetch returns None → dispatcher waits on recv()
- Tokens in channel (at most ~4) get consumed
- Each consumer finds nothing, may send one more token
- Chain winds down in O(dispatcher_count) fetches
- All dispatchers waiting, channel empty

## Multi-Node Behavior

| Event | Behavior |
|-------|----------|
| New immediate work | NOTIFY → All nodes receive → Each node: ONE dispatcher wakes |
| Wake chain | Each node's dispatchers wake each other independently |
| Timer scheduled | NOTIFY → All nodes update `next_timer_ms` |
| Timer expires | Each node's timer task wakes ONE dispatcher |
| N nodes | N queries initially, then wake chains within each node |
| Winner | `FOR UPDATE SKIP LOCKED` → one wins, others find nothing and chain stops |

The wake chain is **per-node**. Cross-node coordination happens via the database (`SKIP LOCKED`).

## Failure Modes

| Failure | Behavior |
|---------|----------|
| Listener disconnects | Auto-reconnect after 1s, timer task continues sending fallbacks |
| NOTIFY lost | Fallback timeout catches it (60s default) |
| Timer task crash | Restart via supervision (or combined with listener task) |
| Channel full | `try_send` drops token, but subsequent timer/fallback will retry |

The fallback interval is the ultimate safety net.

## Performance Comparison

| Configuration | Idle Load (4 dispatchers) | Wake Latency | Notes |
|---------------|---------------------------|--------------|-------|
| Short-poll (50ms) | 160 q/s | 0-50ms avg | Current default |
| Short-poll (1s) | 8 q/s | 0-1000ms avg | Simple tuning |
| **Long-poll** | **~0.07 q/s** | **<5ms** | This design |

*Idle load = 4 dispatchers × (1 query / 60s fallback) = 0.067 q/s*

## Dependencies

Add to `Cargo.toml`:

```toml
[dependencies]
async-channel = "2.0"  # MPMC channel with Clone receiver
```

## Test Plan

### Unit Tests

1. **Trigger creation**: Verify triggers exist after migration
2. **NOTIFY emission**: Insert row, verify notification received
3. **Payload parsing**: Verify `visible_at_ms` correctly parsed

### Integration Tests

4. **Immediate wake**: Insert work → dispatcher wakes < 10ms
5. **Timer wake**: Insert future timer → dispatcher wakes at correct time
6. **Competing consumer**: 4 dispatchers, 1 work item → only 1 queries DB
7. **Fallback**: Disable NOTIFY → still wakes within fallback interval
8. **Timer update**: Schedule timer, then earlier timer → wakes at earlier time

### Stress Tests

9. **Throughput**: Verify no regression vs short-polling
10. **Idle load**: Measure query count over 60s idle period

## Summary

This design provides efficient push-based work detection with:
- **LISTEN/NOTIFY** for immediate work → ONE dispatcher wakes initially
- **Timer Task** for scheduled work → ONE dispatcher wakes initially
- **Wake chain** for parallel draining → finding work wakes another dispatcher
- **Fallback interval** as safety net
- **Self-regulating** - scales dispatchers to match queue depth automatically
- **Competing consumer** pattern eliminates thundering herd
- **Under load**: All dispatchers in tight fetch loops, no NOTIFY overhead

