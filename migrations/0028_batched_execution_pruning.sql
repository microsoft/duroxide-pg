-- Copyright (c) Microsoft Corporation.
-- Licensed under the MIT License.

-- Migration 0028: Bounded cursor-based execution pruning.
-- The page bounds scanned execution keys, including protected and young rows.
-- Callers advance using the returned key even when no execution was deleted.
DO $$
DECLARE
    v_schema_name TEXT := current_schema();
BEGIN
    EXECUTE format($fmt$
        CREATE OR REPLACE FUNCTION %1$I.prune_executions_batch(
            p_after_instance_id TEXT,
            p_after_execution_id BIGINT,
            p_completed_before_ms BIGINT,
            p_limit INTEGER
        )
        RETURNS TABLE(
            last_instance_id TEXT,
            last_execution_id BIGINT,
            executions_scanned BIGINT,
            executions_deleted BIGINT,
            events_deleted BIGINT
        )
        LANGUAGE plpgsql
        SET search_path = pg_catalog, pg_temp
        AS $prune_batch$
        DECLARE
            v_execution RECORD;
            v_current_execution_id BIGINT;
            v_count BIGINT;
        BEGIN
            IF p_after_instance_id IS NULL OR p_after_execution_id IS NULL
               OR p_after_execution_id < 0 OR p_completed_before_ms IS NULL
               OR p_completed_before_ms < 0 OR p_limit IS NULL
               OR p_limit < 1 OR p_limit > 1000 THEN
                RAISE EXCEPTION 'Invalid execution pruning cursor, cutoff, or limit (expected 1..1000)'
                    USING ERRCODE = '22023';
            END IF;

            executions_scanned := 0;
            executions_deleted := 0;
            events_deleted := 0;
            FOR v_execution IN
                SELECT e.instance_id, e.execution_id
                FROM %1$I.executions e
                WHERE (e.instance_id, e.execution_id) >
                      (p_after_instance_id, p_after_execution_id)
                ORDER BY e.instance_id, e.execution_id
                LIMIT p_limit
            LOOP
                last_instance_id := v_execution.instance_id;
                last_execution_id := v_execution.execution_id;
                executions_scanned := executions_scanned + 1;
                -- Lock before touching history, as deletion does. The page may
                -- describe an earlier lifetime of the same instance/execution key.
                SELECT i.current_execution_id INTO v_current_execution_id
                FROM %1$I.instances i
                WHERE i.instance_id = v_execution.instance_id
                FOR NO KEY UPDATE;

                IF NOT FOUND OR v_execution.execution_id >= v_current_execution_id THEN
                    CONTINUE;
                END IF;

                -- Eligibility may have changed while waiting for the instance lock.
                PERFORM 1 FROM %1$I.executions e
                WHERE e.instance_id = v_execution.instance_id
                  AND e.execution_id = v_execution.execution_id
                  AND e.status <> 'Running'
                  AND e.completed_at < to_timestamp(p_completed_before_ms / 1000.0);
                IF NOT FOUND THEN
                    CONTINUE;
                END IF;

                -- Exact-key deletes avoid plans that scan the entire history
                -- table when joining a batch to its many history rows.
                DELETE FROM %1$I.history h
                WHERE h.instance_id = v_execution.instance_id
                  AND h.execution_id = v_execution.execution_id;
                GET DIAGNOSTICS v_count = ROW_COUNT;
                events_deleted := events_deleted + v_count;

                DELETE FROM %1$I.executions e
                WHERE e.instance_id = v_execution.instance_id
                  AND e.execution_id = v_execution.execution_id;
                GET DIAGNOSTICS v_count = ROW_COUNT;
                executions_deleted := executions_deleted + v_count;
            END LOOP;
            RETURN NEXT;
        END;
        $prune_batch$;
$fmt$, v_schema_name);

    -- Both procedures retain instance locks until commit, so they must acquire
    -- them in the same order. The deletion body is otherwise unchanged from 0020.
    EXECUTE format($fmt$
        CREATE OR REPLACE FUNCTION %1$I.delete_instances_atomic(
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
                FROM %1$I.instances i
                JOIN %1$I.executions e ON i.instance_id = e.instance_id
                  AND i.current_execution_id = e.execution_id
                WHERE i.instance_id = ANY(p_instance_ids)
                  AND e.status = 'Running'
                LIMIT 1;

                IF v_instance_id IS NOT NULL THEN
                    RAISE EXCEPTION 'Instance %% is Running. Use force=true to delete.', v_instance_id;
                END IF;
            END IF;

            -- Step 2: Lock parent rows to prevent concurrent child creation
            PERFORM 1 FROM %1$I.instances
            WHERE instance_id = ANY(p_instance_ids)
            ORDER BY instance_id
            FOR UPDATE;

            -- Step 3: Check for orphans (children not in our delete list)
            SELECT i.instance_id INTO v_orphan_id
            FROM %1$I.instances i
            WHERE i.parent_instance_id = ANY(p_instance_ids)
              AND NOT (i.instance_id = ANY(p_instance_ids))
            LIMIT 1;

            IF v_orphan_id IS NOT NULL THEN
                RAISE EXCEPTION 'Orphan detected: instance %% has parent in delete list but is not included', v_orphan_id;
            END IF;

            -- Step 4: Delete from all tables
            DELETE FROM %1$I.history WHERE instance_id = ANY(p_instance_ids);
            GET DIAGNOSTICS v_count = ROW_COUNT;
            v_events_deleted := v_count;

            DELETE FROM %1$I.executions WHERE instance_id = ANY(p_instance_ids);
            GET DIAGNOSTICS v_count = ROW_COUNT;
            v_executions_deleted := v_count;

            DELETE FROM %1$I.orchestrator_queue WHERE instance_id = ANY(p_instance_ids);
            GET DIAGNOSTICS v_count = ROW_COUNT;
            v_queue_deleted := v_count;

            DELETE FROM %1$I.worker_queue WHERE instance_id = ANY(p_instance_ids);
            GET DIAGNOSTICS v_count = ROW_COUNT;
            v_queue_deleted := v_queue_deleted + v_count;

            DELETE FROM %1$I.instance_locks WHERE instance_id = ANY(p_instance_ids);
            DELETE FROM %1$I.kv_delta WHERE instance_id = ANY(p_instance_ids);
            DELETE FROM %1$I.kv_store WHERE instance_id = ANY(p_instance_ids);

            DELETE FROM %1$I.instances WHERE instance_id = ANY(p_instance_ids);
            GET DIAGNOSTICS v_count = ROW_COUNT;
            v_instances_deleted := v_count;

            instances_deleted := v_instances_deleted;
            executions_deleted := v_executions_deleted;
            events_deleted := v_events_deleted;
            queue_messages_deleted := v_queue_deleted;
            RETURN NEXT;
        END;
        $delete_atomic$ LANGUAGE plpgsql;
$fmt$, v_schema_name);

    -- Whole-instance pruning must join the same instance-before-history lock
    -- protocol. Its retention rules and deletion body are unchanged from 0019.
    EXECUTE format($fmt$
        CREATE OR REPLACE FUNCTION %1$I.prune_executions(
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
            v_exec_id BIGINT;
        BEGIN
            -- Get current execution ID (NEVER delete this)
            SELECT i.current_execution_id INTO v_current_execution_id
            FROM %1$I.instances i
            WHERE i.instance_id = p_instance_id
            FOR NO KEY UPDATE;

            IF NOT FOUND THEN
                -- Instance doesn't exist - raise error
                RAISE EXCEPTION 'Instance %% not found', p_instance_id;
            END IF;

            -- Build list of executions to delete
            -- CRITICAL: keep_last semantics - select top N executions INCLUDING current
            -- None, Some(0), Some(1) are all equivalent (only current remains)
            SELECT array_agg(e.execution_id) INTO v_exec_ids_to_delete
            FROM %1$I.executions e
            WHERE e.instance_id = p_instance_id
              AND e.execution_id != v_current_execution_id  -- Never delete current
              AND e.status != 'Running'                   -- Never delete running
              -- Apply completed_before filter if provided
              AND (p_completed_before_ms IS NULL
                   OR e.completed_at < TO_TIMESTAMP(p_completed_before_ms / 1000.0))
              -- Apply keep_last filter: exclude top N by execution_id (including current)
              AND (p_keep_last IS NULL
                   OR e.execution_id NOT IN (
                       SELECT e2.execution_id
                       FROM %1$I.executions e2
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

            FOREACH v_exec_id IN ARRAY v_exec_ids_to_delete LOOP
                -- Delete history for this execution
                DELETE FROM %1$I.history h
                WHERE h.instance_id = p_instance_id
                  AND h.execution_id = v_exec_id;
                GET DIAGNOSTICS v_count = ROW_COUNT;
                v_events_deleted := v_events_deleted + v_count;

            END LOOP;

            -- Delete executions
            DELETE FROM %1$I.executions e
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
$fmt$, v_schema_name);

    EXECUTE format($fmt$
        CREATE OR REPLACE FUNCTION %1$I.cleanup_schema()
        RETURNS VOID AS $cleanup$
        BEGIN
            DROP TABLE IF EXISTS %1$I.sessions CASCADE;
            DROP TABLE IF EXISTS %1$I.kv_delta CASCADE;
            DROP TABLE IF EXISTS %1$I.kv_store CASCADE;
            DROP TABLE IF EXISTS %1$I.instances CASCADE;
            DROP TABLE IF EXISTS %1$I.executions CASCADE;
            DROP TABLE IF EXISTS %1$I.history CASCADE;
            DROP TABLE IF EXISTS %1$I.orchestrator_queue CASCADE;
            DROP TABLE IF EXISTS %1$I.worker_queue CASCADE;
            DROP TABLE IF EXISTS %1$I.instance_locks CASCADE;
            DROP TABLE IF EXISTS %1$I._duroxide_migrations CASCADE;

            DROP FUNCTION IF EXISTS %1$I.cleanup_schema();
            DROP FUNCTION IF EXISTS %1$I.list_instances();
            DROP FUNCTION IF EXISTS %1$I.list_executions(TEXT);
            DROP FUNCTION IF EXISTS %1$I.latest_execution_id(TEXT);
            DROP FUNCTION IF EXISTS %1$I.list_instances_by_status(TEXT);
            DROP FUNCTION IF EXISTS %1$I.get_instance_info(TEXT);
            DROP FUNCTION IF EXISTS %1$I.get_execution_info(TEXT, BIGINT);
            DROP FUNCTION IF EXISTS %1$I.get_system_metrics();
            DROP FUNCTION IF EXISTS %1$I.get_queue_depths(BIGINT);
            DROP FUNCTION IF EXISTS %1$I.enqueue_worker_work(TEXT, BIGINT, TEXT, BIGINT, BIGINT, TEXT);
            DROP FUNCTION IF EXISTS %1$I.enqueue_worker_work(TEXT, BIGINT, TEXT, BIGINT, BIGINT, TEXT, TEXT);
            DROP FUNCTION IF EXISTS %1$I.ack_worker(TEXT, TEXT, TEXT, BIGINT);
            DROP FUNCTION IF EXISTS %1$I.renew_work_item_lock(TEXT, BIGINT, BIGINT);
            DROP FUNCTION IF EXISTS %1$I.fetch_work_item(BIGINT, BIGINT, TEXT, BIGINT);
            DROP FUNCTION IF EXISTS %1$I.fetch_work_item(BIGINT, BIGINT, TEXT, BIGINT, TEXT[], TEXT);
            DROP FUNCTION IF EXISTS %1$I.abandon_work_item(TEXT, BIGINT, BIGINT, BOOLEAN);
            DROP FUNCTION IF EXISTS %1$I.enqueue_orchestrator_work(TEXT, TEXT, TIMESTAMPTZ, TEXT, TEXT, BIGINT);
            DROP FUNCTION IF EXISTS %1$I.fetch_orchestration_item(BIGINT, BIGINT);
            DROP FUNCTION IF EXISTS %1$I.fetch_orchestration_item(BIGINT, BIGINT, BIGINT, BIGINT);
            DROP FUNCTION IF EXISTS %1$I.ack_orchestration_item(TEXT, BIGINT, BIGINT, JSONB, JSONB, JSONB, JSONB, JSONB);
            DROP FUNCTION IF EXISTS %1$I.abandon_orchestration_item(TEXT, BIGINT, BIGINT, BOOLEAN);
            DROP FUNCTION IF EXISTS %1$I.renew_orchestration_item_lock(TEXT, BIGINT, BIGINT);
            DROP FUNCTION IF EXISTS %1$I.fetch_history(TEXT);
            DROP FUNCTION IF EXISTS %1$I.fetch_history_with_execution(TEXT, BIGINT);
            DROP FUNCTION IF EXISTS %1$I.append_history(TEXT, BIGINT, JSONB);
            DROP FUNCTION IF EXISTS %1$I.list_children(TEXT);
            DROP FUNCTION IF EXISTS %1$I.get_parent_id(TEXT);
            DROP FUNCTION IF EXISTS %1$I.delete_instances_atomic(TEXT[], BOOLEAN);
            DROP FUNCTION IF EXISTS %1$I.prune_executions(TEXT, INTEGER, BIGINT);
            DROP FUNCTION IF EXISTS %1$I.prune_executions_batch(TEXT, BIGINT, BIGINT, INTEGER);
            DROP FUNCTION IF EXISTS %1$I.renew_session_lock(TEXT[], BIGINT, BIGINT, BIGINT);
            DROP FUNCTION IF EXISTS %1$I.cleanup_orphaned_sessions(BIGINT);
            DROP FUNCTION IF EXISTS %1$I.get_custom_status(TEXT, BIGINT);
            DROP FUNCTION IF EXISTS %1$I.get_kv_value(TEXT, TEXT);
            DROP FUNCTION IF EXISTS %1$I.get_kv_all_values(TEXT);
            DROP FUNCTION IF EXISTS %1$I.get_instance_stats(TEXT);
        END;
        $cleanup$ LANGUAGE plpgsql;
$fmt$, v_schema_name);
END $$;
