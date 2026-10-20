// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use chrono::{DateTime, Utc};
use duroxide::provider_validation::long_polling;
use duroxide::providers::{
    DispatcherCapabilityFilter, ExecutionMetadata, Provider, SessionFetchConfig, TagFilter,
    WorkItem,
};
use duroxide_pg::{LongPollConfig, MigrationPolicy, PostgresProvider, ProviderConfig};
use sqlx::postgres::{types::Oid, PgPoolOptions};
use sqlx::PgPool;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;

const LOCK: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_secs(5);

fn database_url() -> String {
    dotenvy::dotenv().ok();
    std::env::var("DATABASE_URL").expect("DATABASE_URL must be set")
}

fn schema_name() -> String {
    format!("lp_{}", uuid::Uuid::new_v4().simple())
}

fn polling_config(enabled: bool) -> LongPollConfig {
    let mut config = LongPollConfig::default();
    config.enabled = enabled;
    config
}

fn activity(instance: &str, session: Option<&str>, tag: Option<&str>) -> WorkItem {
    WorkItem::ActivityExecute {
        instance: instance.to_string(),
        execution_id: 1,
        id: 1,
        name: "Test".to_string(),
        input: String::new(),
        session_id: session.map(str::to_string),
        tag: tag.map(str::to_string),
    }
}

#[derive(Clone, Copy, Debug)]
enum Queue {
    Orchestration,
    Worker,
}

#[derive(Debug)]
struct Claim {
    token: String,
    attempt: u32,
}

impl Queue {
    fn table(self) -> &'static str {
        match self {
            Self::Orchestration => "orchestrator_queue",
            Self::Worker => "worker_queue",
        }
    }

    fn fetch_name(self) -> &'static str {
        match self {
            Self::Orchestration => "fetch_orchestration_item",
            Self::Worker => "fetch_work_item",
        }
    }

    async fn enqueue(self, fixture: &Fixture, instance: &str, delay: Option<Duration>) {
        match self {
            Self::Orchestration => fixture
                .provider
                .enqueue_for_orchestrator(
                    WorkItem::StartOrchestration {
                        instance: instance.to_string(),
                        orchestration: "Test".to_string(),
                        version: Some("1.0.0".to_string()),
                        input: String::new(),
                        parent_instance: None,
                        parent_id: None,
                        parent_execution_id: None,
                        execution_id: 1,
                    },
                    delay,
                )
                .await
                .unwrap(),
            Self::Worker => {
                if let Some(delay) = delay {
                    // The worker enqueue API has no initial-delay parameter.
                    let now = Utc::now();
                    sqlx::query(&format!(
                        "INSERT INTO {}.worker_queue \
                         (work_item, visible_at, created_at, instance_id, execution_id, activity_id) \
                         VALUES ($1, $2, $3, $4, 1, 1)",
                        fixture.schema
                    ))
                    .bind(serde_json::to_string(&activity(instance, None, None)).unwrap())
                    .bind(now + chrono::Duration::from_std(delay).unwrap())
                    .bind(now)
                    .bind(instance)
                    .execute(&fixture.control)
                    .await
                    .unwrap();
                } else {
                    fixture
                        .provider
                        .enqueue_for_worker(activity(instance, None, None))
                        .await
                        .unwrap();
                }
            }
        }
    }

    async fn fetch(self, provider: &PostgresProvider, timeout: Duration) -> Option<Claim> {
        match self {
            Self::Orchestration => provider
                .fetch_orchestration_item(LOCK, timeout, None)
                .await
                .unwrap()
                .map(|(_, token, attempt)| Claim { token, attempt }),
            Self::Worker => provider
                .fetch_work_item(LOCK, timeout, None, &TagFilter::Any)
                .await
                .unwrap()
                .map(|(_, token, attempt)| Claim { token, attempt }),
        }
    }

    async fn abandon(self, provider: &PostgresProvider, token: &str, delay: Option<Duration>) {
        match self {
            Self::Orchestration => provider
                .abandon_orchestration_item(token, delay, false)
                .await
                .unwrap(),
            Self::Worker => provider
                .abandon_work_item(token, delay, false)
                .await
                .unwrap(),
        }
    }

    async fn ack(self, provider: &PostgresProvider, token: &str) {
        match self {
            Self::Orchestration => provider
                .ack_orchestration_item(
                    token,
                    1,
                    vec![],
                    vec![],
                    vec![],
                    ExecutionMetadata::default(),
                    vec![],
                )
                .await
                .unwrap(),
            Self::Worker => provider.ack_work_item(token, None).await.unwrap(),
        }
    }
}

struct Fixture {
    provider: Arc<PostgresProvider>,
    control: PgPool,
    schema: String,
}

impl Fixture {
    async fn new(config: LongPollConfig) -> Self {
        Self::with_schema(config, schema_name()).await
    }

    async fn enabled() -> Self {
        let mut config = polling_config(true);
        config.timer_grace_period = Duration::from_millis(20);
        Self::new(config).await
    }

    async fn with_schema(long_poll: LongPollConfig, schema: String) -> Self {
        let enabled = long_poll.enabled;
        let mut config = ProviderConfig::url(database_url());
        config.schema_name = Some(schema.clone());
        config.long_poll = long_poll;
        let provider = Arc::new(PostgresProvider::new_with_config(config).await.unwrap());
        let control = PgPoolOptions::new()
            .max_connections(2)
            .connect(&database_url())
            .await
            .unwrap();
        let fixture = Self {
            provider,
            control,
            schema,
        };
        if enabled {
            fixture
                .wait_for_query(
                    &format!("FROM {schema}.worker_queue", schema = fixture.schema),
                    None,
                )
                .await;
            tokio::task::yield_now().await;
        }
        fixture
    }

