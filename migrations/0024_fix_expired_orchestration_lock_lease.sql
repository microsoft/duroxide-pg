-- Copyright (c) Microsoft Corporation.
-- Licensed under the MIT License.

-- Migration 0024: never hand out an already-expired orchestration lock lease
--
-- NOTE: 0022 is reserved by the concurrent worker_queue dequeue PR
-- (perf/worker-queue-partial-index-dequeue). This migration is independent of it
-- and applies cleanly whether or not 0022 is present.
--
-- Rebuilds fetch_orchestration_item on top of the 0019 definition. Three related
-- defects allowed a message to enter a state the runtime could not recover from:
--
--   1. Every dispatcher selected the same candidate (ORDER BY visible_at, id
--      LIMIT 1 is deterministic) and then blocked on the same advisory key via
--      pg_advisory_xact_lock. The FOR UPDATE ... SKIP LOCKED re-verification
--      could not prevent the convoy because the blocking had already happened.
--
--   2. The lease was computed from p_now_ms -- a timestamp minted by the Rust
--      caller BEFORE the call, never refreshed. When the advisory wait exceeded
--      p_lock_timeout_ms, locked_until was written already in the past, so the
--      item was returned with a dead lease and every ack against it failed with
--      'Invalid lock token'. The same stale p_now_ms was also the liveness
--      reference for candidate visibility and for stealing expired locks, so a
--      long-running call became progressively less able to make progress.
--
--   3. The retry loop had no iteration bound, so each failed attempt took
--      another unbounded blocking advisory lock.
--
-- Fixes:
--
--   1. pg_try_advisory_xact_lock instead of pg_advisory_xact_lock. Contended
--      instances are skipped, not waited on, and are excluded from subsequent
--      candidate selection within the call so the loop cannot re-pick them.
--      This PRESERVES the deadlock protection introduced with instance-level
--      advisory locking: same-instance work is still mutually exclusive. Only
--      the *waiting* is removed.
--
--      The advisory key is also now schema-scoped. PostgreSQL advisory locks
--      live in a single database-wide space, but the key was hashtext() of the
--      instance id alone, so two duroxide schemas in the same database
--      serialized against each other whenever they happened to use the same
--      instance id -- entirely unrelated work contending on one key. While the
--      acquisition blocked this was invisible (the loser waited and then
--      proceeded); with a non-blocking acquisition it would surface as a
--      spurious skip. Hashing '<schema>.<instance_id>' confines contention to
--      the schema that owns the instance.
--
--      Rolling-upgrade note: during a mixed deployment, nodes on the previous
--      migration hash the unscoped key while upgraded nodes hash the scoped
--      one, so the advisory layer does not mutually exclude across that
--      boundary for the duration of the rollout. Correctness does not depend
--      on it -- instance_locks plus FOR UPDATE ... SKIP LOCKED still serialize
--      the claim -- and the provider already retries deadlock (40P01) errors,
--      so the window degrades to the pre-advisory-lock behavior rather than to
--      incorrect results.
--
--   2. An effective clock, v_now_ms, replaces p_now_ms everywhere inside the
--      function. It is the caller epoch advanced by the time actually spent
--      inside this call, measured as an INTERVAL from clock_timestamp():
--
--          v_now_ms := p_now_ms + elapsed_ms_since_function_entry
--
--      Only the elapsed *duration* comes from the server clock, never an
--      absolute server timestamp, so the Rust-supplied epoch established by
--      0006/0012 remains the single time reference and no cross-clock skew is
--      introduced between this function and visible_at/locked_until values
--      written elsewhere. The lease is therefore always p_lock_timeout_ms in
--      the future relative to the moment it is actually written.
--
--   3. The loop is bounded. Exhausting the bound returns no item, which the
--      dispatcher already handles as "nothing available"; the next poll starts
--      with a fresh clock.
--
-- The signature, return shape, and all downstream behavior (message batching,
-- history/metadata fallback, KV snapshot) are unchanged from 0019.

DO $$
DECLARE
    v_schema_name TEXT := current_schema();
