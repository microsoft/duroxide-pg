// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use duroxide::providers::{Provider, SessionFetchConfig, TagFilter, WorkItem};
use duroxide_pg::PostgresProvider;
use std::time::Duration;

#[path = "deadlocks/harness.rs"]
mod harness;

#[tokio::test]
async fn expired_deferrals_yield_to_already_ready_work() {
    for session_bound in [false, true] {
        for tagged in [false, true] {
            harness::with_schema(move |schema| async move {
                let store =
                    PostgresProvider::new_with_schema(&harness::database_url(), Some(&schema))
                        .await
                        .expect("create worker progress provider");
                let session = session_bound.then(|| SessionFetchConfig {
                    owner_id: "progress-worker".to_string(),
                    lock_timeout: Duration::from_secs(30),
                });
                let tag_filter = if tagged {
                    TagFilter::tags(["limited"])
                } else {
                    TagFilter::DefaultOnly
                };
                for id in [1, 2] {
                    store
                        .enqueue_for_worker(WorkItem::ActivityExecute {
                            instance: "progress".to_string(),
                            execution_id: 1,
                            id,
                            name: "Activity".to_string(),
                            input: String::new(),
                            session_id: session_bound.then(|| "progress-session".to_string()),
                            tag: tagged.then(|| "limited".to_string()),
                        })
                        .await
                        .unwrap();
                }
                let (item, token, _) = store
                    .fetch_work_item(
                        Duration::from_secs(30),
                        Duration::ZERO,
                        session.as_ref(),
                        &tag_filter,
                    )
                    .await
                    .unwrap()
                    .unwrap();
                assert!(matches!(item, WorkItem::ActivityExecute { id: 1, .. }));
                store
                    .abandon_work_item(&token, Some(Duration::from_millis(1)), true)
                    .await
                    .unwrap();
                tokio::time::sleep(Duration::from_millis(20)).await;

                let (item, token, _) = store
                    .fetch_work_item(
                        Duration::from_secs(30),
                        Duration::ZERO,
                        session.as_ref(),
                        &tag_filter,
                    )
                    .await
                    .unwrap()
                    .unwrap();
                assert!(
                matches!(item, WorkItem::ActivityExecute { id: 2, .. }),
                "expired deferral overtook ready work: session={session_bound}, tagged={tagged}"
            );
                store.ack_work_item(&token, None).await.unwrap();

                let (item, token, attempts) = store
                    .fetch_work_item(
                        Duration::from_secs(30),
                        Duration::ZERO,
                        session.as_ref(),
                        &tag_filter,
                    )
                    .await
                    .unwrap()
                    .unwrap();
                assert!(matches!(item, WorkItem::ActivityExecute { id: 1, .. }));
                assert_eq!(attempts, 1);
                store.ack_work_item(&token, None).await.unwrap();
            })
            .await;
        }
    }
}