    async fn wait_for_query(&self, query: &str, since: Option<DateTime<Utc>>) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let seen: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM pg_stat_activity \
                 WHERE datname = current_database() AND state = 'idle' \
                 AND strpos(query, $1) > 0 AND ($2::TIMESTAMPTZ IS NULL OR query_start >= $2))",
            )
            .bind(query)
            .bind(since)
            .fetch_one(&self.control)
            .await
            .unwrap();
            if seen {
                return;
            }
            assert!(Instant::now() < deadline, "query was not observed: {query}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn stamp(&self) -> DateTime<Utc> {
        sqlx::query_scalar("SELECT clock_timestamp()")
            .fetch_one(&self.control)
            .await
            .unwrap()
    }

    async fn wait_for_blocked_refresh(&self) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let blocked: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM pg_stat_activity \
                 WHERE wait_event_type = 'Lock' AND strpos(query, $1) > 0)",
            )
            .bind(format!("FROM {}.orchestrator_queue", self.schema))
            .fetch_one(&self.control)
            .await
            .unwrap();
            if blocked {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "refresh did not block on the test lock"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn waiting_fetch(&self, queue: Queue) -> JoinHandle<Option<Claim>> {
        let since = self.stamp().await;
        let provider = self.provider.clone();
        let fetch = tokio::spawn(async move { queue.fetch(&provider, POLL).await });
        self.wait_for_query(
            &format!("{}.{}", self.schema, queue.fetch_name()),
            Some(since),
        )
        .await;
        assert!(
            !fetch.is_finished(),
            "fetch must be waiting before the producer acts"
        );
        fetch
    }

    async fn channel(&self, queue: Queue) -> String {
        let oid: Oid = sqlx::query_scalar("SELECT $1::TEXT::REGCLASS::OID")
            .bind(format!("{}.{}", self.schema, queue.table()))
            .fetch_one(&self.control)
            .await
            .unwrap();
        format!("duroxide_pg_{}", oid.0)
    }

    async fn listener_pid(&self) -> Option<i32> {
        let channel = format!("\"{}\"", self.channel(Queue::Worker).await);
        sqlx::query_scalar(
            "SELECT pid FROM pg_stat_activity WHERE datname = current_database() \
             AND query LIKE 'LISTEN%' AND strpos(query, $1) > 0",
        )
        .bind(channel)
        .fetch_optional(&self.control)
        .await
        .unwrap()
    }

    async fn cleanup(self) {
        let pool = self.provider.pool().clone();
        drop(self.provider);
        tokio::time::timeout(Duration::from_secs(5), pool.close())
            .await
            .expect("listener and refresh must release pooled connections");
        sqlx::query(&format!("DROP SCHEMA IF EXISTS {} CASCADE", self.schema))
            .execute(&self.control)
            .await
            .unwrap();
        self.control.close().await;
    }
}

async fn claimed(fetch: JoinHandle<Option<Claim>>) -> Claim {
    tokio::time::timeout(Duration::from_secs(3), fetch)
        .await
        .expect("notification/timer must beat the five-second polling fallback")
        .expect("fetch task")
        .expect("work must be found")
}

#[tokio::test]
async fn default_and_zero_timeout_preserve_short_polling() {
    for enabled in [false, true] {
        let fixture = Fixture::new(polling_config(enabled)).await;
        if !enabled {
            long_polling::test_short_poll_returns_immediately(
                fixture.provider.as_ref(),
                Duration::from_millis(500),
            )
            .await;
            long_polling::test_short_poll_work_item_returns_immediately(
                fixture.provider.as_ref(),
                Duration::from_millis(500),
            )
            .await;
            assert!(fixture.listener_pid().await.is_none());
        }
        for queue in [Queue::Orchestration, Queue::Worker] {
            let start = Instant::now();
            assert!(queue
                .fetch(&fixture.provider, Duration::ZERO)
                .await
                .is_none());
            assert!(start.elapsed() < Duration::from_secs(1));
        }
        fixture.cleanup().await;
    }
}

#[tokio::test]
async fn enabled_provider_satisfies_upstream_long_poll_contract() {
    let fixture = Fixture::enabled().await;
    long_polling::test_long_poll_waits_for_timeout(fixture.provider.as_ref()).await;
    long_polling::test_long_poll_work_item_waits_for_timeout(fixture.provider.as_ref()).await;
    long_polling::test_fetch_respects_timeout_upper_bound(fixture.provider.as_ref()).await;
    fixture.cleanup().await;
}

#[tokio::test]
async fn existing_work_returns_without_waiting() {
    let fixture = Fixture::enabled().await;
    for queue in [Queue::Orchestration, Queue::Worker] {
        queue.enqueue(&fixture, &fixture.schema, None).await;
        let start = Instant::now();
        let claim = queue.fetch(&fixture.provider, POLL).await.unwrap();
        assert!(start.elapsed() < Duration::from_secs(1));
        queue.ack(&fixture.provider, &claim.token).await;
    }
    fixture.cleanup().await;
}

#[tokio::test]
async fn orphan_cleanup_does_not_put_ready_orchestrations_to_sleep() {
    let producer = Fixture::new(LongPollConfig::default()).await;
    for instance in ["orphan-a", "orphan-b"] {
        producer
            .provider
            .enqueue_for_orchestrator(
                WorkItem::QueueMessage {
                    instance: instance.to_string(),
                    name: "signal".to_string(),
                    data: String::new(),
                },
                None,
            )
            .await
            .unwrap();
    }
    Queue::Orchestration.enqueue(&producer, "ready", None).await;
    let consumer = Fixture::with_schema(polling_config(true), producer.schema.clone()).await;
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        consumer.provider.fetch_orchestration_item(LOCK, POLL, None),
    )
    .await;
    consumer.cleanup().await;
    producer.cleanup().await;
    let (item, _, _) = result
        .expect("orphan cleanup must retry ready work rather than wait for another notification")
        .unwrap()
        .expect("ready orchestration");
    assert_eq!(item.instance, "ready");
}

#[tokio::test]
async fn inserts_wake_already_waiting_fetches_for_both_queues() {
    let fixture = Fixture::enabled().await;
    for queue in [Queue::Orchestration, Queue::Worker] {
        let fetch = fixture.waiting_fetch(queue).await;
        queue.enqueue(&fixture, &fixture.schema, None).await;
        let claim = claimed(fetch).await;
        assert_eq!(claim.attempt, 1);
        queue.ack(&fixture.provider, &claim.token).await;
    }
    fixture.cleanup().await;
}

#[tokio::test]
async fn acknowledgment_wakes_another_provider_after_it_consumed_a_locked_instances_notification() {
    let owner = Fixture::new(LongPollConfig::default()).await;
    Queue::Orchestration
        .enqueue(&owner, "same-instance", None)
        .await;
    let first = Queue::Orchestration
        .fetch(&owner.provider, Duration::ZERO)
        .await
        .unwrap();
    let waiter = Fixture::with_schema(polling_config(true), owner.schema.clone()).await;
    let waiting = waiter.waiting_fetch(Queue::Orchestration).await;
    let inserted_after = waiter.stamp().await;
    owner
        .provider
        .enqueue_for_orchestrator(
            WorkItem::ExternalRaised {
                instance: "same-instance".to_string(),
                name: "event".to_string(),
                data: "arrived during the turn".to_string(),
            },
            None,
        )
        .await
        .unwrap();
    waiter
        .wait_for_query(
            &format!("{}.fetch_orchestration_item", waiter.schema),
            Some(inserted_after),
        )
        .await;
    assert!(!waiting.is_finished(), "the instance is still locked");
    owner
        .provider
        .ack_orchestration_item(
            &first.token,
            1,
            vec![],
            vec![],
            vec![],
            ExecutionMetadata {
                orchestration_name: Some("Test".to_string()),
                orchestration_version: Some("1.0.0".to_string()),
                ..Default::default()
            },
            vec![],
        )
        .await
        .unwrap();
    let next = claimed(waiting).await;
    assert_eq!(next.attempt, 1);
    Queue::Orchestration
        .ack(&waiter.provider, &next.token)
        .await;
    waiter.cleanup().await;
    owner.cleanup().await;
}

