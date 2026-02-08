-- Migration: 0012_require_timestamps.sql
-- Description: Aligns timestamp handling with duroxide-pg-opt.
-- Stored procedure now accepts p_now_ms from Rust (app-server clock)
-- instead of using database NOW(), avoiding clock skew issues.
-- Also changes timestamp columns from DEFAULT CURRENT_TIMESTAMP to NOT NULL.

DO $$
DECLARE
    v_schema_name TEXT := current_schema();
BEGIN
    -- ============================================================================
    -- Part 1: Update ack_orchestration_item to accept p_now_ms from Rust
    -- Signature changes from 7 params to 8 (p_now_ms added as 2nd param)
    -- This must happen BEFORE we add NOT NULL constraints
    -- ============================================================================

    EXECUTE format('DROP FUNCTION IF EXISTS %I.ack_orchestration_item(TEXT, BIGINT, JSONB, JSONB, JSONB, JSONB, JSONB)', v_schema_name);

    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.ack_orchestration_item(
            p_lock_token TEXT,
            p_now_ms BIGINT,
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
            v_orchestration_name TEXT;
            v_orchestration_version TEXT;
            v_parent_instance_id TEXT;
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
            -- Convert Rust-supplied millisecond timestamp to TIMESTAMPTZ
            -- Using app-server clock avoids skew between app and DB servers
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
            v_parent_instance_id := p_metadata->>''parent_instance_id'';
            v_status := p_metadata->>''status'';
            v_output := p_metadata->>''output'';

            -- Step 3: Create or update instance metadata (with explicit timestamps)
            IF v_orchestration_name IS NOT NULL AND v_orchestration_version IS NOT NULL THEN
                INSERT INTO %I.instances (instance_id, orchestration_name, orchestration_version, current_execution_id, parent_instance_id, created_at, updated_at)
                VALUES (v_instance_id, v_orchestration_name, v_orchestration_version, p_execution_id, v_parent_instance_id, v_now_ts, v_now_ts)
                ON CONFLICT (instance_id) DO NOTHING;

                UPDATE %I.instances i
                SET orchestration_name = v_orchestration_name,
                    orchestration_version = v_orchestration_version,
                    parent_instance_id = COALESCE(i.parent_instance_id, v_parent_instance_id),
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

            -- Step 6: Append history_delta (batch insert with explicit timestamps)
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

            -- Step 8: Enqueue worker items FIRST - now with activity identification
            IF p_worker_items IS NOT NULL AND JSONB_ARRAY_LENGTH(p_worker_items) > 0 THEN
                FOR v_elem IN SELECT value FROM JSONB_ARRAY_ELEMENTS(p_worker_items) LOOP
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

            -- Step 9: Delete cancelled activities from worker_queue (lock stealing)
            IF p_cancelled_activities IS NOT NULL AND JSONB_ARRAY_LENGTH(p_cancelled_activities) > 0 THEN
                FOR v_elem IN SELECT value FROM JSONB_ARRAY_ELEMENTS(p_cancelled_activities) LOOP
                    DELETE FROM %I.worker_queue
                    WHERE instance_id = v_elem->>''instance''
                      AND execution_id = (v_elem->>''execution_id'')::BIGINT
                      AND activity_id = (v_elem->>''activity_id'')::BIGINT;
                END LOOP;
            END IF;

            -- Step 10: Enqueue orchestrator items
            IF p_orchestrator_items IS NOT NULL AND JSONB_ARRAY_LENGTH(p_orchestrator_items) > 0 THEN
                FOR v_elem IN SELECT value FROM JSONB_ARRAY_ELEMENTS(p_orchestrator_items) LOOP
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
                        v_item_instance_id := v_instance_id;
                    END IF;

                    IF v_elem ? ''TimerFired'' AND v_fire_at_ms IS NOT NULL AND v_fire_at_ms > 0 THEN
                        v_visible_at := TO_TIMESTAMP(v_fire_at_ms / 1000.0);
                    ELSE
                        v_visible_at := v_now_ts;
                    END IF;

                    INSERT INTO %I.orchestrator_queue (instance_id, work_item, visible_at, created_at)
                    VALUES (v_item_instance_id, v_elem::TEXT, v_visible_at, v_now_ts);
                    
                    v_fire_at_ms := NULL;
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
       v_schema_name, v_schema_name, v_schema_name, v_schema_name);

    -- ============================================================================
    -- Part 2: Schema changes - drop defaults and add NOT NULL constraints
    -- ============================================================================

    -- instances table
    EXECUTE format('ALTER TABLE %I.instances ALTER COLUMN created_at DROP DEFAULT', v_schema_name);
    EXECUTE format('ALTER TABLE %I.instances ALTER COLUMN updated_at DROP DEFAULT', v_schema_name);
    EXECUTE format('UPDATE %I.instances SET created_at = NOW() WHERE created_at IS NULL', v_schema_name);
    EXECUTE format('UPDATE %I.instances SET updated_at = NOW() WHERE updated_at IS NULL', v_schema_name);
    EXECUTE format('ALTER TABLE %I.instances ALTER COLUMN created_at SET NOT NULL', v_schema_name);
    EXECUTE format('ALTER TABLE %I.instances ALTER COLUMN updated_at SET NOT NULL', v_schema_name);

    -- executions table
    EXECUTE format('ALTER TABLE %I.executions ALTER COLUMN started_at DROP DEFAULT', v_schema_name);
    EXECUTE format('UPDATE %I.executions SET started_at = NOW() WHERE started_at IS NULL', v_schema_name);
    EXECUTE format('ALTER TABLE %I.executions ALTER COLUMN started_at SET NOT NULL', v_schema_name);

    -- history table
    EXECUTE format('ALTER TABLE %I.history ALTER COLUMN created_at DROP DEFAULT', v_schema_name);
    EXECUTE format('UPDATE %I.history SET created_at = NOW() WHERE created_at IS NULL', v_schema_name);
    EXECUTE format('ALTER TABLE %I.history ALTER COLUMN created_at SET NOT NULL', v_schema_name);

    -- orchestrator_queue table
    EXECUTE format('ALTER TABLE %I.orchestrator_queue ALTER COLUMN visible_at DROP DEFAULT', v_schema_name);
    EXECUTE format('ALTER TABLE %I.orchestrator_queue ALTER COLUMN created_at DROP DEFAULT', v_schema_name);
    EXECUTE format('UPDATE %I.orchestrator_queue SET visible_at = NOW() WHERE visible_at IS NULL', v_schema_name);
    EXECUTE format('UPDATE %I.orchestrator_queue SET created_at = NOW() WHERE created_at IS NULL', v_schema_name);
    EXECUTE format('ALTER TABLE %I.orchestrator_queue ALTER COLUMN visible_at SET NOT NULL', v_schema_name);
    EXECUTE format('ALTER TABLE %I.orchestrator_queue ALTER COLUMN created_at SET NOT NULL', v_schema_name);

    -- worker_queue table
    EXECUTE format('ALTER TABLE %I.worker_queue ALTER COLUMN visible_at DROP DEFAULT', v_schema_name);
    EXECUTE format('ALTER TABLE %I.worker_queue ALTER COLUMN created_at DROP DEFAULT', v_schema_name);
    EXECUTE format('UPDATE %I.worker_queue SET visible_at = NOW() WHERE visible_at IS NULL', v_schema_name);
    EXECUTE format('UPDATE %I.worker_queue SET created_at = NOW() WHERE created_at IS NULL', v_schema_name);
    EXECUTE format('ALTER TABLE %I.worker_queue ALTER COLUMN visible_at SET NOT NULL', v_schema_name);
    EXECUTE format('ALTER TABLE %I.worker_queue ALTER COLUMN created_at SET NOT NULL', v_schema_name);

    RAISE NOTICE 'Migration 0012: Timestamp columns now require explicit values (aligned with duroxide-pg-opt)';
END $$;