#[tokio::test]
async fn worker_progress_migration_validates_prebuilt_index() {
    harness::with_schema(|schema| async move {
        let database_url = harness::database_url();
        let store = PostgresProvider::new_with_schema(&database_url, Some(&schema))
            .await
            .expect("create worker progress provider");
        let pool = store.pool();
        store
            .enqueue_for_worker(WorkItem::ActivityExecute {
                instance: "progress".to_string(),
                execution_id: 1,
                id: 1,
                name: "Activity".to_string(),
                input: String::new(),
                session_id: None,
                tag: None,
            })
            .await
            .unwrap();

        let signature = format!("{schema}.fetch_work_item(bigint,bigint,text,bigint,text[],text)");
        let current_function: String =
            sqlx::query_scalar("SELECT pg_get_functiondef($1::regprocedure)")
                .bind(&signature)
                .fetch_one(pool)
                .await
                .unwrap();
        assert_eq!(current_function.matches("ORDER BY q.visible_at, q.id").count(), 2);
        let baseline_function = current_function.replace("ORDER BY q.visible_at, q.id", "ORDER BY q.id");

        // Mark this and later migrations pending to preserve the ledger prefix.
        let mut tx = pool.begin().await.unwrap();
        sqlx::query(&format!("DROP INDEX {schema}.idx_worker_ready_order"))
            .execute(&mut *tx)
            .await
            .unwrap();
        assert!(
            sqlx::query(&format!("DELETE FROM {schema}._duroxide_migrations WHERE version >= 29"))
                .execute(&mut *tx)
                .await
                .unwrap()
                .rows_affected()
                >= 1
        );
        sqlx::query(&baseline_function).execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();

        // A writer lets the concurrent build register its index, but prevents
        // its first scan. Timing out leaves a real invalid index behind.
        let mut writer = pool.begin().await.unwrap();
        sqlx::query(&format!("LOCK TABLE {schema}.worker_queue IN ROW EXCLUSIVE MODE"))
            .execute(&mut *writer)
            .await
            .unwrap();
        let mut builder = pool.acquire().await.unwrap();
        sqlx::query("SET lock_timeout = '100ms'")
            .execute(&mut *builder)
            .await
            .unwrap();
        let create_index = format!(
            "CREATE INDEX CONCURRENTLY idx_worker_ready_order ON {schema}.worker_queue (visible_at, id)"
        );
        let error = sqlx::query(&create_index)
            .execute(&mut *builder)
            .await
            .expect_err("the writer should block the concurrent index build");
        assert_eq!(error.as_database_error().unwrap().code().as_deref(), Some("55P03"));
        writer.rollback().await.unwrap();
        sqlx::query("RESET lock_timeout")
            .execute(&mut *builder)
            .await
            .unwrap();

        let index_state = format!(
            "SELECT indexrelid::bigint, indisvalid FROM pg_catalog.pg_index
             WHERE indexrelid = '{schema}.idx_worker_ready_order'::regclass"
        );
        let (_, valid): (i64, bool) = sqlx::query_as(&index_state).fetch_one(pool).await.unwrap();
        assert!(!valid, "the failed concurrent build must leave an invalid index");

        let snapshot = format!(
            "SELECT jsonb_build_object(
                'ledger', (SELECT jsonb_agg(to_jsonb(m) ORDER BY version)
                           FROM {schema}._duroxide_migrations m),
                'function', pg_get_functiondef('{signature}'::regprocedure),
                'queue', (SELECT jsonb_agg(to_jsonb(q) ORDER BY id) FROM {schema}.worker_queue q),
                'index', (SELECT to_jsonb(i) FROM pg_catalog.pg_index i
                          WHERE indexrelid = '{schema}.idx_worker_ready_order'::regclass)
            )"
        );
        let before: serde_json::Value = sqlx::query_scalar(&snapshot).fetch_one(pool).await.unwrap();
        let error = match PostgresProvider::new_with_schema(&database_url, Some(&schema)).await {
            Ok(_) => panic!("migration must reject an invalid prebuilt index"),
            Err(error) => format!("{error:#}"),
        };
        assert!(
            error.contains("idx_worker_ready_order") && error.contains("invalid"),
            "expected an invalid-index error, got: {error}"
        );
        let after: serde_json::Value = sqlx::query_scalar(&snapshot).fetch_one(pool).await.unwrap();
        assert_eq!(after, before, "rejected startup must not change durable or catalog state");

        sqlx::query(&format!("DROP INDEX CONCURRENTLY {schema}.idx_worker_ready_order"))
            .execute(&mut *builder)
            .await
            .unwrap();
        sqlx::query(&create_index).execute(&mut *builder).await.unwrap();
        let prebuilt: (i64, bool) = sqlx::query_as(&index_state).fetch_one(pool).await.unwrap();
        assert!(prebuilt.1, "the completed concurrent build must be valid");
        let _upgraded = PostgresProvider::new_with_schema(&database_url, Some(&schema))
            .await
            .expect("migration must accept the valid prebuilt index");
        let reused: (i64, bool) = sqlx::query_as(&index_state).fetch_one(pool).await.unwrap();
        assert_eq!(reused, prebuilt, "migration must reuse the prebuilt index");
        let after: serde_json::Value = sqlx::query_scalar(&snapshot).fetch_one(pool).await.unwrap();
        assert_eq!(after["queue"], before["queue"]);
        assert_eq!(after["function"], current_function);
        let migration_name: String = sqlx::query_scalar(&format!(
            "SELECT name FROM {schema}._duroxide_migrations WHERE version = 29"
        ))
        .fetch_one(pool)
        .await
        .unwrap();
        assert_eq!(migration_name, "0029_order_worker_queue_by_visibility.sql");
    })
    .await;
}