#[tokio::test]
async fn orchestration_claims_stay_silent_but_acknowledgments_notify() {
    let fixture = Fixture::enabled().await;
    let mut listener = sqlx::postgres::PgListener::connect_with(&fixture.control)
        .await
        .unwrap();
    listener
        .listen(&fixture.channel(Queue::Orchestration).await)
        .await
        .unwrap();
    Queue::Orchestration
        .enqueue(&fixture, "claim-and-ack", None)
        .await;
    tokio::time::timeout(Duration::from_secs(3), listener.recv())
        .await
        .unwrap()
        .unwrap();
    let claim = Queue::Orchestration
        .fetch(&fixture.provider, Duration::ZERO)
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.recv())
            .await
            .is_err(),
        "claiming work must not emit an availability notification",
    );
    Queue::Orchestration
        .ack(&fixture.provider, &claim.token)
        .await;
    let notification = tokio::time::timeout(Duration::from_secs(3), listener.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(notification.payload(), "0");
    drop(listener);
    fixture.cleanup().await;
}

#[tokio::test]
async fn closely_spaced_timers_do_not_disappear_between_paced_scans() {
    for queue in [Queue::Orchestration, Queue::Worker] {
        let mut config = polling_config(true);
        config.timer_grace_period = Duration::ZERO;
        let fixture = Fixture::new(config).await;
        let first = fixture.waiting_fetch(queue).await;
        queue
            .enqueue(
                &fixture,
                "first-close-timer",
                Some(Duration::from_millis(20)),
            )
            .await;
        queue
            .enqueue(
                &fixture,
                "second-close-timer",
                Some(Duration::from_millis(40)),
            )
            .await;
        let first = claimed(first).await;
        // Keep the first item claimed so its acknowledgment cannot supply a
        // notification that would hide a missed timer wake.
        let second =
            tokio::time::timeout(Duration::from_secs(3), queue.fetch(&fixture.provider, POLL))
                .await
                .expect("the paced scan must wake work that matured since the first timer")
                .expect("second timer");
        assert_ne!(first.token, second.token);
        queue.ack(&fixture.provider, &first.token).await;
        queue.ack(&fixture.provider, &second.token).await;
        fixture.cleanup().await;
    }
}

#[tokio::test]
async fn delayed_inserts_and_successive_timers_wake_both_queues() {
    for queue in [Queue::Orchestration, Queue::Worker] {
        let fixture = Fixture::enabled().await;
        let fetch = fixture.waiting_fetch(queue).await;
        let start = Instant::now();
        queue
            .enqueue(&fixture, "first", Some(Duration::from_millis(300)))
            .await;
        queue
            .enqueue(&fixture, "second", Some(Duration::from_millis(700)))
            .await;
        let first = claimed(fetch).await;
        assert!(start.elapsed() >= Duration::from_millis(300));
        queue.ack(&fixture.provider, &first.token).await;
        let next = fixture.waiting_fetch(queue).await;
        let second = claimed(next).await;
        assert!(start.elapsed() >= Duration::from_millis(700));
        queue.ack(&fixture.provider, &second.token).await;
        fixture.cleanup().await;
    }
}

#[tokio::test]
async fn abandon_wakes_waiters_with_and_without_visibility_changes() {
    for queue in [Queue::Orchestration, Queue::Worker] {
        for delay in [None, Some(Duration::from_millis(300))] {
            let fixture = Fixture::enabled().await;
            queue.enqueue(&fixture, &fixture.schema, None).await;
            let first = queue
                .fetch(&fixture.provider, Duration::ZERO)
                .await
                .unwrap();
            let waiting = fixture.waiting_fetch(queue).await;
            let start = Instant::now();
            queue.abandon(&fixture.provider, &first.token, delay).await;
            let second = claimed(waiting).await;
            assert_eq!(second.attempt, 2);
            if let Some(delay) = delay {
                assert!(start.elapsed() >= delay);
            }
            queue.ack(&fixture.provider, &second.token).await;
            fixture.cleanup().await;
        }
    }
}

#[tokio::test]
async fn refresh_recovers_ready_and_future_work_without_notifications() {
    for queue in [Queue::Orchestration, Queue::Worker] {
        for delay in [None, Some(Duration::from_millis(300))] {
            let mut config = polling_config(true);
            config.notifier_poll_interval = Duration::from_millis(100);
            config.timer_grace_period = Duration::from_millis(20);
            let fixture = Fixture::new(config).await;
            sqlx::query(&format!(
                "ALTER TABLE {}.{} DISABLE TRIGGER USER",
                fixture.schema,
                queue.table()
            ))
            .execute(&fixture.control)
            .await
            .unwrap();
            let waiting = fixture.waiting_fetch(queue).await;
            let start = Instant::now();
            queue.enqueue(&fixture, &fixture.schema, delay).await;
            let claim = claimed(waiting).await;
            if let Some(delay) = delay {
                assert!(start.elapsed() >= delay);
            }
            queue.ack(&fixture.provider, &claim.token).await;
            fixture.cleanup().await;
        }
    }
}

#[tokio::test]
async fn reconnect_recovers_missed_work_and_resubscribes_both_channels() {
    for queue in [Queue::Orchestration, Queue::Worker] {
        let fixture = Fixture::enabled().await;
        let waiting = fixture.waiting_fetch(queue).await;
        let old_pid = fixture.listener_pid().await.expect("listener must exist");
        let terminated: bool = sqlx::query_scalar("SELECT pg_terminate_backend($1)")
            .bind(old_pid)
            .fetch_one(&fixture.control)
            .await
            .unwrap();
        assert!(terminated);
        queue.enqueue(&fixture, "during-disconnect", None).await;
        let claim = claimed(waiting).await;
        queue.ack(&fixture.provider, &claim.token).await;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if fixture
                .listener_pid()
                .await
                .is_some_and(|pid| pid != old_pid)
            {
                break;
            }
            assert!(Instant::now() < deadline, "listener did not reconnect");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let waiting = fixture.waiting_fetch(queue).await;
        queue.enqueue(&fixture, "after-reconnect", None).await;
        let claim = claimed(waiting).await;
        queue.ack(&fixture.provider, &claim.token).await;
        fixture.cleanup().await;
    }
}

