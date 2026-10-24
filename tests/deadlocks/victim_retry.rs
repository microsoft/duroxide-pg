// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! What the provider does when PostgreSQL picks renewal as a deadlock victim.

use crate::harness::*;
use duroxide::providers::Provider;
use std::time::Duration;

/// Renewal waits for the instance's advisory lock. When fetches deadlock each
/// other on that lock, PostgreSQL can abort the waiting renewal instead
/// (SQLSTATE 40P01). The provider must retry it rather than return the error,
/// because the runtime stops renewing a turn after any renewal error.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn renewal_retries_when_aborted_as_deadlock_victim() {
    with_schema(|schema| async move {
        let database_url = database_url();
        let (provider, instance, token) =
            fetch_new_instance(&schema, Duration::from_secs(60)).await;
        let pool = raw_pool(&database_url).await;
        let before = read_lease(&pool, &schema, &instance)
            .await
            .expect("the instance must be locked");

        // Abort the first renewal attempt as a deadlock victim. The sequence
        // is not rolled back with the aborted transaction, so the next
        // attempt passes.
        sqlx::query(&format!("CREATE SEQUENCE {schema}.renewal_attempts"))
            .execute(&pool)
            .await
            .expect("failed to create attempt counter");
        sqlx::query(&format!(
            "CREATE FUNCTION {schema}.fail_first_renewal()
             RETURNS trigger
             LANGUAGE plpgsql
             AS $function$
             BEGIN
                 IF NEW.lock_token = {token_sql}
                    AND nextval('{schema}.renewal_attempts') = 1 THEN
                     RAISE EXCEPTION 'injected deadlock' USING ERRCODE = '{DEADLOCK_DETECTED}';
                 END IF;
                 RETURN NEW;
             END;
             $function$",
            token_sql = token_literal(&token),
        ))
        .execute(&pool)
        .await
        .expect("failed to create failure function");
        sqlx::query(&format!(
            "CREATE TRIGGER fail_first_renewal
             BEFORE UPDATE OF locked_until ON {schema}.orchestrator_queue
             FOR EACH ROW EXECUTE FUNCTION {schema}.fail_first_renewal()"
        ))
        .execute(&pool)
        .await
        .expect("failed to create failure trigger");

        timed(
            "renewal",
            provider.renew_orchestration_item_lock(&token, Duration::from_secs(120)),
        )
        .await
        .expect("renewal must be retried after it was aborted as a deadlock victim");

        let attempts: i64 =
            sqlx::query_scalar(&format!("SELECT last_value FROM {schema}.renewal_attempts"))
                .fetch_one(&pool)
                .await
                .expect("failed to read attempt counter");
        assert!(
            attempts >= 2,
            "renewal was not retried: {attempts} attempt(s)"
        );

        let after = read_lease(&pool, &schema, &instance)
            .await
            .expect("the instance must stay locked");
        assert_eq!(after.token, token);
        assert!(
            after.locked_until > before.locked_until,
            "the retried renewal must extend the lease"
        );
        after.assert_queue_matches();
    })
    .await;
}
