// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use anyhow::{ensure, Context, Result};
use duroxide::providers::ProviderError;
use sqlx::postgres::{types::Oid, PgListener, PgNotification};
use sqlx::{pool::PoolConnection, PgPool, Postgres};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::{sleep, sleep_until, timeout_at, Instant};
use tracing::warn;

use crate::provider::PostgresProvider;

const MIN_RETRY_INTERVAL: Duration = Duration::from_millis(100);

/// Optional LISTEN/NOTIFY-based waiting for orchestration and activity work.
///
/// Disabled by default. When enabled, one connection from the provider's pool
/// is reserved for LISTEN, so the pool must allow at least two connections.
///
/// `ApplyAll` construction also activates notification triggers for the schema.
/// `VerifyOnly` requires that a privileged `ApplyAll` provider has done this
/// first. Turning this option off does not deactivate a schema's triggers.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct LongPollConfig {
    /// Enable notification-driven waiting. Default: `false`.
    pub enabled: bool,
    /// Rescan future work and wake waiting fetches to recover missed
    /// notifications. Must be nonzero when enabled. Default: 60 seconds.
    pub notifier_poll_interval: Duration,
    /// Delay added to future visibility timestamps to avoid waking early.
    /// Default: 100 milliseconds.
    pub timer_grace_period: Duration,
}

impl Default for LongPollConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            notifier_poll_interval: Duration::from_secs(60),
            timer_grace_period: Duration::from_millis(100),
        }
    }
}

impl LongPollConfig {
    pub(crate) fn validate(&self, max_connections: u32) -> Result<()> {
        if self.enabled {
            ensure!(
                max_connections >= 2,
                "long-polling requires at least two pool connections: one is reserved for LISTEN"
            );
            ensure!(
                !self.notifier_poll_interval.is_zero(),
                "long-polling notifier_poll_interval must be nonzero"
            );
            deadline_after(self.notifier_poll_interval)?;
            deadline_after(self.timer_grace_period)?;
        }
        Ok(())
    }
}

pub(crate) struct LongPoller {
    pub(crate) orch_notify: Arc<Notify>,
    pub(crate) worker_notify: Arc<Notify>,
    task: JoinHandle<()>,
}

impl LongPoller {
    pub(crate) async fn start(
        pool: PgPool,
        schema: &str,
        config: LongPollConfig,
    ) -> Result<Option<Self>> {
        if !config.enabled {
            return Ok(None);
        }

        // Resolve the same unquoted identifiers used by provider SQL. Table OIDs
        // avoid identifier folding/truncation and NOTIFY's channel-length limit.
        let (orch_oid, worker_oid): (Oid, Oid) = sqlx::query_as(
            "SELECT $1::pg_catalog.text::pg_catalog.regclass::pg_catalog.oid, \
                    $2::pg_catalog.text::pg_catalog.regclass::pg_catalog.oid",
        )
        .bind(format!("{schema}.orchestrator_queue"))
        .bind(format!("{schema}.worker_queue"))
        .fetch_one(&pool)
        .await?;
        let channels = [
            format!("duroxide_pg_{}", orch_oid.0),
            format!("duroxide_pg_{}", worker_oid.0),
        ];
        let listener = subscribe(&pool, &channels).await?;
        let orch_notify = Arc::new(Notify::new());
        let worker_notify = Arc::new(Notify::new());
        let notifications = [orch_notify.clone(), worker_notify.clone()];
        let timer_query = format!(
            "SELECT \
             (SELECT CEIL(EXTRACT(EPOCH FROM visible_at) * 1000)::BIGINT \
              FROM {schema}.orchestrator_queue \
              WHERE visible_at > TO_TIMESTAMP($1::BIGINT / 1000.0) AND lock_token IS NULL \
              ORDER BY visible_at LIMIT 1), \
             (SELECT CEIL(EXTRACT(EPOCH FROM visible_at) * 1000)::BIGINT \
              FROM {schema}.worker_queue \
              WHERE visible_at > TO_TIMESTAMP($1::BIGINT / 1000.0) AND lock_token IS NULL \
              ORDER BY visible_at LIMIT 1)"
        );
        let task = tokio::spawn(run(
            pool,
            listener,
            channels,
            notifications,
            timer_query,
            config,
        ));
        Ok(Some(Self {
            orch_notify,
            worker_notify,
            task,
        }))
    }

