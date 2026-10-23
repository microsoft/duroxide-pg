-- Copyright (c) Microsoft Corporation.
-- Licensed under the MIT License.

-- Migration 0026: serialize orchestration lock renewal with fetch
--
-- fetch_orchestration_item takes pg_advisory_xact_lock(hashtext(instance_id))
-- before it locks orchestrator_queue rows and the instance_locks row. Renewal
-- updated instance_locks and then orchestrator_queue without that lock, so a
-- fetch and a renewal could each hold one row and wait for the other
-- (SQLSTATE 40P01).
--
-- Renewal now:
--   1. Resolves the token to its instance without taking a row lock.
--   2. Takes the same advisory lock as fetch. Neither function then holds a
--      row lock while it waits for the other.
--   3. Reads the time after that wait. p_now_ms was read in Rust before the
--      connection was acquired and before the wait. Checking expiry or
--      computing the deadline from it can renew a lease that expired during
--      the wait, or write a deadline that is already in the past. The clock
--      rule is the one fetch uses since migration 0025.
--   4. Revalidates the token and expiry at that time, and never moves the
--      deadline earlier.
--   5. Updates orchestrator_queue before instance_locks. This is the order of
--      ack_orchestration_item and abandon_orchestration_item, which do not
--      take the advisory lock; the opposite order lets renewal deadlock with
--      them. The advisory lock keeps fetch out whatever the row order.
-- If acknowledge or abandon releases the lock first, the final update matches
-- no row and the function raises, which rolls back the queue update.

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
            v_now_ms BIGINT;
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

            v_now_ms := GREATEST(
                p_now_ms + (EXTRACT(EPOCH FROM (clock_timestamp() - statement_timestamp())) * 1000)::BIGINT,
                (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT
            );

            SELECT GREATEST(il.locked_until, v_now_ms + (p_extend_secs * 1000))
            INTO v_locked_until
            FROM %1$I.instance_locks il
            WHERE il.instance_id = v_instance_id
              AND il.lock_token = p_lock_token
              AND il.locked_until > v_now_ms;

            IF NOT FOUND THEN
                RAISE EXCEPTION 'Lock token invalid, expired, or already released';
            END IF;

            UPDATE %1$I.orchestrator_queue
            SET locked_until = v_locked_until
            WHERE lock_token = p_lock_token;

            UPDATE %1$I.instance_locks
            SET locked_until = v_locked_until
            WHERE instance_id = v_instance_id
              AND lock_token = p_lock_token;

            GET DIAGNOSTICS v_rows_affected = ROW_COUNT;

            IF v_rows_affected = 0 THEN
                RAISE EXCEPTION 'Lock token invalid, expired, or already released';
            END IF;
        END;
        $renew_orch_lock$ LANGUAGE plpgsql;
$fmt$, v_schema_name);
END $$;
