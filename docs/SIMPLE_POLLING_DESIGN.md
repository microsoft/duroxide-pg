# Simple Long Polling Design

> A minimalist approach to reducing PostgreSQL query load using LISTEN/NOTIFY.

## Problem

The current provider polls the database every 50ms (default), generating ~160 queries/second even when idle. We want to reduce this to near-zero without introducing complex architecture.

## Solution: "Notify & Wake"

Instead of complex competing consumer channels or state machines, we use a simple **broadcast** model.

1. **Shared Listener**: A single background task listens for PostgreSQL notifications.
2. **Broadcast Wake**: When a notification arrives, we wake **all** waiting dispatchers using `tokio::sync::Notify`.
3. **Database Locking**: We rely on PostgreSQL's `SKIP LOCKED` to ensure only one dispatcher claims the work. The others will wake up, find nothing, and go back to sleep.

### Architecture

```
┌─────────────────────────────────────────────────────────────────────────┐
│                              PostgreSQL                                  │
│                                                                         │
│   INSERT trigger ──► pg_notify('orch_work', 'visible_at_timestamp')     │
└─────────────────────────────────────────────────────────────────────────┘
                                    │
                                    │ NOTIFY
                                    ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                         PostgresProvider                                 │
│                                                                         │
│  ┌─────────────────────┐      ┌──────────────────────────────────────┐  │
│  │    Listener Task    │      │             Shared State             │  │
│  │                     │      │                                      │  │
│  │  1. Recv NOTIFY     │─────►│  next_orch_wake: AtomicI64           │  │
│  │  2. Update Atomic   │      │  orch_notify: tokio::sync::Notify    │  │
│  │  3. notify_waiters()│──────│                                      │  │
│  └─────────────────────┘      └──────────────────┬───────────────────┘  │
│                                                  │                      │
│                                                  │ notified()           │
│                                                  ▼                      │
│  ┌───────────────────────────────────────────────────────────────────┐  │
│  │                  fetch_orchestration_item()                       │  │
│  │                                                                   │  │
│  │  1. Try Fetch (SELECT ... SKIP LOCKED)                            │  │
│  │  2. If Found: Return Some(item)                                   │  │
│  │  3. If None:                                                      │  │
│  │     a. Calculate wait = min(next_orch_wake, max_poll_interval)    │  │
│  │     b. Wait for (notify OR timeout)                               │  │
│  │     c. Try Fetch again                                            │  │
│  │     d. Return Result (Some or None)                               │  │
│  └───────────────────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────────────────┘
```

## Key Simplifications

1.  **No Competing Consumer Logic**: We don't try to wake exactly one dispatcher. We wake everyone.
    *   *Why?* `SELECT ... FOR UPDATE SKIP LOCKED` is already atomic. If 4 dispatchers wake up, 1 gets the row, 3 get nothing. This is safe and simple.
2.  **No Separate Timer Task**: The dispatchers themselves handle the waiting.
    *   *Why?* We just calculate the sleep duration based on the `next_orch_wake` timestamp updated by the listener.
3.  **No Aggressive Mode State Machine**:
    *   *Why?* If we wake up and find nothing (e.g., stolen by another node), we just return `None`. The runtime will sleep its short `idle_sleep` (50ms) and call us again. This naturally handles "aggressive" retries without custom logic.

## Implementation Details

### 1. Shared State

```rust
pub struct PostgresProvider {
    // ... existing fields ...
    
    // Long-polling state
    long_poll_enabled: bool,
    orch_notify: Arc<tokio::sync::Notify>,
    worker_notify: Arc<tokio::sync::Notify>,
    
    // Timestamp of next scheduled item (from NOTIFY payload)
    next_orch_wake: Arc<AtomicI64>,
    next_worker_wake: Arc<AtomicI64>,
}
```

### 2. Listener Loop

```rust
async fn listener_loop(
    pool: PgPool, 
    orch_notify: Arc<Notify>, 
    next_orch_wake: Arc<AtomicI64>
) {
    let mut listener = PgListener::connect_with(&pool).await.unwrap();
    listener.listen("orch_work").await.unwrap();

    loop {
        if let Ok(notification) = listener.recv().await {
            // Parse payload (visible_at timestamp)
            if let Ok(visible_at) = notification.payload().parse::<i64>() {
                // Update next wake time if this is sooner
                fetch_min(next_orch_wake, visible_at);
            }
            
            // Wake everyone to check
            orch_notify.notify_waiters();
        }
    }
}
```

### 3. Fetch Logic

```rust
async fn fetch_orchestration_item(&self, lock_timeout: Duration) -> Result<Option<Item>> {
    // 1. Try immediate fetch
    if let Some(item) = self.try_fetch_immediate(lock_timeout).await? {
        return Ok(Some(item));
    }

    if !self.long_poll_enabled {
        return Ok(None);
    }

    // 2. Calculate wait time
    let now = current_time_ms();
    let next_wake = self.next_orch_wake.load(Ordering::Relaxed);
    
    let wait_duration = if next_wake > now {
        Duration::from_millis((next_wake - now) as u64)
    } else {
        Duration::from_secs(60) // Default long poll
    };
    
    // Cap at max poll interval (e.g. 1 min) to ensure we eventually check anyway
    let wait_duration = wait_duration.min(Duration::from_secs(60));

    // 3. Wait for notification OR timeout
    tokio::select! {
        _ = self.orch_notify.notified() => {},
        _ = tokio::time::sleep(wait_duration) => {},
    }

    // 4. Try fetch one last time
    self.try_fetch_immediate(lock_timeout).await
}
```

## Pros & Cons

**Pros:**
*   **Drastically Simpler**: ~100 lines of code vs ~500+.
*   **Robust**: Relies on standard PG locking and Tokio primitives.
*   **Low Idle Load**: 0 queries while waiting.

**Cons:**
*   **Thundering Herd**: All dispatchers wake on every INSERT.
    *   *Mitigation*: In most deployments, N is small (2-10). `SKIP LOCKED` handles this efficiently. If N is huge (100+), you should be running fewer dispatchers or multiple nodes anyway.
*   **Spurious Wakes**: If a timer is set for 1 hour, we might wake up, see it's not time, and go back to sleep. (Handled by `next_orch_wake` check).

## Migration

1.  Add `LISTEN/NOTIFY` triggers (same as previous design).
2.  Update `PostgresProvider` with `Notify` and `AtomicI64`.
3.  Spawn listener task in `new()`.
4.  Update `fetch_*` methods to wait on `Notify`.
