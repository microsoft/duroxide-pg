-- Migration: 0004_update_stored_procedures_for_attempt_count.sql
-- Description: Updates stored procedures to support attempt_count for poison message detection
-- This migration updates procedures that were created in 0002 to work with the attempt_count
-- column added in 0003.

DO $$
DECLARE
    v_schema_name TEXT := current_schema();
BEGIN
    -- ============================================================================
    -- Update fetch_work_item to return attempt_count
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
            SELECT q.id INTO v_id
            FROM %I.worker_queue q
            WHERE q.lock_token IS NULL OR q.locked_until <= p_now_ms
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
    -- Update fetch_orchestration_item to return attempt_count
    -- ============================================================================
    EXECUTE format('DROP FUNCTION IF EXISTS %I.fetch_orchestration_item(BIGINT, BIGINT)', v_schema_name);
    
    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.fetch_orchestration_item(
            p_now_ms BIGINT,
            p_lock_timeout_ms BIGINT
        )
        RETURNS TABLE(
            out_instance_id TEXT,
            out_orchestration_name TEXT,
            out_orchestration_version TEXT,
            out_execution_id BIGINT,
            out_history JSONB,
            out_messages JSONB,
            out_lock_token TEXT,
            out_attempt_count INTEGER
        ) AS $fetch_orch$
        DECLARE
            v_instance_id TEXT;
            v_lock_token TEXT;
            v_locked_until BIGINT;
            v_orchestration_name TEXT;
            v_orchestration_version TEXT;
            v_current_execution_id BIGINT;
            v_history JSONB;
            v_messages JSONB;
            v_lock_acquired INTEGER;
            v_max_attempt_count INTEGER;
        BEGIN
            -- Phase 1: Find a candidate instance (no FOR UPDATE yet)
            SELECT q.instance_id INTO v_instance_id
            FROM %I.orchestrator_queue q
            WHERE q.visible_at <= TO_TIMESTAMP(p_now_ms / 1000.0)
              AND NOT EXISTS (
                SELECT 1 FROM %I.instance_locks il
                WHERE il.instance_id = q.instance_id AND il.locked_until > p_now_ms
              )
            ORDER BY q.visible_at, q.id
            LIMIT 1;

            IF NOT FOUND THEN
                RETURN;
            END IF;

            -- Phase 2: Acquire instance-level advisory lock
            PERFORM pg_advisory_xact_lock(hashtext(v_instance_id));

            -- Phase 3: Re-verify the instance is still available with FOR UPDATE
            SELECT q.instance_id INTO v_instance_id
            FROM %I.orchestrator_queue q
            WHERE q.instance_id = v_instance_id
              AND q.visible_at <= TO_TIMESTAMP(p_now_ms / 1000.0)
              AND NOT EXISTS (
                SELECT 1 FROM %I.instance_locks il
                WHERE il.instance_id = q.instance_id AND il.locked_until > p_now_ms
              )
            FOR UPDATE OF q SKIP LOCKED;

            IF NOT FOUND THEN
                RETURN;
            END IF;

            -- Step 2: Generate lock token and acquire instance lock
            v_lock_token := ''lock_'' || gen_random_uuid()::TEXT;
            v_locked_until := p_now_ms + p_lock_timeout_ms;

            INSERT INTO %I.instance_locks (instance_id, lock_token, locked_until, locked_at)
            VALUES (v_instance_id, v_lock_token, v_locked_until, p_now_ms)
            ON CONFLICT(instance_id) DO UPDATE
            SET lock_token = EXCLUDED.lock_token,
                locked_until = EXCLUDED.locked_until,
                locked_at = EXCLUDED.locked_at
            WHERE %I.instance_locks.locked_until <= p_now_ms;

            GET DIAGNOSTICS v_lock_acquired = ROW_COUNT;

            IF v_lock_acquired = 0 THEN
                RETURN;
            END IF;

            -- Step 3: Mark all visible messages with our lock and increment attempt_count
            UPDATE %I.orchestrator_queue q
            SET lock_token = v_lock_token,
                locked_until = v_locked_until,
                attempt_count = q.attempt_count + 1
            WHERE q.instance_id = v_instance_id
              AND q.visible_at <= TO_TIMESTAMP(p_now_ms / 1000.0)
              AND (q.lock_token IS NULL OR q.locked_until <= p_now_ms);

            -- Step 4: Fetch all locked messages and get max attempt_count
            SELECT COALESCE(JSONB_AGG(q.work_item::JSONB ORDER BY q.id), ''[]''::JSONB),
                   COALESCE(MAX(q.attempt_count), 1)
            INTO v_messages, v_max_attempt_count
            FROM %I.orchestrator_queue q
            WHERE q.lock_token = v_lock_token;

            -- Step 5: Load instance metadata
            SELECT i.orchestration_name, i.orchestration_version, i.current_execution_id
            INTO v_orchestration_name, v_orchestration_version, v_current_execution_id
            FROM %I.instances i
            WHERE i.instance_id = v_instance_id;

            -- Step 6: Load history or implement fallback
            IF FOUND THEN
                SELECT COALESCE(JSONB_AGG(h.event_data::JSONB ORDER BY h.event_id), ''[]''::JSONB)
                INTO v_history
                FROM %I.history h
                WHERE h.instance_id = v_instance_id AND h.execution_id = v_current_execution_id;
                
                v_orchestration_version := COALESCE(v_orchestration_version, ''unknown'');
            ELSE
                SELECT COALESCE(JSONB_AGG(h.event_data::JSONB ORDER BY h.execution_id, h.event_id), ''[]''::JSONB)
                INTO v_history
                FROM %I.history h
                WHERE h.instance_id = v_instance_id;

                IF JSONB_ARRAY_LENGTH(v_history) > 0 AND v_history->0 ? ''OrchestrationStarted'' THEN
                    v_orchestration_name := v_history->0->''OrchestrationStarted''->>''name'';
                    v_orchestration_version := v_history->0->''OrchestrationStarted''->>''version'';
                    v_current_execution_id := 1;
                ELSIF JSONB_ARRAY_LENGTH(v_messages) > 0 AND v_messages->0 ? ''StartOrchestration'' THEN
                    v_orchestration_name := v_messages->0->''StartOrchestration''->>''orchestration'';
                    v_orchestration_version := COALESCE(v_messages->0->''StartOrchestration''->>''version'', ''unknown'');
                    v_current_execution_id := COALESCE((v_messages->0->''StartOrchestration''->>''execution_id'')::BIGINT, 1);
                ELSIF JSONB_ARRAY_LENGTH(v_messages) > 0 AND v_messages->0 ? ''ContinueAsNew'' THEN
                    v_orchestration_name := v_messages->0->''ContinueAsNew''->>''orchestration'';
                    v_orchestration_version := COALESCE(v_messages->0->''ContinueAsNew''->>''version'', ''unknown'');
                    v_current_execution_id := 1;
                ELSE
                    v_orchestration_name := ''Unknown'';
                    v_orchestration_version := ''unknown'';
                    v_current_execution_id := 1;
                END IF;
            END IF;

            RETURN QUERY SELECT
                v_instance_id,
                v_orchestration_name,
                v_orchestration_version,
                v_current_execution_id,
                v_history,
                v_messages,
                v_lock_token,
                v_max_attempt_count;
        END;
        $fetch_orch$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, 
       v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, 
       v_schema_name, v_schema_name, v_schema_name);

    -- ============================================================================
    -- Update abandon_work_item to support delay and ignore_attempt
    -- ============================================================================
    EXECUTE format('DROP FUNCTION IF EXISTS %I.abandon_work_item(TEXT, BIGINT, BOOLEAN)', v_schema_name);

    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.abandon_work_item(
            p_lock_token TEXT,
            p_delay_ms BIGINT DEFAULT NULL,
            p_ignore_attempt BOOLEAN DEFAULT FALSE
        )
        RETURNS VOID AS $abandon_worker$
        DECLARE
            v_rows_affected INTEGER;
            v_locked_until BIGINT;
        BEGIN
            IF p_delay_ms IS NOT NULL AND p_delay_ms > 0 THEN
                -- Delay provided: keep lock_token, just update locked_until
                v_locked_until := (EXTRACT(EPOCH FROM NOW()) * 1000)::BIGINT + p_delay_ms;
                
                IF p_ignore_attempt THEN
                    UPDATE %I.worker_queue
                    SET locked_until = v_locked_until,
                        attempt_count = GREATEST(0, attempt_count - 1)
                    WHERE lock_token = p_lock_token;
                ELSE
                    UPDATE %I.worker_queue
                    SET locked_until = v_locked_until
                    WHERE lock_token = p_lock_token;
                END IF;
            ELSE
                -- No delay: clear lock_token for immediate availability
                IF p_ignore_attempt THEN
                    UPDATE %I.worker_queue
                    SET lock_token = NULL,
                        locked_until = NULL,
                        attempt_count = GREATEST(0, attempt_count - 1)
                    WHERE lock_token = p_lock_token;
                ELSE
                    UPDATE %I.worker_queue
                    SET lock_token = NULL,
                        locked_until = NULL
                    WHERE lock_token = p_lock_token;
                END IF;
            END IF;

            GET DIAGNOSTICS v_rows_affected = ROW_COUNT;

            IF v_rows_affected = 0 THEN
                RAISE EXCEPTION ''Invalid lock token or already acked'';
            END IF;
        END;
        $abandon_worker$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name);

    -- ============================================================================
    -- Update abandon_orchestration_item to support ignore_attempt
    -- ============================================================================
    EXECUTE format('DROP FUNCTION IF EXISTS %I.abandon_orchestration_item(TEXT, BIGINT)', v_schema_name);
    EXECUTE format('DROP FUNCTION IF EXISTS %I.abandon_orchestration_item(TEXT, BIGINT, BOOLEAN)', v_schema_name);

    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.abandon_orchestration_item(
            p_lock_token TEXT,
            p_delay_ms BIGINT DEFAULT NULL,
            p_ignore_attempt BOOLEAN DEFAULT FALSE
        )
        RETURNS TEXT AS $abandon_orch$
        DECLARE
            v_instance_id TEXT;
        BEGIN
            SELECT il.instance_id INTO v_instance_id
            FROM %I.instance_locks il
            WHERE il.lock_token = p_lock_token;

            IF NOT FOUND THEN
                RAISE EXCEPTION ''Invalid lock token'';
            END IF;

            IF p_delay_ms IS NOT NULL AND p_delay_ms > 0 THEN
                -- Delay provided: set visible_at to future time
                IF p_ignore_attempt THEN
                    UPDATE %I.orchestrator_queue
                    SET lock_token = NULL,
                        locked_until = NULL,
                        visible_at = NOW() + (p_delay_ms::DOUBLE PRECISION / 1000.0) * INTERVAL ''1 second'',
                        attempt_count = GREATEST(0, attempt_count - 1)
                    WHERE lock_token = p_lock_token;
                ELSE
                    UPDATE %I.orchestrator_queue
                    SET lock_token = NULL,
                        locked_until = NULL,
                        visible_at = NOW() + (p_delay_ms::DOUBLE PRECISION / 1000.0) * INTERVAL ''1 second''
                    WHERE lock_token = p_lock_token;
                END IF;
            ELSE
                -- No delay: just clear lock, don''t update visible_at
                IF p_ignore_attempt THEN
                    UPDATE %I.orchestrator_queue
                    SET lock_token = NULL,
                        locked_until = NULL,
                        attempt_count = GREATEST(0, attempt_count - 1)
                    WHERE lock_token = p_lock_token;
                ELSE
                    UPDATE %I.orchestrator_queue
                    SET lock_token = NULL,
                        locked_until = NULL
                    WHERE lock_token = p_lock_token;
                END IF;
            END IF;

            DELETE FROM %I.instance_locks
            WHERE lock_token = p_lock_token;

            RETURN v_instance_id;
        END;
        $abandon_orch$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name);

    -- ============================================================================
    -- Add renew_orchestration_item_lock procedure
    -- ============================================================================
    EXECUTE format('DROP FUNCTION IF EXISTS %I.renew_orchestration_item_lock(TEXT, BIGINT, BIGINT)', v_schema_name);

    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.renew_orchestration_item_lock(
            p_lock_token TEXT,
            p_now_ms BIGINT,
            p_extend_secs BIGINT
        )
        RETURNS VOID AS $renew_orch_lock$
        DECLARE
            v_locked_until BIGINT;
            v_rows_affected INTEGER;
        BEGIN
            v_locked_until := p_now_ms + (p_extend_secs * 1000);
            
            UPDATE %I.instance_locks
            SET locked_until = v_locked_until
            WHERE lock_token = p_lock_token
              AND locked_until > p_now_ms;
            
            GET DIAGNOSTICS v_rows_affected = ROW_COUNT;
            
            IF v_rows_affected = 0 THEN
                RAISE EXCEPTION ''Lock token invalid, expired, or already released'';
            END IF;

            UPDATE %I.orchestrator_queue
            SET locked_until = v_locked_until
            WHERE lock_token = p_lock_token;
        END;
        $renew_orch_lock$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name, v_schema_name);

END $$;

