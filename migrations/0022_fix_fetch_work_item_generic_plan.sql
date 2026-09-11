-- Copyright (c) Microsoft Corporation.
-- Licensed under the MIT License.

-- Migration 0022: Fix fetch_work_item's generic-plan scan of the future-visible backlog
--
-- Problem
-- -------
-- fetch_work_item selects the oldest eligible row with
--   WHERE q.visible_at <= TO_TIMESTAMP(p_now_ms / 1000.0)
--     AND (q.lock_token IS NULL OR q.locked_until <= p_now_ms) ...
--   ORDER BY q.id LIMIT 1 FOR UPDATE OF q SKIP LOCKED
-- Inside a PL/pgSQL function the parameterised predicate is planned once and
-- cached as a *generic* plan (after ~5 calls). The generic planner cannot
-- estimate the selectivity of `visible_at <= $param`, so with `ORDER BY id
-- LIMIT 1` it assumes it will hit a match early and walks worker_queue_pkey in
-- id order, filtering as it goes. When rows whose `visible_at` is still in the
-- future sort before the first visible row, it filters the entire
-- future-visible backlog by hand -- O(future-visible rows) per dequeue.
--
-- Measured (PostgreSQL 16, one visible row + N future-visible rows):
--   N=10k   -> ~4.4 ms      N=100k -> ~44 ms      N=1,000,000 -> ~440 ms
-- Growth is exactly linear (~0.000439 ms/row, r^2 = 1.00) and crosses 10 ms at
-- ~25,000 future-visible rows. The identical query with literal values runs in
-- ~0.006 ms using the existing idx_worker_visible(visible_at, lock_token); the
-- cost is the generic plan, not a missing index. Reproduces on main
-- independently of any queue/index change. See
-- docs/analysis/fetch-work-item-generic-plan.md.
--
-- Fix
-- ---
-- Add a second, redundant upper bound to the same predicate:
--   AND q.visible_at <= statement_timestamp()
-- `statement_timestamp()` is a stable expression the planner *can* estimate
-- against the visible_at histogram, so the cached generic plan uses
-- idx_worker_visible instead of the primary-key walk. Measured flat at
-- ~12 us out to 100,000 future-visible rows, with no dequeue-throughput
-- regression (unlike forcing a custom plan, which cost ~16-20%). The
-- authoritative `visible_at <= TO_TIMESTAMP(p_now_ms / 1000.0)` bound is kept,
-- so nothing else about the eligibility test changes.
--
-- Correctness caveat (reviewers: please confirm before relying on this)
-- --------------------------------------------------------------------
-- A row is returned only if it passes BOTH bounds, so there is no possibility
-- of returning a not-yet-visible row, no duplication, and no reordering. The
-- only observable effect is a liveness one: because the added bound also gates
-- visibility on the DATABASE clock (statement_timestamp()), a due item is
-- withheld until the server clock reaches its visible_at. If a worker's clock
-- runs AHEAD of the database's, a delayed/timer item fires late by up to the
-- clock skew (self-healing, bounded, never lost).
--
-- Note the tension with migration 0006 (use_rust_timestamps), which
-- deliberately moved all time logic onto the application-supplied p_now_ms to
-- avoid depending on the database clock. This bound re-introduces a
-- database-clock dependency for visibility gating only. In deployments where
-- the application and database hosts are NTP-disciplined the skew is small and
-- the late-firing window is negligible; if the application clock cannot be
-- assumed to be at or behind the database clock, this fix must be guarded or a
-- clock-agnostic alternative (e.g. force_custom_plan, at a throughput cost)
-- chosen instead. See docs/analysis/custom-plan-cost.md.
--
-- Scope: worker_queue's fetch_work_item only. fetch_orchestration_item shares
-- the same PL/pgSQL generic-plan shape and is left as a follow-up.

DO $$
DECLARE
    v_schema_name TEXT := current_schema();
