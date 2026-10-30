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

#[path = "deadlocks/harness.rs"]
mod harness;

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
        self.fetch_with_lock(provider, LOCK, timeout).await
    }

    async fn fetch_with_lock(
        self,
        provider: &PostgresProvider,
        lock_timeout: Duration,
        poll_timeout: Duration,
    ) -> Option<Claim> {
        match self {
            Self::Orchestration => provider
                .fetch_orchestration_item(lock_timeout, poll_timeout, None)
                .await
                .unwrap()
                .map(|(_, token, attempt)| Claim { token, attempt }),
            Self::Worker => provider
                .fetch_work_item(lock_timeout, poll_timeout, None, &TagFilter::Any)
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
        let application = format!("lp_fixture_{}", uuid::Uuid::new_v4().simple());
        let mut url = url::Url::parse(&database_url()).unwrap();
        url.query_pairs_mut()
            .append_pair("application_name", &application);
        let mut config = ProviderConfig::url(url.as_str());
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
                .wait_for_query("COMMIT", None, Some(&application))
                .await;
            tokio::task::yield_now().await;
        }
        fixture
    }

    async fn wait_for_query(
        &self,
        query: &str,
        since: Option<DateTime<Utc>>,
        application: Option<&str>,
    ) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let seen: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM pg_stat_activity \
                 WHERE datname = current_database() AND state = 'idle' \
                 AND strpos(query, $1) > 0 AND ($2::TIMESTAMPTZ IS NULL OR query_start >= $2) \
                 AND ($3::TEXT IS NULL OR application_name = $3))",
            )
            .bind(query)
            .bind(since)
            .bind(application)
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
            None,
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
async fn long_poll_construction_leaves_a_warm_fetch_connection() {
    let schema = schema_name();
    let mut config = ProviderConfig::url(database_url());
    config.schema_name = Some(schema.clone());
    config.long_poll.enabled = true;
    let provider = Arc::new(PostgresProvider::new_with_config(config).await.unwrap());
    let connections = provider.pool().size();
    let control = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url())
        .await
        .unwrap();
    Fixture {
        provider,
        control,
        schema,
    }
    .cleanup()
    .await;
    assert!(
        connections >= 2,
        "LISTEN must not consume the only warm connection: {connections}"
    );
}

#[tokio::test]
async fn expired_claims_wake_fetches_before_the_poll_timeout() {
    let mut failures = Vec::new();
    for queue in [Queue::Orchestration, Queue::Worker] {
        for claim_before_listener in [false, true] {
            let owner = Fixture::new(polling_config(false)).await;
            let waiter = if claim_before_listener {
                None
            } else {
                Some(Fixture::with_schema(polling_config(true), owner.schema.clone()).await)
            };
            queue.enqueue(&owner, "crashed-owner", None).await;
            let first = queue
                .fetch_with_lock(&owner.provider, Duration::from_secs(1), Duration::ZERO)
                .await
                .unwrap();
            let waiter = match waiter {
                Some(waiter) => waiter,
                None => Fixture::with_schema(polling_config(true), owner.schema.clone()).await,
            };
            let pool = owner.provider.pool().clone();
            drop(owner.provider);
            pool.close().await;
            owner.control.close().await;
            let before: (String, i64, i32) = sqlx::query_as(&format!(
                "SELECT lock_token, locked_until, attempt_count FROM {}.{}",
                waiter.schema,
                queue.table(),
            ))
            .fetch_one(&waiter.control)
            .await
            .unwrap();
            assert_eq!(before.0, first.token);
            assert_eq!(before.2, 1);
            let result = tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    if let Some(claim) =
                        queue.fetch(&waiter.provider, Duration::from_secs(10)).await
                    {
                        break claim;
                    }
                }
            })
            .await;
            let after: (String, i64, i32) = sqlx::query_as(&format!(
                "SELECT lock_token, locked_until, attempt_count FROM {}.{}",
                waiter.schema,
                queue.table(),
            ))
            .fetch_one(&waiter.control)
            .await
            .unwrap();
            waiter.cleanup().await;
            match result {
                Ok(claim) => {
                    assert_ne!(claim.token, first.token);
                    assert_eq!(claim.attempt, 2);
                    assert_eq!(after.0, claim.token);
                    assert!(after.1 > before.1);
                    assert_eq!(after.2, 2);
                }
                Err(error) => {
                    assert_eq!(after, before, "a waiting fetch must not alter the claim");
                    failures.push((queue, claim_before_listener, error));
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "lease recovery was delayed: {failures:?}"
    );
}

