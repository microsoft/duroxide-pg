-- Migration 0021: Add get_instance_stats stored procedure
-- Description: Moves the inline get_instance_stats queries into a single stored procedure,
-- consolidating four round trips into one and correctly merging kv_delta + kv_store counts.

DO $$
DECLARE
    v_schema_name TEXT := current_schema();
BEGIN
    -- ============================================================================
    -- Part 1: Add get_instance_stats stored procedure
    -- ============================================================================

    EXECUTE format($fmt$
        CREATE OR REPLACE FUNCTION %1$I.get_instance_stats(
            p_instance TEXT
        )
        RETURNS TABLE(
            out_found BOOLEAN,
            out_history_event_count BIGINT,
            out_history_size_bytes BIGINT,
            out_queue_pending_count BIGINT,
            out_kv_user_key_count BIGINT,
            out_kv_total_value_bytes BIGINT
        ) AS $get_instance_stats$
        DECLARE
            v_execution_id BIGINT;
            v_event_data TEXT;
            v_event_json JSONB;
            v_cf_events JSONB;
        BEGIN
            -- Look up the current execution for this instance
            SELECT current_execution_id
            INTO v_execution_id
            FROM %1$I.instances
            WHERE instance_id = p_instance;

            IF NOT FOUND THEN
                out_found := FALSE;
                out_history_event_count := 0;
                out_history_size_bytes := 0;
                out_queue_pending_count := 0;
                out_kv_user_key_count := 0;
                out_kv_total_value_bytes := 0;
                RETURN NEXT;
                RETURN;
            END IF;

            out_found := TRUE;

            -- History stats for the current execution
            SELECT COUNT(*)::BIGINT,
                   COALESCE(SUM(OCTET_LENGTH(event_data)), 0)::BIGINT
            INTO out_history_event_count, out_history_size_bytes
            FROM %1$I.history
            WHERE instance_id = p_instance AND execution_id = v_execution_id;

            -- KV key count: logical (merged, excluding delta tombstones)
            SELECT COUNT(DISTINCT k.key)::BIGINT
            INTO out_kv_user_key_count
            FROM (
                SELECT key FROM %1$I.kv_store WHERE instance_id = p_instance
                UNION
                SELECT key FROM %1$I.kv_delta WHERE instance_id = p_instance AND value IS NOT NULL
            ) k;

            -- KV value bytes: physical (sum from both tables — both consume storage)
            SELECT (
                COALESCE((SELECT SUM(OCTET_LENGTH(value)) FROM %1$I.kv_store WHERE instance_id = p_instance), 0)
              + COALESCE((SELECT SUM(OCTET_LENGTH(value)) FROM %1$I.kv_delta WHERE instance_id = p_instance), 0)
            )::BIGINT
            INTO out_kv_total_value_bytes;

            -- Queue pending count: extract carry_forward_events array length from
            -- event_id=1 (OrchestrationStarted) in the current execution
            SELECT event_data
            INTO v_event_data
            FROM %1$I.history
            WHERE instance_id = p_instance
              AND execution_id = v_execution_id
              AND event_id = 1;

            out_queue_pending_count := 0;
            IF v_event_data IS NOT NULL THEN
                v_event_json := v_event_data::JSONB;
                v_cf_events := v_event_json -> 'carry_forward_events';
                IF v_cf_events IS NOT NULL AND jsonb_typeof(v_cf_events) = 'array' THEN
                    out_queue_pending_count := jsonb_array_length(v_cf_events)::BIGINT;
                END IF;
            END IF;

            RETURN NEXT;
        END;
        $get_instance_stats$ LANGUAGE plpgsql;
$fmt$, v_schema_name);

    -- ============================================================================
    -- Part 2: Update cleanup_schema to drop get_instance_stats
    -- ============================================================================

    EXECUTE format($fmt$DROP FUNCTION IF EXISTS %1$I.cleanup_schema();$fmt$, v_schema_name);
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
            DROP FUNCTION IF EXISTS %1$I.renew_session_lock(TEXT[], BIGINT, BIGINT, BIGINT);
            DROP FUNCTION IF EXISTS %1$I.cleanup_orphaned_sessions(BIGINT);
            DROP FUNCTION IF EXISTS %1$I.get_custom_status(TEXT, BIGINT);
            DROP FUNCTION IF EXISTS %1$I.get_kv_value(TEXT, TEXT);
            DROP FUNCTION IF EXISTS %1$I.get_kv_all_values(TEXT);
            DROP FUNCTION IF EXISTS %1$I.get_instance_stats(TEXT);
        END;
        $cleanup$ LANGUAGE plpgsql;
$fmt$, v_schema_name);

    RAISE NOTICE 'Migration 0021: Added get_instance_stats stored procedure';
END $$;
