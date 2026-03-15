-- Migration 0020: Add KV delta table
-- Description: Captures current-execution KV mutations in kv_delta, merges them into
-- kv_store only at execution completion boundaries, and cleans up delta rows on deletion.

DO $$
DECLARE
    v_schema_name TEXT := current_schema();
BEGIN
    -- ============================================================================
    -- Part 1: Create kv_delta table
    -- ============================================================================

    EXECUTE format($fmt$
        CREATE TABLE IF NOT EXISTS %1$I.kv_delta (
            instance_id TEXT NOT NULL,
            key TEXT NOT NULL,
            value TEXT,
            last_updated_at_ms BIGINT NOT NULL,
            PRIMARY KEY (instance_id, key)
        );
$fmt$, v_schema_name);

    -- ============================================================================
    -- Part 2: Update ack_orchestration_item for two-table KV semantics
    -- ============================================================================

    EXECUTE format($fmt$DROP FUNCTION IF EXISTS %1$I.ack_orchestration_item(TEXT, BIGINT, BIGINT, JSONB, JSONB, JSONB, JSONB, JSONB);$fmt$, v_schema_name);
    EXECUTE format($fmt$DROP FUNCTION IF EXISTS %1$I.ack_orchestration_item(TEXT, BIGINT, JSONB, JSONB, JSONB, JSONB, JSONB);$fmt$, v_schema_name);
    EXECUTE format($fmt$
        CREATE OR REPLACE FUNCTION %1$I.ack_orchestration_item(
            p_lock_token TEXT,
            p_now_ms BIGINT,
            p_execution_id BIGINT,
            p_history_delta JSONB,
            p_worker_items JSONB,
            p_orchestrator_items JSONB,
            p_metadata JSONB,
            p_cancelled_activities JSONB DEFAULT '[]'::JSONB
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
            v_item_session_id TEXT;
            v_item_tag TEXT;
            v_now_ts TIMESTAMPTZ;
            v_custom_status_action TEXT;
            v_custom_status_value TEXT;
            v_current_execution_id BIGINT;
            v_kv_mutations JSONB;
            v_kv_item JSONB;
            v_kv_action TEXT;
            v_i INTEGER;
            v_is_terminal BOOLEAN;
        BEGIN
            -- Convert Rust-supplied millisecond timestamp to TIMESTAMPTZ
            v_now_ts := TO_TIMESTAMP(p_now_ms / 1000.0);

            -- Step 1: Validate lock token
            SELECT il.instance_id INTO v_instance_id
            FROM %1$I.instance_locks il
            WHERE il.lock_token = p_lock_token AND il.locked_until > p_now_ms;

            IF NOT FOUND THEN
                RAISE EXCEPTION 'Invalid lock token';
            END IF;

            -- Step 2: Extract metadata from JSONB
            v_orchestration_name := p_metadata->>'orchestration_name';
            v_orchestration_version := p_metadata->>'orchestration_version';
            v_parent_instance_id := p_metadata->>'parent_instance_id';
            v_status := p_metadata->>'status';
            v_output := p_metadata->>'output';
            v_current_execution_id := p_execution_id;
            v_is_terminal := v_status IN ('Completed', 'ContinuedAsNew', 'Failed');

            -- Step 3: Create or update instance metadata (with explicit timestamps)
            IF v_orchestration_name IS NOT NULL AND v_orchestration_version IS NOT NULL THEN
                INSERT INTO %1$I.instances (instance_id, orchestration_name, orchestration_version, current_execution_id, parent_instance_id, created_at, updated_at)
                VALUES (v_instance_id, v_orchestration_name, v_orchestration_version, p_execution_id, v_parent_instance_id, v_now_ts, v_now_ts)
                ON CONFLICT (instance_id) DO NOTHING;

                UPDATE %1$I.instances i
                SET orchestration_name = v_orchestration_name,
                    orchestration_version = v_orchestration_version,
                    parent_instance_id = COALESCE(i.parent_instance_id, v_parent_instance_id),
                    updated_at = v_now_ts
                WHERE i.instance_id = v_instance_id;
            END IF;

            -- Step 4: Create execution record (idempotent)
            INSERT INTO %1$I.executions (instance_id, execution_id, status, started_at)
            VALUES (v_instance_id, p_execution_id, 'Running', v_now_ts)
            ON CONFLICT (instance_id, execution_id) DO NOTHING;

            -- Step 5: Update instance current_execution_id
            UPDATE %1$I.instances i
            SET current_execution_id = GREATEST(i.current_execution_id, p_execution_id),
                updated_at = v_now_ts
            WHERE i.instance_id = v_instance_id;

            -- Step 6: Append history_delta (batch insert with explicit timestamps)
            IF p_history_delta IS NOT NULL AND JSONB_ARRAY_LENGTH(p_history_delta) > 0 THEN
                INSERT INTO %1$I.history (instance_id, execution_id, event_id, event_type, event_data, created_at)
                SELECT
                    v_instance_id,
                    p_execution_id,
                    (elem->>'event_id')::BIGINT,
                    elem->>'event_type',
                    elem->>'event_data',
                    v_now_ts
                FROM JSONB_ARRAY_ELEMENTS(p_history_delta) AS elem;
            END IF;

            -- Step 7: Update execution status if provided
            IF v_status IS NOT NULL THEN
                v_completed_at := CASE
                    WHEN v_status IN ('Completed', 'Failed') THEN v_now_ts
                    ELSE NULL
                END;

                UPDATE %1$I.executions e
                SET status = v_status, output = v_output, completed_at = v_completed_at
                WHERE e.instance_id = v_instance_id AND e.execution_id = p_execution_id;
            END IF;

            -- Step 7b: Store pinned duroxide version if provided in metadata
            IF p_metadata ? 'pinned_duroxide_version' AND p_metadata->'pinned_duroxide_version' IS NOT NULL
               AND p_metadata->>'pinned_duroxide_version' != 'null' THEN
                UPDATE %1$I.executions
                SET duroxide_version_major = (p_metadata->'pinned_duroxide_version'->>'major')::INTEGER,
                    duroxide_version_minor = (p_metadata->'pinned_duroxide_version'->>'minor')::INTEGER,
                    duroxide_version_patch = (p_metadata->'pinned_duroxide_version'->>'patch')::INTEGER
                WHERE instance_id = v_instance_id AND execution_id = p_execution_id;
            END IF;

            -- Step 7c: Handle custom_status update on instances table
            v_custom_status_action := p_metadata->>'custom_status_action';
            IF v_custom_status_action = 'set' THEN
                v_custom_status_value := p_metadata->>'custom_status_value';
                UPDATE %1$I.instances
                SET custom_status = v_custom_status_value,
                    custom_status_version = custom_status_version + 1
                WHERE instance_id = v_instance_id;
            ELSIF v_custom_status_action = 'clear' THEN
                UPDATE %1$I.instances
                SET custom_status = NULL,
                    custom_status_version = custom_status_version + 1
                WHERE instance_id = v_instance_id;
            END IF;

            -- Step 7d: Materialize KV mutations into kv_delta only
            v_kv_mutations := p_metadata->'kv_mutations';
            IF v_kv_mutations IS NOT NULL AND jsonb_array_length(v_kv_mutations) > 0 THEN
                FOR v_i IN 0..jsonb_array_length(v_kv_mutations) - 1 LOOP
                    v_kv_item := v_kv_mutations->v_i;
                    v_kv_action := v_kv_item->>'action';
                    IF v_kv_action = 'set' THEN
                        INSERT INTO %1$I.kv_delta (instance_id, key, value, last_updated_at_ms)
                        VALUES (v_instance_id, v_kv_item->>'key', v_kv_item->>'value', COALESCE((v_kv_item->>'last_updated_at_ms')::BIGINT, p_now_ms))
                        ON CONFLICT (instance_id, key)
                        DO UPDATE SET value = EXCLUDED.value, last_updated_at_ms = EXCLUDED.last_updated_at_ms;
                    ELSIF v_kv_action = 'clear_key' THEN
                        INSERT INTO %1$I.kv_delta (instance_id, key, value, last_updated_at_ms)
                        VALUES (v_instance_id, v_kv_item->>'key', NULL, p_now_ms)
                        ON CONFLICT (instance_id, key)
                        DO UPDATE SET value = NULL, last_updated_at_ms = EXCLUDED.last_updated_at_ms;
                    ELSIF v_kv_action = 'clear_all' THEN
                        UPDATE %1$I.kv_delta
                        SET value = NULL,
                            last_updated_at_ms = p_now_ms
                        WHERE instance_id = v_instance_id;

                        INSERT INTO %1$I.kv_delta (instance_id, key, value, last_updated_at_ms)
                        SELECT ks.instance_id, ks.key, NULL, p_now_ms
                        FROM %1$I.kv_store ks
                        WHERE ks.instance_id = v_instance_id
                        ON CONFLICT (instance_id, key) DO NOTHING;
                    END IF;
                END LOOP;
            END IF;

            -- Step 7e: Merge kv_delta into kv_store on terminal execution boundaries
            IF v_is_terminal THEN
                INSERT INTO %1$I.kv_store (instance_id, key, value, execution_id, last_updated_at_ms)
                SELECT kd.instance_id, kd.key, kd.value, p_execution_id, kd.last_updated_at_ms
                FROM %1$I.kv_delta kd
                WHERE kd.instance_id = v_instance_id AND kd.value IS NOT NULL
                ON CONFLICT (instance_id, key)
                DO UPDATE SET value = EXCLUDED.value,
                              execution_id = EXCLUDED.execution_id,
                              last_updated_at_ms = EXCLUDED.last_updated_at_ms;

                DELETE FROM %1$I.kv_store ks
                WHERE ks.instance_id = v_instance_id
                  AND ks.key IN (
                      SELECT kd.key
                      FROM %1$I.kv_delta kd
                      WHERE kd.instance_id = v_instance_id
                        AND kd.value IS NULL
                  );

                DELETE FROM %1$I.kv_delta kd
                WHERE kd.instance_id = v_instance_id;
            END IF;

            -- Step 8: Enqueue worker items with session_id and tag support
            IF p_worker_items IS NOT NULL AND JSONB_ARRAY_LENGTH(p_worker_items) > 0 THEN
                FOR v_elem IN SELECT value FROM JSONB_ARRAY_ELEMENTS(p_worker_items) LOOP
                    IF v_elem ? 'ActivityExecute' THEN
                        v_item_instance_id := v_elem->'ActivityExecute'->>'instance';
                        v_item_execution_id := (v_elem->'ActivityExecute'->>'execution_id')::BIGINT;
                        v_item_activity_id := (v_elem->'ActivityExecute'->>'id')::BIGINT;
                        v_item_session_id := v_elem->'ActivityExecute'->>'session_id';
                        v_item_tag := v_elem->'ActivityExecute'->>'tag';
                    ELSE
                        v_item_instance_id := NULL;
                        v_item_execution_id := NULL;
                        v_item_activity_id := NULL;
                        v_item_session_id := NULL;
                        v_item_tag := NULL;
                    END IF;

                    INSERT INTO %1$I.worker_queue (work_item, visible_at, created_at, instance_id, execution_id, activity_id, session_id, tag)
                    VALUES (v_elem::TEXT, v_now_ts, v_now_ts, v_item_instance_id, v_item_execution_id, v_item_activity_id, v_item_session_id, v_item_tag);
                END LOOP;
            END IF;

            -- Step 9: Delete cancelled activities from worker_queue (lock stealing)
            IF p_cancelled_activities IS NOT NULL AND JSONB_ARRAY_LENGTH(p_cancelled_activities) > 0 THEN
                FOR v_elem IN SELECT value FROM JSONB_ARRAY_ELEMENTS(p_cancelled_activities) LOOP
                    DELETE FROM %1$I.worker_queue
                    WHERE instance_id = v_elem->>'instance'
                      AND execution_id = (v_elem->>'execution_id')::BIGINT
                      AND activity_id = (v_elem->>'activity_id')::BIGINT;
                END LOOP;
            END IF;

            -- Step 10: Enqueue orchestrator items
            IF p_orchestrator_items IS NOT NULL AND JSONB_ARRAY_LENGTH(p_orchestrator_items) > 0 THEN
                FOR v_elem IN SELECT value FROM JSONB_ARRAY_ELEMENTS(p_orchestrator_items) LOOP
                    IF v_elem ? 'StartOrchestration' THEN
                        v_item_instance_id := v_elem->'StartOrchestration'->>'instance';
                    ELSIF v_elem ? 'ContinueAsNew' THEN
                        v_item_instance_id := v_elem->'ContinueAsNew'->>'instance';
                    ELSIF v_elem ? 'TimerFired' THEN
                        v_item_instance_id := v_elem->'TimerFired'->>'instance';
                        v_fire_at_ms := (v_elem->'TimerFired'->>'fire_at_ms')::BIGINT;
                    ELSIF v_elem ? 'ActivityCompleted' THEN
                        v_item_instance_id := v_elem->'ActivityCompleted'->>'instance';
                    ELSIF v_elem ? 'ActivityFailed' THEN
                        v_item_instance_id := v_elem->'ActivityFailed'->>'instance';
                    ELSIF v_elem ? 'ExternalRaised' THEN
                        v_item_instance_id := v_elem->'ExternalRaised'->>'instance';
                    ELSIF v_elem ? 'CancelInstance' THEN
                        v_item_instance_id := v_elem->'CancelInstance'->>'instance';
                    ELSIF v_elem ? 'SubOrchCompleted' THEN
                        v_item_instance_id := v_elem->'SubOrchCompleted'->>'parent_instance';
                    ELSIF v_elem ? 'SubOrchFailed' THEN
                        v_item_instance_id := v_elem->'SubOrchFailed'->>'parent_instance';
                    ELSIF v_elem ? 'QueueMessage' THEN
                        v_item_instance_id := v_elem->'QueueMessage'->>'instance';
                    ELSE
                        v_item_instance_id := v_instance_id;
                    END IF;

                    IF v_elem ? 'TimerFired' AND v_fire_at_ms IS NOT NULL AND v_fire_at_ms > 0 THEN
                        v_visible_at := TO_TIMESTAMP(v_fire_at_ms / 1000.0);
                    ELSE
                        v_visible_at := v_now_ts;
                    END IF;

                    INSERT INTO %1$I.orchestrator_queue (instance_id, work_item, visible_at, created_at)
                    VALUES (v_item_instance_id, v_elem::TEXT, v_visible_at, v_now_ts);

                    v_fire_at_ms := NULL;
                END LOOP;
            END IF;

            -- Step 11: Delete locked messages
            DELETE FROM %1$I.orchestrator_queue q WHERE q.lock_token = p_lock_token;

            -- Step 12: Remove instance lock
            DELETE FROM %1$I.instance_locks il
            WHERE il.instance_id = v_instance_id AND il.lock_token = p_lock_token;
        END;
        $ack_orch$ LANGUAGE plpgsql;
$fmt$, v_schema_name);

    -- ============================================================================
    -- Part 3: Update delete_instances_atomic for KV delta cleanup
    -- ============================================================================

    EXECUTE format($fmt$DROP FUNCTION IF EXISTS %1$I.delete_instances_atomic(TEXT[], BOOLEAN);$fmt$, v_schema_name);
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

    -- ============================================================================
    -- Part 4: Update cleanup_schema to drop kv_delta in public-schema cleanup
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
        END;
        $cleanup$ LANGUAGE plpgsql;
$fmt$, v_schema_name);

    -- ============================================================================
    -- Part 5: Add get_kv_value stored procedure for delta-first lookup
    -- ============================================================================

    EXECUTE format($fmt$DROP FUNCTION IF EXISTS %1$I.get_kv_value(TEXT, TEXT);$fmt$, v_schema_name);
    EXECUTE format($fmt$
        CREATE OR REPLACE FUNCTION %1$I.get_kv_value(
            p_instance TEXT,
            p_key TEXT
        )
        RETURNS TABLE(out_value TEXT, out_found BOOLEAN) AS $get_kv_value$
        DECLARE
            v_delta_value TEXT;
            v_store_value TEXT;
        BEGIN
            SELECT value
            INTO v_delta_value
            FROM %1$I.kv_delta
            WHERE instance_id = p_instance AND key = p_key;

            IF FOUND THEN
                out_value := v_delta_value;
                out_found := v_delta_value IS NOT NULL;
                RETURN NEXT;
                RETURN;
            END IF;

            SELECT value
            INTO v_store_value
            FROM %1$I.kv_store
            WHERE instance_id = p_instance AND key = p_key;

            IF FOUND THEN
                out_value := v_store_value;
                out_found := TRUE;
                RETURN NEXT;
                RETURN;
            END IF;

            out_value := NULL;
            out_found := FALSE;
            RETURN NEXT;
        END;
        $get_kv_value$ LANGUAGE plpgsql;
$fmt$, v_schema_name);

    -- ============================================================================
    -- Part 6: Add get_kv_all_values stored procedure for merged KV reads
    -- ============================================================================

    EXECUTE format($fmt$DROP FUNCTION IF EXISTS %1$I.get_kv_all_values(TEXT);$fmt$, v_schema_name);
    EXECUTE format($fmt$
        CREATE OR REPLACE FUNCTION %1$I.get_kv_all_values(
            p_instance TEXT
        )
        RETURNS TABLE(out_key TEXT, out_value TEXT) AS $get_kv_all_values$
        BEGIN
            RETURN QUERY
            SELECT COALESCE(d.key, s.key) AS out_key,
                   CASE WHEN d.key IS NOT NULL THEN d.value ELSE s.value END AS out_value
            FROM %1$I.kv_store s
            FULL OUTER JOIN %1$I.kv_delta d
                ON s.instance_id = d.instance_id AND s.key = d.key
            WHERE (s.instance_id = p_instance OR d.instance_id = p_instance)
              AND CASE WHEN d.key IS NOT NULL THEN d.value IS NOT NULL ELSE TRUE END;
        END;
        $get_kv_all_values$ LANGUAGE plpgsql;
$fmt$, v_schema_name);

    RAISE NOTICE 'Migration 0020: Added kv_delta table and two-table KV semantics';
END $$;
