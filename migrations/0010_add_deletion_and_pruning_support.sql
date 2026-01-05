-- Migration 0010: Add deletion and pruning support
-- This migration adds:
-- 1. parent_instance_id column to instances table (for cascade deletion)
-- 2. Stored procedures for new ProviderAdmin methods:
--    - list_children
--    - get_parent_id
--    - delete_instances_atomic
--    - delete_instance_bulk
--    - prune_executions
--    - prune_executions_bulk
-- 3. Updates get_instance_info to include parent_instance_id
-- 4. Updates ack_orchestration_item to store parent_instance_id from metadata

-- Add parent_instance_id column to instances table
ALTER TABLE instances ADD COLUMN IF NOT EXISTS parent_instance_id TEXT;

-- Add index for efficient child lookups
CREATE INDEX IF NOT EXISTS idx_instances_parent ON instances(parent_instance_id);

-- Get the current schema name (set by migration runner)
DO $$
DECLARE
    v_schema_name TEXT := current_schema();
BEGIN
    -- ============================================================================
    -- Update ack_orchestration_item to store parent_instance_id from metadata
    -- This is critical for hierarchy tracking (cascade deletion, list_children, etc)
    -- ============================================================================
    EXECUTE format('DROP FUNCTION IF EXISTS %I.ack_orchestration_item(TEXT, BIGINT, JSONB, JSONB, JSONB, JSONB, JSONB)', v_schema_name);

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

            -- Step 2: Extract metadata from JSONB (including parent_instance_id)
            v_orchestration_name := p_metadata->>''orchestration_name'';
            v_orchestration_version := p_metadata->>''orchestration_version'';
            v_parent_instance_id := p_metadata->>''parent_instance_id'';
            v_status := p_metadata->>''status'';
            v_output := p_metadata->>''output'';

            -- Step 3: Create or update instance metadata (including parent_instance_id)
            IF v_orchestration_name IS NOT NULL AND v_orchestration_version IS NOT NULL THEN
                INSERT INTO %I.instances (instance_id, orchestration_name, orchestration_version, current_execution_id, parent_instance_id)
                VALUES (v_instance_id, v_orchestration_name, v_orchestration_version, p_execution_id, v_parent_instance_id)
                ON CONFLICT (instance_id) DO NOTHING;

                -- Update existing instance (but only set parent_instance_id if it was NULL)
                UPDATE %I.instances i
                SET orchestration_name = v_orchestration_name,
                    orchestration_version = v_orchestration_version,
                    parent_instance_id = COALESCE(i.parent_instance_id, v_parent_instance_id)
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
       v_schema_name, v_schema_name, v_schema_name, v_schema_name);

    -- ============================================================================
    -- Update get_instance_info to include parent_instance_id
    -- ============================================================================
    EXECUTE format('DROP FUNCTION IF EXISTS %I.get_instance_info(TEXT)', v_schema_name);
    
    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.get_instance_info(p_instance_id TEXT)
        RETURNS TABLE(
            instance_id TEXT,
            orchestration_name TEXT,
            orchestration_version TEXT,
            current_execution_id BIGINT,
            created_at TIMESTAMPTZ,
            updated_at TIMESTAMPTZ,
            status TEXT,
            output TEXT,
            parent_instance_id TEXT
        ) AS $get_inst_info$
        BEGIN
            RETURN QUERY
            SELECT i.instance_id, i.orchestration_name, 
                   COALESCE(i.orchestration_version, ''unknown'') as orchestration_version,
                   i.current_execution_id, i.created_at, i.updated_at,
                   e.status, e.output, i.parent_instance_id
            FROM %I.instances i
            LEFT JOIN %I.executions e ON i.instance_id = e.instance_id 
              AND i.current_execution_id = e.execution_id
            WHERE i.instance_id = p_instance_id;
        END;
        $get_inst_info$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name, v_schema_name);

    -- ============================================================================
    -- Hierarchy Primitive Procedures
    -- ============================================================================

    -- Procedure: list_children
    -- Returns direct children of an instance (instances where parent_instance_id = given instance)
    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.list_children(p_instance_id TEXT)
        RETURNS TABLE(child_instance_id TEXT) AS $list_children$
        BEGIN
            RETURN QUERY
            SELECT i.instance_id
            FROM %I.instances i
            WHERE i.parent_instance_id = p_instance_id
            ORDER BY i.created_at;
        END;
        $list_children$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name);

    -- Procedure: get_parent_id
    -- Returns the parent_instance_id for a given instance, or NULL for root orchestrations
    -- Raises exception if instance doesn''t exist
    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.get_parent_id(p_instance_id TEXT)
        RETURNS TEXT AS $get_parent$
        DECLARE
            v_parent_id TEXT;
        BEGIN
            SELECT i.parent_instance_id
            INTO v_parent_id
            FROM %I.instances i
            WHERE i.instance_id = p_instance_id;
            
            IF NOT FOUND THEN
                RAISE EXCEPTION ''Instance not found: %%'', p_instance_id;
            END IF;
            
            RETURN v_parent_id;
        END;
        $get_parent$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name);

    -- ============================================================================
    -- Deletion Procedures
    -- ============================================================================

    -- Procedure: delete_instances_atomic
    -- Atomically deletes a batch of instances with orphan detection
    -- Returns counts of deleted rows
    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.delete_instances_atomic(
            p_instance_ids TEXT[],
            p_force BOOLEAN
        )
        RETURNS TABLE(
            instances_deleted BIGINT,
            executions_deleted BIGINT,
            events_deleted BIGINT,
            queue_messages_deleted BIGINT
        ) AS $delete_atomic$
        DECLARE
            v_instance_id TEXT;
            v_orphan_id TEXT;
            v_instances_deleted BIGINT := 0;
            v_executions_deleted BIGINT := 0;
            v_events_deleted BIGINT := 0;
            v_queue_deleted BIGINT := 0;
            v_count BIGINT;
        BEGIN
            -- Check for empty input
            IF p_instance_ids IS NULL OR array_length(p_instance_ids, 1) IS NULL THEN
                instances_deleted := 0;
                executions_deleted := 0;
                events_deleted := 0;
                queue_messages_deleted := 0;
                RETURN NEXT;
                RETURN;
            END IF;

            -- Step 1: If not force, check all instances are terminal (single query, no loop)
            IF NOT p_force THEN
                SELECT i.instance_id INTO v_instance_id
                FROM %I.instances i
                JOIN %I.executions e ON i.instance_id = e.instance_id 
                  AND i.current_execution_id = e.execution_id
                WHERE i.instance_id = ANY(p_instance_ids)
                  AND e.status = ''Running''
                LIMIT 1;
                
                IF v_instance_id IS NOT NULL THEN
                    RAISE EXCEPTION ''Instance %% is Running. Use force=true to delete.'', v_instance_id;
                END IF;
            END IF;

            -- Step 2: Lock parent rows to prevent concurrent child creation
            -- This prevents the race condition where a child could be inserted
            -- between our orphan check and the actual delete.
            PERFORM 1 FROM %I.instances
            WHERE instance_id = ANY(p_instance_ids)
            FOR UPDATE;

            -- Step 3: Check for orphans (children not in our delete list)
            -- Now safe because we hold locks on all parent rows
            SELECT i.instance_id INTO v_orphan_id
            FROM %I.instances i
            WHERE i.parent_instance_id = ANY(p_instance_ids)
              AND NOT (i.instance_id = ANY(p_instance_ids))
            LIMIT 1;
            
            IF v_orphan_id IS NOT NULL THEN
                RAISE EXCEPTION ''Orphan detected: instance %% has parent in delete list but is not included'', v_orphan_id;
            END IF;

            -- Step 4: Delete from all tables
            -- Delete history events
            DELETE FROM %I.history WHERE instance_id = ANY(p_instance_ids);
            GET DIAGNOSTICS v_count = ROW_COUNT;
            v_events_deleted := v_count;

            -- Delete executions
            DELETE FROM %I.executions WHERE instance_id = ANY(p_instance_ids);
            GET DIAGNOSTICS v_count = ROW_COUNT;
            v_executions_deleted := v_count;

            -- Delete orchestrator queue messages
            DELETE FROM %I.orchestrator_queue WHERE instance_id = ANY(p_instance_ids);
            GET DIAGNOSTICS v_count = ROW_COUNT;
            v_queue_deleted := v_count;

            -- Delete worker queue messages
            DELETE FROM %I.worker_queue WHERE instance_id = ANY(p_instance_ids);
            GET DIAGNOSTICS v_count = ROW_COUNT;
            v_queue_deleted := v_queue_deleted + v_count;

            -- Delete instance locks
            DELETE FROM %I.instance_locks WHERE instance_id = ANY(p_instance_ids);

            -- Delete instances
            DELETE FROM %I.instances WHERE instance_id = ANY(p_instance_ids);
            GET DIAGNOSTICS v_count = ROW_COUNT;
            v_instances_deleted := v_count;

            instances_deleted := v_instances_deleted;
            executions_deleted := v_executions_deleted;
            events_deleted := v_events_deleted;
            queue_messages_deleted := v_queue_deleted;
            RETURN NEXT;
        END;
        $delete_atomic$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, 
       v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name);

    -- ============================================================================
    -- Pruning Procedures
    -- ============================================================================

    -- Procedure: prune_executions
    -- Prunes old executions from a single instance
    -- Never deletes the current execution
    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.prune_executions(
            p_instance_id TEXT,
            p_keep_last INTEGER DEFAULT NULL,
            p_completed_before_ms BIGINT DEFAULT NULL
        )
        RETURNS TABLE(
            instances_processed BIGINT,
            executions_deleted BIGINT,
            events_deleted BIGINT
        ) AS $prune_exec$
        DECLARE
            v_current_execution_id BIGINT;
            v_executions_deleted BIGINT := 0;
            v_events_deleted BIGINT := 0;
            v_count BIGINT;
            v_exec_ids_to_delete BIGINT[];
        BEGIN
            -- Get current execution ID (NEVER delete this)
            SELECT i.current_execution_id INTO v_current_execution_id
            FROM %I.instances i
            WHERE i.instance_id = p_instance_id;
            
            IF NOT FOUND THEN
                -- Instance doesn''t exist - raise error
                RAISE EXCEPTION ''Instance %% not found'', p_instance_id;
            END IF;

            -- Build list of executions to delete
            -- CRITICAL: keep_last semantics - select top N executions INCLUDING current
            -- None, Some(0), Some(1) are all equivalent (only current remains)
            SELECT array_agg(e.execution_id) INTO v_exec_ids_to_delete
            FROM %I.executions e
            WHERE e.instance_id = p_instance_id
              AND e.execution_id != v_current_execution_id  -- Never delete current
              AND e.status != ''Running''                   -- Never delete running
              -- Apply completed_before filter if provided
              AND (p_completed_before_ms IS NULL 
                   OR e.completed_at < TO_TIMESTAMP(p_completed_before_ms / 1000.0))
              -- Apply keep_last filter: exclude top N by execution_id (including current)
              AND (p_keep_last IS NULL 
                   OR e.execution_id NOT IN (
                       SELECT e2.execution_id 
                       FROM %I.executions e2
                       WHERE e2.instance_id = p_instance_id
                       ORDER BY e2.execution_id DESC
                       LIMIT p_keep_last
                   ));

            IF v_exec_ids_to_delete IS NULL OR array_length(v_exec_ids_to_delete, 1) IS NULL THEN
                instances_processed := 1;
                executions_deleted := 0;
                events_deleted := 0;
                RETURN NEXT;
                RETURN;
            END IF;

            -- Delete history for pruned executions
            DELETE FROM %I.history h
            WHERE h.instance_id = p_instance_id
              AND h.execution_id = ANY(v_exec_ids_to_delete);
            GET DIAGNOSTICS v_count = ROW_COUNT;
            v_events_deleted := v_count;

            -- Delete executions
            DELETE FROM %I.executions e
            WHERE e.instance_id = p_instance_id
              AND e.execution_id = ANY(v_exec_ids_to_delete);
            GET DIAGNOSTICS v_count = ROW_COUNT;
            v_executions_deleted := v_count;

            instances_processed := 1;
            executions_deleted := v_executions_deleted;
            events_deleted := v_events_deleted;
            RETURN NEXT;
        END;
        $prune_exec$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name);

END $$;
