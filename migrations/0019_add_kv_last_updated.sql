-- Migration 0019: Add KV last-updated timestamps
-- Description: Adds per-key last_updated_at_ms tracking, returns timestamped KV snapshots,
-- materializes KV timestamps during ack, and preserves KV state across execution pruning.

DO $$
DECLARE
    v_schema_name TEXT := current_schema();
BEGIN
    -- ============================================================================
    -- Part 1: Add last_updated_at_ms column to kv_store
    -- ============================================================================

    EXECUTE format($fmt$
        ALTER TABLE %I.kv_store ADD COLUMN last_updated_at_ms BIGINT NOT NULL DEFAULT 0;
$fmt$, v_schema_name);

    -- ============================================================================
    -- Part 2: Update fetch_orchestration_item to return timestamped KV snapshot
    -- ============================================================================

    EXECUTE format($fmt$DROP FUNCTION IF EXISTS %I.fetch_orchestration_item(BIGINT, BIGINT, BIGINT, BIGINT);$fmt$, v_schema_name);
    EXECUTE format($fmt$
        CREATE OR REPLACE FUNCTION %I.fetch_orchestration_item(
            p_now_ms BIGINT,
            p_lock_timeout_ms BIGINT,
            p_min_version_packed BIGINT DEFAULT NULL,
            p_max_version_packed BIGINT DEFAULT NULL
        )
        RETURNS TABLE(
            out_instance_id TEXT,
            out_orchestration_name TEXT,
            out_orchestration_version TEXT,
            out_execution_id BIGINT,
            out_history JSONB,
            out_messages JSONB,
            out_lock_token TEXT,
            out_attempt_count INTEGER,
            out_kv_snapshot JSONB
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
            v_kv_snapshot JSONB;
            v_lock_acquired INTEGER;
            v_max_attempt_count INTEGER;
        BEGIN
            LOOP
                v_instance_id := NULL;

                -- Phase 1: Find a candidate instance (no FOR UPDATE yet)
                -- When version filter is provided, join to instances+executions to filter
                IF p_min_version_packed IS NOT NULL THEN
                    SELECT q.instance_id INTO v_instance_id
                    FROM %I.orchestrator_queue q
                    LEFT JOIN %I.instances i ON i.instance_id = q.instance_id
                    LEFT JOIN %I.executions e ON e.instance_id = i.instance_id
                        AND e.execution_id = i.current_execution_id
                    WHERE q.visible_at <= TO_TIMESTAMP(p_now_ms / 1000.0)
                      AND NOT EXISTS (
                        SELECT 1 FROM %I.instance_locks il
                        WHERE il.instance_id = q.instance_id AND il.locked_until > p_now_ms
                      )
                      AND (
                        e.duroxide_version_major IS NULL
                        OR (e.duroxide_version_major * 1000000 + e.duroxide_version_minor * 1000 + e.duroxide_version_patch)
                           BETWEEN p_min_version_packed AND p_max_version_packed
                      )
                    ORDER BY q.visible_at, q.id
                    LIMIT 1;
                ELSE
                    SELECT q.instance_id INTO v_instance_id
                    FROM %I.orchestrator_queue q
                    WHERE q.visible_at <= TO_TIMESTAMP(p_now_ms / 1000.0)
                      AND NOT EXISTS (
                        SELECT 1 FROM %I.instance_locks il
                        WHERE il.instance_id = q.instance_id AND il.locked_until > p_now_ms
                      )
                    ORDER BY q.visible_at, q.id
                    LIMIT 1;
                END IF;

                IF NOT FOUND THEN
                    RETURN;
                END IF;

                -- Phase 2: Acquire instance-level advisory lock
                PERFORM pg_advisory_xact_lock(hashtext(v_instance_id));

                -- Phase 3: Re-verify the instance is still available with FOR UPDATE.
                -- If contention invalidated the candidate, keep searching.
                IF p_min_version_packed IS NOT NULL THEN
                    SELECT q.instance_id INTO v_instance_id
                    FROM %I.orchestrator_queue q
                    LEFT JOIN %I.instances i ON i.instance_id = q.instance_id
                    LEFT JOIN %I.executions e ON e.instance_id = i.instance_id
                        AND e.execution_id = i.current_execution_id
                    WHERE q.instance_id = v_instance_id
                      AND q.visible_at <= TO_TIMESTAMP(p_now_ms / 1000.0)
                      AND NOT EXISTS (
                        SELECT 1 FROM %I.instance_locks il
                        WHERE il.instance_id = q.instance_id AND il.locked_until > p_now_ms
                      )
                      AND (
                        e.duroxide_version_major IS NULL
                        OR (e.duroxide_version_major * 1000000 + e.duroxide_version_minor * 1000 + e.duroxide_version_patch)
                           BETWEEN p_min_version_packed AND p_max_version_packed
                      )
                    FOR UPDATE OF q SKIP LOCKED;
                ELSE
                    SELECT q.instance_id INTO v_instance_id
                    FROM %I.orchestrator_queue q
                    WHERE q.instance_id = v_instance_id
                      AND q.visible_at <= TO_TIMESTAMP(p_now_ms / 1000.0)
                      AND NOT EXISTS (
                        SELECT 1 FROM %I.instance_locks il
                        WHERE il.instance_id = q.instance_id AND il.locked_until > p_now_ms
                      )
                    FOR UPDATE OF q SKIP LOCKED;
                END IF;

                IF NOT FOUND THEN
                    CONTINUE;
                END IF;

                -- Step 2: Generate lock token and acquire instance lock
                v_lock_token := 'lock_' || gen_random_uuid()::TEXT;
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
                    CONTINUE;
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
                SELECT COALESCE(JSONB_AGG(q.work_item::JSONB ORDER BY q.id), '[]'::JSONB),
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
                    SELECT COALESCE(JSONB_AGG(h.event_data::JSONB ORDER BY h.event_id), '[]'::JSONB)
                    INTO v_history
                    FROM %I.history h
                    WHERE h.instance_id = v_instance_id AND h.execution_id = v_current_execution_id;

                    v_orchestration_version := COALESCE(v_orchestration_version, 'unknown');
                ELSE
                    SELECT COALESCE(JSONB_AGG(h.event_data::JSONB ORDER BY h.execution_id, h.event_id), '[]'::JSONB)
                    INTO v_history
                    FROM %I.history h
                    WHERE h.instance_id = v_instance_id;

                    IF JSONB_ARRAY_LENGTH(v_history) > 0 AND v_history->0 ? 'OrchestrationStarted' THEN
                        v_orchestration_name := v_history->0->'OrchestrationStarted'->>'name';
                        v_orchestration_version := v_history->0->'OrchestrationStarted'->>'version';
                        v_current_execution_id := 1;
                    ELSIF JSONB_ARRAY_LENGTH(v_messages) > 0 AND v_messages->0 ? 'StartOrchestration' THEN
                        v_orchestration_name := v_messages->0->'StartOrchestration'->>'orchestration';
                        v_orchestration_version := COALESCE(v_messages->0->'StartOrchestration'->>'version', 'unknown');
                        v_current_execution_id := COALESCE((v_messages->0->'StartOrchestration'->>'execution_id')::BIGINT, 1);
                    ELSIF JSONB_ARRAY_LENGTH(v_messages) > 0 AND v_messages->0 ? 'ContinueAsNew' THEN
                        v_orchestration_name := v_messages->0->'ContinueAsNew'->>'orchestration';
                        v_orchestration_version := COALESCE(v_messages->0->'ContinueAsNew'->>'version', 'unknown');
                        v_current_execution_id := 1;
                    ELSE
                        v_orchestration_name := 'Unknown';
                        v_orchestration_version := 'unknown';
                        v_current_execution_id := 1;
                    END IF;
                END IF;

                -- Load KV snapshot for this instance
                SELECT COALESCE(
                    jsonb_object_agg(
                        ks.key,
                        jsonb_build_object('value', ks.value, 'last_updated_at_ms', ks.last_updated_at_ms)
                    ),
                    '{}'::jsonb
                )
                INTO v_kv_snapshot
                FROM %I.kv_store ks
                WHERE ks.instance_id = v_instance_id;

                RETURN QUERY SELECT
                    v_instance_id,
                    v_orchestration_name,
                    v_orchestration_version,
                    v_current_execution_id,
                    v_history,
                    v_messages,
                    v_lock_token,
                    v_max_attempt_count,
                    v_kv_snapshot;
                RETURN;
            END LOOP;
        END;
        $fetch_orch$ LANGUAGE plpgsql;
$fmt$, v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name);


    -- ============================================================================
    -- Part 3: Update ack_orchestration_item to materialize KV timestamps
    -- ============================================================================

    EXECUTE format($fmt$DROP FUNCTION IF EXISTS %I.ack_orchestration_item(TEXT, BIGINT, BIGINT, JSONB, JSONB, JSONB, JSONB, JSONB);$fmt$, v_schema_name);
    EXECUTE format($fmt$DROP FUNCTION IF EXISTS %I.ack_orchestration_item(TEXT, BIGINT, JSONB, JSONB, JSONB, JSONB, JSONB);$fmt$, v_schema_name);
    EXECUTE format($fmt$
        CREATE OR REPLACE FUNCTION %I.ack_orchestration_item(
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
        BEGIN
            -- Convert Rust-supplied millisecond timestamp to TIMESTAMPTZ
            v_now_ts := TO_TIMESTAMP(p_now_ms / 1000.0);

            -- Step 1: Validate lock token
            SELECT il.instance_id INTO v_instance_id
            FROM %I.instance_locks il
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
            VALUES (v_instance_id, p_execution_id, 'Running', v_now_ts)
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

                UPDATE %I.executions e
                SET status = v_status, output = v_output, completed_at = v_completed_at
                WHERE e.instance_id = v_instance_id AND e.execution_id = p_execution_id;
            END IF;

            -- Step 7b: Store pinned duroxide version if provided in metadata
            IF p_metadata ? 'pinned_duroxide_version' AND p_metadata->'pinned_duroxide_version' IS NOT NULL
               AND p_metadata->>'pinned_duroxide_version' != 'null' THEN
                UPDATE %I.executions
                SET duroxide_version_major = (p_metadata->'pinned_duroxide_version'->>'major')::INTEGER,
                    duroxide_version_minor = (p_metadata->'pinned_duroxide_version'->>'minor')::INTEGER,
                    duroxide_version_patch = (p_metadata->'pinned_duroxide_version'->>'patch')::INTEGER
                WHERE instance_id = v_instance_id AND execution_id = p_execution_id;
            END IF;

            -- Step 7c: Handle custom_status update on instances table
            v_custom_status_action := p_metadata->>'custom_status_action';
            IF v_custom_status_action = 'set' THEN
                v_custom_status_value := p_metadata->>'custom_status_value';
                UPDATE %I.instances
                SET custom_status = v_custom_status_value,
                    custom_status_version = custom_status_version + 1
                WHERE instance_id = v_instance_id;
            ELSIF v_custom_status_action = 'clear' THEN
                UPDATE %I.instances
                SET custom_status = NULL,
                    custom_status_version = custom_status_version + 1
                WHERE instance_id = v_instance_id;
            END IF;

            -- Step 7d: Materialize KV mutations
            v_kv_mutations := p_metadata->'kv_mutations';
            IF v_kv_mutations IS NOT NULL AND jsonb_array_length(v_kv_mutations) > 0 THEN
                FOR v_i IN 0..jsonb_array_length(v_kv_mutations) - 1 LOOP
                    v_kv_item := v_kv_mutations->v_i;
                    v_kv_action := v_kv_item->>'action';
                    IF v_kv_action = 'set' THEN
                        INSERT INTO %I.kv_store (instance_id, key, value, execution_id, last_updated_at_ms)
                        VALUES (v_instance_id, v_kv_item->>'key', v_kv_item->>'value', v_current_execution_id, (v_kv_item->>'last_updated_at_ms')::BIGINT)
                        ON CONFLICT (instance_id, key)
                        DO UPDATE SET value = EXCLUDED.value, execution_id = EXCLUDED.execution_id, last_updated_at_ms = EXCLUDED.last_updated_at_ms;
                    ELSIF v_kv_action = 'clear_key' THEN
                        DELETE FROM %I.kv_store
                        WHERE instance_id = v_instance_id AND key = v_kv_item->>'key';
                    ELSIF v_kv_action = 'clear_all' THEN
                        DELETE FROM %I.kv_store
                        WHERE instance_id = v_instance_id;
                    END IF;
                END LOOP;
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

                    INSERT INTO %I.worker_queue (work_item, visible_at, created_at, instance_id, execution_id, activity_id, session_id, tag)
                    VALUES (v_elem::TEXT, v_now_ts, v_now_ts, v_item_instance_id, v_item_execution_id, v_item_activity_id, v_item_session_id, v_item_tag);
                END LOOP;
            END IF;

            -- Step 9: Delete cancelled activities from worker_queue (lock stealing)
            IF p_cancelled_activities IS NOT NULL AND JSONB_ARRAY_LENGTH(p_cancelled_activities) > 0 THEN
                FOR v_elem IN SELECT value FROM JSONB_ARRAY_ELEMENTS(p_cancelled_activities) LOOP
                    DELETE FROM %I.worker_queue
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
$fmt$, v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name);

    -- ============================================================================
    -- Part 4: Update prune_executions for instance-scoped KV retention
    -- ============================================================================

    EXECUTE format($fmt$DROP FUNCTION IF EXISTS %I.prune_executions(TEXT, INTEGER, BIGINT);$fmt$, v_schema_name);
    EXECUTE format($fmt$
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
            v_exec_id BIGINT;
        BEGIN
            -- Get current execution ID (NEVER delete this)
            SELECT i.current_execution_id INTO v_current_execution_id
            FROM %I.instances i
            WHERE i.instance_id = p_instance_id;

            IF NOT FOUND THEN
                -- Instance doesn't exist - raise error
                RAISE EXCEPTION 'Instance %% not found', p_instance_id;
            END IF;

            -- Build list of executions to delete
            -- CRITICAL: keep_last semantics - select top N executions INCLUDING current
            -- None, Some(0), Some(1) are all equivalent (only current remains)
            SELECT array_agg(e.execution_id) INTO v_exec_ids_to_delete
            FROM %I.executions e
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

            FOREACH v_exec_id IN ARRAY v_exec_ids_to_delete LOOP
                -- Delete history for this execution
                DELETE FROM %I.history h
                WHERE h.instance_id = p_instance_id
                  AND h.execution_id = v_exec_id;
                GET DIAGNOSTICS v_count = ROW_COUNT;
                v_events_deleted := v_events_deleted + v_count;

            END LOOP;

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
$fmt$, v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name);
END $$;
