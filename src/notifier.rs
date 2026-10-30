// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use anyhow::{ensure, Context, Result};
use duroxide::providers::ProviderError;
use sqlx::postgres::{types::Oid, PgListener, PgNotification};
use sqlx::{pool::PoolConnection, Connection, PgPool, Postgres};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::{interval_at, sleep, sleep_until, timeout_at, Instant, MissedTickBehavior};
use tracing::warn;

use crate::provider::PostgresProvider;

const MIN_RETRY_INTERVAL: Duration = Duration::from_millis(100);
const DEGRADED_RETRY_INTERVAL: Duration = Duration::from_secs(1);
const TIMER_REFRESH_BACKSTOP: Duration = Duration::from_secs(1);

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
            "SELECT LEAST(\
             (SELECT CEIL(EXTRACT(EPOCH FROM visible_at) * 1000)::BIGINT \
              FROM {schema}.orchestrator_queue \
              WHERE visible_at > TO_TIMESTAMP($1::BIGINT / 1000.0) \
              ORDER BY visible_at LIMIT 1), \
             (SELECT locked_until FROM {schema}.instance_locks \
              WHERE locked_until > $1 ORDER BY locked_until LIMIT 1)), \
             LEAST(\
             (SELECT CEIL(EXTRACT(EPOCH FROM visible_at) * 1000)::BIGINT \
              FROM {schema}.worker_queue \
              WHERE visible_at > TO_TIMESTAMP($1::BIGINT / 1000.0) \
              ORDER BY visible_at LIMIT 1), \
             (SELECT MIN(locked_until) FROM {schema}.worker_queue \
              WHERE lock_token IS NOT NULL AND locked_until > $1), \
             (SELECT MIN(locked_until) FROM {schema}.sessions WHERE locked_until > $1))"
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
    // LISTEN must not consume the pool's only warm connection.
    drop(
        pool.acquire()
            .await
            .context("Warming a fetch connection after LISTEN")?,
    );
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
    let mut refresh = TimerRefresh::default();
    loop {
        let result = run_connected(
            &pool,
            &mut listener,
            &channels,
            &notifications,
            &timer_query,
            &config,
            &mut refresh,
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
        let reconnect = async {
            loop {
                sleep(DEGRADED_RETRY_INTERVAL).await;
                match subscribe(&pool, &channels).await {
                    Ok(listener) => break listener,
                    Err(error) => warn!(
                        target: "duroxide::providers::postgres::notifier",
                        error = %error,
                        "Long-poll reconnect failed; continuing one-second polling fallback"
                    ),
                }
            }
        };
        tokio::pin!(reconnect);
        let mut fallback = interval_at(
            Instant::now() + DEGRADED_RETRY_INTERVAL,
            DEGRADED_RETRY_INTERVAL,
        );
        fallback.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = pool.close_event() => {
                    for notify in &notifications {
                        notify.notify_waiters();
                    }
                    return;
                }
                _ = fallback.tick() => {
                    for notify in &notifications {
                        notify.notify_waiters();
                    }
                }
                reconnected = &mut reconnect => {
                    listener = reconnected;
                    for notify in &notifications {
                        notify.notify_waiters();
                    }
                    break;
                }
            }
        }
    }
}

type TimerDeadlines = (Option<i64>, Option<i64>);

#[derive(Default)]
struct TimerRefresh {
    task: Option<JoinHandle<Result<TimerDeadlines>>>,
}

impl TimerRefresh {
    fn start(&mut self, pool: &PgPool, query: &str) {
        // Keep the same scan across listener reconnects and client watchdog wakes.
        if self.task.is_none() {
            let pool = pool.clone();
            let query = query.to_string();
            self.task = Some(tokio::spawn(
                async move { refresh_timers(&pool, &query).await },
            ));
        }
    }

