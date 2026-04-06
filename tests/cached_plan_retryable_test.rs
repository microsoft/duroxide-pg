//! Tests that `cached plan must not change result type` (SQLSTATE 0A000) is
//! handled as a retryable error, allowing transparent recovery when a stored
//! procedure is replaced by a concurrent migration.

mod common;

use duroxide::providers::Provider;
use duroxide_pg::PostgresProvider;

fn get_database_url() -> String {
    dotenvy::dotenv().ok();
    std::env::var("DATABASE_URL").expect("DATABASE_URL must be set")
}

fn next_schema_name(prefix: &str) -> String {
    let guid = uuid::Uuid::new_v4().to_string();
    let suffix = &guid[guid.len() - 8..];
    format!("{prefix}_{suffix}")
}

/// Simulates the scenario from the production incident:
/// 1. Worker boots, starts polling (sqlx caches prepared statement)
/// 2. Concurrent migration replaces a stored procedure (new OID + return type)
/// 3. Next poll hits `cached plan must not change result type`
/// 4. Retry re-prepares and succeeds
#[tokio::test]
async fn cached_plan_invalidation_is_retried() {
    let database_url = get_database_url();
    let schema = next_schema_name("cached_plan");

    // 1. Create provider — migrations run, schema is fully set up.
    let provider = PostgresProvider::new_with_schema(&database_url, Some(&schema))
        .await
        .expect("Provider creation should succeed");

    // 2. Call fetch_orchestration_item to cache the prepared statement on a
    //    pool connection. Result doesn't matter (empty queue → None).
    let _ = provider
        .fetch_orchestration_item(
            std::time::Duration::from_secs(10),
            std::time::Duration::ZERO,
            None,
        )
        .await;

    // 3. Replace the stored procedure with one that returns a different type.
    //    This simulates what happens when a migration runs DROP + CREATE OR
    //    REPLACE on a function while workers are actively polling.
    sqlx::query(&format!(
        r#"
        DROP FUNCTION IF EXISTS {schema}.fetch_orchestration_item(BIGINT, BIGINT, BIGINT, BIGINT);
        "#
    ))
    .execute(provider.pool())
    .await
    .expect("DROP FUNCTION should succeed");

    sqlx::query(&format!(
        r#"
        CREATE FUNCTION {schema}.fetch_orchestration_item(
            p_now_ms BIGINT,
            p_lock_timeout_ms BIGINT,
            p_min_version_packed BIGINT DEFAULT NULL,
            p_max_version_packed BIGINT DEFAULT NULL
        )
        RETURNS TABLE(
            instance_id TEXT,
            execution_id TEXT,
            lock_token TEXT,
            locked_until_ms BIGINT,
            history JSONB,
            work_items JSONB,
            orchestration_name TEXT,
            attempt_count INT,
            kv_snapshot JSONB,
            extra_dummy_column TEXT
        ) AS $$
        BEGIN
            -- Return nothing, just need the different return type to trigger 0A000
            RETURN;
        END;
        $$ LANGUAGE plpgsql;
        "#
    ))
    .execute(provider.pool())
    .await
    .expect("CREATE FUNCTION should succeed");

    // 4. Call fetch_orchestration_item again. On the connection that has the
    //    stale cached plan, this will get 0A000. With our fix marking it
    //    retryable, the retry loop should re-prepare on a fresh connection
    //    and succeed.
    //
    //    Note: the replacement function returns no rows (empty result set),
    //    so the call should succeed with Ok(None) or a deserialization error
    //    that is NOT 0A000. The key assertion is that we don't get a permanent
    //    ProviderError with "cached plan" in the message.
    let result = provider
        .fetch_orchestration_item(
            std::time::Duration::from_secs(10),
            std::time::Duration::ZERO,
            None,
        )
        .await;

    match &result {
        Ok(_) => {} // Success — retry worked
        Err(e) => {
            // If we get an error, it must NOT be the cached plan error.
            // Any other error (e.g., column mismatch on the dummy function)
            // is acceptable — the point is 0A000 was retried, not surfaced.
            assert!(
                !e.message.contains("cached plan"),
                "cached plan error should have been retried, not surfaced: {e:?}"
            );
        }
    }

    // Cleanup
    common::cleanup_schema(&schema).await;
}
