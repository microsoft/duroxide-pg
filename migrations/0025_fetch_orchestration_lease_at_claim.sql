-- Copyright (c) Microsoft Corporation.
-- Licensed under the MIT License.

-- Migration 0025: never hand out an expired orchestration lock lease
--
-- 0022 and 0024 are used by open pull requests; the runner applies pending
-- migrations by version, so the gap is harmless.
--
-- fetch_orchestration_item (baseline: 0019) set the lease to
-- p_now_ms + p_lock_timeout_ms. The caller reads p_now_ms before the call,
-- before any connection-pool wait and before the advisory-lock wait inside the
-- call. When the waits were longer than the lock timeout, the item was returned
-- with a lease that had already expired, so every ack, including the
-- max_attempts poison ack, failed with 'Invalid lock token' and the message was
-- retried forever (microsoft/duroxide-pg#21, microsoft/duroxide#46).
--
-- The lease now starts when the instance is claimed:
--
--     GREATEST(p_now_ms + time spent in this call, database clock_timestamp())
--         + p_lock_timeout_ms
--
-- The first term covers waits inside the call; the second covers a p_now_ms
-- that was already stale on arrival. GREATEST can only make the lease longer.
-- Nothing else changes: the same signature, result shape, locking, candidate
-- selection and predicates as 0019. CREATE OR REPLACE keeps the function OID
-- and EXECUTE grants.

DO $$
DECLARE
    v_schema_name TEXT := current_schema();
BEGIN
    EXECUTE format($fmt$
        CREATE OR REPLACE FUNCTION %1$I.fetch_orchestration_item(
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
            v_lease_base_ms BIGINT;
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
                    FROM %1$I.orchestrator_queue q
                    LEFT JOIN %1$I.instances i ON i.instance_id = q.instance_id
                    LEFT JOIN %1$I.executions e ON e.instance_id = i.instance_id
                        AND e.execution_id = i.current_execution_id
                    WHERE q.visible_at <= TO_TIMESTAMP(p_now_ms / 1000.0)
                      AND NOT EXISTS (
                        SELECT 1 FROM %1$I.instance_locks il
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
                    FROM %1$I.orchestrator_queue q
                    WHERE q.visible_at <= TO_TIMESTAMP(p_now_ms / 1000.0)
                      AND NOT EXISTS (
                        SELECT 1 FROM %1$I.instance_locks il
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
                    FROM %1$I.orchestrator_queue q
                    LEFT JOIN %1$I.instances i ON i.instance_id = q.instance_id
                    LEFT JOIN %1$I.executions e ON e.instance_id = i.instance_id
                        AND e.execution_id = i.current_execution_id
                    WHERE q.instance_id = v_instance_id
                      AND q.visible_at <= TO_TIMESTAMP(p_now_ms / 1000.0)
                      AND NOT EXISTS (
                        SELECT 1 FROM %1$I.instance_locks il
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
                    FROM %1$I.orchestrator_queue q
                    WHERE q.instance_id = v_instance_id
                      AND q.visible_at <= TO_TIMESTAMP(p_now_ms / 1000.0)
                      AND NOT EXISTS (
                        SELECT 1 FROM %1$I.instance_locks il
                        WHERE il.instance_id = q.instance_id AND il.locked_until > p_now_ms
                      )
                    FOR UPDATE OF q SKIP LOCKED;
                END IF;

                IF NOT FOUND THEN
                    CONTINUE;
                END IF;

                -- Step 2: Generate lock token and acquire instance lock.
                -- The lease starts now: the later of the caller clock advanced by the
                -- time spent in this call and the database clock. p_now_ms was read
                -- before the call and any wait, so it alone can give an expired lease.
                v_lease_base_ms := GREATEST(
                    p_now_ms + (EXTRACT(EPOCH FROM (clock_timestamp() - statement_timestamp())) * 1000)::BIGINT,
                    (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT
                );
                v_lock_token := 'lock_' || gen_random_uuid()::TEXT;
                v_locked_until := v_lease_base_ms + p_lock_timeout_ms;

                INSERT INTO %1$I.instance_locks (instance_id, lock_token, locked_until, locked_at)
                VALUES (v_instance_id, v_lock_token, v_locked_until, p_now_ms)
                ON CONFLICT(instance_id) DO UPDATE
                SET lock_token = EXCLUDED.lock_token,
                    locked_until = EXCLUDED.locked_until,
                    locked_at = EXCLUDED.locked_at
                WHERE %1$I.instance_locks.locked_until <= p_now_ms;

                GET DIAGNOSTICS v_lock_acquired = ROW_COUNT;

                IF v_lock_acquired = 0 THEN
                    CONTINUE;
                END IF;

                -- Step 3: Mark all visible messages with our lock and increment attempt_count
                UPDATE %1$I.orchestrator_queue q
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
                FROM %1$I.orchestrator_queue q
                WHERE q.lock_token = v_lock_token;

                -- Step 5: Load instance metadata
                SELECT i.orchestration_name, i.orchestration_version, i.current_execution_id
                INTO v_orchestration_name, v_orchestration_version, v_current_execution_id
                FROM %1$I.instances i
                WHERE i.instance_id = v_instance_id;

                -- Step 6: Load history or implement fallback
                IF FOUND THEN
                    SELECT COALESCE(JSONB_AGG(h.event_data::JSONB ORDER BY h.event_id), '[]'::JSONB)
                    INTO v_history
                    FROM %1$I.history h
                    WHERE h.instance_id = v_instance_id AND h.execution_id = v_current_execution_id;

                    v_orchestration_version := COALESCE(v_orchestration_version, 'unknown');
                ELSE
                    SELECT COALESCE(JSONB_AGG(h.event_data::JSONB ORDER BY h.execution_id, h.event_id), '[]'::JSONB)
                    INTO v_history
                    FROM %1$I.history h
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
                FROM %1$I.kv_store ks
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
$fmt$, v_schema_name);
END $$;
