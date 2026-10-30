// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use duroxide::providers::{ExecutionMetadata, InstanceFilter, Provider, ProviderAdmin, WorkItem};
use duroxide::{Client, Event, EventKind, OrchestrationStatus};
use duroxide_pg::PostgresProvider;
use std::sync::Arc;
use std::time::Duration;

type BatchResult = (Option<String>, Option<i64>, i64, i64, i64);

async fn provider() -> (Arc<PostgresProvider>, String) {
    dotenvy::dotenv().ok();
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let schema = format!("pruning_{}", uuid::Uuid::new_v4().simple());
    let provider = PostgresProvider::new_with_schema(&url, Some(&schema))
        .await
        .unwrap();
    (Arc::new(provider), schema)
}

async fn cleanup(provider: &PostgresProvider, schema: &str) {
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(provider.pool())
        .await
        .unwrap();
    provider.pool().close().await;
}

#[tokio::test]
async fn batch_pruning_bounds_scanned_keys_and_rolls_back_atomically() {
    let (provider, schema) = provider().await;
    sqlx::raw_sql(&format!(
        "INSERT INTO {schema}.instances
             (instance_id,orchestration_name,current_execution_id,created_at,updated_at)
         SELECT 'a'||lpad(n::text,4,'0'),'test',1,now(),now() FROM generate_series(1,1500) n;
         INSERT INTO {schema}.instances
             (instance_id,orchestration_name,current_execution_id,created_at,updated_at)
         VALUES ('z','test',1201,now(),now());
         INSERT INTO {schema}.executions(instance_id,execution_id,status,started_at,completed_at)
         SELECT instance_id,1,'Completed',to_timestamp(0),to_timestamp(1) FROM {schema}.instances
         WHERE instance_id<>'z';
         INSERT INTO {schema}.executions(instance_id,execution_id,status,started_at,completed_at)
         SELECT 'z',n,CASE WHEN n=1201 THEN 'Running' ELSE 'ContinuedAsNew' END,
             to_timestamp(0),CASE WHEN n<1201 THEN to_timestamp(1) END FROM generate_series(1,1201) n;
         INSERT INTO {schema}.history(instance_id,execution_id,event_id,event_type,event_data,created_at)
         SELECT instance_id,execution_id,1,'Event','{{}}',now() FROM {schema}.executions;"
    )).execute(provider.pool()).await.unwrap();
    let sql = format!("SELECT * FROM {schema}.prune_executions_batch($1,$2,$3,$4)");
    let mut hostile = provider.pool().begin().await.unwrap();
    sqlx::raw_sql(&format!(
        "UPDATE {schema}.instances SET current_execution_id=2 WHERE instance_id='a0001';
         INSERT INTO {schema}.executions(instance_id,execution_id,status,started_at)
         VALUES ('a0001',2,'Running',to_timestamp(1));
         CREATE FUNCTION {schema}.to_timestamp(double precision) RETURNS timestamptz
             LANGUAGE plpgsql AS $fn$ BEGIN RAISE EXCEPTION 'pruning function trap'; END $fn$;
         SET LOCAL search_path={schema},pg_catalog;
         CREATE TEMP TABLE type_anchor(id int);
         CREATE DOMAIN pg_temp.text AS pg_catalog.text CHECK (false);
         SAVEPOINT trap_probe;"
    ))
    .execute(&mut *hostile)
    .await
    .unwrap();
    let error = sqlx::query("SELECT to_timestamp(0::double precision)")
        .execute(&mut *hostile)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("pruning function trap"));
    sqlx::query("ROLLBACK TO SAVEPOINT trap_probe")
        .execute(&mut *hostile)
        .await
        .unwrap();
    let first: BatchResult = sqlx::query_as(&sql)
        .bind("")
        .bind(0i64)
        .bind(2000i64)
        .bind(1000i32)
        .fetch_one(&mut *hostile)
        .await
        .unwrap();
    assert_eq!(first, (Some("a0999".into()), Some(1), 1000, 1, 1));
    hostile.rollback().await.unwrap();

    sqlx::raw_sql(&format!(
        "CREATE FUNCTION {schema}.reject_prune() RETURNS trigger LANGUAGE plpgsql AS $fn$
         BEGIN IF OLD.instance_id='z' AND OLD.execution_id=3 THEN
             RAISE EXCEPTION 'intentional batch failure';
         END IF; RETURN OLD; END $fn$;
         CREATE TRIGGER reject_prune BEFORE DELETE ON {schema}.executions
         FOR EACH ROW EXECUTE FUNCTION {schema}.reject_prune();"
    ))
    .execute(provider.pool())
    .await
    .unwrap();
    let error = sqlx::query(&sql)
        .bind("a1000")
        .bind(1i64)
        .bind(2000i64)
        .bind(1000i32)
        .execute(provider.pool())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("intentional batch failure"));
    let counts: (i64, i64) = sqlx::query_as(&format!(
        "SELECT (SELECT count(*) FROM {schema}.executions), (SELECT count(*) FROM {schema}.history)"
    ))
    .fetch_one(provider.pool())
    .await
    .unwrap();
    assert_eq!(
        counts,
        (2701, 2701),
        "no partial deletion may survive a failed batch"
    );
    sqlx::query(&format!("DROP TRIGGER reject_prune ON {schema}.executions"))
        .execute(provider.pool())
        .await
        .unwrap();

    let second: BatchResult = sqlx::query_as(&sql)
        .bind("a1000")
        .bind(1i64)
        .bind(2000i64)
        .bind(1000i32)
        .fetch_one(provider.pool())
        .await
        .unwrap();
    assert_eq!(second, (Some("z".into()), Some(500), 1000, 500, 500));
    let third: BatchResult = sqlx::query_as(&sql)
        .bind("z")
        .bind(500i64)
        .bind(2000i64)
        .bind(1000i32)
        .fetch_one(provider.pool())
        .await
        .unwrap();
    assert_eq!(third, (Some("z".into()), Some(1201), 701, 700, 700));
    let empty: BatchResult = sqlx::query_as(&sql)
        .bind("z")
        .bind(1201i64)
        .bind(2000i64)
        .bind(1000i32)
        .fetch_one(provider.pool())
        .await
        .unwrap();
    assert_eq!(empty, (None, None, 0, 0, 0));
    assert_eq!(
        provider.get_execution_info("z", 1201).await.unwrap().status,
        "Running"
    );
    for invalid_limit in [0, 1001] {
        let error = sqlx::query(&sql)
            .bind("")
            .bind(0i64)
            .bind(2000i64)
            .bind(invalid_limit)
            .execute(provider.pool())
            .await
            .unwrap_err();
        assert_eq!(
            error.as_database_error().unwrap().code().as_deref(),
            Some("22023")
        );
    }
    cleanup(&provider, &schema).await;
}