#[tokio::test]
async fn session_expiry_wakes_a_waiting_new_owner() {
    let fixture = Fixture::enabled().await;
    for instance in ["held-session-item", "waiting-session-item"] {
        fixture
            .provider
            .enqueue_for_worker(activity(instance, Some("expiring-session"), None))
            .await
            .unwrap();
    }
    let first = fixture
        .provider
        .fetch_work_item(
            LOCK,
            Duration::ZERO,
            Some(&SessionFetchConfig {
                owner_id: "crashed-session-owner".to_string(),
                lock_timeout: Duration::from_secs(1),
            }),
            &TagFilter::Any,
        )
        .await
        .unwrap()
        .unwrap();
    let recovered = tokio::time::timeout(
        Duration::from_secs(3),
        fixture.provider.fetch_work_item(
            LOCK,
            Duration::from_secs(10),
            Some(&SessionFetchConfig {
                owner_id: "new-session-owner".to_string(),
                lock_timeout: LOCK,
            }),
            &TagFilter::Any,
        ),
    )
    .await;
    let owner: String = sqlx::query_scalar(&format!(
        "SELECT worker_id FROM {}.sessions WHERE session_id = 'expiring-session'",
        fixture.schema,
    ))
    .fetch_one(&fixture.control)
    .await
    .unwrap();
    fixture.cleanup().await;
    let (item, token, attempt) = recovered
        .expect("session expiry must beat the ten-second polling timeout")
        .unwrap()
        .unwrap();
    assert!(matches!(
        item,
        WorkItem::ActivityExecute { instance, .. } if instance == "waiting-session-item"
    ));
    assert_ne!(token, first.1);
    assert_eq!(attempt, 1);
    assert_eq!(owner, "new-session-owner");
}

