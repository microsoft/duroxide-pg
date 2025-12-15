# Optimized Polling Design

> Reducing PostgreSQL query load using LISTEN/NOTIFY, competing consumers, and adaptive polling modes.

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

1. **Reduce idle query load by 99%+** (from ~160 q/s to ~0.01 q/s)
2. **Maintain or improve work detection latency** (<5ms vs current 0-50ms average)
3. **Zero changes to duroxide core** - all changes in provider only
4. **Support multi-dispatcher and multi-node deployments**
5. **Graceful degradation** - fall back to polling if LISTEN fails

## Design Overview

### Core Mechanism

Use PostgreSQL's `LISTEN/NOTIFY` for push-based work detection:

1. **Triggers** fire `pg_notify()` on INSERT to queue tables
2. **Shared PgListener** maintains one dedicated connection per provider instance
3. **Competing consumer channel** distributes notifications to ONE waiting dispatcher (not all)
4. **Timer tracking** handles future-scheduled work items
5. **Aggressive polling mode** provides resilience after wake events

### Architecture: Competing Consumer Model

Unlike a broadcast model where ALL dispatchers wake up and race, we use a **competing consumer** 
pattern where only ONE dispatcher receives each notification:

```
┌─────────────────────────────────────────────────────────────────────┐
│                         PostgreSQL                                   │
│  ┌─────────────────┐  ┌─────────────────┐  ┌──────────────────────┐ │
│  │ orchestrator_q  │  │   worker_queue  │  │   NOTIFY channels    │ │
│  │                 │  │                 │  │ - schema_orch_work   │ │
│  │ INSERT trigger ─┼──┼─────────────────┼──┼─► pg_notify()        │ │
│  └─────────────────┘  └────────┬────────┘  └──────────┬───────────┘ │
└────────────────────────────────┼──────────────────────┼─────────────┘
                                 │                      │
                                 ▼                      │
┌──────────────────────────────────────────────────────────────────────────────┐
│                              Node A (Provider Instance)                       │
│                                                                              │
│  ┌─────────────────────────────────────────────────────────────────────────┐ │
│  │                    Single Listener Task                                  │ │
│  │                                                                         │ │
│  │   NOTIFY received → mpsc::send(WakeToken)  // ONE consumer gets it     │ │
│  └─────────────────────────────────────────────────────────────────────────┘ │
│                           │                                                  │
│                           ▼                                                  │
│  ┌─────────────────────────────────────────────────────────────────────────┐ │
│  │           async_channel (bounded, multi-producer single-consumer)       │ │
│  │                                                                         │ │
│  │   Only ONE dispatcher receives each wake token                          │ │
│  └───────┬─────────────────────┬─────────────────────┬─────────────────────┘ │
│          │                     │                     │                       │
│          ▼                     ▼                     ▼                       │
│   ┌─────────────┐       ┌─────────────┐       ┌─────────────┐               │
│   │ Dispatcher 1│       │ Dispatcher 2│       │ Dispatcher 3│               │
│   │ (gets token)│       │ (waiting...)│       │ (waiting...)│               │
│   └──────┬──────┘       └─────────────┘       └─────────────┘               │
│          │                                                                   │
│          ▼                                                                   │
│   SELECT ... FOR UPDATE SKIP LOCKED                                          │
│   (Only this dispatcher queries)                                             │
└──────────────────────────────────────────────────────────────────────────────┘

┌──────────────────────────────────────────────────────────────────────────────┐
│                              Node B (Separate Provider Instance)              │
│                                                                              │
│   (Same structure - its own listener, its own channel, its own dispatchers) │
│   PostgreSQL NOTIFYs reach BOTH nodes, but within each node only ONE        │
│   dispatcher wakes per notification.                                         │
└──────────────────────────────────────────────────────────────────────────────┘
```

**Key benefits of competing consumer:**
- Only 1 query per notification per node (not N queries)
- No thundering herd within a node
- Multi-node still works: each node gets NOTIFY, one dispatcher per node queries
- Losers stay asleep, not racing

### Polling State Machine

After any wake event (NOTIFY, timer, fallback), the provider enters **aggressive polling mode**
for a configurable duration before returning to long-poll mode. This handles transient failures:

```
┌─────────────────────────────────────────────────────────────────────────────────┐
│                           Polling State Machine                                  │
│                                                                                 │
│   ┌───────────────────┐                         ┌───────────────────────────┐   │
│   │    LONG_POLL      │                         │    AGGRESSIVE_POLL        │   │
│   │                   │                         │                           │   │
│   │  Wait for:        │    any wake event       │  Short poll interval      │   │
│   │  - NOTIFY token   │ ───────────────────────►│  (e.g., 100ms)            │   │
│   │  - Timer expiry   │                         │                           │   │
│   │  - Fallback (5m)  │                         │  Duration: 1 minute       │   │
│   │                   │◄────────────────────────│  (configurable)           │   │
│   │  Queries: ~0/sec  │    timeout expires      │                           │   │
│   │                   │    (no work found)      │  Queries: 10/sec          │   │
│   └───────────────────┘                         └───────────────────────────┘   │
│                                                                                 │
│   Why aggressive mode?                                                          │
│   - Timer fires but PG error occurs → retry quickly, don't wait 5 minutes      │
│   - NOTIFY received but work stolen → quickly check for more work              │
│   - Handles transient connection issues gracefully                              │
│   - Automatic fallback to efficient long-poll when system is idle              │
└─────────────────────────────────────────────────────────────────────────────────┘
```

### Provider Internal Structure

```
┌──────────────────────────────────────────────────────────────────────────────┐
│                              PostgresProvider                                 │
│                                                                              │
│  ┌─────────────────────────────────────────────────────────────────────────┐ │
│  │                    Background Listener Task (spawned once)              │ │
│  │                                                                         │ │
│  │   PgListener (dedicated connection)                                     │ │
│  │   - LISTEN schema_orch_work                                             │ │
│  │   - LISTEN schema_worker_work                                           │ │
│  │                                                                         │ │
│  │   loop {                                                                │ │
│  │       notification = listener.recv().await   // Blocks until NOTIFY    │ │
│  │       update_timer_tracking(notification)                               │ │
│  │       orch_wake_tx.send(token).await         // ONE dispatcher gets it │ │
│  │   }                                                                     │ │
│  └─────────────────────────────────────────────────────────────────────────┘ │
│                           │                                                  │
│                           ▼                                                  │
│  ┌─────────────────────────────────────────────────────────────────────────┐ │
│  │  Shared State                                                           │ │
│  │  - orch_wake_tx: async_channel::Sender<WakeToken>  (competing consumer)│ │
│  │  - orch_wake_rx: async_channel::Receiver<WakeToken> (cloneable)        │ │
│  │  - worker_wake_tx/rx: same pattern for worker queue                     │ │
│  │  - next_orch_wake_ms: AtomicI64 (timestamp of next timer)              │ │
│  └─────────────────────────────────────────────────────────────────────────┘ │
│                                                                              │
│  ┌─────────────────────────────────────────────────────────────────────────┐ │
│  │  fetch_orchestration_item() / fetch_work_item()                         │ │
│  │                                                                         │ │
│  │  1. Try immediate fetch (existing logic)                                │ │
│  │  2. If None and long_poll enabled:                                      │ │
│  │     a. If in AGGRESSIVE mode: short sleep, retry                        │ │
│  │     b. If in LONG_POLL mode: wait for token/timer/fallback             │ │
│  │  3. On wake: enter AGGRESSIVE mode, retry fetch                         │ │
│  │  4. If AGGRESSIVE timeout expires with no work: return to LONG_POLL    │ │
│  └─────────────────────────────────────────────────────────────────────────┘ │
└──────────────────────────────────────────────────────────────────────────────┘
```

## Detailed Design

### 1. Database Schema Changes (Migration 0003)

