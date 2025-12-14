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
2. **Shared PgListener** maintains one dedicated connection per provider instance
3. **Competing consumer channel** distributes notifications to ONE waiting dispatcher
4. **App-level timer tracking** wakes dispatchers when scheduled work becomes due
5. **Fallback timeout** ensures work is never missed (safety net)

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
│  ┌───────────────────────────────────────────────────────────────────┐  │
│  │                   Background Listener Task                         │  │
│  │                                                                   │  │
│  │   PgListener (dedicated connection)                               │  │
│  │   ├─ LISTEN {schema}_orch_work                                    │  │
│  │   └─ LISTEN {schema}_worker_work                                  │  │
│  │                                                                   │  │
│  │   On NOTIFY:                                                      │  │
│  │   ├─ Parse visible_at_ms from payload                             │  │
│  │   ├─ If future timer → update next_timer_ms atomic                │  │
│  │   └─ If immediate → send WakeToken to channel                     │  │
│  └───────────────────────────────────────────────────────────────────┘  │
│                              │                                          │
│                              ▼                                          │
│  ┌───────────────────────────────────────────────────────────────────┐  │
│  │              async_channel (bounded, multi-consumer)               │  │
│  │                                                                   │  │
│  │   Only ONE dispatcher receives each wake token                    │  │
│  └───────┬───────────────────┬───────────────────┬───────────────────┘  │
│          │                   │                   │                      │
│          ▼                   ▼                   ▼                      │
│   ┌─────────────┐     ┌─────────────┐     ┌─────────────┐              │
│   │ Dispatcher 1│     │ Dispatcher 2│     │ Dispatcher 3│              │
│   │             │     │             │     │             │              │
│   │ select! {   │     │ select! {   │     │ select! {   │              │
│   │   channel   │     │   channel   │     │   channel   │              │
│   │   timer     │     │   timer     │     │   timer     │              │
│   │   fallback  │     │   fallback  │     │   fallback  │              │
│   │ }           │     │ }           │     │ }           │              │
│   └─────────────┘     └─────────────┘     └─────────────┘              │
│                                                                         │
│  Shared State:                                                          │
│  ├─ next_orch_timer_ms: AtomicI64 (earliest pending timer)             │
│  └─ next_worker_timer_ms: AtomicI64                                    │
└─────────────────────────────────────────────────────────────────────────┘
```

### Dispatcher Wait Logic

Each dispatcher waits for ONE of three events:

```rust
tokio::select! {
    // 1. NOTIFY wake token (competing consumer - only ONE dispatcher wins)
    _ = wake_channel.recv() => { /* new work arrived */ }
    
    // 2. Timer expiry (all dispatchers wake, first one wins via SKIP LOCKED)
    _ = sleep_until(next_timer_ms) => { /* scheduled work now due */ }
    
    // 3. Fallback timeout (safety net)
    _ = sleep(fallback_interval) => { /* poll just in case */ }
}
```

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
    
    /// Maximum time to block in a single fetch call.
    /// Prevents indefinite blocking; allows graceful shutdown checks.
    /// Default: 30 seconds
    pub max_block_time: Duration,
    
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
            max_block_time: Duration::from_secs(30),
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
use tokio::task::JoinHandle;

pub struct PostgresProvider {
    pool: Arc<PgPool>,
    schema_name: String,
    
    // Long-poll infrastructure (None if disabled)
    long_poll: Option<LongPollState>,
}

struct LongPollState {
    config: LongPollConfig,
    
    // Competing consumer channels
    orch_wake_tx: Sender<()>,
    orch_wake_rx: Receiver<()>,
    worker_wake_tx: Sender<()>,
    worker_wake_rx: Receiver<()>,
    
    // Timer tracking (0 = no pending timer)
    next_orch_timer_ms: Arc<AtomicI64>,
    next_worker_timer_ms: Arc<AtomicI64>,
    
    // Background listener
    listener_handle: JoinHandle<()>,
}
```

### 4. Constructors