#[tokio::test]
async fn cancelled_fetch_does_not_consume_later_notifications() {
    let fixture = Fixture::enabled().await;
    for queue in [Queue::Orchestration, Queue::Worker] {
        let cancelled = fixture.waiting_fetch(queue).await;
        cancelled.abort();
        assert!(cancelled.await.unwrap_err().is_cancelled());
        let waiting = fixture.waiting_fetch(queue).await;
        queue.enqueue(&fixture, &fixture.schema, None).await;
        let claim = claimed(waiting).await;
        queue.ack(&fixture.provider, &claim.token).await;
    }
    fixture.cleanup().await;
}

#[tokio::test]
async fn schema_names_do_not_constrain_notification_channel_length_or_case() {
    for schema in [
        format!("lp_{}{}", uuid::Uuid::new_v4().simple(), "x".repeat(28)),
        format!("LP_{}", uuid::Uuid::new_v4().simple()),
    ] {
        let fixture = Fixture::with_schema(polling_config(true), schema).await;
        for queue in [Queue::Orchestration, Queue::Worker] {
            assert!(fixture.channel(queue).await.len() < 63);
            let waiting = fixture.waiting_fetch(queue).await;
            queue.enqueue(&fixture, &fixture.schema, None).await;
            let claim = claimed(waiting).await;
            queue.ack(&fixture.provider, &claim.token).await;
        }
        fixture.cleanup().await;
    }
}