```sql
-- Migration 0003: LISTEN/NOTIFY infrastructure for long-polling

DO $$
DECLARE
    v_schema_name TEXT := current_schema();
BEGIN
    -- ============================================================================
    -- Notification Functions
    -- ============================================================================

    -- Orchestrator queue notification
    -- Payload contains visible_at timestamp for timer tracking
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

    -- Worker queue notification (no payload needed)
    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.notify_worker_work()
        RETURNS trigger AS $notify$
        BEGIN
            PERFORM pg_notify(%L, '''');
            RETURN NEW;
        END;
        $notify$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name || '_worker_work');

    -- ============================================================================
    -- Triggers
    -- ============================================================================

    -- Drop existing triggers if any (idempotent)
    EXECUTE format('
        DROP TRIGGER IF EXISTS trg_orch_notify ON %I.orchestrator_queue;
    ', v_schema_name);

    EXECUTE format('
        DROP TRIGGER IF EXISTS trg_worker_notify ON %I.worker_queue;
    ', v_schema_name);

    -- Create triggers
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

### 2. Provider Configuration

```rust
/// Configuration for long-polling behavior
#[derive(Clone, Debug)]
pub struct LongPollConfig {
    /// Enable LISTEN/NOTIFY based long-polling
    pub enabled: bool,
    
    /// Maximum time to wait before forcing a poll (safety fallback)
    /// Default: 5 minutes
    pub fallback_poll_interval: Duration,
    
    /// Maximum time for a single wait cycle in long-poll mode
    /// Prevents blocking too long on a single fetch call
    /// Default: 30 seconds
    pub max_wait_time: Duration,
    
    /// Grace period to add when waking for timers
    /// Accounts for clock skew between app and DB
    /// Default: 100ms
    pub timer_grace_period: Duration,
    
    // =========================================================================
    // Aggressive Polling Configuration
    // =========================================================================
    
    /// Minimum time to stay in aggressive polling mode after a wake event.
    /// Actual exit requires BOTH this time to pass AND successful empty fetches.
    /// Default: 60 seconds
    pub aggressive_poll_duration: Duration,
    
    /// Number of consecutive successful fetches with no results required to exit
    /// aggressive mode (in addition to time requirement).
    /// This ensures we don't exit while:
    /// - Infrastructure is having intermittent errors
    /// - There's still work being processed
    /// Default: 5
    pub aggressive_exit_threshold: u32,
    
    /// How long to wait for a dispatcher to receive a NOTIFY wake token
    /// before triggering aggressive mode on all dispatchers.
    /// This handles the case where all dispatchers are busy when NOTIFY arrives.
    /// Default: 100ms
    pub notify_delivery_timeout: Duration,
}

impl Default for LongPollConfig {
    fn default() -> Self {
        Self {
            enabled: false,  // Disabled by default for backward compatibility
            fallback_poll_interval: Duration::from_secs(300),
            max_wait_time: Duration::from_secs(30),
            timer_grace_period: Duration::from_millis(100),
            aggressive_poll_duration: Duration::from_secs(60),
            aggressive_exit_threshold: 5,
            notify_delivery_timeout: Duration::from_millis(100),
        }
    }
}
```

**Aggressive Mode Entry/Exit Summary:**

| Condition | Enter Aggressive? | Exit Aggressive? |
|-----------|------------------|------------------|
| NOTIFY received | ✅ Enter | - |
| Timer fired | ✅ Enter | - |
| Fallback timeout | ✅ Enter | - |
| Channel send failed | ✅ Enter | - |
| Listener disconnected | ✅ Enter | - |
| **NOTIFY delivery timeout** | ✅ Enter (all dispatchers!) | - |
| Fetch DB error | ✅ Enter / Reset counter | ❌ Stay (reset counter) |
| Fetch success, found work | - | ❌ Stay (reset counter) |
| Fetch success, no work | - | Check conditions |
| Time passed + N empty successes | - | ✅ Exit |

**Philosophy: Be aggressive about entering aggressive mode!**

It's better to briefly poll more frequently than to miss work. The exit conditions ensure 
we don't stay in aggressive mode forever - we'll exit once the system stabilizes.

**Note on Aggressive Poll Rate:**

The poll rate during aggressive mode is controlled by the **runtime's** `dispatcher_idle_sleep`, 
not by the provider. This is intentional:

```rust
// In your application:
let options = RuntimeOptions {
    dispatcher_idle_sleep: Duration::from_millis(50),  // Controls aggressive poll rate
    ..Default::default()
};
let runtime = Runtime::start_with_options(provider, activities, orchestrations, options).await;
```
```

### 3. Provider Structure

```rust
pub struct PostgresProvider {
    pool: Arc<PgPool>,
    schema_name: String,
    
    // Long-poll infrastructure
    long_poll_config: LongPollConfig,
    
    // Competing consumer channels (only ONE dispatcher gets each token)
    orch_wake_tx: async_channel::Sender<WakeToken>,
    orch_wake_rx: async_channel::Receiver<WakeToken>,  // Cloneable!
    worker_wake_tx: async_channel::Sender<WakeToken>,
    worker_wake_rx: async_channel::Receiver<WakeToken>,
    
    // Timer tracking
    next_orch_wake_ms: Arc<AtomicI64>,   // 0 = no scheduled timer
    next_worker_wake_ms: Arc<AtomicI64>,
    
    // Listener management
    listener_handle: Option<JoinHandle<()>>,
}

/// Token sent to wake up ONE waiting dispatcher
#[derive(Clone, Debug)]
struct WakeToken {
    /// For orchestrator queue: the visible_at timestamp (0 = immediate)
    visible_at_ms: i64,
    /// Reason for wake (for debugging/metrics)
    reason: WakeReason,
}

#[derive(Clone, Debug)]
enum WakeReason {
    Notify,      // PostgreSQL NOTIFY received
    Timer,       // Scheduled timer expired
    Fallback,    // Fallback poll interval reached
}

/// Tracks the current polling state for a dispatcher
#[derive(Clone, Debug)]
enum PollState {
    /// Waiting for NOTIFY, timer, or fallback timeout
    LongPoll,
    /// Aggressively polling after a wake event
    AggressivePoll {
        /// When aggressive mode started
        started_at: Instant,
    },
}
```

**Why `async_channel`?**

`async_channel` is an MPMC (multi-producer, **multi-consumer**) bounded channel:

```rust
// async_channel - Receiver is Clone!
let (tx, rx) = async_channel::bounded::<WakeToken>(64);

let rx1 = rx.clone();  // Dispatcher 1 gets a clone
let rx2 = rx.clone();  // Dispatcher 2 gets a clone
let rx3 = rx.clone();  // Dispatcher 3 gets a clone

// When tx.send(token) is called:
// - Only ONE of rx1/rx2/rx3 receives it (competing consumer!)
// - The others keep waiting
```

Compare to alternatives:
- `tokio::sync::mpsc` - Receiver is NOT Clone (single consumer only)
- `tokio::sync::broadcast` - ALL receivers get every message (thundering herd)
- `async_channel` - Exactly what we need: multiple consumers, one winner per message

### 4. Constructor Changes

```rust
impl PostgresProvider {
    /// Create provider with default configuration (short-polling, backward compatible)
    pub async fn new(database_url: &str) -> Result<Self> {
        Self::new_with_options(database_url, None, LongPollConfig::default()).await
    }

    /// Create provider with custom schema
    pub async fn new_with_schema(database_url: &str, schema_name: Option<&str>) -> Result<Self> {
        Self::new_with_options(database_url, schema_name, LongPollConfig::default()).await
    }

    /// Create provider with long-polling enabled
    pub async fn new_with_long_poll(database_url: &str) -> Result<Self> {
        Self::new_with_long_poll_and_schema(database_url, None).await
    }

    /// Create provider with long-polling and custom schema
    pub async fn new_with_long_poll_and_schema(
        database_url: &str,
        schema_name: Option<&str>,
    ) -> Result<Self> {
        let config = LongPollConfig {
            enabled: true,
            ..Default::default()
        };
        Self::new_with_options(database_url, schema_name, config).await
    }

    /// Create provider with full configuration control
    pub async fn new_with_options(
        database_url: &str,
        schema_name: Option<&str>,
        long_poll_config: LongPollConfig,
    ) -> Result<Self> {
        // ... existing pool creation and migration logic ...

        // Create competing consumer channels
        // Bounded to prevent unbounded growth if dispatchers are slow
        let (orch_wake_tx, orch_wake_rx) = async_channel::bounded(64);
        let (worker_wake_tx, worker_wake_rx) = async_channel::bounded(64);
        
        let mut provider = Self {
            pool: Arc::new(pool),
            schema_name: schema_name.unwrap_or("public").to_string(),
            long_poll_config,
            orch_wake_tx,
            orch_wake_rx,
            worker_wake_tx,
            worker_wake_rx,
            next_orch_wake_ms: Arc::new(AtomicI64::new(0)),
            next_worker_wake_ms: Arc::new(AtomicI64::new(0)),
            listener_handle: None,
        };

        // Start listener if long-polling is enabled
        if provider.long_poll_config.enabled {
            provider.start_listener().await?;
        }

        Ok(provider)
    }
}
```