    pub(crate) fn stop(&self) {
        self.task.abort();
    }
}

impl Drop for LongPoller {
    fn drop(&mut self) {
        self.stop();
    }
}

async fn subscribe(pool: &PgPool, channels: &[String; 2]) -> Result<PgListener> {
    let mut listener = PgListener::connect_with(pool).await?;
    // Detect loss before SQLx silently reconnects, so timers and ready work can
    // be recovered after resubscribing.
    listener.eager_reconnect(false);
    listener
        .listen_all(channels.iter().map(String::as_str))
        .await?;
    Ok(listener)
}

async fn run(
    pool: PgPool,
    mut listener: PgListener,
    channels: [String; 2],
    notifications: [Arc<Notify>; 2],
    timer_query: String,
    config: LongPollConfig,
) {
    loop {
        let result = run_connected(
            &pool,
            &mut listener,
            &channels,
            &notifications,
            &timer_query,
            &config,
        )
        .await;
        for notify in &notifications {
            notify.notify_waiters();
        }
        if pool.is_closed() {
            return;
        }
        match result {
            Ok(()) => warn!(
                target: "duroxide::providers::postgres::notifier",
                "LISTEN connection lost; reconnecting"
            ),
            Err(error) => warn!(
                target: "duroxide::providers::postgres::notifier",
                error = %error,
                "Long-poll listener failed; reconnecting"
            ),
        }
        drop(listener);
        loop {
            tokio::select! {
                _ = pool.close_event() => {
                    for notify in &notifications {
                        notify.notify_waiters();
                    }
                    return;
                }
                _ = sleep(Duration::from_secs(1)) => {}
            }
            match subscribe(&pool, &channels).await {
                Ok(reconnected) => {
                    listener = reconnected;
                    for notify in &notifications {
                        notify.notify_waiters();
                    }
                    break;
                }
                Err(error) => warn!(
                    target: "duroxide::providers::postgres::notifier",
                    error = %error,
                    "Long-poll reconnect failed; fetch timeouts retain polling fallback"
                ),
            }
        }
    }
}

struct RefreshConnection(Option<PoolConnection<Postgres>>);

impl Drop for RefreshConnection {
    fn drop(&mut self) {
        // SQLx's normal pool return pings the connection, which can wait forever
        // for a cancelled query. Discard only unfinished refresh connections.
        if let Some(connection) = self.0.take() {
            drop(connection.detach());
        }
    }
}

async fn refresh_timers(pool: &PgPool, query: &str) -> Result<(Option<i64>, Option<i64>)> {
    let mut connection = RefreshConnection(Some(pool.acquire().await?));
    let result = sqlx::query_as(query)
        .bind(PostgresProvider::now_millis())
        .fetch_one(&mut **connection.0.as_mut().expect("refresh owns its connection"))
        .await?;
    drop(connection.0.take());
    Ok(result)
}