#[tokio::test]
async fn notification_timestamp_does_not_round_before_submillisecond_visibility() {
    let fixture = Fixture::enabled().await;
    let mut listener = sqlx::postgres::PgListener::connect_with(&fixture.control)
        .await
        .unwrap();
    listener
        .listen(&fixture.channel(Queue::Worker).await)
        .await
        .unwrap();
    sqlx::query(&format!(
        "INSERT INTO {}.worker_queue (work_item, visible_at, created_at) \
         VALUES ($1, '2100-01-01 00:00:00.000001+00', now())",
        fixture.schema,
    ))
    .bind(serde_json::to_string(&activity("fractional", None, None)).unwrap())
    .execute(&fixture.control)
    .await
    .unwrap();
    let notification = tokio::time::timeout(Duration::from_secs(3), listener.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(notification.payload(), "4102444800001");
    drop(listener);
    fixture.cleanup().await;
}

#[tokio::test]
async fn disabled_producer_wakes_verify_only_consumer() {
    let fixture = Fixture::enabled().await;
    let mut producer_config = ProviderConfig::url(database_url());
    producer_config.schema_name = Some(fixture.schema.clone());
    producer_config.migration_policy = MigrationPolicy::VerifyOnly;
    let producer = PostgresProvider::new_with_config(producer_config)
        .await
        .unwrap();
    let mut config = ProviderConfig::url(database_url());
    config.schema_name = Some(fixture.schema.clone());
    config.migration_policy = MigrationPolicy::VerifyOnly;
    config.long_poll.enabled = true;
    let consumer = Arc::new(PostgresProvider::new_with_config(config).await.unwrap());
    let since = fixture.stamp().await;
    let waiting = {
        let consumer = consumer.clone();
        tokio::spawn(async move { Queue::Worker.fetch(&consumer, POLL).await })
    };
    fixture
        .wait_for_query(&format!("{}.fetch_work_item", fixture.schema), Some(since))
        .await;
    producer
        .enqueue_for_worker(activity("mixed-config", None, None))
        .await
        .unwrap();
    let claim = claimed(waiting).await;
    Queue::Worker.ack(&consumer, &claim.token).await;
    let pool = consumer.pool().clone();
    drop(consumer);
    pool.close().await;
    let pool = producer.pool().clone();
    drop(producer);
    pool.close().await;
    fixture.cleanup().await;
}

#[tokio::test]
async fn verify_only_listener_initializes_without_ddl() {
    let fixture = Fixture::enabled().await;
    let mut url = url::Url::parse(&database_url()).unwrap();
    url.query_pairs_mut()
        .append_pair("options", "-c default_transaction_read_only=on");
    let mut config = ProviderConfig::url(url.as_str());
    config.schema_name = Some(fixture.schema.clone());
    config.migration_policy = MigrationPolicy::VerifyOnly;
    config.long_poll.enabled = true;
    let provider = PostgresProvider::new_with_config(config).await.unwrap();
    let pool = provider.pool().clone();
    drop(provider);
    pool.close().await;
    fixture.cleanup().await;
}

#[tokio::test]
async fn verify_only_requires_explicit_schema_opt_in() {
    let fixture = Fixture::new(LongPollConfig::default()).await;
    let mut config = ProviderConfig::url(database_url());
    config.schema_name = Some(fixture.schema.clone());
    config.migration_policy = MigrationPolicy::VerifyOnly;
    config.long_poll.enabled = true;
    let error = match PostgresProvider::new_with_config(config).await {
        Ok(_) => panic!("VerifyOnly must not activate notifications"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("ApplyAll"));
    let active: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_trigger WHERE tgrelid IN \
         ($1::TEXT::REGCLASS, $2::TEXT::REGCLASS) AND NOT tgisinternal AND tgenabled <> 'D'",
    )
    .bind(format!("{}.orchestrator_queue", fixture.schema))
    .bind(format!("{}.worker_queue", fixture.schema))
    .fetch_one(&fixture.control)
    .await
    .unwrap();
    assert_eq!(active, 0);
    fixture.cleanup().await;
}

#[tokio::test]
async fn pool_closure_wakes_waiters_and_releases_listener() {
    let fixture = Fixture::enabled().await;
    let since = fixture.stamp().await;
    let provider = fixture.provider.clone();
    let waiting = tokio::spawn(async move {
        provider
            .fetch_work_item(LOCK, POLL, None, &TagFilter::Any)
            .await
    });
    fixture
        .wait_for_query(&format!("{}.fetch_work_item", fixture.schema), Some(since))
        .await;
    tokio::time::timeout(Duration::from_secs(3), fixture.provider.pool().close())
        .await
        .expect("LISTEN must not prevent pool closure");
    let error = tokio::time::timeout(Duration::from_secs(2), waiting)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(error.is_retryable());
    fixture.cleanup().await;
}

#[tokio::test]
async fn pool_closure_during_reconnect_does_not_leave_fetch_waiting() {
    let fixture = Fixture::enabled().await;
    let pid = fixture.listener_pid().await.unwrap();
    let terminated: bool = sqlx::query_scalar("SELECT pg_terminate_backend($1)")
        .bind(pid)
        .fetch_one(&fixture.control)
        .await
        .unwrap();
    assert!(terminated);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let since = fixture.stamp().await;
    let provider = fixture.provider.clone();
    let waiting = tokio::spawn(async move {
        provider
            .fetch_work_item(LOCK, POLL, None, &TagFilter::Any)
            .await
    });
    fixture
        .wait_for_query(&format!("{}.fetch_work_item", fixture.schema), Some(since))
        .await;
    tokio::time::timeout(Duration::from_secs(2), fixture.provider.pool().close())
        .await
        .unwrap();
    let error = tokio::time::timeout(Duration::from_secs(2), waiting)
        .await
        .expect("pool closure must wake fetches even during reconnect backoff")
        .unwrap()
        .unwrap_err();
    assert!(error.is_retryable());
    fixture.cleanup().await;
}

#[tokio::test]
async fn ineligible_filters_do_not_wait() {
    let fixture = Fixture::enabled().await;
    let empty = DispatcherCapabilityFilter {
        supported_duroxide_versions: vec![],
    };
    let started = Instant::now();
    assert!(fixture
        .provider
        .fetch_orchestration_item(LOCK, POLL, Some(&empty))
        .await
        .unwrap()
        .is_none());
    assert!(fixture
        .provider
        .fetch_work_item(LOCK, POLL, None, &TagFilter::None)
        .await
        .unwrap()
        .is_none());
    assert!(started.elapsed() < Duration::from_millis(300));
    fixture.cleanup().await;
}

#[tokio::test]
async fn notifications_wake_the_matching_tag_waiter() {
    let fixture = Fixture::enabled().await;
    let since = fixture.stamp().await;
    let mut waits = Vec::new();
    for tag in ["red", "blue"] {
        let provider = fixture.provider.clone();
        waits.push(tokio::spawn(async move {
            provider
                .fetch_work_item(LOCK, POLL, None, &TagFilter::tags([tag]))
                .await
                .unwrap()
        }));
    }
    fixture
        .wait_for_query(&format!("{}.fetch_work_item", fixture.schema), Some(since))
        .await;
    fixture
        .provider
        .enqueue_for_worker(activity("blue", None, Some("blue")))
        .await
        .unwrap();
    let blue = tokio::time::timeout(Duration::from_secs(3), waits.pop().unwrap())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(blue.0, activity("blue", None, Some("blue")));
    assert!(!waits[0].is_finished());
    fixture
        .provider
        .enqueue_for_worker(activity("red", None, Some("red")))
        .await
        .unwrap();
    let red = tokio::time::timeout(Duration::from_secs(3), waits.pop().unwrap())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(red.0, activity("red", None, Some("red")));
    fixture.cleanup().await;
}

#[tokio::test]
async fn notifications_wake_the_session_owner_not_just_an_arbitrary_slot() {
    let fixture = Fixture::enabled().await;
    let owner = SessionFetchConfig {
        owner_id: "owner".to_string(),
        lock_timeout: LOCK,
    };
    fixture
        .provider
        .enqueue_for_worker(activity("first", Some("session"), None))
        .await
        .unwrap();
    let (_, token, _) = fixture
        .provider
        .fetch_work_item(LOCK, Duration::ZERO, Some(&owner), &TagFilter::Any)
        .await
        .unwrap()
        .unwrap();
    fixture.provider.ack_work_item(&token, None).await.unwrap();
    let since = fixture.stamp().await;
    let other = {
        let provider = fixture.provider.clone();
        tokio::spawn(async move {
            let session = SessionFetchConfig {
                owner_id: "other".to_string(),
                lock_timeout: LOCK,
            };
            provider
                .fetch_work_item(LOCK, POLL, Some(&session), &TagFilter::Any)
                .await
                .unwrap()
        })
    };
    let eligible = {
        let provider = fixture.provider.clone();
        tokio::spawn(async move {
            provider
                .fetch_work_item(LOCK, POLL, Some(&owner), &TagFilter::Any)
                .await
                .unwrap()
        })
    };
    fixture
        .wait_for_query(&format!("{}.fetch_work_item", fixture.schema), Some(since))
        .await;
    fixture
        .provider
        .enqueue_for_worker(activity("second", Some("session"), None))
        .await
        .unwrap();
    let claim = tokio::time::timeout(Duration::from_secs(3), eligible)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(claim.0, activity("second", Some("session"), None));
    assert!(!other.is_finished());
    other.abort();
    assert!(other.await.unwrap_err().is_cancelled());
    fixture.cleanup().await;
}

#[tokio::test]
async fn public_style_cleanup_removes_notification_function() {
    let fixture = Fixture::new(LongPollConfig::default()).await;
    sqlx::query(&format!("SELECT {}.cleanup_schema()", fixture.schema))
        .execute(&fixture.control)
        .await
        .unwrap();
    let exists: bool = sqlx::query_scalar("SELECT to_regprocedure($1) IS NOT NULL")
        .bind(format!("{}.duroxide_notify_queue_work()", fixture.schema))
        .fetch_one(&fixture.control)
        .await
        .unwrap();
    assert!(!exists);
    fixture.cleanup().await;
}

#[tokio::test]
async fn notification_migration_preserves_an_application_function_with_the_old_generic_name() {
    let control = PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url())
        .await
        .unwrap();
    let schema = schema_name();
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&control)
        .await
        .unwrap();
    sqlx::query(&format!(
        "CREATE FUNCTION {schema}.notify_queue_work() RETURNS TRIGGER \
         LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'application-owned function'; END $$",
    ))
    .execute(&control)
    .await
    .unwrap();
    let fixture = Fixture::with_schema(polling_config(true), schema.clone()).await;
    Queue::Worker
        .enqueue(&fixture, "provider-owned-trigger", None)
        .await;
    let claimed = Queue::Worker
        .fetch(&fixture.provider, Duration::ZERO)
        .await
        .unwrap();
    Queue::Worker.ack(&fixture.provider, &claimed.token).await;
    sqlx::query(&format!("SELECT {schema}.cleanup_schema()"))
        .execute(&control)
        .await
        .unwrap();
    let definition: Option<String> =
        sqlx::query_scalar("SELECT prosrc FROM pg_proc WHERE oid = to_regprocedure($1)")
            .bind(format!("{schema}.notify_queue_work()"))
            .fetch_optional(&control)
            .await
            .unwrap();
    assert!(definition.is_some_and(|definition| definition.contains("application-owned function")));
    fixture.cleanup().await;
    control.close().await;
}