BEGIN
    EXECUTE format($fmt$DROP FUNCTION IF EXISTS %1$I.fetch_orchestration_item(BIGINT, BIGINT, BIGINT, BIGINT);$fmt$, v_schema_name);

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
            v_orchestration_name TEXT;
            v_orchestration_version TEXT;
            v_current_execution_id BIGINT;
            v_history JSONB;
            v_messages JSONB;
            v_kv_snapshot JSONB;
            v_lock_acquired INTEGER;
            v_max_attempt_count INTEGER;
            -- Entry instant, used only to measure elapsed time within this call.
            v_call_start TIMESTAMPTZ := clock_timestamp();
            -- Caller epoch advanced by elapsed time inside this call.
            v_now_ms BIGINT;
            -- Instances proven unavailable during this call; never re-picked.
            v_skipped TEXT[] := ARRAY[]::TEXT[];
            v_iterations INTEGER := 0;
            c_max_iterations CONSTANT INTEGER := 50;
        BEGIN
            LOOP
                v_iterations := v_iterations + 1;
                IF v_iterations > c_max_iterations THEN
                    -- Heavily contended; report nothing available. The next poll
                    -- re-enters with a fresh clock and a fresh skip list.
                    RETURN;
                END IF;

                v_now_ms := p_now_ms
                    + (EXTRACT(EPOCH FROM (clock_timestamp() - v_call_start)) * 1000)::BIGINT;

                v_instance_id := NULL;

                -- Phase 1: Find a candidate instance (no FOR UPDATE yet)
                -- When version filter is provided, join to instances+executions to filter
                IF p_min_version_packed IS NOT NULL THEN
                    SELECT q.instance_id INTO v_instance_id
                    FROM %1$I.orchestrator_queue q
                    LEFT JOIN %1$I.instances i ON i.instance_id = q.instance_id
                    LEFT JOIN %1$I.executions e ON e.instance_id = i.instance_id
                        AND e.execution_id = i.current_execution_id
                    WHERE q.visible_at <= TO_TIMESTAMP(v_now_ms / 1000.0)
                      AND NOT (q.instance_id = ANY(v_skipped))
                      AND NOT EXISTS (
                        SELECT 1 FROM %1$I.instance_locks il
                        WHERE il.instance_id = q.instance_id AND il.locked_until > v_now_ms
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
                    WHERE q.visible_at <= TO_TIMESTAMP(v_now_ms / 1000.0)
                      AND NOT (q.instance_id = ANY(v_skipped))
                      AND NOT EXISTS (
                        SELECT 1 FROM %1$I.instance_locks il
                        WHERE il.instance_id = q.instance_id AND il.locked_until > v_now_ms
                      )
                    ORDER BY q.visible_at, q.id
                    LIMIT 1;
                END IF;

                IF NOT FOUND THEN
                    RETURN;
                END IF;

                -- Phase 2: Acquire the instance-level advisory lock WITHOUT waiting.
                -- Same-instance mutual exclusion is preserved (this is still the
                -- deadlock guard); a contended instance is skipped instead of
                -- convoying every dispatcher onto one key.
                IF NOT pg_try_advisory_xact_lock(hashtext(%2$L || '.' || v_instance_id)) THEN
                    v_skipped := array_append(v_skipped, v_instance_id);
                    CONTINUE;
                END IF;

                -- Re-read the clock after the acquisition attempt so the lease and
                -- the liveness predicates below reflect time actually elapsed.
                v_now_ms := p_now_ms
                    + (EXTRACT(EPOCH FROM (clock_timestamp() - v_call_start)) * 1000)::BIGINT;

                -- Phase 3: Re-verify the instance is still available with FOR UPDATE.
                -- If contention invalidated the candidate, keep searching.
                IF p_min_version_packed IS NOT NULL THEN
                    SELECT q.instance_id INTO v_instance_id
                    FROM %1$I.orchestrator_queue q
                    LEFT JOIN %1$I.instances i ON i.instance_id = q.instance_id
                    LEFT JOIN %1$I.executions e ON e.instance_id = i.instance_id
                        AND e.execution_id = i.current_execution_id
                    WHERE q.instance_id = v_instance_id
                      AND q.visible_at <= TO_TIMESTAMP(v_now_ms / 1000.0)
                      AND NOT EXISTS (
                        SELECT 1 FROM %1$I.instance_locks il
                        WHERE il.instance_id = q.instance_id AND il.locked_until > v_now_ms
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
                      AND q.visible_at <= TO_TIMESTAMP(v_now_ms / 1000.0)
                      AND NOT EXISTS (
                        SELECT 1 FROM %1$I.instance_locks il
                        WHERE il.instance_id = q.instance_id AND il.locked_until > v_now_ms
                      )
                    FOR UPDATE OF q SKIP LOCKED;
                END IF;

                IF NOT FOUND THEN
                    v_skipped := array_append(v_skipped, v_instance_id);
                    CONTINUE;
                END IF;

                -- Step 2: Generate lock token and acquire instance lock.
                -- v_locked_until is derived from the refreshed clock, so the lease
                -- is always p_lock_timeout_ms in the future when it is written.
                v_lock_token := 'lock_' || gen_random_uuid()::TEXT;
                v_locked_until := v_now_ms + p_lock_timeout_ms;

                INSERT INTO %1$I.instance_locks (instance_id, lock_token, locked_until, locked_at)
                VALUES (v_instance_id, v_lock_token, v_locked_until, v_now_ms)
                ON CONFLICT(instance_id) DO UPDATE
                SET lock_token = EXCLUDED.lock_token,
                    locked_until = EXCLUDED.locked_until,
                    locked_at = EXCLUDED.locked_at
                WHERE %1$I.instance_locks.locked_until <= v_now_ms;

                GET DIAGNOSTICS v_lock_acquired = ROW_COUNT;

                IF v_lock_acquired = 0 THEN
                    v_skipped := array_append(v_skipped, v_instance_id);
                    CONTINUE;
                END IF;

                -- Step 3: Mark all visible messages with our lock and increment attempt_count
                UPDATE %1$I.orchestrator_queue q
                SET lock_token = v_lock_token,
                    locked_until = v_locked_until,
                    attempt_count = q.attempt_count + 1
                WHERE q.instance_id = v_instance_id
                  AND q.visible_at <= TO_TIMESTAMP(v_now_ms / 1000.0)
                  AND (q.lock_token IS NULL OR q.locked_until <= v_now_ms);

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
$fmt$, v_schema_name, v_schema_name);

    RAISE NOTICE 'Migration 0024: fetch_orchestration_item never returns an expired lease; advisory lock is non-blocking and the retry loop is bounded';
END $$;
