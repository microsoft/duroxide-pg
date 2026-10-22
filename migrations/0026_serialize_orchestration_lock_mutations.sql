-- Copyright (c) Microsoft Corporation.
-- Licensed under the MIT License.

-- Migration 0026: serialize renewal with orchestration fetches
--
-- fetch_orchestration_item takes pg_advisory_xact_lock(hashtext(instance_id))
-- before it locks the queue row and the instance_locks row. Renewal previously
-- updated instance_locks first and then orchestrator_queue without that
-- advisory lock. The opposing row-lock order permits a fetch/renew deadlock.
--
-- Resolve the live token to its instance without taking a row lock, acquire
-- the same advisory lock as fetch, then update the lease rows. A concurrent
-- fetch or renewal for the instance now waits before either transaction takes
-- its first conflicting row lock. The second UPDATE repeats the token and
-- expiry predicates because the advisory-lock wait can make a token stale.

DO $$
DECLARE
    v_schema_name TEXT := current_schema();
BEGIN
    EXECUTE format($fmt$
        CREATE OR REPLACE FUNCTION %1$I.renew_orchestration_item_lock(
            p_lock_token TEXT,
            p_now_ms BIGINT,
            p_extend_secs BIGINT
        )
        RETURNS VOID AS $renew_orch_lock$
        DECLARE
            v_instance_id TEXT;
            v_locked_until BIGINT;
            v_rows_affected INTEGER;
        BEGIN
            SELECT il.instance_id INTO v_instance_id
            FROM %1$I.instance_locks il
            WHERE il.lock_token = p_lock_token
              AND il.locked_until > p_now_ms;

            IF NOT FOUND THEN
                RAISE EXCEPTION 'Lock token invalid, expired, or already released';
            END IF;

            PERFORM pg_advisory_xact_lock(hashtext(v_instance_id));

            v_locked_until := p_now_ms + (p_extend_secs * 1000);

            UPDATE %1$I.instance_locks
            SET locked_until = v_locked_until
            WHERE instance_id = v_instance_id
              AND lock_token = p_lock_token
              AND locked_until > p_now_ms;

            GET DIAGNOSTICS v_rows_affected = ROW_COUNT;

            IF v_rows_affected = 0 THEN
                RAISE EXCEPTION 'Lock token invalid, expired, or already released';
            END IF;

            UPDATE %1$I.orchestrator_queue
            SET locked_until = v_locked_until
            WHERE lock_token = p_lock_token;
        END;
        $renew_orch_lock$ LANGUAGE plpgsql;
$fmt$, v_schema_name);
END $$;