#[tokio::test]
async fn batch_pruning_independently_protects_ineligible_executions() {
    let (provider, schema) = provider().await;
    sqlx::raw_sql(&format!(
        "INSERT INTO {schema}.instances
             (instance_id, orchestration_name, current_execution_id, created_at, updated_at)
         VALUES ('eligibility', 'test', 6, to_timestamp(0), to_timestamp(0));
         INSERT INTO {schema}.executions
             (instance_id, execution_id, status, started_at, completed_at)
         VALUES ('eligibility', 1, 'Running', to_timestamp(0), to_timestamp(1)),
                ('eligibility', 2, 'ContinuedAsNew', to_timestamp(0), NULL),
                ('eligibility', 3, 'Completed', to_timestamp(0), to_timestamp(2)),
                ('eligibility', 4, 'Failed', to_timestamp(0), to_timestamp(3)),
                ('eligibility', 5, 'ContinuedAsNew', to_timestamp(0), to_timestamp(1)),
                ('eligibility', 6, 'Completed', to_timestamp(0), to_timestamp(1)),
                ('eligibility', 7, 'Completed', to_timestamp(0), to_timestamp(1));
         INSERT INTO {schema}.history
             (instance_id, execution_id, event_id, event_type, event_data, created_at)
         SELECT 'eligibility', n, 1, 'Event', '{{}}', to_timestamp(0)
         FROM generate_series(1, 7) n;"
    ))
    .execute(provider.pool())
    .await
    .unwrap();
    let protected_sql = format!(
        "SELECT e.execution_id, to_jsonb(e), to_jsonb(h)
         FROM {schema}.executions e JOIN {schema}.history h
           USING (instance_id, execution_id)
         WHERE e.execution_id <> 5 ORDER BY e.execution_id, h.event_id"
    );
    let before: Vec<(i64, serde_json::Value, serde_json::Value)> = sqlx::query_as(&protected_sql)
        .fetch_all(provider.pool())
        .await
        .unwrap();
    assert_eq!(before.len(), 6);
    let sql = format!("SELECT * FROM {schema}.prune_executions_batch($1, $2, 2000, 1)");
    let mut cursor = (String::new(), 0i64);
    for expected_id in 1..=7 {
        let result: BatchResult = sqlx::query_as(&sql)
            .bind(&cursor.0)
            .bind(cursor.1)
            .fetch_one(provider.pool())
            .await
            .unwrap();
        let deleted = if expected_id == 5 { 1 } else { 0 };
        assert_eq!(
            result,
            (
                Some("eligibility".into()),
                Some(expected_id),
                1,
                deleted,
                deleted
            ),
            "only the eligible historical execution may be pruned"
        );
        cursor = (result.0.unwrap(), result.1.unwrap());
    }
    let after: Vec<(i64, serde_json::Value, serde_json::Value)> = sqlx::query_as(&protected_sql)
        .fetch_all(provider.pool())
        .await
        .unwrap();
    assert_eq!(
        after, before,
        "protected executions and histories must not change"
    );
    cleanup(&provider, &schema).await;
}