### 5. Listener Task

```rust
impl PostgresProvider {
    async fn start_listener(&mut self) -> Result<()> {
        let pool = self.pool.clone();
        let schema = self.schema_name.clone();
        let orch_wake_tx = self.orch_wake_tx.clone();
        let worker_wake_tx = self.worker_wake_tx.clone();
        let next_orch_wake = self.next_orch_wake_ms.clone();

        let handle = tokio::spawn(async move {
            Self::listener_loop(pool, schema, orch_wake_tx, worker_wake_tx, next_orch_wake).await;
        });

        self.listener_handle = Some(handle);
        Ok(())
    }

    async fn listener_loop(
        pool: Arc<PgPool>,
        schema: String,
        orch_wake_tx: async_channel::Sender<WakeToken>,
        worker_wake_tx: async_channel::Sender<WakeToken>,
        next_orch_wake: Arc<AtomicI64>,
    ) {
        let orch_channel = format!("{}_orch_work", schema);
        let worker_channel = format!("{}_worker_work", schema);

        loop {
            match Self::run_listener(
                &pool, 
                &orch_channel, 
                &worker_channel, 
                &orch_wake_tx, 
                &worker_wake_tx,
                &next_orch_wake
            ).await {
                Ok(_) => {
                    // Clean shutdown requested
                    break;
                }
                Err(e) => {
                    error!(
                        target = "duroxide::providers::postgres::listener",
                        error = %e,
                        "Listener connection lost, reconnecting in 1s"
                    );
                    sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }

    async fn run_listener(
        pool: &PgPool,
        orch_channel: &str,
        worker_channel: &str,
        orch_wake_tx: &async_channel::Sender<WakeToken>,
        worker_wake_tx: &async_channel::Sender<WakeToken>,
        next_orch_wake: &AtomicI64,
    ) -> Result<()> {
        let mut listener = PgListener::connect_with(pool).await?;
        listener.listen(orch_channel).await?;
        listener.listen(worker_channel).await?;

        debug!(
            target = "duroxide::providers::postgres::listener",
            orch_channel = %orch_channel,
            worker_channel = %worker_channel,
            "LISTEN connections established"
        );

        loop {
            let notification = listener.recv().await?;
            let channel = notification.channel();

            if channel == orch_channel {
                let visible_at_ms = notification.payload()
                    .parse::<i64>()
                    .unwrap_or(0);

                // Update next wake time if this is a future timer
                let now_ms = Self::now_millis();
                if visible_at_ms > now_ms {
                    // Future timer - update tracking but don't wake immediately
                    next_orch_wake.fetch_update(
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                        |current| {
                            if current == 0 || visible_at_ms < current {
                                Some(visible_at_ms)
                            } else {
                                None
                            }
                        },
                    ).ok();
                    // Don't send wake token for future timers - timer waker handles it
                    continue;
                }

                // Immediate work - send wake token to ONE dispatcher
                let token = WakeToken {
                    visible_at_ms,
                    reason: WakeReason::Notify,
                };
                // Use try_send to avoid blocking if channel is full
                // (if full, dispatchers are already awake/busy)
                let _ = orch_wake_tx.try_send(token);
                
            } else if channel == worker_channel {
                let token = WakeToken {
                    visible_at_ms: 0,
                    reason: WakeReason::Notify,
                };
                let _ = worker_wake_tx.try_send(token);
            }
        }
    }
}
```

### 6. Modified Fetch Methods (with Aggressive Polling State Machine)

```rust
#[async_trait::async_trait]
impl Provider for PostgresProvider {
    async fn fetch_orchestration_item(
        &self,
        lock_timeout: Duration,
    ) -> Result<Option<OrchestrationItem>, ProviderError> {
        // 1. Try immediate fetch (existing logic)
        if let Some(item) = self.try_fetch_orchestration_item_immediate(lock_timeout).await? {
            self.next_orch_wake_ms.store(0, Ordering::SeqCst);
            return Ok(Some(item));
        }

        // 2. If long-poll disabled, return immediately (backward compatible)
        if !self.long_poll_config.enabled {
            return Ok(None);
        }

        // 3. Wait for wake event OR aggressive poll
        let poll_state = self.wait_for_orchestrator_work().await;

        // 4. Try fetch after wake
        if let Some(item) = self.try_fetch_orchestration_item_immediate(lock_timeout).await? {
            self.next_orch_wake_ms.store(0, Ordering::SeqCst);
            return Ok(Some(item));
        }

        // 5. If we got a wake event but no work, enter aggressive mode
        //    The caller (runtime) will call us again - we track state via returned None
        //    and handle aggressive polling in subsequent calls
        //    
        //    Note: Aggressive polling is handled by the internal state machine,
        //    not by blocking here. This keeps the Provider interface clean.
        
        Ok(None)
    }

    async fn fetch_work_item(
        &self,
        lock_timeout: Duration,
    ) -> Result<Option<(WorkItem, String)>, ProviderError> {
        // Same pattern for worker queue
        if let Some(item) = self.try_fetch_work_item_immediate(lock_timeout).await? {
            return Ok(Some(item));
        }

        if !self.long_poll_config.enabled {
            return Ok(None);
        }

        self.wait_for_worker_work().await;
        self.try_fetch_work_item_immediate(lock_timeout).await
    }
}

impl PostgresProvider {
    /// Wait for orchestrator work using competing consumer pattern
    /// Returns the poll state for the caller to track
    async fn wait_for_orchestrator_work(&self) -> PollState {
        // Check if we should be in aggressive mode
        // (This would be tracked per-dispatcher via thread-local or passed state)
        
        let timeout = self.compute_orch_wait_timeout();

        tokio::select! {
            biased;

            // Competing consumer: only ONE dispatcher gets each token
            result = self.orch_wake_rx.recv() => {
                match result {
                    Ok(token) => {
                        debug!(
                            target = "duroxide::providers::postgres",
                            reason = ?token.reason,
                            visible_at_ms = token.visible_at_ms,
                            "Received wake token"
                        );
                        // Enter aggressive mode after receiving a wake token
                        PollState::AggressivePoll { started_at: Instant::now() }
                    }
                    Err(_) => {
                        // Channel closed, return to poll
                        PollState::LongPoll
                    }
                }
            }

            // Timer wake (computed from next_orch_wake_ms)
            _ = sleep(timeout) => {
                let next_wake = self.next_orch_wake_ms.load(Ordering::SeqCst);
                let now_ms = Self::now_millis();
                
                if next_wake > 0 && next_wake <= now_ms + 100 {
                    // Timer fired - enter aggressive mode
                    self.next_orch_wake_ms.store(0, Ordering::SeqCst);
                    PollState::AggressivePoll { started_at: Instant::now() }
                } else {
                    // Fallback timeout - stay in long poll
                    PollState::LongPoll
                }
            }
        }
    }

    async fn wait_for_worker_work(&self) -> PollState {
        let timeout = self.long_poll_config.fallback_poll_interval
            .min(self.long_poll_config.max_wait_time);

        tokio::select! {
            biased;

            result = self.worker_wake_rx.recv() => {
                match result {
                    Ok(_token) => PollState::AggressivePoll { started_at: Instant::now() },
                    Err(_) => PollState::LongPoll,
                }
            }

            _ = sleep(timeout) => {
                PollState::LongPoll
            }
        }
    }

    fn compute_orch_wait_timeout(&self) -> Duration {
        let now_ms = Self::now_millis();
        let next_wake = self.next_orch_wake_ms.load(Ordering::SeqCst);

        if next_wake > 0 && next_wake > now_ms {
            // Wake slightly before timer fires (grace period)
            let wait_ms = (next_wake - now_ms) as u64;
            let wait = Duration::from_millis(wait_ms)
                .saturating_sub(self.long_poll_config.timer_grace_period);
            wait.min(self.long_poll_config.max_wait_time)
        } else {
            // No pending timers, use fallback
            self.long_poll_config.fallback_poll_interval
                .min(self.long_poll_config.max_wait_time)
        }
    }
}
```

