-- Migration 0005: Remove DB-generated timestamps, use Rust clock only
-- All timestamps must be provided by the Rust provider via p_now_ms parameters.
-- This ensures a single clock source across all nodes for consistent timing.

-- Get the current schema name (set by migration runner)
DO $$
DECLARE
    v_schema_name TEXT := current_schema();
BEGIN
    -- ============================================================================
    -- Update enqueue_worker_work to accept timestamp
    -- ============================================================================
    EXECUTE format('DROP FUNCTION IF EXISTS %I.enqueue_worker_work(TEXT)', v_schema_name);

    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.enqueue_worker_work(
            p_work_item TEXT,
            p_now_ms BIGINT
        )
        RETURNS VOID AS $enq_worker$
        BEGIN
            INSERT INTO %I.worker_queue (work_item, created_at)
            VALUES (p_work_item, TO_TIMESTAMP(p_now_ms / 1000.0));
        END;
        $enq_worker$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name);

    -- ============================================================================
    -- Update ack_worker to accept timestamp
    -- ============================================================================
    EXECUTE format('DROP FUNCTION IF EXISTS %I.ack_worker(TEXT, TEXT, TEXT)', v_schema_name);

    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.ack_worker(
            p_lock_token TEXT,
            p_instance_id TEXT,
            p_completion_json TEXT,
            p_now_ms BIGINT
        )
        RETURNS VOID AS $ack_worker$
        DECLARE
            v_rows_affected INTEGER;
            v_now_ts TIMESTAMPTZ;
        BEGIN
            v_now_ts := TO_TIMESTAMP(p_now_ms / 1000.0);
            
            -- Delete the worker queue item
            DELETE FROM %I.worker_queue WHERE lock_token = p_lock_token;
            GET DIAGNOSTICS v_rows_affected = ROW_COUNT;

            IF v_rows_affected = 0 THEN
                RAISE EXCEPTION ''Worker queue item not found or already processed'';
            END IF;

            -- Enqueue completion to orchestrator queue
            INSERT INTO %I.orchestrator_queue (instance_id, work_item, visible_at, created_at)
            VALUES (p_instance_id, p_completion_json, v_now_ts, v_now_ts);
        END;
        $ack_worker$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name, v_schema_name);

    -- ============================================================================
    -- Update enqueue_orchestrator_work to accept created_at timestamp
    -- ============================================================================
    EXECUTE format('DROP FUNCTION IF EXISTS %I.enqueue_orchestrator_work(TEXT, TEXT, TIMESTAMPTZ, TEXT, TEXT, BIGINT)', v_schema_name);

    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.enqueue_orchestrator_work(
            p_instance_id TEXT,
            p_work_item TEXT,
            p_visible_at TIMESTAMPTZ,
            p_now_ms BIGINT,
            p_orchestration_name TEXT DEFAULT NULL,
            p_orchestration_version TEXT DEFAULT NULL,
            p_execution_id BIGINT DEFAULT NULL
        )
        RETURNS VOID AS $enq_orch$
        BEGIN
            -- ⚠️ CRITICAL: Parameters p_orchestration_name, p_orchestration_version, p_execution_id are ignored
            -- Instance creation happens ONLY via ack_orchestration_item metadata
            
            -- Insert into orchestrator queue
            INSERT INTO %I.orchestrator_queue (instance_id, work_item, visible_at, created_at)
            VALUES (p_instance_id, p_work_item, p_visible_at, TO_TIMESTAMP(p_now_ms / 1000.0));
        END;
        $enq_orch$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name);

    -- ============================================================================
    -- Update ack_orchestration_item to accept timestamp
    -- ============================================================================
    EXECUTE format('DROP FUNCTION IF EXISTS %I.ack_orchestration_item(TEXT, BIGINT, JSONB, JSONB, JSONB, JSONB)', v_schema_name);

    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.ack_orchestration_item(
            p_lock_token TEXT,
            p_now_ms BIGINT,
            p_execution_id BIGINT,
            p_history_delta JSONB,
            p_worker_items JSONB,
            p_orchestrator_items JSONB,
            p_metadata JSONB
        )
        RETURNS VOID AS $ack_orch$
        DECLARE
            v_instance_id TEXT;
            v_now_ts TIMESTAMPTZ;
            v_orchestration_name TEXT;
            v_orchestration_version TEXT;
            v_status TEXT;
            v_output TEXT;
            v_completed_at TIMESTAMPTZ;
            v_elem JSONB;
            v_visible_at TIMESTAMPTZ;
            v_fire_at_ms BIGINT;
            v_item_instance_id TEXT;
        BEGIN
            v_now_ts := TO_TIMESTAMP(p_now_ms / 1000.0);

            -- Step 1: Validate lock token
            SELECT il.instance_id INTO v_instance_id
            FROM %I.instance_locks il
            WHERE il.lock_token = p_lock_token AND il.locked_until > p_now_ms;

            IF NOT FOUND THEN
                RAISE EXCEPTION ''Invalid lock token'';
            END IF;

            -- Step 2: Extract metadata from JSONB
            v_orchestration_name := p_metadata->>''orchestration_name'';
            v_orchestration_version := p_metadata->>''orchestration_version'';
            v_status := p_metadata->>''status'';
            v_output := p_metadata->>''output'';

            -- Step 3: Create or update instance metadata
            IF v_orchestration_name IS NOT NULL AND v_orchestration_version IS NOT NULL THEN
                INSERT INTO %I.instances (instance_id, orchestration_name, orchestration_version, current_execution_id, created_at, updated_at)
                VALUES (v_instance_id, v_orchestration_name, v_orchestration_version, p_execution_id, v_now_ts, v_now_ts)
                ON CONFLICT (instance_id) DO NOTHING;

                UPDATE %I.instances i
                SET orchestration_name = v_orchestration_name,
                    orchestration_version = v_orchestration_version,
                    updated_at = v_now_ts
                WHERE i.instance_id = v_instance_id;
            END IF;

            -- Step 4: Create execution record (idempotent)
            INSERT INTO %I.executions (instance_id, execution_id, status, started_at)
            VALUES (v_instance_id, p_execution_id, ''Running'', v_now_ts)
            ON CONFLICT (instance_id, execution_id) DO NOTHING;

            -- Step 5: Update instance current_execution_id
            UPDATE %I.instances i
            SET current_execution_id = GREATEST(i.current_execution_id, p_execution_id),
                updated_at = v_now_ts
            WHERE i.instance_id = v_instance_id;

            -- Step 6: Append history_delta (batch insert)
            IF p_history_delta IS NOT NULL AND JSONB_ARRAY_LENGTH(p_history_delta) > 0 THEN
                INSERT INTO %I.history (instance_id, execution_id, event_id, event_type, event_data, created_at)
                SELECT
                    v_instance_id,
                    p_execution_id,
                    (elem->>''event_id'')::BIGINT,
                    elem->>''event_type'',
                    elem->>''event_data'',
                    v_now_ts
                FROM JSONB_ARRAY_ELEMENTS(p_history_delta) AS elem;
            END IF;

            -- Step 7: Update execution status if provided
            IF v_status IS NOT NULL THEN
                v_completed_at := CASE 
                    WHEN v_status IN (''Completed'', ''Failed'') THEN v_now_ts 
                    ELSE NULL 
                END;
                
                UPDATE %I.executions e
                SET status = v_status, output = v_output, completed_at = v_completed_at
                WHERE e.instance_id = v_instance_id AND e.execution_id = p_execution_id;
            END IF;

            -- Step 8: Enqueue worker items (batch)
            IF p_worker_items IS NOT NULL AND JSONB_ARRAY_LENGTH(p_worker_items) > 0 THEN
                INSERT INTO %I.worker_queue (work_item, created_at)
                SELECT elem::TEXT, v_now_ts
                FROM JSONB_ARRAY_ELEMENTS(p_worker_items) AS elem;
            END IF;

            -- Step 9: Enqueue orchestrator items (batch with instance extraction and visible_at handling)
            IF p_orchestrator_items IS NOT NULL AND JSONB_ARRAY_LENGTH(p_orchestrator_items) > 0 THEN
                FOR v_elem IN SELECT value FROM JSONB_ARRAY_ELEMENTS(p_orchestrator_items) LOOP
                    -- Extract instance_id from work item based on type
                    IF v_elem ? ''StartOrchestration'' THEN
                        v_item_instance_id := v_elem->''StartOrchestration''->>''instance'';
                    ELSIF v_elem ? ''ContinueAsNew'' THEN
                        v_item_instance_id := v_elem->''ContinueAsNew''->>''instance'';
                    ELSIF v_elem ? ''TimerFired'' THEN
                        v_item_instance_id := v_elem->''TimerFired''->>''instance'';
                        v_fire_at_ms := (v_elem->''TimerFired''->>''fire_at_ms'')::BIGINT;
                    ELSIF v_elem ? ''ActivityCompleted'' THEN
                        v_item_instance_id := v_elem->''ActivityCompleted''->>''instance'';
                    ELSIF v_elem ? ''ActivityFailed'' THEN
                        v_item_instance_id := v_elem->''ActivityFailed''->>''instance'';
                    ELSIF v_elem ? ''ExternalRaised'' THEN
                        v_item_instance_id := v_elem->''ExternalRaised''->>''instance'';
                    ELSIF v_elem ? ''CancelInstance'' THEN
                        v_item_instance_id := v_elem->''CancelInstance''->>''instance'';
                    ELSIF v_elem ? ''SubOrchCompleted'' THEN
                        v_item_instance_id := v_elem->''SubOrchCompleted''->>''parent_instance'';
                    ELSIF v_elem ? ''SubOrchFailed'' THEN
                        v_item_instance_id := v_elem->''SubOrchFailed''->>''parent_instance'';
                    ELSE
                        v_item_instance_id := v_instance_id; -- Fallback
                    END IF;

                    -- Handle TimerFired special case for visible_at
                    IF v_elem ? ''TimerFired'' AND v_fire_at_ms IS NOT NULL AND v_fire_at_ms > 0 THEN
                        v_visible_at := TO_TIMESTAMP(v_fire_at_ms / 1000.0);
                    ELSE
                        v_visible_at := v_now_ts;
                    END IF;

                    INSERT INTO %I.orchestrator_queue (instance_id, work_item, visible_at, created_at)
                    VALUES (v_item_instance_id, v_elem::TEXT, v_visible_at, v_now_ts);
                    
                    v_fire_at_ms := NULL; -- Reset for next iteration
                END LOOP;
            END IF;

            -- Step 10: Delete locked messages
            DELETE FROM %I.orchestrator_queue q WHERE q.lock_token = p_lock_token;

            -- Step 11: Remove instance lock
            DELETE FROM %I.instance_locks il
            WHERE il.instance_id = v_instance_id AND il.lock_token = p_lock_token;
        END;
        $ack_orch$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name,
       v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name,
       v_schema_name, v_schema_name);

    -- ============================================================================
    -- Update abandon_orchestration_item to accept timestamp
    -- ============================================================================
    EXECUTE format('DROP FUNCTION IF EXISTS %I.abandon_orchestration_item(TEXT, BIGINT, BOOLEAN)', v_schema_name);

    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.abandon_orchestration_item(
            p_lock_token TEXT,
            p_now_ms BIGINT,
            p_delay_ms BIGINT DEFAULT NULL,
            p_ignore_attempt BOOLEAN DEFAULT FALSE
        )
        RETURNS TEXT AS $abandon_orch$
        DECLARE
            v_instance_id TEXT;
            v_visible_at TIMESTAMPTZ;
        BEGIN
            SELECT il.instance_id INTO v_instance_id
            FROM %I.instance_locks il
            WHERE il.lock_token = p_lock_token;

            IF NOT FOUND THEN
                RAISE EXCEPTION ''Invalid lock token'';
            END IF;

            IF p_delay_ms IS NOT NULL AND p_delay_ms > 0 THEN
                -- Delay provided: set visible_at to future time (now + delay)
                v_visible_at := TO_TIMESTAMP((p_now_ms + p_delay_ms) / 1000.0);
                
                IF p_ignore_attempt THEN
                    UPDATE %I.orchestrator_queue
                    SET lock_token = NULL,
                        locked_until = NULL,
                        visible_at = v_visible_at,
                        attempt_count = GREATEST(0, attempt_count - 1)
                    WHERE lock_token = p_lock_token;
                ELSE
                    UPDATE %I.orchestrator_queue
                    SET lock_token = NULL,
                        locked_until = NULL,
                        visible_at = v_visible_at
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
    -- Update abandon_work_item to accept timestamp
    -- ============================================================================
    EXECUTE format('DROP FUNCTION IF EXISTS %I.abandon_work_item(TEXT, BIGINT, BOOLEAN)', v_schema_name);

    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.abandon_work_item(
            p_lock_token TEXT,
            p_now_ms BIGINT,
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
                v_locked_until := p_now_ms + p_delay_ms;
                
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

END $$;