```rust
impl PostgresProvider {
    /// Create provider with default configuration (short-polling, backward compatible)
    pub async fn new(database_url: &str) -> Result<Self> {
        Self::new_with_schema(database_url, None).await
    }

    /// Create provider with custom schema (short-polling)
    pub async fn new_with_schema(database_url: &str, schema: Option<&str>) -> Result<Self> {
        Self::new_with_config(database_url, schema, LongPollConfig::default()).await
    }

    /// Create provider with long-polling enabled
    pub async fn new_with_long_poll(database_url: &str) -> Result<Self> {
        let config = LongPollConfig {
            enabled: true,
            ..Default::default()
        };
        Self::new_with_config(database_url, None, config).await
    }

    /// Create provider with full configuration control
    pub async fn new_with_config(
        database_url: &str,
        schema: Option<&str>,
        config: LongPollConfig,
    ) -> Result<Self> {
        // ... existing pool setup and migrations ...
        
        let long_poll = if config.enabled {
            Some(Self::setup_long_poll(&pool, &schema_name, config).await?)
        } else {
            None
        };
        
        Ok(Self { pool, schema_name, long_poll })
    }
    
    async fn setup_long_poll(
        pool: &PgPool,
        schema: &str,
        config: LongPollConfig,
    ) -> Result<LongPollState> {
        // Create competing consumer channels (bounded to prevent memory growth)
        let (orch_wake_tx, orch_wake_rx) = async_channel::bounded(16);
        let (worker_wake_tx, worker_wake_rx) = async_channel::bounded(16);
        
        let next_orch_timer_ms = Arc::new(AtomicI64::new(0));
        let next_worker_timer_ms = Arc::new(AtomicI64::new(0));
        
        // Start background listener
        let listener_handle = Self::spawn_listener(
            pool.clone(),
            schema.to_string(),
            orch_wake_tx.clone(),
            worker_wake_tx.clone(),
            next_orch_timer_ms.clone(),
        );
        
        Ok(LongPollState {
            config,
            orch_wake_tx,
            orch_wake_rx,
            worker_wake_tx,
            worker_wake_rx,
            next_orch_timer_ms,
            next_worker_timer_ms,
            listener_handle,
        })
    }
}
```

### 5. Background Listener

```rust
impl PostgresProvider {
    fn spawn_listener(
        pool: PgPool,
        schema: String,
        orch_wake_tx: Sender<()>,
        worker_wake_tx: Sender<()>,
        next_orch_timer_ms: Arc<AtomicI64>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            let orch_channel = format!("{}_orch_work", schema);
            let worker_channel = format!("{}_worker_work", schema);
            
            loop {
                if let Err(e) = Self::run_listener(
                    &pool,
                    &orch_channel,
                    &worker_channel,
                    &orch_wake_tx,
                    &worker_wake_tx,
                    &next_orch_timer_ms,
                ).await {
                    warn!(
                        target = "duroxide_pg::listener",
                        error = %e,
                        "Listener disconnected, reconnecting in 1s"
                    );
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        })
    }
    
    async fn run_listener(
        pool: &PgPool,
        orch_channel: &str,
        worker_channel: &str,
        orch_wake_tx: &Sender<()>,
        worker_wake_tx: &Sender<()>,
        next_orch_timer_ms: &AtomicI64,
    ) -> Result<()> {
        let mut listener = PgListener::connect_with(pool).await?;
        listener.listen(orch_channel).await?;
        listener.listen(worker_channel).await?;
        
        debug!(
            target = "duroxide_pg::listener",
            orch = %orch_channel,
            worker = %worker_channel,
            "LISTEN established"
        );
        
        loop {
            let notification = listener.recv().await?;
            let channel = notification.channel();
            
            if channel == orch_channel {
                // Parse visible_at from payload
                let visible_at_ms: i64 = notification.payload()
                    .parse()
                    .unwrap_or(0);
                
                let now_ms = Self::now_millis();
                
                if visible_at_ms > now_ms {
                    // Future timer - update tracking, don't wake yet
                    Self::update_next_timer(next_orch_timer_ms, visible_at_ms);
                } else {
                    // Immediate work - wake ONE dispatcher
                    let _ = orch_wake_tx.try_send(());
                }
            } else if channel == worker_channel {
                // Worker items are always immediate
                let _ = worker_wake_tx.try_send(());
            }
        }
    }
    
    fn update_next_timer(atomic: &AtomicI64, new_time_ms: i64) {
        // Atomically update if new timer is earlier than current
        let _ = atomic.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
            if current == 0 || new_time_ms < current {
                Some(new_time_ms)
            } else {
                None
            }
        });
    }
}
```