### 6b. Aggressive Polling: Defer to Runtime

**Key Design Decision**: During aggressive mode, we **defer to the runtime** for poll timing.

The runtime already has a polling loop with `dispatcher_idle_sleep`. Instead of sleeping 
inside the provider (which would double the delay), we:

1. **Long-Poll Mode**: Provider blocks internally waiting for NOTIFY/timer
2. **Aggressive Mode**: Provider returns `None` immediately → runtime's `dispatcher_idle_sleep` controls poll rate

### 6c. Aggressive Mode State Machine

Aggressive mode has sophisticated entry and exit conditions to handle infrastructure failures:

```rust
use std::cell::RefCell;

/// Tracks aggressive polling state per dispatcher (thread-local)
#[derive(Clone, Debug)]
struct AggressiveState {
    /// When aggressive mode was entered
    entered_at: Instant,
    /// Count of consecutive successful fetches with no results
    consecutive_empty_successes: u32,
}

thread_local! {
    static ORCH_AGGRESSIVE_STATE: RefCell<Option<AggressiveState>> = RefCell::new(None);
    static WORKER_AGGRESSIVE_STATE: RefCell<Option<AggressiveState>> = RefCell::new(None);
}
```

**Entry Conditions** (enter aggressive mode when ANY of these occur):

```rust
impl PostgresProvider {
    /// Reasons to enter aggressive polling mode
    fn should_enter_aggressive(&self, reason: AggressiveReason) -> bool {
        match reason {
            // Normal wake events
            AggressiveReason::NotifyReceived => true,
            AggressiveReason::TimerFired => true,
            AggressiveReason::FallbackTimeout => true,
            
            // Infrastructure failures - CRITICAL to catch these!
            AggressiveReason::ChannelSendFailed => true,    // try_send failed
            AggressiveReason::ListenerDisconnected => true, // PgListener error
            AggressiveReason::FetchError => true,           // DB query error
        }
    }

    fn enter_aggressive_mode_orch(&self, reason: AggressiveReason) {
        ORCH_AGGRESSIVE_STATE.with(|state| {
            *state.borrow_mut() = Some(AggressiveState {
                entered_at: Instant::now(),
                consecutive_empty_successes: 0,
            });
        });
        debug!(
            target = "duroxide::providers::postgres",
            reason = ?reason,
            max_duration_secs = self.long_poll_config.aggressive_poll_duration.as_secs(),
            "Entering aggressive poll mode"
        );
    }
}

#[derive(Debug, Clone, Copy)]
enum AggressiveReason {
    NotifyReceived,
    TimerFired,
    FallbackTimeout,
    ChannelSendFailed,
    ListenerDisconnected,
    FetchError,
    NotifyDeliveryTimeout,  // NEW: NOTIFY arrived but no dispatcher received it in time
}
```

**Exit Conditions** (exit aggressive mode when ALL of these are true):

```rust
impl PostgresProvider {
    /// Check if we should exit aggressive mode
    /// 
    /// Exit when:
    /// 1. Time limit reached (aggressive_poll_duration), AND
    /// 2. N consecutive successful fetches returned no results
    /// 
    /// This prevents exiting while:
    /// - There's still work being processed
    /// - Infrastructure is having intermittent errors
    fn should_exit_aggressive(&self, fetch_result: &FetchOutcome) -> bool {
        ORCH_AGGRESSIVE_STATE.with(|state| {
            let mut state_ref = state.borrow_mut();
            let Some(ref mut aggressive) = *state_ref else {
                return true; // Not in aggressive mode
            };

            // Update consecutive success counter based on fetch outcome
            match fetch_result {
                FetchOutcome::Success { found_work: false } => {
                    // Successful fetch, no work found - increment counter
                    aggressive.consecutive_empty_successes += 1;
                }
                FetchOutcome::Success { found_work: true } => {
                    // Found work - reset counter, stay aggressive
                    aggressive.consecutive_empty_successes = 0;
                    return false;
                }
                FetchOutcome::Error => {
                    // Infrastructure error - reset counter, stay aggressive
                    aggressive.consecutive_empty_successes = 0;
                    return false;
                }
            }

            // Check exit conditions
            let time_elapsed = aggressive.entered_at.elapsed() >= 
                self.long_poll_config.aggressive_poll_duration;
            let enough_empty_polls = aggressive.consecutive_empty_successes >= 
                self.long_poll_config.aggressive_exit_threshold;

            if time_elapsed && enough_empty_polls {
                debug!(
                    target = "duroxide::providers::postgres",
                    duration_secs = aggressive.entered_at.elapsed().as_secs(),
                    empty_polls = aggressive.consecutive_empty_successes,
                    "Exiting aggressive poll mode"
                );
                *state_ref = None; // Clear aggressive state
                true
            } else {
                false
            }
        })
    }

    fn is_aggressive_mode_orch(&self) -> bool {
        ORCH_AGGRESSIVE_STATE.with(|state| state.borrow().is_some())
    }
}

#[derive(Debug)]
enum FetchOutcome {
    Success { found_work: bool },
    Error,
}
```

**Updated Configuration:**

```rust
pub struct LongPollConfig {
    // ... existing fields ...

    /// How long to stay in aggressive polling mode (minimum)
    /// Default: 60 seconds
    pub aggressive_poll_duration: Duration,
    
    /// Number of consecutive successful empty fetches required to exit aggressive mode
    /// (in addition to time requirement)
    /// Default: 5
    pub aggressive_exit_threshold: u32,
}

impl Default for LongPollConfig {
    fn default() -> Self {
        Self {
            // ... existing defaults ...
            aggressive_poll_duration: Duration::from_secs(60),
            aggressive_exit_threshold: 5,
        }
    }
}
```

**Updated fetch_orchestration_item:**

```rust
async fn fetch_orchestration_item(
    &self,
    lock_timeout: Duration,
) -> Result<Option<OrchestrationItem>, ProviderError> {
    // 1. Try immediate fetch
    let fetch_result = self.try_fetch_orchestration_item_immediate(lock_timeout).await;
    
    // 2. Handle fetch result and update aggressive state
    match &fetch_result {
        Ok(Some(item)) => {
            self.next_orch_wake_ms.store(0, Ordering::SeqCst);
            // Successful fetch with work - update state but don't exit aggressive yet
            self.should_exit_aggressive(&FetchOutcome::Success { found_work: true });
            return Ok(Some(item.clone()));
        }
        Ok(None) => {
            // Check if we should exit aggressive mode
            if self.should_exit_aggressive(&FetchOutcome::Success { found_work: false }) {
                // Exited aggressive mode, will block on next call
            }
        }
        Err(e) => {
            // Infrastructure error - enter/stay in aggressive mode
            if !self.is_aggressive_mode_orch() {
                self.enter_aggressive_mode_orch(AggressiveReason::FetchError);
            } else {
                self.should_exit_aggressive(&FetchOutcome::Error); // Reset counter
            }
            return Err(e.clone());
        }
    }

    // 3. If long-poll disabled, return immediately (backward compatible)
    if !self.long_poll_config.enabled {
        return Ok(None);
    }

    // 4. Check current polling mode
    if self.is_aggressive_mode_orch() {
        // AGGRESSIVE MODE: Return immediately, let runtime control poll rate
        return Ok(None);
    }

    // 5. LONG-POLL MODE: Block waiting for wake event
    let wake_reason = self.wait_for_orchestrator_work().await;
    
    // 6. Enter aggressive mode based on wake reason
    if let Some(reason) = wake_reason {
        self.enter_aggressive_mode_orch(reason);
    }

    // 7. Try fetch after wake
    match self.try_fetch_orchestration_item_immediate(lock_timeout).await {
        Ok(Some(item)) => {
            self.next_orch_wake_ms.store(0, Ordering::SeqCst);
            Ok(Some(item))
        }
        Ok(None) => Ok(None),
        Err(e) => {
            // Error after wake - already in aggressive mode from step 6
            Err(e)
        }
    }
}
```

