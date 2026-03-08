-- Migration: 0016_add_activity_tags.sql
-- Description: Adds activity tag routing support for worker queue items.
-- Adds tag column to worker_queue, updates enqueue_worker_work to accept tag,
-- and updates fetch_work_item to filter by tag.

DO $$
DECLARE
    v_schema_name TEXT := current_schema();
BEGIN
    -- ============================================================================
    -- Part 1: Add tag column and index to worker_queue
    -- ============================================================================

    EXECUTE format('ALTER TABLE %I.worker_queue ADD COLUMN IF NOT EXISTS tag TEXT', v_schema_name);
    EXECUTE format('CREATE INDEX IF NOT EXISTS idx_worker_queue_tag ON %I.worker_queue(tag)', v_schema_name);

    -- ============================================================================
    -- Part 2: Update enqueue_worker_work to accept tag parameter
    -- ============================================================================

    EXECUTE format('DROP FUNCTION IF EXISTS %I.enqueue_worker_work(TEXT, BIGINT, TEXT, BIGINT, BIGINT, TEXT)', v_schema_name);

    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.enqueue_worker_work(
            p_work_item TEXT,
            p_now_ms BIGINT,
            p_instance_id TEXT DEFAULT NULL,
            p_execution_id BIGINT DEFAULT NULL,
            p_activity_id BIGINT DEFAULT NULL,
            p_session_id TEXT DEFAULT NULL,
            p_tag TEXT DEFAULT NULL
        )
        RETURNS VOID AS $enq_worker$
        DECLARE
            v_now_ts TIMESTAMPTZ;
        BEGIN
            v_now_ts := TO_TIMESTAMP(p_now_ms / 1000.0);
            INSERT INTO %I.worker_queue (work_item, visible_at, created_at, instance_id, execution_id, activity_id, session_id, tag)
            VALUES (p_work_item, v_now_ts, v_now_ts, p_instance_id, p_execution_id, p_activity_id, p_session_id, p_tag);
        END;
        $enq_worker$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name);

    -- ============================================================================
    -- Part 3: Update fetch_work_item with tag filtering
    -- ============================================================================

    EXECUTE format('DROP FUNCTION IF EXISTS %I.fetch_work_item(BIGINT, BIGINT, TEXT, BIGINT)', v_schema_name);

    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.fetch_work_item(
            p_now_ms BIGINT,
            p_lock_timeout_ms BIGINT,
            p_owner_id TEXT DEFAULT NULL,
            p_session_lock_timeout_ms BIGINT DEFAULT NULL,
            p_tag_filter TEXT[] DEFAULT NULL,
            p_tag_mode TEXT DEFAULT ''default_only''
        )
        RETURNS TABLE(
            out_work_item TEXT,
            out_lock_token TEXT,
            out_attempt_count INTEGER
        ) AS $fetch_worker$
        DECLARE
            v_id BIGINT;
            v_session_id TEXT;
            v_session_locked_until BIGINT;
        BEGIN
            -- none mode: return immediately with no results
            IF p_tag_mode = ''none'' THEN
                RETURN;
            END IF;

            IF p_owner_id IS NOT NULL THEN
                -- Session-aware fetch with tag filtering
                SELECT q.id, q.session_id INTO v_id, v_session_id
                FROM %I.worker_queue q
                LEFT JOIN %I.sessions s ON s.session_id = q.session_id AND s.locked_until > p_now_ms
                WHERE q.visible_at <= TO_TIMESTAMP(p_now_ms / 1000.0)
                  AND (q.lock_token IS NULL OR q.locked_until <= p_now_ms)
                  AND (
                    q.session_id IS NULL
                    OR s.worker_id = p_owner_id
                    OR s.session_id IS NULL
                  )
                  AND (
                    CASE p_tag_mode
                        WHEN ''default_only'' THEN q.tag IS NULL
                        WHEN ''tags'' THEN q.tag = ANY(p_tag_filter)
                        WHEN ''default_and'' THEN (q.tag IS NULL OR q.tag = ANY(p_tag_filter))
                        WHEN ''any'' THEN TRUE
                        ELSE FALSE
                    END
                  )
                ORDER BY q.id
                LIMIT 1
                FOR UPDATE OF q SKIP LOCKED;
            ELSE
                -- Non-session fetch with tag filtering
                SELECT q.id, q.session_id INTO v_id, v_session_id
                FROM %I.worker_queue q
                WHERE q.visible_at <= TO_TIMESTAMP(p_now_ms / 1000.0)
                  AND (q.lock_token IS NULL OR q.locked_until <= p_now_ms)
                  AND q.session_id IS NULL
                  AND (
                    CASE p_tag_mode
                        WHEN ''default_only'' THEN q.tag IS NULL
                        WHEN ''tags'' THEN q.tag = ANY(p_tag_filter)
                        WHEN ''default_and'' THEN (q.tag IS NULL OR q.tag = ANY(p_tag_filter))
                        WHEN ''any'' THEN TRUE
                        ELSE FALSE
                    END
                  )
                ORDER BY q.id
                LIMIT 1
                FOR UPDATE OF q SKIP LOCKED;
            END IF;

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

            -- If session-bound, upsert the sessions row
            IF v_session_id IS NOT NULL AND p_owner_id IS NOT NULL THEN
                v_session_locked_until := p_now_ms + COALESCE(p_session_lock_timeout_ms, p_lock_timeout_ms);

                INSERT INTO %I.sessions (session_id, worker_id, locked_until, last_activity_at)
                VALUES (v_session_id, p_owner_id, v_session_locked_until, p_now_ms)
                ON CONFLICT (session_id) DO UPDATE
                SET worker_id = p_owner_id,
                    locked_until = v_session_locked_until,
                    last_activity_at = p_now_ms
                WHERE %I.sessions.locked_until <= p_now_ms OR %I.sessions.worker_id = p_owner_id;

                -- If upsert affected 0 rows, another worker owns this session.
                -- Roll back: clear lock so item can be retried.
                IF NOT FOUND THEN
                    UPDATE %I.worker_queue
                    SET lock_token = NULL,
                        locked_until = NULL,
                        attempt_count = attempt_count - 1
                    WHERE id = v_id;
                    RETURN;
                END IF;
            END IF;

            RETURN NEXT;
        END;
        $fetch_worker$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name, v_schema_name, v_schema_name,
       v_schema_name, v_schema_name, v_schema_name, v_schema_name,
       v_schema_name, v_schema_name);

    -- ============================================================================
    -- Part 4: Update ack_orchestration_item to extract tag from worker items
    -- Based on the working SP from migration 0015, with tag addition in Step 8
    -- ============================================================================

    EXECUTE format('DROP FUNCTION IF EXISTS %I.ack_orchestration_item(TEXT, BIGINT, BIGINT, JSONB, JSONB, JSONB, JSONB, JSONB)', v_schema_name);
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
            v_item_session_id TEXT;
            v_item_tag TEXT;
            v_now_ts TIMESTAMPTZ;
            v_custom_status_action TEXT;
            v_custom_status_value TEXT;
        BEGIN
            -- Convert Rust-supplied millisecond timestamp to TIMESTAMPTZ
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

            -- Step 7b: Store pinned duroxide version if provided in metadata
            IF p_metadata ? ''pinned_duroxide_version'' AND p_metadata->''pinned_duroxide_version'' IS NOT NULL
               AND p_metadata->>''pinned_duroxide_version'' != ''null'' THEN
                UPDATE %I.executions
                SET duroxide_version_major = (p_metadata->''pinned_duroxide_version''->>''major'')::INTEGER,
                    duroxide_version_minor = (p_metadata->''pinned_duroxide_version''->>''minor'')::INTEGER,
                    duroxide_version_patch = (p_metadata->''pinned_duroxide_version''->>''patch'')::INTEGER
                WHERE instance_id = v_instance_id AND execution_id = p_execution_id;
            END IF;

            -- Step 7c: Handle custom_status update on instances table
            v_custom_status_action := p_metadata->>''custom_status_action'';
            IF v_custom_status_action = ''set'' THEN
                v_custom_status_value := p_metadata->>''custom_status_value'';
                UPDATE %I.instances
                SET custom_status = v_custom_status_value,
                    custom_status_version = custom_status_version + 1
                WHERE instance_id = v_instance_id;
            ELSIF v_custom_status_action = ''clear'' THEN
                UPDATE %I.instances
                SET custom_status = NULL,
                    custom_status_version = custom_status_version + 1
                WHERE instance_id = v_instance_id;
            END IF;

            -- Step 8: Enqueue worker items with session_id and tag support
            IF p_worker_items IS NOT NULL AND JSONB_ARRAY_LENGTH(p_worker_items) > 0 THEN
                FOR v_elem IN SELECT value FROM JSONB_ARRAY_ELEMENTS(p_worker_items) LOOP
                    IF v_elem ? ''ActivityExecute'' THEN
                        v_item_instance_id := v_elem->''ActivityExecute''->>''instance'';
                        v_item_execution_id := (v_elem->''ActivityExecute''->>''execution_id'')::BIGINT;
                        v_item_activity_id := (v_elem->''ActivityExecute''->>''id'')::BIGINT;
                        v_item_session_id := v_elem->''ActivityExecute''->>''session_id'';
                        v_item_tag := v_elem->''ActivityExecute''->>''tag'';
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
                    ELSIF v_elem ? ''QueueMessage'' THEN
                        v_item_instance_id := v_elem->''QueueMessage''->>''instance'';
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
       v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name,
       v_schema_name, v_schema_name, v_schema_name, v_schema_name);

    -- ============================================================================
    -- Part 5: Update cleanup_schema to drop new function signatures
    -- ============================================================================

    EXECUTE format('DROP FUNCTION IF EXISTS %I.cleanup_schema()', v_schema_name);

    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.cleanup_schema()
        RETURNS VOID AS $cleanup$
        BEGIN
            -- Drop tables first
            DROP TABLE IF EXISTS %I.sessions CASCADE;
            DROP TABLE IF EXISTS %I.instances CASCADE;
            DROP TABLE IF EXISTS %I.executions CASCADE;
            DROP TABLE IF EXISTS %I.history CASCADE;
            DROP TABLE IF EXISTS %I.orchestrator_queue CASCADE;
            DROP TABLE IF EXISTS %I.worker_queue CASCADE;
            DROP TABLE IF EXISTS %I.instance_locks CASCADE;
            DROP TABLE IF EXISTS %I._duroxide_migrations CASCADE;
            
            -- Drop all stored procedures (old + new signatures)
            DROP FUNCTION IF EXISTS %I.cleanup_schema();
            DROP FUNCTION IF EXISTS %I.list_instances();
            DROP FUNCTION IF EXISTS %I.list_executions(TEXT);
            DROP FUNCTION IF EXISTS %I.latest_execution_id(TEXT);
            DROP FUNCTION IF EXISTS %I.list_instances_by_status(TEXT);
            DROP FUNCTION IF EXISTS %I.get_instance_info(TEXT);
            DROP FUNCTION IF EXISTS %I.get_execution_info(TEXT, BIGINT);
            DROP FUNCTION IF EXISTS %I.get_system_metrics();
            DROP FUNCTION IF EXISTS %I.get_queue_depths(BIGINT);
            DROP FUNCTION IF EXISTS %I.enqueue_worker_work(TEXT, BIGINT, TEXT, BIGINT, BIGINT, TEXT);
            DROP FUNCTION IF EXISTS %I.enqueue_worker_work(TEXT, BIGINT, TEXT, BIGINT, BIGINT, TEXT, TEXT);
            DROP FUNCTION IF EXISTS %I.ack_worker(TEXT, TEXT, TEXT, BIGINT);
            DROP FUNCTION IF EXISTS %I.renew_work_item_lock(TEXT, BIGINT, BIGINT);
            DROP FUNCTION IF EXISTS %I.fetch_work_item(BIGINT, BIGINT, TEXT, BIGINT);
            DROP FUNCTION IF EXISTS %I.fetch_work_item(BIGINT, BIGINT, TEXT, BIGINT, TEXT[], TEXT);
            DROP FUNCTION IF EXISTS %I.abandon_work_item(TEXT, BIGINT, BIGINT, BOOLEAN);
            DROP FUNCTION IF EXISTS %I.enqueue_orchestrator_work(TEXT, TEXT, TIMESTAMPTZ, TEXT, TEXT, BIGINT);
            DROP FUNCTION IF EXISTS %I.fetch_orchestration_item(BIGINT, BIGINT);
            DROP FUNCTION IF EXISTS %I.fetch_orchestration_item(BIGINT, BIGINT, BIGINT, BIGINT);
            DROP FUNCTION IF EXISTS %I.ack_orchestration_item(TEXT, BIGINT, BIGINT, JSONB, JSONB, JSONB, JSONB, JSONB);
            DROP FUNCTION IF EXISTS %I.abandon_orchestration_item(TEXT, BIGINT, BIGINT, BOOLEAN);
            DROP FUNCTION IF EXISTS %I.renew_orchestration_item_lock(TEXT, BIGINT, BIGINT);
            DROP FUNCTION IF EXISTS %I.fetch_history(TEXT);
            DROP FUNCTION IF EXISTS %I.fetch_history_with_execution(TEXT, BIGINT);
            DROP FUNCTION IF EXISTS %I.append_history(TEXT, BIGINT, JSONB);
            DROP FUNCTION IF EXISTS %I.list_children(TEXT);
            DROP FUNCTION IF EXISTS %I.get_parent_id(TEXT);
            DROP FUNCTION IF EXISTS %I.delete_instances_atomic(TEXT[], BOOLEAN);
            DROP FUNCTION IF EXISTS %I.prune_executions(TEXT, INTEGER, BIGINT);
            DROP FUNCTION IF EXISTS %I.renew_session_lock(TEXT[], BIGINT, BIGINT, BIGINT);
            DROP FUNCTION IF EXISTS %I.cleanup_orphaned_sessions(BIGINT);
            DROP FUNCTION IF EXISTS %I.get_custom_status(TEXT, BIGINT);
        END;
        $cleanup$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name, v_schema_name, v_schema_name,
       v_schema_name, v_schema_name, v_schema_name, v_schema_name,
       v_schema_name, v_schema_name, v_schema_name, v_schema_name,
       v_schema_name, v_schema_name, v_schema_name, v_schema_name,
       v_schema_name, v_schema_name, v_schema_name, v_schema_name,
       v_schema_name, v_schema_name, v_schema_name, v_schema_name,
       v_schema_name, v_schema_name, v_schema_name, v_schema_name,
       v_schema_name, v_schema_name, v_schema_name, v_schema_name,
       v_schema_name, v_schema_name, v_schema_name, v_schema_name,
       v_schema_name, v_schema_name, v_schema_name, v_schema_name,
       v_schema_name);

    RAISE NOTICE 'Migration 0016: Added activity tag routing support (tag column, tag index, updated enqueue_worker_work and fetch_work_item, updated cleanup_schema)';
END $$;
