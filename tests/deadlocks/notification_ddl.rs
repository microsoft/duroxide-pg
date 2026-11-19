// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::harness::*;
use duroxide_pg::migrations::MigrationRunner;
use duroxide_pg::{PostgresProvider, ProviderConfig};
use serde_json::Value;
use sqlx::{postgres::PgPoolOptions, PgPool};
use std::sync::Arc;
use std::time::{Duration, Instant};

async fn snapshot(pool: &PgPool, schema: &str) -> Value {
    sqlx::query_scalar(&format!(
        "SELECT jsonb_build_object(
            'ledger', (SELECT jsonb_agg(to_jsonb(m) ORDER BY version) FROM {schema}._duroxide_migrations m),
            'functions', (SELECT jsonb_agg(pg_get_functiondef(p.oid) ORDER BY p.oid)
                          FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace WHERE n.nspname = $1),
            'triggers', (SELECT jsonb_agg(pg_get_triggerdef(t.oid) ORDER BY t.oid)
                         FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid
                         JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = $1),
            'worker', (SELECT jsonb_agg(to_jsonb(q) ORDER BY id) FROM {schema}.worker_queue q),
            'orchestration', (SELECT jsonb_agg(to_jsonb(q) ORDER BY id) FROM {schema}.orchestrator_queue q))"
    ))
    .bind(schema)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn notification_upgrade_retries_reverse_order_delete_contention() {
    with_schema(|schema| async move {
        let pool = raw_pool(&database_url()).await;
        migrate_through(&pool, &schema, 29).await;
        sqlx::Executor::execute(
            &pool,
            format!(
            "INSERT INTO {schema}.instances(instance_id, orchestration_name, created_at, updated_at)
             VALUES ('delete-me', 'Test', now(), now());
             INSERT INTO {schema}.orchestrator_queue(instance_id, work_item, visible_at, created_at)
             VALUES ('delete-me', '{{}}', now(), now());
             INSERT INTO {schema}.worker_queue(instance_id, work_item, visible_at, created_at)
             VALUES ('delete-me', '{{}}', now(), now());"
        )
            .as_str(),
        )
        .await
        .unwrap();
        let before = snapshot(&pool, &schema).await;
        let migrator = PgPoolOptions::new()
            .max_connections(1)
            .after_connect(|conn, _| {
                Box::pin(async move {
                    sqlx::query("SET deadlock_timeout = '500ms'")
                        .execute(conn)
                        .await?;
                    Ok(())
                })
            })
            .connect(&database_url())
            .await
            .unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&migrator)
            .await
            .unwrap();
        let mut delete = pool.begin().await.unwrap();
        sqlx::Executor::execute(
            &mut *delete,
            format!(
                "SET LOCAL deadlock_timeout = '10s';
             SET LOCAL statement_timeout = '5s';
             LOCK TABLE {schema}.orchestrator_queue IN ROW EXCLUSIVE MODE"
            )
            .as_str(),
        )
        .await
        .unwrap();
        let runner = MigrationRunner::new(Arc::new(migrator.clone()), schema.clone());
        let upgrade = tokio::spawn(async move { runner.migrate().await });
        wait_for_lock_waiter(&pool, pid).await;
        let deleted: (i64, i64, i64, i64) = timed(
            "reverse-order admin delete",
            sqlx::query_as(&format!(
                "SELECT * FROM {schema}.delete_instances_atomic(ARRAY['delete-me'], true)"
            ))
            .fetch_one(&mut *delete),
        )
        .await
        .expect("admin delete must finish when the DDL transaction rolls back");
        delete.commit().await.unwrap();
        assert_eq!(deleted, (1, 0, 0, 2));
        let result = timed("notification upgrade retry", upgrade).await.unwrap();
        let after = snapshot(&pool, &schema).await;
        migrator.close().await;
        pool.close().await;
        result.expect("notification upgrade must retry its aborted transaction");
        let before_ledger = before["ledger"].as_array().unwrap();
        let after_ledger = after["ledger"].as_array().unwrap();
        assert_eq!(&after_ledger[..before_ledger.len()], before_ledger);
        assert_eq!(after_ledger.len(), before_ledger.len() + 1);
        assert_eq!(after_ledger.last().unwrap()["version"], 30);
        assert!(after["worker"].is_null());
        assert!(after["orchestration"].is_null());
    })
    .await;
}