async fn run_connected(
    pool: &PgPool,
    listener: &mut PgListener,
    channels: &[String; 2],
    notifications: &[Arc<Notify>; 2],
    timer_query: &str,
    config: &LongPollConfig,
) -> Result<()> {
    let mut timers: [Option<Instant>; 2] = [None, None];
    let mut next_refresh = Instant::now();
    let mut next_recovery = next_refresh;
    let mut refresh_started = next_refresh;
    let refresh = refresh_timers(pool, timer_query);
    tokio::pin!(refresh);
    let mut refreshing = true;
    // A paced scan must recheck work that matured since a queue's timer wake.
    // Keep events during an in-flight scan pending for the following scan.
    let mut pending_catchup = [false; 2];
    let mut refreshing_catchup = [false; 2];

    loop {
        let next_timer = timers.iter().flatten().copied().min();
        tokio::select! {
            _ = pool.close_event() => return Ok(()),
            notification = listener.try_recv() => {
                let Some(notification) = notification? else {
                    return Ok(());
                };
                handle_notification(notification, channels, notifications, &mut timers, config);
            }
            result = &mut refresh, if refreshing => {
                refreshing = false;
                let recover = Instant::now() >= next_recovery;
                if recover {
                    next_recovery = deadline_after(config.notifier_poll_interval)?;
                }
                next_refresh = next_recovery;
                match result {
                    Ok((orch, worker)) => {
                        for (index, timestamp) in [orch, worker].into_iter().enumerate() {
                            // Recovery must also find ready work whose notification
                            // was lost. Timer-only scans need not wake the other queue.
                            if recover || refreshing_catchup[index] {
                                notifications[index].notify_waiters();
                            }
                            if let Some(timestamp) = timestamp {
                                pending_catchup[index] |= timestamp <= PostgresProvider::now_millis();
                                schedule(timestamp, &notifications[index], &mut timers[index], config);
                            }
                        }
                        if pending_catchup.iter().any(|pending| *pending) {
                            next_refresh = next_refresh.min(
                                (refresh_started + MIN_RETRY_INTERVAL).max(Instant::now())
                            );
                        }
                    }
                    Err(error) => {
                        for (pending, attempted) in pending_catchup.iter_mut().zip(refreshing_catchup) {
                            *pending |= attempted;
                        }
                        warn!(
                            target: "duroxide::providers::postgres::notifier",
                            error = %error,
                            "Long-poll timer refresh failed; fetch timeouts retain polling fallback"
                        );
                    }
                }
            }
            _ = sleep_until(next_refresh), if !refreshing => {
                refresh_started = Instant::now();
                refreshing_catchup = std::mem::take(&mut pending_catchup);
                refresh.set(refresh_timers(pool, timer_query));
                refreshing = true;
            }
            _ = sleep_until(next_timer.unwrap_or(next_refresh)), if next_timer.is_some() => {
                let now = Instant::now();
                for (index, timer) in timers.iter_mut().enumerate() {
                    if timer.is_some_and(|deadline| deadline <= now) {
                        *timer = None;
                        pending_catchup[index] = true;
                        notifications[index].notify_waiters();
                    }
                }
                // Only the earliest timer per queue is retained. Rescan after it
                // fires to discover the next one without an unbounded timer heap.
                next_refresh = next_refresh.min(
                    (refresh_started + MIN_RETRY_INTERVAL).max(now)
                );
            }
        }
    }
}

fn handle_notification(
    notification: PgNotification,
    channels: &[String; 2],
    notifications: &[Arc<Notify>; 2],
    timers: &mut [Option<Instant>; 2],
    config: &LongPollConfig,
) {
    let Some(index) = channels
        .iter()
        .position(|channel| channel == notification.channel())
    else {
        warn!(
            target: "duroxide::providers::postgres::notifier",
            "Ignoring a notification from an unexpected channel"
        );
        return;
    };
    match notification.payload().parse::<i64>() {
        Ok(timestamp) => schedule(timestamp, &notifications[index], &mut timers[index], config),
        Err(error) => {
            warn!(
                target: "duroxide::providers::postgres::notifier",
                error = %error,
                "Invalid long-poll timestamp; waking fetches to recheck the queue"
            );
            notifications[index].notify_waiters();
        }
    }
}

fn schedule(timestamp: i64, notify: &Notify, timer: &mut Option<Instant>, config: &LongPollConfig) {
    let now = PostgresProvider::now_millis();
    if timestamp <= now {
        notify.notify_waiters();
        return;
    }
    let delay = Duration::from_millis(timestamp.saturating_sub(now) as u64)
        .checked_add(config.timer_grace_period);
    let deadline = delay.and_then(|delay| Instant::now().checked_add(delay));
    match deadline {
        Some(deadline) => *timer = Some(timer.map_or(deadline, |current| current.min(deadline))),
        None => warn!(
            target: "duroxide::providers::postgres::notifier",
            "Long-poll timer exceeds the clock range; relying on polling fallback"
        ),
    }
}

fn deadline_after(duration: Duration) -> Result<Instant> {
    Instant::now()
        .checked_add(duration)
        .context("long-poll duration exceeds the supported clock range")
}

