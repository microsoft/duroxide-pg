-- Migration: 0008_remove_execution_state.sql
-- Description: Removes ExecutionState from fetch_work_item and renew_work_item_lock
-- Required for duroxide main branch which removed ExecutionState from provider API
-- ExecutionState was part of 0.1.7 but has been removed in favor of simpler API

DO $$
DECLARE
    v_schema_name TEXT := current_schema();
BEGIN
    -- ============================================================================
    -- Update fetch_work_item to return 3 columns (remove ExecutionState)
    -- This simplifies the API - ExecutionState is no longer part of provider trait
    -- ============================================================================
    EXECUTE format('DROP FUNCTION IF EXISTS %I.fetch_work_item(BIGINT, BIGINT)', v_schema_name);

    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.fetch_work_item(
            p_now_ms BIGINT,
            p_lock_timeout_ms BIGINT
        )
        RETURNS TABLE(
            out_work_item TEXT,
            out_lock_token TEXT,
            out_attempt_count INTEGER
        ) AS $fetch_worker$
        DECLARE
            v_id BIGINT;
        BEGIN
            -- Item is available if:
            -- 1. visible_at <= now (not delayed)
            -- 2. AND (lock_token IS NULL OR locked_until <= now) (not locked or lock expired)
            SELECT q.id INTO v_id
            FROM %I.worker_queue q
            WHERE q.visible_at <= TO_TIMESTAMP(p_now_ms / 1000.0)
              AND (q.lock_token IS NULL OR q.locked_until <= p_now_ms)
            ORDER BY q.id
            LIMIT 1
            FOR UPDATE OF q SKIP LOCKED;

            IF NOT FOUND THEN
                RETURN;
            END IF;

            out_lock_token := ''lock_'' || gen_random_uuid()::TEXT;

            -- Increment attempt_count and lock the item
            UPDATE %I.worker_queue
            SET lock_token = out_lock_token,
                locked_until = p_now_ms + p_lock_timeout_ms,
                attempt_count = attempt_count + 1
            WHERE id = v_id;

            SELECT work_item, attempt_count
            INTO out_work_item, out_attempt_count
            FROM %I.worker_queue
            WHERE id = v_id;

            RETURN NEXT;
        END;
        $fetch_worker$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name, v_schema_name, v_schema_name);

    -- ============================================================================
    -- Update renew_work_item_lock to return VOID (remove ExecutionState)
    -- This simplifies the API - ExecutionState is no longer part of provider trait
    -- ============================================================================
    EXECUTE format('DROP FUNCTION IF EXISTS %I.renew_work_item_lock(TEXT, BIGINT, BIGINT)', v_schema_name);

    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.renew_work_item_lock(
            p_lock_token TEXT,
            p_now_ms BIGINT,
            p_extend_secs BIGINT
        )
        RETURNS VOID AS $renew_lock$
        DECLARE
            v_locked_until BIGINT;
            v_rows_affected INTEGER;
        BEGIN
            -- Calculate new locked_until timestamp
            v_locked_until := p_now_ms + (p_extend_secs * 1000);
            
            -- Update lock timeout only if lock is still valid
            -- Use p_now_ms (from application) for consistent time reference
            UPDATE %I.worker_queue
            SET locked_until = v_locked_until
            WHERE lock_token = p_lock_token
              AND locked_until > p_now_ms;
            
            GET DIAGNOSTICS v_rows_affected = ROW_COUNT;
            
            IF v_rows_affected = 0 THEN
                RAISE EXCEPTION ''Lock token invalid, expired, or already acked'';
            END IF;
        END;
        $renew_lock$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name);
END $$;