#[tokio::test]
async fn notification_migration_retries_only_transient_errors_and_rolls_back() {
    for code in ["40P01", "55P03", "42501"] {
        with_schema(move |schema| async move {
            let pool = raw_pool(&database_url()).await;
            migrate_through(&pool, &schema, 29).await;
            sqlx::Executor::execute(&pool, format!(
                "CREATE SEQUENCE {schema}.migration_attempts;
                 CREATE FUNCTION {schema}.reject_first_migration() RETURNS trigger LANGUAGE plpgsql AS $$
                 BEGIN
                     IF NEW.version = 30 AND nextval('{schema}.migration_attempts') = 1 THEN
                         RAISE EXCEPTION 'injected migration failure' USING ERRCODE = '{code}';
                     END IF;
                     RETURN NEW;
                 END $$;
                 CREATE TRIGGER reject_first_migration BEFORE INSERT ON {schema}._duroxide_migrations
                 FOR EACH ROW EXECUTE FUNCTION {schema}.reject_first_migration();
                 INSERT INTO {schema}.worker_queue(work_item, visible_at, created_at)
                 VALUES ('retained work', now(), now());"
            ).as_str()).await.unwrap();
            let before = snapshot(&pool, &schema).await;
            let runner = MigrationRunner::new(Arc::new(pool.clone()), schema.clone());
            let result = runner.migrate().await;
            let attempts: i64 = sqlx::query_scalar(&format!(
                "SELECT last_value FROM {schema}.migration_attempts"
            )).fetch_one(&pool).await.unwrap();
            let after = snapshot(&pool, &schema).await;
            pool.close().await;
            if code == "42501" {
                let error = result.expect_err("permanent migration failures must be returned");
                let sql_error = error.downcast_ref::<sqlx::Error>().expect("original SQL error must survive");
                assert_eq!(sqlstate(sql_error).as_deref(), Some(code));
                assert_eq!(attempts, 1, "permanent errors must not be retried");
                assert_eq!(after, before, "failed DDL must not change the ledger, functions, triggers, or work");
            } else {
                result.expect("transient migration failures must retry");
                assert_eq!(attempts, 2, "the failed transaction must have been retried once");
                assert_eq!(after["worker"], before["worker"]);
                assert_eq!(after["orchestration"], before["orchestration"]);
                let before_ledger = before["ledger"].as_array().unwrap();
                let after_ledger = after["ledger"].as_array().unwrap();
                assert_eq!(&after_ledger[..before_ledger.len()], before_ledger);
                assert_eq!(after_ledger.len(), before_ledger.len() + 1);
                assert_eq!(after_ledger.last().unwrap()["version"], 30);
            }
        }).await;
    }
}