pub(crate) async fn fetch_with_poll<T, F, Fut>(
    notify: Option<&Notify>,
    poll_timeout: Duration,
    operation: &'static str,
    mut fetch: F,
) -> Result<Option<T>, ProviderError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Option<T>, ProviderError>>,
{
    let Some(notify) = notify.filter(|_| !poll_timeout.is_zero()) else {
        return fetch().await;
    };
    let deadline = Instant::now().checked_add(poll_timeout).ok_or_else(|| {
        ProviderError::permanent(operation, "poll_timeout exceeds the supported clock range")
    })?;
    let mut earliest_retry = Instant::now();
    loop {
        let notified = notify.notified();
        tokio::pin!(notified);
        // Register before reading the queue so a concurrent commit cannot be lost.
        notified.as_mut().enable();
        let result = fetch().await?;
        if result.is_some() {
            return Ok(result);
        }
        if Instant::now() >= deadline || timeout_at(deadline, notified).await.is_err() {
            return Ok(None);
        }
        // The first wake is immediate; repeated unsuccessful wakes coalesce.
        sleep_until(earliest_retry.min(deadline)).await;
        if Instant::now() >= deadline {
            return Ok(None);
        }
        earliest_retry = Instant::now() + MIN_RETRY_INTERVAL;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn defaults_preserve_short_polling() {
        let config = LongPollConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.notifier_poll_interval, Duration::from_secs(60));
        assert_eq!(config.timer_grace_period, Duration::from_millis(100));
        config.validate(1).unwrap();
    }

    #[test]
    fn enabled_configuration_requires_usable_pool_and_timers() {
        let mut config = LongPollConfig {
            enabled: true,
            ..Default::default()
        };
        assert!(config.validate(1).unwrap_err().to_string().contains("two"));
        config.validate(2).unwrap();
        config.notifier_poll_interval = Duration::ZERO;
        assert!(config.validate(2).is_err());
        config.notifier_poll_interval = Duration::MAX;
        assert!(config.validate(2).is_err());
        config.notifier_poll_interval = Duration::from_secs(60);
        config.timer_grace_period = Duration::MAX;
        assert!(config.validate(2).is_err());
    }

    #[tokio::test]
    async fn empty_long_poll_fetches_once_without_busy_polling() {
        let notify = Notify::new();
        let calls = AtomicUsize::new(0);
        let start = Instant::now();
        let result = fetch_with_poll(Some(&notify), Duration::from_millis(50), "test", || async {
            calls.fetch_add(1, Ordering::Relaxed);
            Ok(None::<()>)
        })
        .await
        .unwrap();
        assert!(result.is_none());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(start.elapsed() >= Duration::from_millis(50));
    }

    #[tokio::test]
    async fn notification_between_empty_read_and_wait_is_not_lost() {
        let notify = Notify::new();
        let calls = AtomicUsize::new(0);
        let result = fetch_with_poll(
            Some(&notify),
            Duration::from_millis(100),
            "test",
            || async {
                if calls.fetch_add(1, Ordering::Relaxed) == 0 {
                    notify.notify_waiters();
                    Ok(None)
                } else {
                    Ok(Some(7))
                }
            },
        )
        .await
        .unwrap();
        assert_eq!(result, Some(7));
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn repeated_unmatched_notifications_do_not_extend_deadline() {
        let notify = Notify::new();
        let calls = AtomicUsize::new(0);
        let start = Instant::now();
        let result = fetch_with_poll(Some(&notify), Duration::from_millis(30), "test", || async {
            calls.fetch_add(1, Ordering::Relaxed);
            notify.notify_waiters();
            Ok(None::<()>)
        })
        .await
        .unwrap();
        assert!(result.is_none());
        assert!(calls.load(Ordering::Relaxed) > 1);
        assert!(start.elapsed() < Duration::from_millis(300));
    }

    #[tokio::test]
    async fn repeated_empty_wakes_are_paced_but_the_first_wake_is_immediate() {
        let notify = Notify::new();
        let calls = std::sync::Mutex::new(Vec::new());
        let result = fetch_with_poll(
            Some(&notify),
            Duration::from_millis(350),
            "test",
            || async {
                calls.lock().unwrap().push(Instant::now());
                notify.notify_waiters();
                Ok(None::<()>)
            },
        )
        .await
        .unwrap();
        assert!(result.is_none());
        let calls = calls.lock().unwrap();
        assert!(
            (2..=5).contains(&calls.len()),
            "fetch count: {}",
            calls.len()
        );
        assert!(calls[1].duration_since(calls[0]) < MIN_RETRY_INTERVAL);
        for pair in calls[1..].windows(2) {
            assert!(pair[1].duration_since(pair[0]) >= MIN_RETRY_INTERVAL);
        }
    }

    #[tokio::test]
    async fn dense_overdue_refreshes_are_paced_and_do_not_wake_the_other_queue() {
        dotenvy::dotenv().ok();
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("DATABASE_URL not set; skipping timer-refresh database test");
            return;
        };
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(3)
            .connect(&url)
            .await
            .unwrap();
        let schema = format!("timer_scan_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(&format!("CREATE SEQUENCE {schema}.calls"))
            .execute(&pool)
            .await
            .unwrap();
        let channels = [format!("{schema}_orch"), format!("{schema}_worker")];
        let mut listener = subscribe(&pool, &channels).await.unwrap();
        let notifications = [Arc::new(Notify::new()), Arc::new(Notify::new())];
        let config = LongPollConfig {
            enabled: true,
            ..Default::default()
        };
        // A future timestamp matures during every query, as with dense timers
        // and nontrivial round-trip latency. Count scans without global resets.
        let query = format!(
            "SELECT $1::BIGINT + 1, NULL::BIGINT FROM pg_catalog.pg_sleep(0.005), \
             pg_catalog.nextval('{schema}.calls')",
        );
        let mut worker_wakes = 0;
        {
            let run = run_connected(
                &pool,
                &mut listener,
                &channels,
                &notifications,
                &query,
                &config,
            );
            tokio::pin!(run);
            let duration = tokio::time::timeout(Duration::from_millis(350), async {
                loop {
                    tokio::select! {
                        result = &mut run => panic!("listener stopped unexpectedly: {result:?}"),
                        _ = notifications[1].notified() => worker_wakes += 1,
                    }
                }
            });
            assert!(duration.await.is_err());
        }
        let calls: i64 = sqlx::query_scalar(&format!("SELECT last_value FROM {schema}.calls"))
            .fetch_one(&pool)
            .await
            .unwrap();
        drop(listener);
        sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
        assert!((2..=5).contains(&calls), "refreshes in 350ms: {calls}");
        assert_eq!(
            worker_wakes, 1,
            "only the initial recovery should wake the empty worker queue"
        );
    }

    #[tokio::test]
    async fn zero_timeout_and_disabled_mode_fetch_once() {
        let notify = Notify::new();
        for (notify, timeout) in [
            (Some(&notify), Duration::ZERO),
            (None, Duration::from_secs(30)),
        ] {
            let result = tokio::time::timeout(
                Duration::from_millis(100),
                fetch_with_poll(notify, timeout, "test", || async { Ok(None::<()>) }),
            )
            .await
            .expect("short polling must not wait")
            .unwrap();
            assert!(result.is_none());
        }
    }

    #[tokio::test]
    async fn a_claim_completing_after_the_wait_deadline_is_still_returned() {
        let notify = Notify::new();
        let result = fetch_with_poll(Some(&notify), Duration::from_millis(10), "test", || async {
            sleep(Duration::from_millis(30)).await;
            Ok(Some("claimed"))
        })
        .await
        .unwrap();
        assert_eq!(result, Some("claimed"));
    }

    #[tokio::test]
    async fn fetch_errors_and_invalid_timeout_are_explicit() {
        let notify = Notify::new();
        let error = fetch_with_poll(Some(&notify), Duration::from_secs(30), "test", || async {
            Err::<Option<()>, _>(ProviderError::retryable("test", "unavailable"))
        })
        .await
        .unwrap_err();
        assert!(error.is_retryable());
        let error = fetch_with_poll(Some(&notify), Duration::MAX, "test", || async {
            Ok(None::<()>)
        })
        .await
        .unwrap_err();
        assert!(!error.is_retryable());
    }
}