**Updated Listener with Delivery Timeout Detection:**

When a NOTIFY arrives but no dispatcher receives it within a timeout, we set a shared flag
that triggers aggressive mode on all dispatchers:

```rust
pub struct PostgresProvider {
    // ... existing fields ...
    
    // Shared flag: set when NOTIFY delivery fails/times out
    // Dispatchers check this and enter aggressive mode if set
    orch_notify_delivery_failed: Arc<AtomicBool>,
    worker_notify_delivery_failed: Arc<AtomicBool>,
}
```

```rust
impl PostgresProvider {
    // Configuration for delivery timeout
    const NOTIFY_DELIVERY_TIMEOUT: Duration = Duration::from_millis(100);
}

async fn run_listener(
    pool: &PgPool,
    orch_channel: &str,
    worker_channel: &str,
    orch_wake_tx: &async_channel::Sender<WakeToken>,
    worker_wake_tx: &async_channel::Sender<WakeToken>,
    next_orch_wake: &AtomicI64,
    orch_notify_delivery_failed: &AtomicBool,
    worker_notify_delivery_failed: &AtomicBool,
) -> Result<()> {
    // ... setup code ...

    loop {
        let notification = listener.recv().await?;
        let channel = notification.channel();

        if channel == orch_channel {
            let visible_at_ms = notification.payload()
                .parse::<i64>()
                .unwrap_or(0);

            // Update next wake time if this is a future timer
            let now_ms = Self::now_millis();
            if visible_at_ms > now_ms {
                // Future timer - update tracking, don't wake
                next_orch_wake.fetch_update(/* ... */).ok();
                continue;
            }

            // Immediate work - try to deliver to a dispatcher with timeout
            let token = WakeToken {
                visible_at_ms,
                reason: WakeReason::Notify,
            };
            
            // Try to send with timeout - if no dispatcher receives within timeout,
            // set the failure flag to trigger aggressive polling
            match tokio::time::timeout(
                Self::NOTIFY_DELIVERY_TIMEOUT,
                orch_wake_tx.send(token)
            ).await {
                Ok(Ok(_)) => {
                    // Successfully delivered to a dispatcher
                }
                Ok(Err(_)) => {
                    // Channel closed - listener should exit
                    return Err(anyhow!("Wake channel closed"));
                }
                Err(_timeout) => {
                    // TIMEOUT: No dispatcher received within deadline
                    // Set flag to trigger aggressive mode on ALL dispatchers
                    orch_notify_delivery_failed.store(true, Ordering::SeqCst);
                    warn!(
                        target = "duroxide::providers::postgres::listener",
                        timeout_ms = Self::NOTIFY_DELIVERY_TIMEOUT.as_millis(),
                        "NOTIFY delivery timeout - triggering aggressive polling"
                    );
                }
            }
        } else if channel == worker_channel {
            let token = WakeToken {
                visible_at_ms: 0,
                reason: WakeReason::Notify,
            };
            
            match tokio::time::timeout(
                Self::NOTIFY_DELIVERY_TIMEOUT,
                worker_wake_tx.send(token)
            ).await {
                Ok(Ok(_)) => {}
                Ok(Err(_)) => return Err(anyhow!("Worker wake channel closed")),
                Err(_timeout) => {
                    worker_notify_delivery_failed.store(true, Ordering::SeqCst);
                    warn!(
                        target = "duroxide::providers::postgres::listener",
                        "Worker NOTIFY delivery timeout - triggering aggressive polling"
                    );
                }
            }
        }
    }
}
```

**Dispatcher checks for delivery failure:**

```rust
impl PostgresProvider {
    /// Check if there was a recent NOTIFY that couldn't be delivered
    /// If so, clear the flag and return true to trigger aggressive mode
    fn check_notify_delivery_failed_orch(&self) -> bool {
        self.orch_notify_delivery_failed.swap(false, Ordering::SeqCst)
    }

    async fn wait_for_orchestrator_work(&self) -> Option<AggressiveReason> {
        let timeout = self.compute_orch_wait_timeout();

        tokio::select! {
            biased;

            // Check for delivery failure flag FIRST (highest priority)
            _ = async {
                loop {
                    if self.check_notify_delivery_failed_orch() {
                        return;
                    }
                    // Check periodically
                    sleep(Duration::from_millis(50)).await;
                }
            } => {
                Some(AggressiveReason::NotifyDeliveryTimeout)
            }

            // Normal wake token from channel
            result = self.orch_wake_rx.recv() => {
                match result {
                    Ok(_token) => Some(AggressiveReason::NotifyReceived),
                    Err(_) => None,
                }
            }

            // Timer or fallback timeout
            _ = sleep(timeout) => {
                let next_wake = self.next_orch_wake_ms.load(Ordering::SeqCst);
                let now_ms = Self::now_millis();
                
                if next_wake > 0 && next_wake <= now_ms + 100 {
                    self.next_orch_wake_ms.store(0, Ordering::SeqCst);
                    Some(AggressiveReason::TimerFired)
                } else {
                    Some(AggressiveReason::FallbackTimeout)
                }
            }
        }
    }
}
```

**Why this is important:**

```
Scenario: All dispatchers busy when NOTIFY arrives

T+0.0s    Work item inserted → NOTIFY sent
T+0.0s    Listener receives NOTIFY
T+0.0s    Listener tries to send wake token...
T+0.0s    All dispatchers are busy processing work!
T+0.1s    Delivery timeout! (100ms elapsed)
T+0.1s    → Set orch_notify_delivery_failed = true
T+0.15s   Dispatcher 1 finishes processing, checks flag
T+0.15s   → Flag is true! Enter aggressive mode
T+0.15s   → Poll immediately, find the waiting work

Without this safeguard:
T+0.0s    NOTIFY arrives, delivery times out
T+0.0s    Token dropped (channel full or timeout)
T+???s    Work waits until fallback timeout (5 minutes!)
```

**Timeline Example with Failure Recovery:**

```
Runtime config: dispatcher_idle_sleep = 50ms
Provider config: aggressive_poll_duration = 60s, aggressive_exit_threshold = 5

INFRASTRUCTURE FAILURE SCENARIO:
────────────────────────────────────────────────────────────────────────
T+0.0s    NOTIFY received, dispatcher wakes
T+0.0s    fetch_orchestration_item() called
T+0.0s    → Try fetch: PG connection error! 
T+0.0s    → Enter AGGRESSIVE mode (reason: FetchError)
T+0.0s    → Return Error, consecutive_empty_successes = 0

T+0.05s   Runtime retries after dispatcher_idle_sleep
T+0.05s   → is_aggressive_mode = true
T+0.05s   → Try fetch: PG error again!
T+0.05s   → consecutive_empty_successes = 0 (reset on error)
T+0.05s   → Return Error

T+0.1s    → Try fetch: Success! No work (None)
T+0.1s    → consecutive_empty_successes = 1

T+0.15s   → Try fetch: PG error! (intermittent)
T+0.15s   → consecutive_empty_successes = 0 (reset)

T+0.2s    → Try fetch: Success! No work
T+0.2s    → consecutive_empty_successes = 1
...
T+60.0s   → Time limit reached
T+60.0s   → consecutive_empty_successes = 3 (not enough, need 5)
T+60.0s   → Stay in aggressive mode!

T+60.15s  → Try fetch: Success! No work
T+60.15s  → consecutive_empty_successes = 4
T+60.2s   → Try fetch: Success! No work  
T+60.2s   → consecutive_empty_successes = 5
T+60.2s   → Time limit passed AND 5 consecutive empty successes
T+60.2s   → EXIT aggressive mode → back to LONG_POLL

T+60.2s   fetch_orchestration_item() called
T+60.2s   → is_aggressive_mode = false
T+60.2s   → Block waiting for NOTIFY/timer...
```