#[tokio::test]
async fn a_blocked_timer_refresh_does_not_block_notifications() {
    let mut config = polling_config(true);
    config.notifier_poll_interval = Duration::from_millis(50);
    let fixture = Fixture::new(config).await;
    let mut blocker = fixture.control.begin().await.unwrap();
    sqlx::query(&format!(
        "LOCK TABLE {}.orchestrator_queue IN ACCESS EXCLUSIVE MODE",
        fixture.schema,
    ))
    .execute(&mut *blocker)
    .await
    .unwrap();
    fixture.wait_for_blocked_refresh().await;
    let waiting = fixture.waiting_fetch(Queue::Worker).await;
    Queue::Worker
        .enqueue(&fixture, "refresh-blocked", None)
        .await;
    let claim = claimed(waiting).await;
    Queue::Worker.ack(&fixture.provider, &claim.token).await;
    blocker.rollback().await.unwrap();
    fixture.cleanup().await;
}

#[tokio::test]
async fn cancelling_a_blocked_refresh_does_not_strand_a_pool_connection() {
    for reconnect in [false, true] {
        let mut config = polling_config(true);
        config.notifier_poll_interval = Duration::from_millis(50);
        let fixture = Fixture::new(config).await;
        let mut blocker = fixture.control.begin().await.unwrap();
        sqlx::query(&format!(
            "LOCK TABLE {}.orchestrator_queue IN ACCESS EXCLUSIVE MODE",
            fixture.schema,
        ))
        .execute(&mut *blocker)
        .await
        .unwrap();
        fixture.wait_for_blocked_refresh().await;
        if reconnect {
            let pid = fixture.listener_pid().await.unwrap();
            sqlx::query("SELECT pg_terminate_backend($1)")
                .bind(pid)
                .execute(&fixture.control)
                .await
                .unwrap();
        }
        let Fixture {
            provider,
            control,
            schema,
        } = fixture;
        let pool = provider.pool().clone();
        let mut provider = Some(provider);
        if !reconnect {
            drop(provider.take());
        }
        // Let SQLx begin returning the cancelled query while the pool is still open.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let closed = tokio::time::timeout(Duration::from_secs(2), pool.close()).await;
        blocker.rollback().await.unwrap();
        drop(provider);
        pool.close().await;
        sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
            .execute(&control)
            .await
            .unwrap();
        control.close().await;
        assert!(
            closed.is_ok(),
            "pool closure waited for a cancelled refresh's database lock; reconnect={reconnect}"
        );
    }
}

#[tokio::test]
async fn timers_expiring_during_refresh_do_not_hide_the_next_timer() {
    let fixture = Fixture::enabled().await;
    let mut blocker = fixture.control.begin().await.unwrap();
    sqlx::query(&format!(
        "LOCK TABLE {}.orchestrator_queue IN ACCESS EXCLUSIVE MODE",
        fixture.schema,
    ))
    .execute(&mut *blocker)
    .await
    .unwrap();
    let first = fixture.waiting_fetch(Queue::Worker).await;
    Queue::Worker
        .enqueue(&fixture, "start-refresh", Some(Duration::from_millis(200)))
        .await;
    let first = claimed(first).await;
    Queue::Worker.ack(&fixture.provider, &first.token).await;
    fixture.wait_for_blocked_refresh().await;

    let second = fixture.waiting_fetch(Queue::Worker).await;
    Queue::Worker
        .enqueue(&fixture, "during-refresh", Some(Duration::from_millis(200)))
        .await;
    Queue::Worker
        .enqueue(&fixture, "after-refresh", Some(Duration::from_millis(700)))
        .await;
    let second = claimed(second).await;
    Queue::Worker.ack(&fixture.provider, &second.token).await;
    blocker.rollback().await.unwrap();

    let third = fixture.waiting_fetch(Queue::Worker).await;
    let third = claimed(third).await;
    Queue::Worker.ack(&fixture.provider, &third.token).await;
    fixture.cleanup().await;
}

#[tokio::test]
async fn multiple_providers_receive_notifications_without_duplicate_claims() {
    let fixture = Fixture::enabled().await;
    let mut config = ProviderConfig::url(database_url());
    config.schema_name = Some(fixture.schema.clone());
    config.migration_policy = MigrationPolicy::VerifyOnly;
    config.long_poll.enabled = true;
    let second = Arc::new(PostgresProvider::new_with_config(config).await.unwrap());
    for queue in [Queue::Orchestration, Queue::Worker] {
        let first_waiter = fixture.waiting_fetch(queue).await;
        let second_waiter = {
            let provider = second.clone();
            tokio::spawn(async move { queue.fetch(&provider, POLL).await })
        };
        queue.enqueue(&fixture, "node-a", None).await;
        queue.enqueue(&fixture, "node-b", None).await;
        let a = claimed(first_waiter).await;
        let b = claimed(second_waiter).await;
        assert_ne!(a.token, b.token);
        assert_eq!((a.attempt, b.attempt), (1, 1));
        queue.ack(&fixture.provider, &a.token).await;
        queue.ack(&second, &b.token).await;
    }
    let pool = second.pool().clone();
    drop(second);
    pool.close().await;
    fixture.cleanup().await;
}

#[tokio::test]
async fn concurrent_schema_activation_is_idempotent() {
    let fixture = Fixture::new(LongPollConfig::default()).await;
    let mut config = ProviderConfig::url(database_url());
    config.schema_name = Some(fixture.schema.clone());
    config.long_poll.enabled = true;
    let (a, b) = tokio::join!(
        PostgresProvider::new_with_config(config.clone()),
        PostgresProvider::new_with_config(config),
    );
    for provider in [a.unwrap(), b.unwrap()] {
        let pool = provider.pool().clone();
        drop(provider);
        pool.close().await;
    }
    let mut config = ProviderConfig::url(database_url());
    config.schema_name = Some(fixture.schema.clone());
    config.long_poll.enabled = true;
    config.migration_policy = MigrationPolicy::VerifyOnly;
    let provider = PostgresProvider::new_with_config(config).await.unwrap();
    let pool = provider.pool().clone();
    drop(provider);
    pool.close().await;
    fixture.cleanup().await;
}

