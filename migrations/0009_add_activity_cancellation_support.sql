-- Migration: 0009_add_activity_cancellation_support.sql
-- Description: Adds support for activity cancellation via lock stealing
-- Required for duroxide main branch which adds cancelled_activities to ack_orchestration_item
-- This enables the runtime to cancel in-flight activities when orchestration completes/fails

DO $$
DECLARE
    v_schema_name TEXT := current_schema();
BEGIN
    -- ============================================================================
    -- Step 1: Add columns to worker_queue for activity identification
    -- These columns allow us to find and delete cancelled activities efficiently
    -- ============================================================================
    
    -- Add instance_id column (nullable for backward compatibility during migration)
    IF NOT EXISTS (
        SELECT 1 FROM information_schema.columns 
        WHERE table_schema = v_schema_name 
        AND table_name = 'worker_queue' 
        AND column_name = 'instance_id'
    ) THEN
        EXECUTE format('ALTER TABLE %I.worker_queue ADD COLUMN instance_id TEXT', v_schema_name);
    END IF;

    -- Add execution_id column
    IF NOT EXISTS (
        SELECT 1 FROM information_schema.columns 
        WHERE table_schema = v_schema_name 
        AND table_name = 'worker_queue' 
        AND column_name = 'execution_id'
    ) THEN
        EXECUTE format('ALTER TABLE %I.worker_queue ADD COLUMN execution_id BIGINT', v_schema_name);
    END IF;

    -- Add activity_id column
    IF NOT EXISTS (
        SELECT 1 FROM information_schema.columns 
        WHERE table_schema = v_schema_name 
        AND table_name = 'worker_queue' 
        AND column_name = 'activity_id'
    ) THEN
        EXECUTE format('ALTER TABLE %I.worker_queue ADD COLUMN activity_id BIGINT', v_schema_name);
    END IF;

    -- ============================================================================
    -- Step 2: Create index for efficient activity lookup during cancellation
    -- ============================================================================
    EXECUTE format('
        CREATE INDEX IF NOT EXISTS idx_worker_activity_lookup 
        ON %I.worker_queue(instance_id, execution_id, activity_id)
    ', v_schema_name);

    -- ============================================================================
    -- Step 3: Update enqueue_worker_work to accept activity identification
    -- Now accepts instance_id, execution_id, activity_id for ActivityExecute items
    -- ============================================================================
    EXECUTE format('DROP FUNCTION IF EXISTS %I.enqueue_worker_work(TEXT)', v_schema_name);
    EXECUTE format('DROP FUNCTION IF EXISTS %I.enqueue_worker_work(TEXT, BIGINT)', v_schema_name);

    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.enqueue_worker_work(
            p_work_item TEXT,
            p_now_ms BIGINT,
            p_instance_id TEXT DEFAULT NULL,
            p_execution_id BIGINT DEFAULT NULL,
            p_activity_id BIGINT DEFAULT NULL
        )
        RETURNS VOID AS $enq_worker$
        DECLARE
            v_now_ts TIMESTAMPTZ;
        BEGIN
            v_now_ts := TO_TIMESTAMP(p_now_ms / 1000.0);
            INSERT INTO %I.worker_queue (work_item, visible_at, created_at, instance_id, execution_id, activity_id)
            VALUES (p_work_item, v_now_ts, v_now_ts, p_instance_id, p_execution_id, p_activity_id);
        END;
        $enq_worker$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name);

    -- ============================================================================
    -- Step 4: Update ack_orchestration_item to handle cancelled activities
    -- Adds p_cancelled_activities JSONB parameter for batch deletion
    -- ============================================================================
    EXECUTE format('DROP FUNCTION IF EXISTS %I.ack_orchestration_item(TEXT, BIGINT, JSONB, JSONB, JSONB, JSONB)', v_schema_name);

    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.ack_orchestration_item(
            p_lock_token TEXT,
            p_execution_id BIGINT,
            p_history_delta JSONB,
            p_worker_items JSONB,
            p_orchestrator_items JSONB,
            p_metadata JSONB,
            p_cancelled_activities JSONB DEFAULT ''[]''::JSONB
        )
        RETURNS VOID AS $ack_orch$
        DECLARE
            v_instance_id TEXT;
            v_now_ms BIGINT;
            v_orchestration_name TEXT;
            v_orchestration_version TEXT;
            v_status TEXT;
            v_output TEXT;
            v_completed_at TIMESTAMPTZ;
            v_elem JSONB;
            v_visible_at TIMESTAMPTZ;
            v_fire_at_ms BIGINT;
            v_item_instance_id TEXT;
            v_item_execution_id BIGINT;
            v_item_activity_id BIGINT;
            v_now_ts TIMESTAMPTZ;
        BEGIN
            -- NOTE: v_now_ms is computed from database time only for lock validation
            -- All timestamps stored in tables use v_now_ts derived from this
            v_now_ms := (EXTRACT(EPOCH FROM NOW()) * 1000)::BIGINT;
            v_now_ts := TO_TIMESTAMP(v_now_ms / 1000.0);

            -- Step 1: Validate lock token
            SELECT il.instance_id INTO v_instance_id
            FROM %I.instance_locks il
            WHERE il.lock_token = p_lock_token AND il.locked_until > v_now_ms;

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
                INSERT INTO %I.instances (instance_id, orchestration_name, orchestration_version, current_execution_id)
                VALUES (v_instance_id, v_orchestration_name, v_orchestration_version, p_execution_id)
                ON CONFLICT (instance_id) DO NOTHING;

                UPDATE %I.instances i
                SET orchestration_name = v_orchestration_name,
                    orchestration_version = v_orchestration_version
                WHERE i.instance_id = v_instance_id;
            END IF;

            -- Step 4: Create execution record (idempotent)
            INSERT INTO %I.executions (instance_id, execution_id, status, started_at)
            VALUES (v_instance_id, p_execution_id, ''Running'', v_now_ts)
            ON CONFLICT (instance_id, execution_id) DO NOTHING;

            -- Step 5: Update instance current_execution_id
            UPDATE %I.instances i
            SET current_execution_id = GREATEST(i.current_execution_id, p_execution_id)
            WHERE i.instance_id = v_instance_id;

            -- Step 6: Append history_delta (batch insert)
            IF p_history_delta IS NOT NULL AND JSONB_ARRAY_LENGTH(p_history_delta) > 0 THEN
                INSERT INTO %I.history (instance_id, execution_id, event_id, event_type, event_data)
                SELECT
                    v_instance_id,
                    p_execution_id,
                    (elem->>''event_id'')::BIGINT,
                    elem->>''event_type'',
                    elem->>''event_data''
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

            -- Step 8: Delete cancelled activities from worker_queue (lock stealing)
            -- This removes activities that were scheduled but not yet started/completed
            IF p_cancelled_activities IS NOT NULL AND JSONB_ARRAY_LENGTH(p_cancelled_activities) > 0 THEN
                FOR v_elem IN SELECT value FROM JSONB_ARRAY_ELEMENTS(p_cancelled_activities) LOOP
                    DELETE FROM %I.worker_queue
                    WHERE instance_id = v_elem->>''instance''
                      AND execution_id = (v_elem->>''execution_id'')::BIGINT
                      AND activity_id = (v_elem->>''activity_id'')::BIGINT;
                END LOOP;
            END IF;

            -- Step 9: Enqueue worker items (batch) - now with activity identification
            IF p_worker_items IS NOT NULL AND JSONB_ARRAY_LENGTH(p_worker_items) > 0 THEN
                FOR v_elem IN SELECT value FROM JSONB_ARRAY_ELEMENTS(p_worker_items) LOOP
                    -- Extract activity identification from ActivityExecute work item
                    IF v_elem ? ''ActivityExecute'' THEN
                        v_item_instance_id := v_elem->''ActivityExecute''->>''instance'';
                        v_item_execution_id := (v_elem->''ActivityExecute''->>''execution_id'')::BIGINT;
                        v_item_activity_id := (v_elem->''ActivityExecute''->>''id'')::BIGINT;
                    ELSE
                        v_item_instance_id := NULL;
                        v_item_execution_id := NULL;
                        v_item_activity_id := NULL;
                    END IF;

                    INSERT INTO %I.worker_queue (work_item, visible_at, created_at, instance_id, execution_id, activity_id)
                    VALUES (v_elem::TEXT, v_now_ts, v_now_ts, v_item_instance_id, v_item_execution_id, v_item_activity_id);
                END LOOP;
            END IF;

            -- Step 10: Enqueue orchestrator items (batch with instance extraction and visible_at handling)
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

            -- Step 11: Delete locked messages
            DELETE FROM %I.orchestrator_queue q WHERE q.lock_token = p_lock_token;

            -- Step 12: Remove instance lock
            DELETE FROM %I.instance_locks il
            WHERE il.instance_id = v_instance_id AND il.lock_token = p_lock_token;
        END;
        $ack_orch$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name,
       v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name,
       v_schema_name, v_schema_name, v_schema_name);

END $$;