**Benefits of Smart Exit Conditions:**

1. **Resilience**: Don't exit while infrastructure is flaky
2. **Work completion**: Don't exit while there's still work being processed
3. **Confidence**: Only exit after proving system is stable and empty
4. **Prevents oscillation**: Won't rapidly flip between modes

### 7. Clone Implementation

All dispatchers share the same notification infrastructure:

```rust
impl Clone for PostgresProvider {
    fn clone(&self) -> Self {
        Self {
            pool: self.pool.clone(),
            schema_name: self.schema_name.clone(),
            long_poll_config: self.long_poll_config.clone(),
            notify_tx: self.notify_tx.clone(),           // Shared
            next_orch_wake_ms: self.next_orch_wake_ms.clone(),  // Shared
            next_worker_wake_ms: self.next_worker_wake_ms.clone(),
            listener_handle: None,  // Only original has handle
        }
    }
}
```

## Multi-Dispatcher Behavior

### Single Node, Multiple Dispatchers (Competing Consumer)

With the MPMC channel (`async_channel`), only ONE dispatcher receives each wake token:

| Event | Behavior |
|-------|----------|
| New immediate work | ONE dispatcher gets wake token, queries DB |
| Timer scheduled | Timer tracking updated, no immediate wake |
| Timer fires | ONE dispatcher wakes on timeout, queries DB |
| Idle | All dispatchers wait on channel, no queries |

```
NOTIFY arrives → Listener sends 1 WakeToken → Channel → ONE dispatcher receives
                                                        ↓
                                              Other dispatchers stay asleep
```

**Query count per work item**: 1 query (not N!)

### Multiple Nodes (Competing Consumer per Node)

Each node has its own `async_channel`. PostgreSQL NOTIFY reaches ALL nodes, but within 
each node only ONE dispatcher wakes:

| Event | Behavior |
|-------|----------|
| New work | PostgreSQL NOTIFYs ALL node listeners |
| Per node | ONE dispatcher per node gets wake token |
| N nodes | N queries total (one per node) |
| One winner | `FOR UPDATE SKIP LOCKED` → one processes |
| N-1 losers | Find nothing, enter aggressive mode briefly |

```
                    PostgreSQL NOTIFY
                           │
           ┌───────────────┼───────────────┐
           ▼               ▼               ▼
        Node A          Node B          Node C
           │               │               │
    ┌──────┴──────┐ ┌──────┴──────┐ ┌──────┴──────┐
    │ 1 dispatcher│ │ 1 dispatcher│ │ 1 dispatcher│
    │   wakes     │ │   wakes     │ │   wakes     │
    └──────┬──────┘ └──────┬──────┘ └──────┬──────┘
           │               │               │
           └───────────────┼───────────────┘
                           ▼
              SELECT ... FOR UPDATE SKIP LOCKED
              (Only ONE of the 3 wins)
```

**Query count**: N queries (one per node), not N×M (dispatchers per node × nodes)

### Thundering Herd: Eliminated Within Node

| Deployment | Old (Broadcast) | New (Competing Consumer) |
|------------|-----------------|--------------------------|
| 1 node, 4 dispatchers | 4 queries | **1 query** |
| 3 nodes, 4 dispatchers each | 12 queries | **3 queries** |
| 10 nodes, 8 dispatchers each | 80 queries | **10 queries** |

**Improvement**: Query count scales with NODES, not DISPATCHERS.

### Timer Wake Behavior

Timers don't send wake tokens (they're future events). Instead:

1. Timer scheduled → `next_orch_wake_ms` updated atomically
2. All dispatchers see the same `next_orch_wake_ms` value
3. When timer time arrives, dispatcher(s) wake from `sleep(timeout)`
4. First one to query gets the work via `SKIP LOCKED`

For timers, there's still potential for multiple dispatchers to wake simultaneously
(if they all have similar timeout calculations). This is acceptable because:
- Timer events are less frequent than NOTIFY events
- `SKIP LOCKED` handles contention gracefully
- Losers enter aggressive mode briefly, then return to long-poll

## Test Plan

### Unit Tests

#### 1. Trigger Installation Tests

```rust
#[tokio::test]
async fn test_triggers_created_on_migration() {
    let provider = PostgresProvider::new_with_long_poll(&database_url()).await.unwrap();
    
    // Verify triggers exist
    let triggers: Vec<String> = sqlx::query_scalar(
        "SELECT tgname FROM pg_trigger WHERE tgname LIKE '%_notify'"
    )
    .fetch_all(provider.pool())
    .await
    .unwrap();
    
    assert!(triggers.contains(&"trg_orch_notify".to_string()));
    assert!(triggers.contains(&"trg_worker_notify".to_string()));
}
```

#### 2. NOTIFY Emission Tests

```rust
#[tokio::test]
async fn test_notify_emitted_on_orchestrator_insert() {
    let provider = PostgresProvider::new_with_long_poll(&database_url()).await.unwrap();
    
    // Set up listener
    let mut listener = PgListener::connect_with(provider.pool()).await.unwrap();
    listener.listen(&format!("{}_orch_work", provider.schema_name())).await.unwrap();
    
    // Insert work item
    provider.enqueue_for_orchestrator(
        WorkItem::StartOrchestration { 
            instance: "test-1".to_string(),
            orchestration: "Test".to_string(),
            version: "1.0".to_string(),
            input: "{}".to_string(),
            execution_id: 1,
        },
        None,
    ).await.unwrap();
    
    // Should receive notification
    let notification = tokio::time::timeout(
        Duration::from_secs(5),
        listener.recv()
    ).await.unwrap().unwrap();
    
    assert!(notification.channel().ends_with("_orch_work"));
}

#[tokio::test]
async fn test_notify_emitted_on_worker_insert() {
    // Similar test for worker queue
}

#[tokio::test]
async fn test_notify_payload_contains_visible_at() {
    let provider = PostgresProvider::new_with_long_poll(&database_url()).await.unwrap();
    
    let mut listener = PgListener::connect_with(provider.pool()).await.unwrap();
    listener.listen(&format!("{}_orch_work", provider.schema_name())).await.unwrap();
    
    // Insert timer with future visible_at
    let fire_at = SystemTime::now() + Duration::from_secs(60);
    let fire_at_ms = fire_at.duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
    
    provider.enqueue_for_orchestrator(
        WorkItem::TimerFired {
            instance: "test-1".to_string(),
            timer_id: 1,
            fire_at_ms,
        },
        None,
    ).await.unwrap();
    
    let notification = tokio::time::timeout(
        Duration::from_secs(5),
        listener.recv()
    ).await.unwrap().unwrap();
    
    let payload_ms: i64 = notification.payload().parse().unwrap();
    assert!(payload_ms > 0);
    // Should be approximately the fire_at_ms (within 1 second)
    assert!((payload_ms - fire_at_ms as i64).abs() < 1000);
}
```

### Integration Tests

#### 3. Long-Poll Wait Behavior

```rust
#[tokio::test]
async fn test_fetch_waits_when_no_work() {
    let provider = PostgresProvider::new_with_options(
        &database_url(),
        None,
        LongPollConfig {
            enabled: true,
            fallback_poll_interval: Duration::from_secs(300),
            max_wait_time: Duration::from_secs(2), // Short for testing
            ..Default::default()
        },
    ).await.unwrap();
    
    let start = Instant::now();
    let result = provider.fetch_orchestration_item(Duration::from_secs(30)).await.unwrap();
    let elapsed = start.elapsed();
    
    assert!(result.is_none());
    // Should have waited approximately max_wait_time
    assert!(elapsed >= Duration::from_secs(1));
    assert!(elapsed <= Duration::from_secs(3));
}

#[tokio::test]
async fn test_fetch_returns_immediately_when_work_available() {
    let provider = PostgresProvider::new_with_long_poll(&database_url()).await.unwrap();
    
    // Pre-insert work
    provider.enqueue_for_orchestrator(
        WorkItem::StartOrchestration { /* ... */ },
        None,
    ).await.unwrap();
    
    let start = Instant::now();
    let result = provider.fetch_orchestration_item(Duration::from_secs(30)).await.unwrap();
    let elapsed = start.elapsed();
    
    assert!(result.is_some());
    // Should return very quickly
    assert!(elapsed < Duration::from_millis(500));
}
```