### 6. Fetch Methods

```rust
#[async_trait::async_trait]
impl Provider for PostgresProvider {
    async fn fetch_orchestration_item(
        &self,
        lock_timeout: Duration,
        poll_timeout: Duration,
    ) -> Result<Option<(OrchestrationItem, String, u32)>, ProviderError> {
        // 1. Always try immediate fetch first
        if let Some(item) = self.try_fetch_orchestration_immediate(lock_timeout).await? {
            // Clear timer if we found work
            if let Some(ref lp) = self.long_poll {
                lp.next_orch_timer_ms.store(0, Ordering::SeqCst);
            }
            return Ok(Some(item));
        }
        
        // 2. If long-poll disabled, return immediately (backward compatible)
        let Some(ref lp) = self.long_poll else {
            return Ok(None);
        };
        
        // 3. Wait for wake event
        let wait_time = poll_timeout
            .min(lp.config.max_block_time)
            .min(lp.config.fallback_interval);
        
        self.wait_for_orch_work(lp, wait_time).await;
        
        // 4. Try fetch after wake
        let result = self.try_fetch_orchestration_immediate(lock_timeout).await?;
        if result.is_some() {
            lp.next_orch_timer_ms.store(0, Ordering::SeqCst);
        }
        Ok(result)
    }
    
    async fn fetch_work_item(
        &self,
        lock_timeout: Duration,
        poll_timeout: Duration,
    ) -> Result<Option<(WorkItem, String, u32)>, ProviderError> {
        // Same pattern for worker queue
        if let Some(item) = self.try_fetch_work_immediate(lock_timeout).await? {
            return Ok(Some(item));
        }
        
        let Some(ref lp) = self.long_poll else {
            return Ok(None);
        };
        
        let wait_time = poll_timeout
            .min(lp.config.max_block_time)
            .min(lp.config.fallback_interval);
        
        self.wait_for_worker_work(lp, wait_time).await;
        
        self.try_fetch_work_immediate(lock_timeout).await
    }
}

impl PostgresProvider {
    async fn wait_for_orch_work(&self, lp: &LongPollState, max_wait: Duration) {
        let wait_duration = self.compute_orch_wait(lp, max_wait);
        
        tokio::select! {
            biased;
            
            // Wake on NOTIFY (competing consumer)
            _ = lp.orch_wake_rx.recv() => {
                debug!(target = "duroxide_pg", "Woke on NOTIFY");
            }
            
            // Wake on timeout (timer expiry or fallback)
            _ = tokio::time::sleep(wait_duration) => {
                debug!(target = "duroxide_pg", "Woke on timeout");
            }
        }
    }
    
    fn compute_orch_wait(&self, lp: &LongPollState, max_wait: Duration) -> Duration {
        let now_ms = Self::now_millis();
        let next_timer = lp.next_orch_timer_ms.load(Ordering::SeqCst);
        
        if next_timer > 0 && next_timer > now_ms {
            // Wake slightly before timer fires (grace period)
            let until_timer_ms = (next_timer - now_ms) as u64;
            let until_timer = Duration::from_millis(
                until_timer_ms.saturating_sub(lp.config.timer_grace_ms)
            );
            until_timer.min(max_wait)
        } else if next_timer > 0 {
            // Timer already due
            Duration::ZERO
        } else {
            // No pending timer, use max wait
            max_wait
        }
    }
    
    async fn wait_for_worker_work(&self, lp: &LongPollState, max_wait: Duration) {
        tokio::select! {
            biased;
            _ = lp.worker_wake_rx.recv() => {}
            _ = tokio::time::sleep(max_wait) => {}
        }
    }
}
```

### 7. Clone Implementation