#[tokio::test]
async fn notification_installation_and_activation_follow_writer_lock_order() {
    let mut failures = Vec::new();
    for install in [false, true] {
        let fixture = Fixture::new(LongPollConfig::default()).await;
        let mut blocker = fixture.control.begin().await.unwrap();
        sqlx::query("SET LOCAL lock_timeout = '500ms'")
            .execute(&mut *blocker)
            .await
            .unwrap();
        sqlx::query(&format!(
            "LOCK TABLE {}.worker_queue IN ROW EXCLUSIVE MODE",
            fixture.schema,
        ))
        .execute(&mut *blocker)
        .await
        .unwrap();
        let application = format!("{}_activation", fixture.schema);
        let mut url = url::Url::parse(&database_url()).unwrap();
        url.query_pairs_mut()
            .append_pair("application_name", &application);
        let schema = fixture.schema.clone();
        let activation = async move {
            if install {
                let pool = PgPoolOptions::new()
                    .max_connections(1)
                    .connect(url.as_str())
                    .await
                    .unwrap();
                let mut tx = pool.begin().await.unwrap();
                sqlx::query(&format!("SET LOCAL search_path TO {schema}, pg_temp"))
                    .execute(&mut *tx)
                    .await
                    .unwrap();
                sqlx::raw_sql(include_str!(
                    "../migrations/0027_add_long_poll_notifications.sql"
                ))
                .execute(&mut *tx)
                .await
                .unwrap();
                tx.commit().await.unwrap();
                pool.close().await;
            } else {
                let mut config = ProviderConfig::url(url.as_str());
                config.schema_name = Some(schema);
                config.long_poll.enabled = true;
                let provider = PostgresProvider::new_with_config(config).await.unwrap();
                let pool = provider.pool().clone();
                drop(provider);
                pool.close().await;
            }
        };
        let writer = async {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let waiting: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM pg_stat_activity a \
                     JOIN pg_locks l ON l.pid = a.pid \
                     WHERE a.application_name = $1 AND a.wait_event_type = 'Lock' \
                     AND NOT l.granted AND l.relation = $2::TEXT::REGCLASS)",
                )
                .bind(&application)
                .bind(format!("{}.worker_queue", fixture.schema))
                .fetch_one(&fixture.control)
                .await
                .unwrap();
                if waiting {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "activation did not reach the worker lock"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let writer_lock = sqlx::query(&format!(
                "LOCK TABLE {}.orchestrator_queue IN ROW EXCLUSIVE MODE",
                fixture.schema,
            ))
            .execute(&mut *blocker)
            .await;
            blocker.rollback().await.unwrap();
            writer_lock
        };
        let ((), writer_lock) = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(activation, writer)
        })
        .await
        .unwrap();
        fixture.cleanup().await;
        if let Err(error) = writer_lock {
            failures.push((install, error.to_string()));
        }
    }
    assert!(
        failures.is_empty(),
        "installation/activation must not hold the orchestration queue while waiting for a writer's worker lock: {failures:?}",
    );
}

#[tokio::test]
async fn activation_releases_worker_lock_before_retrying_reverse_order_contention() {
    let fixture = Fixture::new(LongPollConfig::default()).await;
    let mut blocker = fixture.control.begin().await.unwrap();
    sqlx::query("SET LOCAL lock_timeout = '500ms'")
        .execute(&mut *blocker)
        .await
        .unwrap();
    sqlx::query(&format!(
        "LOCK TABLE {}.orchestrator_queue IN ROW EXCLUSIVE MODE",
        fixture.schema,
    ))
    .execute(&mut *blocker)
    .await
    .unwrap();
    let application = format!("{}_retry", fixture.schema);
    let mut url = url::Url::parse(&database_url()).unwrap();
    url.query_pairs_mut()
        .append_pair("application_name", &application);
    let mut config = ProviderConfig::url(url.as_str());
    config.schema_name = Some(fixture.schema.clone());
    config.long_poll.enabled = true;
    let activation = tokio::spawn(PostgresProvider::new_with_config(config));
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let tried_second_lock: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE application_name = $1 \
             AND strpos(query, 'orchestrator_queue IN SHARE ROW EXCLUSIVE MODE NOWAIT') > 0)",
        )
        .bind(&application)
        .fetch_one(&fixture.control)
        .await
        .unwrap();
        if tried_second_lock {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "activation did not try the contended lock"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let writer_lock = sqlx::query(&format!(
        "LOCK TABLE {}.worker_queue IN ROW EXCLUSIVE MODE",
        fixture.schema,
    ))
    .execute(&mut *blocker)
    .await;
    blocker.rollback().await.unwrap();
    let provider = tokio::time::timeout(Duration::from_secs(5), activation)
        .await
        .unwrap()
        .unwrap()
        .expect("activation should retry after the blocker exits");
    let pool = provider.pool().clone();
    drop(provider);
    pool.close().await;
    fixture.cleanup().await;
    writer_lock.expect("activation must release the worker lock before retrying");
}

#[tokio::test]
async fn runtime_executes_activities_and_timer_then_shuts_down() {
    use duroxide::runtime::{registry::ActivityRegistry, Runtime};
    use duroxide::{ActivityContext, Client, OrchestrationContext, OrchestrationRegistry};

    let fixture = Fixture::enabled().await;
    let activities = ActivityRegistry::builder()
        .register("Echo", |_ctx: ActivityContext, input: String| async move {
            Ok(input)
        })
        .build();
    let orchestrations = OrchestrationRegistry::builder()
        .register(
            "Sequence",
            |ctx: OrchestrationContext, input: String| async move {
                let value = ctx.schedule_activity("Echo", input).await?;
                ctx.schedule_timer(Duration::from_millis(200)).await;
                ctx.schedule_activity_on_session("Echo", value, "session")
                    .await
            },
        )
        .build();
    let runtime =
        Runtime::start_with_store(fixture.provider.clone(), activities, orchestrations).await;
    let client = Client::new(fixture.provider.clone());
    client
        .start_orchestration("runtime-test", "Sequence", "result")
        .await
        .unwrap();
    let status = client
        .wait_for_orchestration("runtime-test", Duration::from_secs(10))
        .await
        .unwrap();
    assert!(
        matches!(status, duroxide::OrchestrationStatus::Completed { output, .. } if output == "result")
    );
    tokio::time::timeout(Duration::from_secs(3), runtime.shutdown(Some(1000)))
        .await
        .expect("shutdown must cancel waiting dispatchers");
    drop(client);
    fixture.cleanup().await;
}