#### 4. Wake on NOTIFY Tests

```rust
#[tokio::test]
async fn test_fetch_wakes_on_notify() {
    let provider = PostgresProvider::new_with_long_poll(&database_url()).await.unwrap();
    let provider_clone = provider.clone();
    
    // Spawn fetch task (will wait)
    let fetch_handle = tokio::spawn(async move {
        let start = Instant::now();
        let result = provider_clone.fetch_orchestration_item(Duration::from_secs(30)).await;
        (start.elapsed(), result)
    });
    
    // Wait a bit then insert work
    sleep(Duration::from_millis(500)).await;
    provider.enqueue_for_orchestrator(
        WorkItem::StartOrchestration { /* ... */ },
        None,
    ).await.unwrap();
    
    let (elapsed, result) = fetch_handle.await.unwrap();
    
    assert!(result.unwrap().is_some());
    // Should have woken up quickly after insert (not waiting full timeout)
    assert!(elapsed < Duration::from_secs(2));
    assert!(elapsed >= Duration::from_millis(400)); // At least the sleep time
}
```

#### 5. Timer Wake Tests

```rust
#[tokio::test]
async fn test_timer_wakes_at_correct_time() {
    let provider = PostgresProvider::new_with_long_poll(&database_url()).await.unwrap();
    
    // Schedule timer for 2 seconds from now
    let fire_at = SystemTime::now() + Duration::from_secs(2);
    let fire_at_ms = fire_at.duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
    
    provider.enqueue_for_orchestrator(
        WorkItem::TimerFired {
            instance: "timer-test".to_string(),
            timer_id: 1,
            fire_at_ms,
        },
        None,
    ).await.unwrap();
    
    let start = Instant::now();
    
    // First fetch should wait for timer
    let result = provider.fetch_orchestration_item(Duration::from_secs(30)).await.unwrap();
    let elapsed = start.elapsed();
    
    assert!(result.is_some());
    // Should have waited approximately 2 seconds
    assert!(elapsed >= Duration::from_millis(1800));
    assert!(elapsed <= Duration::from_millis(2500));
}
```

#### 6. Competing Consumer Tests

```rust
#[tokio::test]
async fn test_only_one_dispatcher_wakes_per_notify() {
    let provider = PostgresProvider::new_with_long_poll(&database_url()).await.unwrap();
    let wake_count = Arc::new(AtomicU32::new(0));
    
    // Spawn 4 dispatchers that count wakes
    let mut handles = vec![];
    for _ in 0..4 {
        let p = provider.clone();
        let counter = wake_count.clone();
        handles.push(tokio::spawn(async move {
            let start = Instant::now();
            let _ = p.fetch_orchestration_item(Duration::from_secs(30)).await;
            let elapsed = start.elapsed();
            
            // If we woke up quickly (< 1s), we got the wake token
            if elapsed < Duration::from_secs(1) {
                counter.fetch_add(1, Ordering::SeqCst);
            }
        }));
    }
    
    // Wait for all to be waiting
    sleep(Duration::from_millis(200)).await;
    
    // Insert one work item - should only wake ONE dispatcher
    provider.enqueue_for_orchestrator(
        WorkItem::StartOrchestration { /* ... */ },
        None,
    ).await.unwrap();
    
    // Wait for dispatchers to process
    sleep(Duration::from_millis(500)).await;
    
    // Only ONE dispatcher should have woken up
    assert_eq!(wake_count.load(Ordering::SeqCst), 1, 
        "Only one dispatcher should receive wake token");
}

#[tokio::test]
async fn test_multiple_dispatchers_single_winner() {
    let provider = PostgresProvider::new_with_long_poll(&database_url()).await.unwrap();
    
    // Spawn 4 dispatchers
    let mut handles = vec![];
    for i in 0..4 {
        let p = provider.clone();
        handles.push(tokio::spawn(async move {
            let result = p.fetch_orchestration_item(Duration::from_secs(30)).await;
            (i, result)
        }));
    }
    
    // Wait for all to be waiting
    sleep(Duration::from_millis(200)).await;
    
    // Insert one work item
    provider.enqueue_for_orchestrator(
        WorkItem::StartOrchestration { /* ... */ },
        None,
    ).await.unwrap();
    
    // Wait for all to complete
    let results: Vec<_> = futures::future::join_all(handles)
        .await
        .into_iter()
        .map(|r| r.unwrap())
        .collect();
    
    // Exactly one should have gotten work
    let winners: Vec<_> = results.iter()
        .filter(|(_, r)| r.as_ref().unwrap().is_some())
        .collect();
    
    assert_eq!(winners.len(), 1, "Exactly one dispatcher should win");
}
```

### Stress Tests

#### 15. High-Throughput with Long-Poll

```rust
#[tokio::test]
#[ignore] // Run with --ignored
async fn stress_test_long_poll_throughput() {
    let provider = PostgresProvider::new_with_long_poll(&database_url()).await.unwrap();
    
    let config = StressTestConfig {
        max_concurrent: 20,
        duration_secs: 30,
        tasks_per_instance: 5,
        activity_delay_ms: 10,
        orch_concurrency: 4,
        worker_concurrency: 4,
    };
    
    // Run stress test
    let result = run_stress_test(provider, config).await;
    
    assert_eq!(result.failed, 0);
    assert!(result.completed > 0);
    println!("Throughput: {} orch/sec", result.completed as f64 / 30.0);
}
```

#### 16. Idle Query Load Test

```rust
#[tokio::test]
#[ignore]
async fn stress_test_idle_query_load() {
    let provider = PostgresProvider::new_with_long_poll(&database_url()).await.unwrap();
    
    // Reset pg_stat_statements
    sqlx::query("SELECT pg_stat_statements_reset()")
        .execute(provider.pool())
        .await
        .ok();
    
    // Spawn 4 idle dispatchers
    let mut handles = vec![];
    for _ in 0..4 {
        let p = provider.clone();
        handles.push(tokio::spawn(async move {
            for _ in 0..10 {
                let _ = p.fetch_orchestration_item(Duration::from_secs(30)).await;
            }
        }));
    }
    
    // Let them run for 30 seconds (should be mostly waiting)
    sleep(Duration::from_secs(30)).await;
    
    // Query pg_stat_statements for call counts
    let stats: Vec<(String, i64)> = sqlx::query_as(
        "SELECT query, calls FROM pg_stat_statements WHERE query LIKE '%fetch_orchestration_item%'"
    )
    .fetch_all(provider.pool())
    .await
    .unwrap();
    
    // Should have very few queries (mostly just the fallback polls)
    let total_calls: i64 = stats.iter().map(|(_, c)| c).sum();
    
    // With 4 dispatchers, 30 second max_wait, we expect ~4 polls
    // Allow some margin for initial polls and timing
    assert!(total_calls < 20, "Too many queries during idle: {}", total_calls);
}
```

### Aggressive Mode State Machine Tests

#### 7. Enter Aggressive Mode on Wake

```rust
#[tokio::test]
async fn test_enters_aggressive_mode_on_notify() {
    let provider = PostgresProvider::new_with_long_poll(&database_url()).await.unwrap();
    
    // Initially not in aggressive mode
    assert!(!provider.is_aggressive_mode_orch());
    
    // Trigger a wake event
    provider.enqueue_for_orchestrator(
        WorkItem::StartOrchestration { /* ... */ },
        None,
    ).await.unwrap();
    
    // Fetch should wake and enter aggressive mode
    let _ = provider.fetch_orchestration_item(Duration::from_secs(30)).await;
    
    // Should now be in aggressive mode
    assert!(provider.is_aggressive_mode_orch());
}
```