```rust
impl Clone for PostgresProvider {
    fn clone(&self) -> Self {
        Self {
            pool: self.pool.clone(),
            schema_name: self.schema_name.clone(),
            long_poll: self.long_poll.as_ref().map(|lp| LongPollState {
                config: lp.config.clone(),
                orch_wake_tx: lp.orch_wake_tx.clone(),
                orch_wake_rx: lp.orch_wake_rx.clone(),  // async_channel::Receiver is Clone!
                worker_wake_tx: lp.worker_wake_tx.clone(),
                worker_wake_rx: lp.worker_wake_rx.clone(),
                next_orch_timer_ms: lp.next_orch_timer_ms.clone(),
                next_worker_timer_ms: lp.next_worker_timer_ms.clone(),
                listener_handle: /* shared via Arc or not cloned */,
            }),
        }
    }
}
```

## Multi-Dispatcher Behavior

### Single Node, Multiple Dispatchers

| Event | Behavior |
|-------|----------|
| New immediate work | NOTIFY → ONE dispatcher wakes via channel |
| Timer scheduled | NOTIFY with future time → atomic updated, no wake |
| Timer expires | All dispatchers wake on timeout, first wins via SKIP LOCKED |
| Idle | All dispatchers wait, no queries |

**Competing consumer**: When work arrives, only ONE dispatcher queries the DB.

### Multiple Nodes

Each node has its own listener. PostgreSQL NOTIFY reaches ALL nodes:

| Event | Behavior |
|-------|----------|
| New work | NOTIFY → All node listeners receive it |
| Per node | ONE dispatcher per node wakes |
| N nodes | N queries total (one per node) |
| Winner | `FOR UPDATE SKIP LOCKED` → one processes |

**Query count scales with nodes, not dispatchers.**

### Timer Synchronization

All nodes track the same timer via NOTIFY payload:
- INSERT timer → NOTIFY with `visible_at_ms` → All nodes update their `next_timer_ms`
- When timer is due → All nodes wake → First one wins via `SKIP LOCKED`

## Failure Modes

| Failure | Behavior |
|---------|----------|
| Listener disconnects | Auto-reconnect after 1s, fallback polling continues |
| NOTIFY lost | Fallback timeout catches it (60s default) |
| Timer missed | Fallback timeout catches it |
| Channel full | `try_send` drops token, but fallback polling will catch |

The fallback timeout is the safety net. Even if everything else fails, work will be found within `fallback_interval`.

## Performance Comparison

| Configuration | Idle Load (4 dispatchers) | Wake Latency | Notes |
|---------------|---------------------------|--------------|-------|
| Short-poll (50ms) | 160 q/s | 0-50ms avg | Current default |
| Short-poll (1s) | 8 q/s | 0-1000ms avg | Simple tuning |
| **Long-poll** | **~0.07 q/s** | **<5ms** | This design |

*Idle load with long-poll = 4 dispatchers × (1 query / 60s fallback) = 0.067 q/s*

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

4. **Immediate wake**: Insert work → dispatcher wakes immediately
5. **Timer wake**: Insert future timer → dispatcher wakes at correct time
6. **Competing consumer**: Multiple dispatchers, one work item → only one queries
7. **Fallback timeout**: No NOTIFY → wakes after fallback interval

### Stress Tests

8. **Throughput**: Verify no regression vs short-polling
9. **Idle load**: Measure query count over 60s idle period

## Migration Guide

```rust
// Before (short-polling)
let provider = PostgresProvider::new(&database_url).await?;

// After (long-polling)
let provider = PostgresProvider::new_with_long_poll(&database_url).await?;

// With custom config
let config = LongPollConfig {
    enabled: true,
    fallback_interval: Duration::from_secs(120),
    ..Default::default()
};
let provider = PostgresProvider::new_with_config(&database_url, None, config).await?;
```

## Summary

This design provides efficient push-based work detection with:
- **LISTEN/NOTIFY** for immediate work
- **Atomic timer tracking** for scheduled work
- **Fallback polling** as safety net
- **Competing consumer** to eliminate thundering herd
- **No complex state machines** - simple select! loop