#[tokio::test]
async fn age_filtered_bulk_delete_preserves_queued_continuation() {
    let (provider, schema) = provider().await;
    let client = Client::new(provider.clone());
    for status in ["Completed", "Running", "ContinuedAsNew"] {
        client
            .start_orchestration(status, "test", "{}")
            .await
            .unwrap();
        let (_, token, _) = provider
            .fetch_orchestration_item(Duration::from_secs(30), Duration::ZERO, None)
            .await
            .unwrap()
            .unwrap();
        let mut history = vec![Event::with_event_id(
            1,
            status,
            1,
            None,
            EventKind::OrchestrationStarted {
                name: "test".into(),
                version: "1.0.0".into(),
                input: "{}".into(),
                parent_instance: None,
                parent_id: None,
                parent_execution_id: None,
                carry_forward_events: None,
                initial_custom_status: None,
            },
        )];
        let mut followup = Vec::new();
        if status == "ContinuedAsNew" {
            history.push(Event::with_event_id(
                2,
                status,
                1,
                None,
                EventKind::OrchestrationContinuedAsNew {
                    input: "next".into(),
                },
            ));
            followup.push(WorkItem::ContinueAsNew {
                instance: status.into(),
                orchestration: "test".into(),
                input: "next".into(),
                version: Some("1.0.0".into()),
                parent_instance: None,
                parent_id: None,
                parent_execution_id: None,
                carry_forward_events: vec![],
                initial_custom_status: None,
            });
        } else if status == "Completed" {
            history.push(Event::with_event_id(
                2,
                status,
                1,
                None,
                EventKind::OrchestrationCompleted {
                    output: "done".into(),
                },
            ));
        }
        provider
            .ack_orchestration_item(
                &token,
                1,
                history,
                vec![],
                followup,
                ExecutionMetadata {
                    status: Some(status.into()),
                    orchestration_name: Some("test".into()),
                    orchestration_version: Some("1.0.0".into()),
                    ..Default::default()
                },
                vec![],
            )
            .await
            .unwrap();
    }
    let cutoff = u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap() + 1;
    let deleted = client
        .delete_instance_bulk(InstanceFilter {
            completed_before: Some(cutoff),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(deleted.instances_deleted, 1);
    assert!(matches!(
        client
            .get_orchestration_status("ContinuedAsNew")
            .await
            .unwrap(),
        OrchestrationStatus::Running { .. }
    ));
    assert_eq!(
        provider
            .get_execution_info("Running", 1)
            .await
            .unwrap()
            .status,
        "Running"
    );
    let (item, token, _) = provider
        .fetch_orchestration_item(Duration::from_secs(30), Duration::ZERO, None)
        .await
        .unwrap()
        .expect("continuation must remain fetchable");
    assert_eq!(item.instance, "ContinuedAsNew");
    assert!(item.messages.iter().any(
        |message| matches!(message, WorkItem::ContinueAsNew { input, .. } if input == "next")
    ));
    provider
        .abandon_orchestration_item(&token, None, false)
        .await
        .unwrap();
    let unfiltered = client
        .delete_instance_bulk(InstanceFilter {
            instance_ids: Some(vec!["ContinuedAsNew".into()]),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        unfiltered.instances_deleted, 1,
        "unfiltered deletion retains its existing behavior"
    );
    cleanup(&provider, &schema).await;
}