#### 8. Enter Aggressive Mode on Fetch Error

```rust
#[tokio::test]
async fn test_enters_aggressive_mode_on_db_error() {
    // This test requires ability to inject errors - may need mock
    let provider = PostgresProvider::new_with_long_poll(&database_url()).await.unwrap();
    
    // Simulate DB error (e.g., by dropping connection pool)
    // After error, should be in aggressive mode
    
    // Verify aggressive mode entered
    assert!(provider.is_aggressive_mode_orch());
}
```

#### 9. Stay in Aggressive Mode While Errors Continue

```rust
#[tokio::test]
async fn test_stays_aggressive_during_intermittent_errors() {
    let provider = PostgresProvider::new_with_options(
        &database_url(),
        None,
        LongPollConfig {
            enabled: true,
            aggressive_poll_duration: Duration::from_secs(2),
            aggressive_exit_threshold: 3,
            ..Default::default()
        },
    ).await.unwrap();
    
    // Enter aggressive mode
    provider.enter_aggressive_mode_orch(AggressiveReason::NotifyReceived);
    
    // Simulate: success, error, success, error pattern
    // Should NOT exit because consecutive_empty_successes keeps resetting
    provider.record_fetch_outcome(FetchOutcome::Success { found_work: false }); // count = 1
    provider.record_fetch_outcome(FetchOutcome::Error);                          // count = 0
    provider.record_fetch_outcome(FetchOutcome::Success { found_work: false }); // count = 1
    provider.record_fetch_outcome(FetchOutcome::Error);                          // count = 0
    
    // Wait past aggressive_poll_duration
    sleep(Duration::from_secs(3)).await;
    
    // Should still be in aggressive mode (not enough consecutive successes)
    assert!(provider.is_aggressive_mode_orch());
}
```

#### 10. Exit Aggressive Mode After Stable Empty Results

```rust
#[tokio::test]
async fn test_exits_aggressive_after_consecutive_empty_successes() {
    let provider = PostgresProvider::new_with_options(
        &database_url(),
        None,
        LongPollConfig {
            enabled: true,
            aggressive_poll_duration: Duration::from_secs(1),
            aggressive_exit_threshold: 3,
            ..Default::default()
        },
    ).await.unwrap();
    
    // Enter aggressive mode
    provider.enter_aggressive_mode_orch(AggressiveReason::NotifyReceived);
    assert!(provider.is_aggressive_mode_orch());
    
    // Wait past time threshold
    sleep(Duration::from_secs(2)).await;
    
    // Record 3 consecutive successful empty fetches
    for _ in 0..3 {
        let exited = provider.should_exit_aggressive(&FetchOutcome::Success { found_work: false });
        if exited {
            break;
        }
    }
    
    // Should have exited aggressive mode
    assert!(!provider.is_aggressive_mode_orch());
}
```

#### 11. Stay Aggressive While Finding Work

```rust
#[tokio::test]
async fn test_stays_aggressive_while_processing_work() {
    let provider = PostgresProvider::new_with_options(
        &database_url(),
        None,
        LongPollConfig {
            enabled: true,
            aggressive_poll_duration: Duration::from_secs(1),
            aggressive_exit_threshold: 3,
            ..Default::default()
        },
    ).await.unwrap();
    
    // Enter aggressive mode
    provider.enter_aggressive_mode_orch(AggressiveReason::NotifyReceived);
    
    // Wait past time threshold
    sleep(Duration::from_secs(2)).await;
    
    // Keep finding work - should NOT exit
    provider.should_exit_aggressive(&FetchOutcome::Success { found_work: true });
    provider.should_exit_aggressive(&FetchOutcome::Success { found_work: false });
    provider.should_exit_aggressive(&FetchOutcome::Success { found_work: true }); // Resets counter
    
    // Should still be in aggressive mode (counter keeps resetting)
    assert!(provider.is_aggressive_mode_orch());
}
```

#### 11b. Enter Aggressive on NOTIFY Delivery Timeout

```rust
#[tokio::test]
async fn test_enters_aggressive_when_notify_delivery_times_out() {
    let provider = PostgresProvider::new_with_options(
        &database_url(),
        None,
        LongPollConfig {
            enabled: true,
            notify_delivery_timeout: Duration::from_millis(50),
            ..Default::default()
        },
    ).await.unwrap();
    
    // Simulate all dispatchers being busy by not having any receivers
    // (In real test, would spawn busy dispatchers)
    
    // Insert work - NOTIFY will be sent but no dispatcher to receive
    provider.enqueue_for_orchestrator(
        WorkItem::StartOrchestration { /* ... */ },
        None,
    ).await.unwrap();
    
    // Wait for delivery timeout
    sleep(Duration::from_millis(100)).await;
    
    // Check the delivery failed flag
    assert!(provider.check_notify_delivery_failed_orch());
    
    // Next dispatcher to check should enter aggressive mode
    // (This would be tested via full integration test with runtime)
}
```

### Failure Mode Tests

#### 12. Listener Reconnection

```rust
#[tokio::test]
async fn test_listener_reconnects_on_failure() {
    let provider = PostgresProvider::new_with_long_poll(&database_url()).await.unwrap();
    
    // TODO: Simulate connection drop (requires test infrastructure)
    // Verify listener reconnects and notifications resume
}
```

#### 13. Graceful Degradation

```rust
#[tokio::test]
async fn test_falls_back_to_polling_on_listener_failure() {
    // Test that fetch still works even if LISTEN connection fails
    // System should enter aggressive mode and poll normally
}
```

### Backward Compatibility Tests

#### 14. Default Behavior Unchanged

```rust
#[tokio::test]
async fn test_default_provider_does_not_long_poll() {
    let provider = PostgresProvider::new(&database_url()).await.unwrap();
    
    let start = Instant::now();
    let result = provider.fetch_orchestration_item(Duration::from_secs(30)).await.unwrap();
    let elapsed = start.elapsed();
    
    assert!(result.is_none());
    // Should return immediately (no waiting)
    assert!(elapsed < Duration::from_millis(500));
}
```

## Migration Guide

### Enabling Long-Poll

```rust
// Before (short-polling)
let provider = PostgresProvider::new(&database_url).await?;

// After (long-polling enabled)
let provider = PostgresProvider::new_with_long_poll(&database_url).await?;
```

### Custom Configuration

```rust
let config = LongPollConfig {
    enabled: true,
    fallback_poll_interval: Duration::from_secs(600),  // 10 minutes
    max_wait_time: Duration::from_secs(60),            // 1 minute cycles
    timer_grace_period: Duration::from_millis(50),     // Tighter timing
};

let provider = PostgresProvider::new_with_options(
    &database_url,
    Some("my_schema"),
    config,
).await?;
```

### Connection Pool Considerations

Long-polling adds one dedicated connection for the listener. Adjust pool size if needed:

```bash
# Before: 10 connections for queries
DUROXIDE_PG_POOL_MAX=10

# After: 10 for queries + 1 for listener
DUROXIDE_PG_POOL_MAX=11
```

## Performance Comparison

| Configuration | Idle Load (4 dispatchers) | Detection Latency | Notes |
|---------------|---------------------------|-------------------|-------|
| Short-poll (50ms) | 160 q/s | 0-50ms avg | Current default |
| Short-poll (1s) | 8 q/s | 0-1000ms avg | Simple tuning |
| **Long-poll** | **~0.01 q/s** | **<5ms** | This design |

## Future Enhancements

1. **Partitioned Notifications**: Use instance_id hash to reduce thundering herd
2. **Adaptive Timeouts**: Adjust based on workload patterns
3. **Metrics Export**: Query load, wake reasons, latency histograms
4. **Leader Election**: Single dispatcher polls on behalf of all

## References

- [PostgreSQL LISTEN/NOTIFY](https://www.postgresql.org/docs/current/sql-notify.html)
- [sqlx PgListener](https://docs.rs/sqlx/latest/sqlx/postgres/struct.PgListener.html)
- [tokio broadcast channel](https://docs.rs/tokio/latest/tokio/sync/broadcast/index.html)