#[tokio::test]
async fn delayed_abandon_immediately_releases_other_visible_orchestration_messages() {
    let fixture = Fixture::enabled().await;
    Queue::Orchestration
        .enqueue(&fixture, "delayed-abandon", None)
        .await;
    let first = Queue::Orchestration
        .fetch(&fixture.provider, Duration::ZERO)
        .await
        .unwrap();
    fixture
        .provider
        .enqueue_for_orchestrator(
            WorkItem::ExternalRaised {
                instance: "delayed-abandon".to_string(),
                name: "ready".to_string(),
                data: String::new(),
            },
            None,
        )
        .await
        .unwrap();
    let waiting = fixture.waiting_fetch(Queue::Orchestration).await;
    let start = Instant::now();
    Queue::Orchestration
        .abandon(
            &fixture.provider,
            &first.token,
            Some(Duration::from_secs(2)),
        )
        .await;
    let next = claimed(waiting).await;
    let elapsed = start.elapsed();
    Queue::Orchestration
        .ack(&fixture.provider, &next.token)
        .await;
    fixture.cleanup().await;
    assert!(elapsed < Duration::from_secs(1), "release took {elapsed:?}");
    assert_eq!(
        next.attempt, 1,
        "only the already-visible message should be claimed"
    );
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
            None,
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
async fn orchestration_claims_announce_only_expiry_but_acknowledgments_notify() {
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
    let notification = tokio::time::timeout(Duration::from_secs(3), listener.recv())
        .await
        .unwrap()
        .unwrap();
    let deadline: i64 = sqlx::query_scalar(&format!(
        "SELECT locked_until FROM {}.instance_locks WHERE lock_token = $1",
        fixture.schema,
    ))
    .bind(&claim.token)
    .fetch_one(&fixture.control)
    .await
    .unwrap();
    assert_eq!(notification.payload().parse::<i64>().unwrap(), deadline);
    assert!(deadline > Utc::now().timestamp_millis());
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
async fn renewed_claims_are_not_recovered_at_the_original_expiry() {
    for queue in [Queue::Orchestration, Queue::Worker] {
        let fixture = Fixture::enabled().await;
        queue.enqueue(&fixture, "renewed", None).await;
        let first = queue
            .fetch_with_lock(&fixture.provider, Duration::from_secs(1), Duration::ZERO)
            .await
            .unwrap();
        match queue {
            Queue::Orchestration => fixture
                .provider
                .renew_orchestration_item_lock(&first.token, Duration::from_secs(2))
                .await
                .unwrap(),
            Queue::Worker => fixture
                .provider
                .renew_work_item_lock(&first.token, Duration::from_secs(2))
                .await
                .unwrap(),
        }
        let start = Instant::now();
        let second = claimed(fixture.waiting_fetch(queue).await).await;
        assert!(start.elapsed() >= Duration::from_millis(1800));
        assert_ne!(first.token, second.token);
        assert_eq!(second.attempt, 2);
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
        .wait_for_query(
            &format!("{}.fetch_work_item", fixture.schema),
            Some(since),
            None,
        )
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
        .wait_for_query(
            &format!("{}.fetch_work_item", fixture.schema),
            Some(since),
            None,
        )
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
        .wait_for_query(
            &format!("{}.fetch_work_item", fixture.schema),
            Some(since),
            None,
        )
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
        .wait_for_query(
            &format!("{}.fetch_work_item", fixture.schema),
            Some(since),
            None,
        )
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
        .wait_for_query(
            &format!("{}.fetch_work_item", fixture.schema),
            Some(since),
            None,
        )
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
    let pruning_function = format!(
        "{}.prune_executions_batch(TEXT, BIGINT, BIGINT, INTEGER)",
        fixture.schema,
    );
    let pruning_exists: bool = sqlx::query_scalar("SELECT to_regprocedure($1) IS NOT NULL")
        .bind(&pruning_function)
        .fetch_one(&fixture.control)
        .await
        .unwrap();
    if !pruning_exists {
        sqlx::query(&format!(
            "CREATE FUNCTION {pruning_function} RETURNS VOID LANGUAGE plpgsql AS $$ BEGIN RETURN; END $$"
        )).execute(&fixture.control).await.unwrap();
    }
    sqlx::query(&format!("SELECT {}.cleanup_schema()", fixture.schema))
        .execute(&fixture.control)
        .await
        .unwrap();
    let exists: bool = sqlx::query_scalar("SELECT to_regprocedure($1) IS NOT NULL")
        .bind(format!("{}.duroxide_notify_queue_work()", fixture.schema))
        .fetch_one(&fixture.control)
        .await
        .unwrap();
    let pruning_exists: bool = sqlx::query_scalar("SELECT to_regprocedure($1) IS NOT NULL")
        .bind(&pruning_function)
        .fetch_one(&fixture.control)
        .await
        .unwrap();
    fixture.cleanup().await;
    assert!(!exists);
    assert!(
        !pruning_exists,
        "cleanup must retain migration 0028's batched-pruning DROP"
    );
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
async fn adv_blocked_timer_scans_do_not_accumulate_server_backends() {
    let mut config = polling_config(true);
    config.notifier_poll_interval = Duration::from_millis(500);
    let fixture = Fixture::new(config).await;
    let schema = fixture.schema.clone();
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url())
        .await
        .unwrap();
    let count_waiting = |admin: PgPool| {
        let schema = schema.clone();
        async move {
            // Other parallel tests deliberately hold unrelated database locks.
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM pg_stat_activity \
                 WHERE datname = current_database() AND wait_event_type = 'Lock' \
                 AND strpos(query, $1) > 0",
            )
            .bind(format!("FROM {schema}."))
            .fetch_one(&admin)
            .await
            .unwrap()
        }
    };
    let count_backends = |admin: PgPool| async move {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM pg_stat_activity \
             WHERE datname = current_database() AND backend_type = 'client backend'",
        )
        .fetch_one(&admin)
        .await
        .unwrap()
    };
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let backends_before = count_backends(admin.clone()).await;
    let mut locker = admin.begin().await.unwrap();
    sqlx::query(&format!(
        "LOCK TABLE {schema}.worker_queue IN ACCESS EXCLUSIVE MODE"
    ))
    .execute(&mut *locker)
    .await
    .unwrap();
    let mut waiting = Vec::new();
    for _ in 0..8 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        waiting.push(count_waiting(admin.clone()).await);
    }
    let backends_locked = count_backends(admin.clone()).await;
    locker.rollback().await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let waiting_after = count_waiting(admin.clone()).await;
    let backends_after = count_backends(admin.clone()).await;
    println!(
        "ADV-RESULT schema={schema} lock=ACCESS_EXCLUSIVE(worker_queue) held_s=8 notifier_poll_ms=500 lock_waiting_backends_each_second={waiting:?} database_client_backends_before={backends_before} at_8s={backends_locked} 2s_after_release={backends_after} lock_waiting_after_release={waiting_after}"
    );
    admin.close().await;
    fixture.cleanup().await;
    assert!(
        waiting.iter().all(|n| *n <= 2),
        "blocked timer scans must not accumulate waiting server backends: {waiting:?}"
    );
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
        .enqueue(&fixture, "during-refresh", Some(Duration::from_millis(50)))
        .await;
    let third_enqueued = Instant::now();
    Queue::Worker
        .enqueue(&fixture, "after-refresh", Some(Duration::from_millis(300)))
        .await;
    let second = claimed(second).await;
    Queue::Worker.ack(&fixture.provider, &second.token).await;
    blocker.rollback().await.unwrap();

    let third = fixture.waiting_fetch(Queue::Worker).await;
    let third = claimed(third).await;
    let elapsed = third_enqueued.elapsed();
    Queue::Worker.ack(&fixture.provider, &third.token).await;
    fixture.cleanup().await;
    assert!(
        elapsed >= Duration::from_millis(300),
        "timer fired early: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_millis(600),
        "the timer, not the one-second polling fallback, must wake this fetch: {elapsed:?}"
    );
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
                    "../migrations/0030_add_long_poll_notifications.sql"
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
             AND strpos(query, 'orchestrator_queue IN SHARE ROW EXCLUSIVE MODE') > 0)",
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
    harness::migrate_through(&control, &schema, 29).await;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "local release-mode benchmark; requires PostgreSQL without assertions"]