#[tokio::test]
async fn notification_activation_retries_sustained_contention_on_either_queue() {
    for table in ["worker_queue", "orchestrator_queue"] {
        with_schema(move |schema| async move {
            let provider = PostgresProvider::new_with_schema(&database_url(), Some(&schema))
                .await
                .unwrap();
            let pool = raw_pool(&database_url()).await;
            let before = snapshot(&pool, &schema).await;
            let mut blocker = pool.begin().await.unwrap();
            sqlx::query(&format!(
                "LOCK TABLE {schema}.{table} IN ROW EXCLUSIVE MODE"
            ))
            .execute(&mut *blocker)
            .await
            .unwrap();
            let application = format!("{schema}_activation");
            let mut url = url::Url::parse(&database_url()).unwrap();
            url.query_pairs_mut()
                .append_pair("application_name", &application);
            let mut config = ProviderConfig::url(url.as_str());
            config.schema_name = Some(schema.clone());
            config.long_poll.enabled = true;
            let mut activation = tokio::spawn(PostgresProvider::new_with_config(config));
            let deadline = Instant::now() + WAIT_BUDGET;
            let first_query = loop {
                let started: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
                    "SELECT query_start FROM pg_stat_activity WHERE application_name = $1 \
                     AND strpos(query, $2) > 0",
                )
                .bind(&application)
                .bind(format!("{table} IN SHARE ROW EXCLUSIVE MODE"))
                .fetch_optional(&pool)
                .await
                .unwrap();
                if let Some(started) = started {
                    break started;
                }

                assert!(
                    Instant::now() < deadline,
                    "activation never attempted the queue lock"
                );
                tokio::time::sleep(POLL_INTERVAL).await;
            };
            tokio::time::sleep(Duration::from_secs(1)).await;
            let retry_started: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE application_name = $1 \
                 AND query_start > $2)",
            )
            .bind(&application)
            .bind(first_query)
            .fetch_one(&pool)
            .await
            .unwrap();
            let still_retrying = !activation.is_finished();
            blocker.rollback().await.unwrap();
            let result = tokio::time::timeout(Duration::from_secs(5), &mut activation).await;
            let activated = result
                .expect("activation should finish after the writer exits")
                .unwrap()
                .expect("activation needs a retry budget longer than 300ms");
            let active: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND tgname LIKE 'trg_notify_%' AND tgenabled = 'O'",
            )
            .bind(&schema)
            .fetch_one(&pool)
            .await
            .unwrap();
            let after = snapshot(&pool, &schema).await;
            let listener_pool = activated.pool().clone();
            drop(activated);
            listener_pool.close().await;
            provider.pool().close().await;
            pool.close().await;
            assert!(
                retry_started,
                "{table}: the first lock wait must be bounded"
            );
            assert!(
                still_retrying,
                "{table}: activation exhausted its budget too early"
            );
            assert_eq!(
                active, 6,
                "all notification triggers must be enabled together"
            );
            assert_eq!(
                after, before,
                "activation must preserve the ledger, function definitions, and work"
            );
        })
        .await;
    }
}

#[tokio::test]
async fn notification_activation_exhausts_its_budget_without_partial_activation() {
    with_schema(|schema| async move {
        let provider = PostgresProvider::new_with_schema(&database_url(), Some(&schema))
            .await
            .unwrap();
        let pool = raw_pool(&database_url()).await;
        let before = snapshot(&pool, &schema).await;
        let mut blocker = pool.begin().await.unwrap();
        sqlx::query(&format!(
            "LOCK TABLE {schema}.worker_queue IN ROW EXCLUSIVE MODE"
        ))
        .execute(&mut *blocker)
        .await
        .unwrap();
        let mut config = ProviderConfig::url(database_url());
        config.schema_name = Some(schema.clone());
        config.long_poll.enabled = true;
        let start = Instant::now();
        let result = tokio::time::timeout(
            Duration::from_secs(12),
            PostgresProvider::new_with_config(config.clone()),
        )
        .await;
        let elapsed = start.elapsed();
        blocker.rollback().await.unwrap();
        let error = match result.expect("activation's lock retry budget must be bounded") {
            Ok(_) => panic!("activation must not succeed while the writer holds its lock"),
            Err(error) => error,
        };
        assert_eq!(
            sqlstate(
                error
                    .downcast_ref::<sqlx::Error>()
                    .expect("original lock error")
            )
            .as_deref(),
            Some("55P03"),
        );
        assert!(
            elapsed >= Duration::from_secs(9),
            "retry budget ended after {elapsed:?}"
        );
        assert_eq!(snapshot(&pool, &schema).await, before);
        let active: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND tgname LIKE 'trg_notify_%' AND tgenabled <> 'D'",
        )
        .bind(&schema)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            active, 0,
            "failed activation must leave every trigger disabled"
        );
        let activated = PostgresProvider::new_with_config(config)
            .await
            .expect("activation must succeed after contention is removed");
        let listener_pool = activated.pool().clone();
        drop(activated);
        listener_pool.close().await;
        provider.pool().close().await;
        pool.close().await;
    })
    .await;
}