BEGIN
    EXECUTE format('DROP FUNCTION IF EXISTS %1$I.fetch_work_item(BIGINT, BIGINT, TEXT, BIGINT)', v_schema_name);
    EXECUTE format('DROP FUNCTION IF EXISTS %1$I.fetch_work_item(BIGINT, BIGINT, TEXT, BIGINT, TEXT[], TEXT)', v_schema_name);

    EXECUTE format($body$
        CREATE OR REPLACE FUNCTION %1$I.fetch_work_item(
            p_now_ms BIGINT,
            p_lock_timeout_ms BIGINT,
            p_owner_id TEXT DEFAULT NULL,
            p_session_lock_timeout_ms BIGINT DEFAULT NULL,
            p_tag_filter TEXT[] DEFAULT NULL,
            p_tag_mode TEXT DEFAULT 'default_only'
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
            IF p_tag_mode = 'none' THEN
                RETURN;
            END IF;

            IF p_owner_id IS NOT NULL THEN
                -- Session-aware fetch with tag filtering
                SELECT q.id, q.session_id INTO v_id, v_session_id
                FROM %1$I.worker_queue q
                LEFT JOIN %1$I.sessions s ON s.session_id = q.session_id AND s.locked_until > p_now_ms
                WHERE q.visible_at <= TO_TIMESTAMP(p_now_ms / 1000.0)
                  -- Redundant bound so the cached generic plan can estimate
                  -- selectivity and use idx_worker_visible (see migration header).
                  AND q.visible_at <= statement_timestamp()
                  AND (q.lock_token IS NULL OR q.locked_until <= p_now_ms)
                  AND (
                    q.session_id IS NULL
                    OR s.worker_id = p_owner_id
                    OR s.session_id IS NULL
                  )
                  AND (
                    CASE p_tag_mode
                        WHEN 'default_only' THEN q.tag IS NULL
                        WHEN 'tags' THEN q.tag = ANY(p_tag_filter)
                        WHEN 'default_and' THEN (q.tag IS NULL OR q.tag = ANY(p_tag_filter))
                        WHEN 'any' THEN TRUE
                        ELSE FALSE
                    END
                  )
                ORDER BY q.id
                LIMIT 1
                FOR UPDATE OF q SKIP LOCKED;
            ELSE
                -- Non-session fetch with tag filtering
                SELECT q.id, q.session_id INTO v_id, v_session_id
                FROM %1$I.worker_queue q
                WHERE q.visible_at <= TO_TIMESTAMP(p_now_ms / 1000.0)
                  -- Redundant bound so the cached generic plan can estimate
                  -- selectivity and use idx_worker_visible (see migration header).
                  AND q.visible_at <= statement_timestamp()
                  AND (q.lock_token IS NULL OR q.locked_until <= p_now_ms)
                  AND q.session_id IS NULL
                  AND (
                    CASE p_tag_mode
                        WHEN 'default_only' THEN q.tag IS NULL
                        WHEN 'tags' THEN q.tag = ANY(p_tag_filter)
                        WHEN 'default_and' THEN (q.tag IS NULL OR q.tag = ANY(p_tag_filter))
                        WHEN 'any' THEN TRUE
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

            out_lock_token := 'lock_' || gen_random_uuid()::TEXT;

            -- Increment attempt_count and lock the item
            UPDATE %1$I.worker_queue
            SET lock_token = out_lock_token,
                locked_until = p_now_ms + p_lock_timeout_ms,
                attempt_count = attempt_count + 1
            WHERE id = v_id;

            SELECT work_item, attempt_count
            INTO out_work_item, out_attempt_count
            FROM %1$I.worker_queue
            WHERE id = v_id;

            -- If session-bound, upsert the sessions row
            IF v_session_id IS NOT NULL AND p_owner_id IS NOT NULL THEN
                v_session_locked_until := p_now_ms + COALESCE(p_session_lock_timeout_ms, p_lock_timeout_ms);

                INSERT INTO %1$I.sessions (session_id, worker_id, locked_until, last_activity_at)
                VALUES (v_session_id, p_owner_id, v_session_locked_until, p_now_ms)
                ON CONFLICT (session_id) DO UPDATE
                SET worker_id = p_owner_id,
                    locked_until = v_session_locked_until,
                    last_activity_at = p_now_ms
                WHERE %1$I.sessions.locked_until <= p_now_ms OR %1$I.sessions.worker_id = p_owner_id;

                -- If upsert affected 0 rows, another worker owns this session.
                -- Roll back: clear lock so item can be retried.
                IF NOT FOUND THEN
                    UPDATE %1$I.worker_queue
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
    $body$, v_schema_name);

    RAISE NOTICE 'Migration 0022: fetch_work_item now bounds visible_at by statement_timestamp() so the cached plan uses idx_worker_visible';
END $$;