async fn workflow_latency_and_throughput_with_long_polling() {
    use duroxide::runtime::{registry::ActivityRegistry, Runtime, RuntimeOptions};
    use duroxide::{
        ActivityContext, Client, OrchestrationContext, OrchestrationRegistry, OrchestrationStatus,
    };

    const CLIENTS: usize = 20;
    const REPETITIONS: usize = 3;
    const DURATION: Duration = Duration::from_secs(10);
    const SPARSE_SAMPLES: usize = 20;
    const SPARSE_GAP: Duration = Duration::from_millis(200);

    async fn complete(client: &Client, instance: &str, activities: usize) -> Duration {
        let start = Instant::now();
        let input = activities.to_string();
        client
            .start_orchestration(instance, "Bench", &input)
            .await
            .unwrap();
        let status = client
            .wait_for_orchestration(instance, Duration::from_secs(30))
            .await
            .unwrap();
        assert!(matches!(status, OrchestrationStatus::Completed { output, .. } if output == input));
        start.elapsed()
    }

    for enabled in [false, true] {
        for activities_per_workflow in [1, 5] {
            let mut throughputs = Vec::new();
            for repetition in 0..REPETITIONS {
                let fixture = Fixture::new(polling_config(enabled)).await;
                let settings: serde_json::Value = sqlx::query_scalar(
                    "SELECT jsonb_build_object(
                        'postgres_version', current_setting('server_version'),
                        'debug_assertions', current_setting('debug_assertions'),
                        'fsync', current_setting('fsync'),
                        'full_page_writes', current_setting('full_page_writes'),
                        'synchronous_commit', current_setting('synchronous_commit'),
                        'synchronous_standby_names', current_setting('synchronous_standby_names'))",
                )
                .fetch_one(&fixture.control)
                .await
                .unwrap();
                assert_eq!(settings["debug_assertions"], "off");
                let activities = ActivityRegistry::builder()
                    .register("Echo", |_ctx: ActivityContext, input: String| async move {
                        Ok(input)
                    })
                    .build();
                let orchestrations = OrchestrationRegistry::builder()
                    .register(
                        "Bench",
                        |ctx: OrchestrationContext, input: String| async move {
                            let count =
                                input.parse::<usize>().map_err(|error| error.to_string())?;
                            for _ in 0..count {
                                ctx.schedule_activity("Echo", input.clone()).await?;
                            }
                            Ok(input)
                        },
                    )
                    .build();
                let options = RuntimeOptions {
                    orchestration_concurrency: 2,
                    worker_concurrency: 2,
                    dispatcher_min_poll_interval: Duration::from_millis(100),
                    dispatcher_long_poll_timeout: Duration::from_secs(30),
                    ..Default::default()
                };
                println!(
                    "BENCH {}",
                    serde_json::json!({
                        "phase": "config", "long_poll": enabled, "repetition": repetition,
                        "rust_debug_assertions": cfg!(debug_assertions),
                        "activities_per_workflow": activities_per_workflow,
                        "orch_concurrency": options.orchestration_concurrency,
                        "worker_concurrency": options.worker_concurrency,
                        "pool_max": fixture.provider.pool().options().get_max_connections(),
                        "clients": CLIENTS, "duration_seconds": DURATION.as_secs(),
                        "postgres": settings,
                    })
                );
                let runtime = Runtime::start_with_options(
                    fixture.provider.clone(),
                    activities,
                    orchestrations,
                    options,
                )
                .await;
                let client = Arc::new(Client::new(fixture.provider.clone()));
                complete(&client, "warmup", activities_per_workflow).await;
                if activities_per_workflow == 1 && repetition == 0 {
                    let mut latencies = Vec::new();
                    for sample in 0..SPARSE_SAMPLES {
                        tokio::time::sleep(SPARSE_GAP).await;
                        latencies.push(complete(&client, &format!("sparse-{sample}"), 1).await);
                    }
                    latencies.sort_unstable();
                    let median_ms = (latencies[SPARSE_SAMPLES / 2 - 1].as_secs_f64()
                        + latencies[SPARSE_SAMPLES / 2].as_secs_f64())
                        * 500.0;
                    println!(
                        "BENCH {}",
                        serde_json::json!({
                            "phase": "sparse", "long_poll": enabled, "samples": SPARSE_SAMPLES,
                            "idle_gap_ms": SPARSE_GAP.as_millis(), "median_ms": median_ms,
                        })
                    );
                }
                let start = Instant::now();
                let deadline = start + DURATION;
                let mut tasks = tokio::task::JoinSet::new();
                for lane in 0..CLIENTS {
                    let client = client.clone();
                    tasks.spawn(async move {
                        let mut completed = 0;
                        while Instant::now() < deadline {
                            complete(
                                &client,
                                &format!("loaded-{lane}-{completed}"),
                                activities_per_workflow,
                            )
                            .await;
                            completed += 1;
                        }
                        completed
                    });
                }
                let mut completed = 0;
                while let Some(result) = tasks.join_next().await {
                    completed += result.unwrap();
                }
                let elapsed = start.elapsed().as_secs_f64();
                assert!(completed > 0);
                let throughput = completed as f64 / elapsed;
                throughputs.push(throughput);
                println!(
                    "BENCH {}",
                    serde_json::json!({
                        "phase": "loaded", "long_poll": enabled, "repetition": repetition,
                        "activities_per_workflow": activities_per_workflow,
                        "completed": completed, "elapsed_seconds": elapsed,
                        "workflows_per_second": throughput,
                    })
                );
                runtime.shutdown(Some(1000)).await;
                drop(client);
                fixture.cleanup().await;
            }
            throughputs.sort_by(f64::total_cmp);
            println!(
                "BENCH {}",
                serde_json::json!({
                    "phase": "loaded_median", "long_poll": enabled,
                    "activities_per_workflow": activities_per_workflow,
                    "workflows_per_second": throughputs[REPETITIONS / 2],
                })
            );
        }
    }
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