#[tokio::test]
async fn upgrading_pre_notification_schema_preserves_work_and_defaults_to_disabled() {
    let control = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url())
        .await
        .unwrap();
    let schema = schema_name();
    sqlx::query(&format!("CREATE SCHEMA {schema};",))
        .execute(&control)
        .await
        .unwrap();
    sqlx::query(&format!(
        "CREATE TABLE {schema}._duroxide_migrations \
         (version BIGINT PRIMARY KEY, name TEXT NOT NULL, applied_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP)",
    )).execute(&control).await.unwrap();
    let directory = include_dir::include_dir!("$CARGO_MANIFEST_DIR/migrations");
    let mut files: Vec<_> = directory
        .files()
        .filter(|file| {
            file.path()
                .extension()
                .is_some_and(|extension| extension == "sql")
        })
        .filter(|file| file.path().file_name().unwrap().to_str().unwrap() < "0027")
        .collect();
    files.sort_by_key(|file| file.path());
    for file in files {
        let name = file.path().file_name().unwrap().to_str().unwrap();
        let version: i64 = name.split('_').next().unwrap().parse().unwrap();
        let mut tx = control.begin().await.unwrap();
        sqlx::query(&format!("SET LOCAL search_path TO {schema}, pg_temp"))
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::raw_sql(file.contents_utf8().unwrap())
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query(&format!(
            "INSERT INTO {schema}._duroxide_migrations(version,name) VALUES ($1,$2)"
        ))
        .bind(version)
        .bind(name)
        .execute(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }
    let payload = serde_json::to_string(&activity("retained", None, None)).unwrap();
    sqlx::query(&format!(
        "INSERT INTO {schema}.worker_queue \
         (work_item, visible_at, created_at, instance_id, execution_id, activity_id) \
         VALUES ($1, now(), now(), 'retained', 1, 1)",
    ))
    .bind(&payload)
    .execute(&control)
    .await
    .unwrap();
    let provider = PostgresProvider::new_with_schema(&database_url(), Some(&schema))
        .await
        .unwrap();
    let active: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_trigger WHERE tgrelid = $1::TEXT::REGCLASS \
         AND NOT tgisinternal AND tgenabled <> 'D'",
    )
    .bind(format!("{schema}.worker_queue"))
    .fetch_one(&control)
    .await
    .unwrap();
    assert_eq!(active, 0);
    let retained: String =
        sqlx::query_scalar(&format!("SELECT work_item FROM {schema}.worker_queue"))
            .fetch_one(&control)
            .await
            .unwrap();
    assert_eq!(retained, payload);
    let fetched = provider
        .fetch_work_item(LOCK, Duration::ZERO, None, &TagFilter::Any)
        .await
        .unwrap();
    assert!(fetched.is_some());
    provider.cleanup_schema().await.unwrap();
    let pool = provider.pool().clone();
    drop(provider);
    pool.close().await;
    control.close().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL max_prepared_transactions >= 1"]
async fn two_phase_commit_is_preserved_until_explicit_notification_activation() {
    let fixture = Fixture::new(LongPollConfig::default()).await;
    let setting: String = sqlx::query_scalar("SHOW max_prepared_transactions")
        .fetch_one(&fixture.control)
        .await
        .unwrap();
    assert!(
        setting.parse::<u32>().unwrap() > 0,
        "enable max_prepared_transactions for this test"
    );
    let mut connection = fixture.control.acquire().await.unwrap();
    for enabled in [false, true] {
        if enabled {
            let mut config = ProviderConfig::url(database_url());
            config.schema_name = Some(fixture.schema.clone());
            config.long_poll.enabled = true;
            let provider = PostgresProvider::new_with_config(config).await.unwrap();
            let pool = provider.pool().clone();
            drop(provider);
            pool.close().await;
        }
        sqlx::query("BEGIN")
            .execute(&mut *connection)
            .await
            .unwrap();
        sqlx::query(&format!(
            "INSERT INTO {}.worker_queue(work_item, visible_at, created_at) VALUES ($1, now(), now())",
            fixture.schema,
        ))
        .bind(serde_json::to_string(&activity("prepared", None, None)).unwrap())
        .execute(&mut *connection).await.unwrap();
        let prepared = sqlx::query(&format!("PREPARE TRANSACTION '{}'", fixture.schema))
            .execute(&mut *connection)
            .await;
        if enabled {
            assert!(prepared.unwrap_err().to_string().contains("NOTIFY"));
            sqlx::query("ROLLBACK")
                .execute(&mut *connection)
                .await
                .unwrap();
        } else {
            prepared.expect("short-polling-only schemas must still support two-phase commit");
            sqlx::query(&format!("ROLLBACK PREPARED '{}'", fixture.schema))
                .execute(&mut *connection)
                .await
                .unwrap();
        }
    }
    drop(connection);
    fixture.cleanup().await;
}

#[tokio::test]
#[ignore = "slow local benchmark; requires pg_stat_statements and PostgreSQL without assertions"]
async fn idle_query_load_is_reduced_with_long_polling() {
    use duroxide::runtime::{registry::ActivityRegistry, Runtime};
    use duroxide::OrchestrationRegistry;

    let mut calls = Vec::new();
    for enabled in [false, true] {
        let fixture = Fixture::new(polling_config(enabled)).await;
        let assertions: String = sqlx::query_scalar("SHOW debug_assertions")
            .fetch_one(&fixture.control)
            .await
            .unwrap();
        assert_eq!(
            assertions, "off",
            "use a non-assert PostgreSQL build for benchmarks"
        );
        let statement_count = || async {
            sqlx::query_scalar::<_, i64>(
                "SELECT COALESCE(SUM(calls), 0)::BIGINT FROM pg_stat_statements \
                 WHERE toplevel AND strpos(query, $1) > 0",
            )
            .bind(format!("{}.", fixture.schema))
            .fetch_one(&fixture.control)
            .await
            .expect("create pg_stat_statements in the benchmark database")
        };
        let runtime = Runtime::start_with_store(
            fixture.provider.clone(),
            ActivityRegistry::builder().build(),
            OrchestrationRegistry::builder().build(),
        )
        .await;
        let before = statement_count().await;
        let start = Instant::now();
        tokio::time::sleep(Duration::from_secs(65)).await;
        let elapsed = start.elapsed().as_secs_f64();
        let count = statement_count().await - before;
        println!(
            "long_poll={enabled}: {count} provider SQL calls in {elapsed:.3}s ({:.3}/s)",
            count as f64 / elapsed,
        );
        calls.push(count);
        runtime.shutdown(Some(1000)).await;
        fixture.cleanup().await;
    }
    assert!(calls[0] > 0);
    assert!(
        calls[1] < calls[0] / 20,
        "idle query counts: short={}, long={}",
        calls[0],
        calls[1]
    );
}