    async fn result(&mut self) -> Result<TimerDeadlines> {
        let result = self.task.as_mut().expect("timer scan was started").await;
        self.task.take();
        result.context("Long-poll timer scan task failed")?
    }
}

impl Drop for TimerRefresh {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
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

async fn refresh_timers(pool: &PgPool, query: &str) -> Result<TimerDeadlines> {
    let mut connection = RefreshConnection(Some(pool.acquire().await?));
    let mut tx = connection
        .0
        .as_mut()
        .expect("refresh owns its connection")
        .begin()
        .await?;
    let result: Result<_> = async {
        sqlx::Executor::execute(
            &mut *tx,
            "SET LOCAL lock_timeout = '200ms'; SET LOCAL statement_timeout = '200ms'",
        )
        .await
        .context("Setting long-poll timer scan timeouts")?;
        sqlx::query_as(query)
            .bind(PostgresProvider::now_millis())
            .fetch_one(&mut *tx)
            .await
            .context("Reading long-poll timer deadlines")
    }
    .await;
    match &result {
        Ok(_) => tx
            .commit()
            .await
            .context("Committing long-poll timer scan")?,
        Err(error) => tx
            .rollback()
            .await
            .with_context(|| format!("Rolling back long-poll timer scan after {error:#}"))?,
    }
    drop(connection.0.take());
    result
}

async fn run_connected(
    pool: &PgPool,
    listener: &mut PgListener,
    channels: &[String; 2],
    notifications: &[Arc<Notify>; 2],
    timer_query: &str,
    config: &LongPollConfig,
    refresh: &mut TimerRefresh,
) -> Result<()> {
    let mut timers: [Option<Instant>; 2] = [None, None];
    let mut next_refresh = Instant::now();
    let mut next_recovery = next_refresh;
    let mut refresh_started = next_refresh;
    refresh.start(pool, timer_query);
    let mut refreshing = true;
    let mut refresh_failed = false;
    let mut refresh_stalled = false;
    let mut next_backstop = refresh_started + TIMER_REFRESH_BACKSTOP;
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
            result = refresh.result(), if refreshing => {
                refreshing = false;
                let recover = refresh_failed || Instant::now() >= next_recovery;
                if recover {
                    next_recovery = deadline_after(config.notifier_poll_interval)?;
                }
                next_refresh = next_recovery;
                match result {
                    Ok((orch, worker)) => {
                        refresh_failed = false;
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
                        refresh_failed = true;
                        next_refresh = (refresh_started + DEGRADED_RETRY_INTERVAL).max(Instant::now());
                        for notify in notifications {
                            notify.notify_waiters();
                        }
                        for (pending, attempted) in pending_catchup.iter_mut().zip(refreshing_catchup) {
                            *pending |= attempted;
                        }
                        warn!(
                            target: "duroxide::providers::postgres::notifier",
                            error = %format_args!("{error:#}"),
                            "Long-poll timer refresh failed; retrying with one-second polling fallback"
                        );
                    }
                }
            }
            _ = sleep_until(next_backstop), if refreshing => {
                if !refresh_stalled {
                    warn!(
                        target: "duroxide::providers::postgres::notifier",
                        "Long-poll timer scan is still running; polling without cancelling or starting another scan"
                    );
                }
                refresh_stalled = true;
                refresh_failed = true;
                for notify in notifications {
                    notify.notify_waiters();
                }
                next_backstop = Instant::now() + DEGRADED_RETRY_INTERVAL;
            }
            _ = sleep_until(next_refresh), if !refreshing => {
                refresh_started = Instant::now();
                refreshing_catchup = std::mem::take(&mut pending_catchup);
                refresh.start(pool, timer_query);
                refreshing = true;
                refresh_stalled = false;
                next_backstop = refresh_started + TIMER_REFRESH_BACKSTOP;
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
    use tokio::time::timeout;

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
    async fn timer_scan_timeouts_are_server_errors_and_reuse_the_connection() {
        dotenvy::dotenv().ok();
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("DATABASE_URL not set; skipping timer-scan database test");
            return;
        };
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap();
        let control = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .unwrap();
        let schema = format!("bounded_scan_{}", uuid::Uuid::new_v4().simple());
        sqlx::Executor::execute(
            &control,
            format!(
                "CREATE SCHEMA {schema}; CREATE TABLE {schema}.probe (id INTEGER);
             INSERT INTO {schema}.probe VALUES (1)"
            )
            .as_str(),
        )
        .await
        .unwrap();
        let before: (i32, String, String) = sqlx::query_as(
            "SELECT pg_backend_pid(), current_setting('lock_timeout'), current_setting('statement_timeout')",
        ).fetch_one(&pool).await.unwrap();
        let valid = "SELECT $1::BIGINT, NULL::BIGINT";
        assert!(refresh_timers(&pool, valid).await.unwrap().0.is_some());
        let mut blocker = control.begin().await.unwrap();
        sqlx::query(&format!(
            "LOCK TABLE {schema}.probe IN ACCESS EXCLUSIVE MODE"
        ))
        .execute(&mut *blocker)
        .await
        .unwrap();
        let mut outcomes = Vec::new();
        for query in [
            "SELECT $1::BIGINT, NULL::BIGINT FROM pg_sleep(2)".to_string(),
            format!("SELECT $1::BIGINT, NULL::BIGINT FROM {schema}.probe"),
        ] {
            let error = timeout(Duration::from_secs(1), refresh_timers(&pool, &query))
                .await
                .expect("server timeout should beat the client backstop")
                .unwrap_err();
            let after: (i32, String, String) = sqlx::query_as(
                "SELECT pg_backend_pid(), current_setting('lock_timeout'), current_setting('statement_timeout')",
            ).fetch_one(&pool).await.unwrap();
            outcomes.push((error, after));
        }
        blocker.rollback().await.unwrap();
        let rows: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {schema}.probe"))
            .fetch_one(&control)
            .await
            .unwrap();
        assert!(refresh_timers(&pool, valid).await.unwrap().0.is_some());
        pool.close().await;
        sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
            .execute(&control)
            .await
            .unwrap();
        control.close().await;
        assert_eq!(rows, 1, "failed scans must not change the durable state");
        for (error, after) in outcomes {
            let code = error
                .downcast_ref::<sqlx::Error>()
                .and_then(sqlx::Error::as_database_error)
                .and_then(|error| error.code());
            assert!(
                matches!(code.as_deref(), Some("55P03" | "57014")),
                "{error:#}"
            );
            assert_eq!(
                after, before,
                "timeouts must roll back and return the same healthy connection"
            );
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
            let mut refresh = TimerRefresh::default();
            let run = run_connected(
                &pool,
                &mut listener,
                &channels,
                &notifications,
                &query,
                &config,
                &mut refresh,
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
    async fn paced_catchup_wakes_waiters_when_no_future_timestamp_remains() {
        dotenvy::dotenv().ok();
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("DATABASE_URL not set; skipping catch-up database test");
            return;
        };
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .unwrap();
        let control = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .unwrap();
        let schema = format!("catchup_{}", uuid::Uuid::new_v4().simple());
        let gate_key = i64::from_be_bytes(uuid::Uuid::new_v4().as_bytes()[..8].try_into().unwrap());
        sqlx::Executor::execute(
            &control,
            format!(
                "CREATE SCHEMA {schema};
             CREATE SEQUENCE {schema}.calls;
             CREATE FUNCTION {schema}.scan(p_now BIGINT) RETURNS TABLE(orch BIGINT, worker BIGINT)
             LANGUAGE plpgsql AS $$
             DECLARE n BIGINT := nextval('{schema}.calls');
             BEGIN
                 IF n = 2 THEN
                     PERFORM pg_advisory_xact_lock({gate_key});
                 END IF;
                 RETURN QUERY SELECT CASE WHEN n = 1 THEN p_now + 20 END, NULL::BIGINT;
             END $$"
            )
            .as_str(),
        )
        .await
        .unwrap();
        let mut gate = control.begin().await.unwrap();
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(gate_key)
            .execute(&mut *gate)
            .await
            .unwrap();
        let channels = [format!("{schema}_orch"), format!("{schema}_worker")];
        let mut listener = subscribe(&pool, &channels).await.unwrap();
        let notifications = [Arc::new(Notify::new()), Arc::new(Notify::new())];
        let task = tokio::spawn({
            let pool = pool.clone();
            let notifications = notifications.clone();
            let query = format!("SELECT * FROM {schema}.scan($1)");
            async move {
                let mut refresh = TimerRefresh::default();
                run_connected(
                    &pool,
                    &mut listener,
                    &channels,
                    &notifications,
                    &query,
                    &LongPollConfig {
                        enabled: true,
                        ..Default::default()
                    },
                    &mut refresh,
                )
                .await
            }
        });
        let pid = timeout(Duration::from_secs(2), async {
            loop {
                let pid: Option<i32> = sqlx::query_scalar(
                    "SELECT pid FROM pg_stat_activity WHERE wait_event_type = 'Lock' \
                     AND strpos(query, $1) > 0",
                )
                .bind(format!("{schema}.scan"))
                .fetch_optional(&control)
                .await
                .unwrap();
                if let Some(pid) = pid {
                    break pid;
                }
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the catch-up scan must reach the gate");
        // Register after the timer's wake but before its catch-up scan completes.
        let notified = notifications[0].notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        gate.rollback().await.unwrap();
        let wake = timeout(Duration::from_secs(1), notified).await;
        let scan_succeeded: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE pid = $1 AND state = 'idle')",
        )
        .bind(pid)
        .fetch_one(&control)
        .await
        .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        pool.close().await;
        sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
            .execute(&control)
            .await
            .unwrap();
        control.close().await;
        wake.expect("a catch-up scan must wake work that matured before its snapshot");
        assert!(
            scan_succeeded,
            "a scan timeout must not substitute for the catch-up wake"
        );
    }

    #[tokio::test]
    async fn failed_refresh_retries_before_the_recovery_interval() {
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
        let schema = format!("failed_scan_{}", uuid::Uuid::new_v4().simple());
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
        let query = format!(
            "WITH attempt AS MATERIALIZED (SELECT nextval('{schema}.calls') AS n) \
             SELECT 1 / (n - 1), NULL::BIGINT FROM attempt WHERE $1::BIGINT IS NOT NULL"
        );
        let mut refresh = TimerRefresh::default();
        let result = tokio::time::timeout(
            Duration::from_millis(1500),
            run_connected(
                &pool,
                &mut listener,
                &channels,
                &notifications,
                &query,
                &LongPollConfig {
                    enabled: true,
                    ..Default::default()
                },
                &mut refresh,
            ),
        )
        .await;
        drop(refresh);
        assert!(
            result.is_err(),
            "the listener should stay connected after a scan error"
        );
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
        assert!(
            calls >= 2,
            "a failed scan waited for the 60-second recovery: {calls}"
        );
    }

    #[tokio::test]
    async fn subscribing_keeps_a_warm_fetch_connection() {
        dotenvy::dotenv().ok();
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("DATABASE_URL not set; skipping listener database test");
            return;
        };
        let pool = sqlx::postgres::PgPoolOptions::new()
            .min_connections(1)
            .max_connections(2)
            .connect(&url)
            .await
            .unwrap();
        let channel = format!("warm_listener_{}", uuid::Uuid::new_v4().simple());
        let listener = subscribe(
            &pool,
            &[format!("{channel}_orch"), format!("{channel}_worker")],
        )
        .await
        .unwrap();
        let connections = pool.size();
        drop(listener);
        pool.close().await;
        assert_eq!(connections, 2, "LISTEN must leave another connection warm");
    }

    #[tokio::test]
    async fn watchdog_and_reconnect_keep_the_same_pending_timer_scan() {
        use std::sync::atomic::AtomicBool;

        dotenvy::dotenv().ok();
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("DATABASE_URL not set; skipping pending-scan database test");
            return;
        };
        let block_next = Arc::new(AtomicBool::new(false));
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let pool = sqlx::postgres::PgPoolOptions::new()
            .min_connections(3)
            .max_connections(3)
            .before_acquire({
                let block_next = block_next.clone();
                let entered = entered.clone();
                let release = release.clone();
                move |_, _| {
                    let block_next = block_next.clone();
                    let entered = entered.clone();
                    let release = release.clone();
                    Box::pin(async move {
                        if block_next.swap(false, Ordering::Relaxed) {
                            let released = release.notified();
                            tokio::pin!(released);
                            released.as_mut().enable();
                            entered.notify_one();
                            released.await;
                        }
                        Ok(true)
                    })
                }
            })
            .connect(&url)
            .await
            .unwrap();
        let control = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .unwrap();
        let schema = format!("pending_scan_{}", uuid::Uuid::new_v4().simple());
        sqlx::Executor::execute(
            &control,
            format!("CREATE SCHEMA {schema}; CREATE SEQUENCE {schema}.calls").as_str(),
        )
        .await
        .unwrap();
        let channels = [format!("{schema}_orch"), format!("{schema}_worker")];
        let listener = subscribe(&pool, &channels).await.unwrap();
        let pid: i32 = sqlx::query_scalar(
            "SELECT pid FROM pg_stat_activity WHERE query LIKE 'LISTEN%' AND strpos(query, $1) > 0",
        )
        .bind(&channels[0])
        .fetch_one(&control)
        .await
        .unwrap();
        let notifications = [Arc::new(Notify::new()), Arc::new(Notify::new())];
        let initial_scan = notifications[0].notified();
        tokio::pin!(initial_scan);
        initial_scan.as_mut().enable();
        let task = tokio::spawn(run(
            pool.clone(), listener, channels.clone(), notifications.clone(),
            format!("SELECT NULL::BIGINT, NULL::BIGINT FROM nextval('{schema}.calls') WHERE $1::BIGINT IS NOT NULL"),
            LongPollConfig {
                enabled: true,
                notifier_poll_interval: Duration::from_millis(50),
                ..Default::default()
            },
        ));
        timeout(Duration::from_secs(3), initial_scan).await.unwrap();
        block_next.store(true, Ordering::Relaxed);
        timeout(Duration::from_secs(3), entered.notified())
            .await
            .unwrap();
        for _ in 0..2 {
            timeout(Duration::from_millis(1500), notifications[0].notified())
                .await
                .expect("the watchdog must wake fetches while a scan is pending");
        }
        let before_reconnect: i64 =
            sqlx::query_scalar(&format!("SELECT last_value FROM {schema}.calls"))
                .fetch_one(&control)
                .await
                .unwrap();
        sqlx::query("SELECT pg_terminate_backend($1)")
            .bind(pid)
            .execute(&control)
            .await
            .unwrap();
        timeout(Duration::from_secs(3), async {
            loop {
                let reconnected: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE query LIKE 'LISTEN%' \
                     AND strpos(query, $1) > 0 AND pid <> $2)",
                )
                .bind(&channels[0])
                .bind(pid)
                .fetch_one(&control)
                .await
                .unwrap();
                if reconnected {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("listener must reconnect without replacing the pending scan");
        let after_reconnect: i64 =
            sqlx::query_scalar(&format!("SELECT last_value FROM {schema}.calls"))
                .fetch_one(&control)
                .await
                .unwrap();
        release.notify_waiters();
        timeout(Duration::from_secs(3), async {
            loop {
                let calls: i64 =
                    sqlx::query_scalar(&format!("SELECT last_value FROM {schema}.calls"))
                        .fetch_one(&control)
                        .await
                        .unwrap();
                if calls >= 2 {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the original scan must resume once connection acquisition finishes");
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        timeout(Duration::from_secs(3), pool.close()).await.unwrap();
        sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
            .execute(&control)
            .await
            .unwrap();
        control.close().await;
        assert_eq!(
            before_reconnect, 1,
            "the watchdog must not replace the blocked scan"
        );
        assert_eq!(
            after_reconnect, 1,
            "reconnecting must not replace the blocked scan"
        );
    }

    #[tokio::test]
    async fn fallback_wakes_fetches_while_reconnect_is_pending() {
        use std::sync::atomic::AtomicBool;

        dotenvy::dotenv().ok();
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("DATABASE_URL not set; skipping reconnect database test");
            return;
        };
        let blocked = Arc::new(AtomicBool::new(false));
        let reconnecting = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let pool = sqlx::postgres::PgPoolOptions::new()
            .min_connections(2)
            .max_connections(2)
            .before_acquire({
                let blocked = blocked.clone();
                let reconnecting = reconnecting.clone();
                let release = release.clone();
                move |_, _| {
                    let blocked = blocked.clone();
                    let reconnecting = reconnecting.clone();
                    let release = release.clone();
                    Box::pin(async move {
                        if blocked.load(Ordering::Relaxed) {
                            let released = release.notified();
                            tokio::pin!(released);
                            released.as_mut().enable();
                            reconnecting.notify_one();
                            released.await;
                        }
                        Ok(true)
                    })
                }
            })
            .connect(&url)
            .await
            .unwrap();
        let control = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap();
        let channel = format!("pending_reconnect_{}", uuid::Uuid::new_v4().simple());
        let channels = [format!("{channel}_orch"), format!("{channel}_worker")];
        let listener = subscribe(&pool, &channels).await.unwrap();
        let pid: i32 = sqlx::query_scalar(
            "SELECT pid FROM pg_stat_activity WHERE query LIKE 'LISTEN%' AND strpos(query, $1) > 0",
        )
        .bind(&channels[0])
        .fetch_one(&control)
        .await
        .unwrap();
        let notifications = [Arc::new(Notify::new()), Arc::new(Notify::new())];
        let initial_scan = notifications[0].notified();
        tokio::pin!(initial_scan);
        initial_scan.as_mut().enable();
        let task = tokio::spawn(run(
            pool.clone(),
            listener,
            channels,
            notifications.clone(),
            "SELECT NULL::BIGINT, NULL::BIGINT WHERE $1::BIGINT IS NOT NULL".to_string(),
            LongPollConfig {
                enabled: true,
                ..Default::default()
            },
        ));
        tokio::time::timeout(Duration::from_secs(3), initial_scan)
            .await
            .expect("initial scan must finish");
        blocked.store(true, Ordering::Relaxed);
        sqlx::query("SELECT pg_terminate_backend($1)")
            .bind(pid)
            .execute(&control)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), reconnecting.notified())
            .await
            .expect("reconnect must enter the blocked acquire hook");
        let result = tokio::time::timeout(Duration::from_millis(2500), async {
            for notify in &notifications {
                let calls = AtomicUsize::new(0);
                let item =
                    fetch_with_poll(Some(notify), Duration::from_secs(10), "test", || async {
                        Ok((calls.fetch_add(1, Ordering::Relaxed) != 0).then_some("ready"))
                    })
                    .await
                    .unwrap();
                assert_eq!(item, Some("ready"));
                assert_eq!(calls.load(Ordering::Relaxed), 2);
            }
        })
        .await;
        blocked.store(false, Ordering::Relaxed);
        release.notify_waiters();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(3), pool.close())
            .await
            .unwrap();
        control.close().await;
        result.expect("fallback wakes must continue independently of a pending reconnect");
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
