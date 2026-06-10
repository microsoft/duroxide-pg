-- Copyright (c) Microsoft Corporation.
-- Licensed under the MIT License.

-- Migration: 0005_add_visible_at_to_worker_queue.sql
-- Description: Adds visible_at column to worker_queue for delayed visibility (duroxide 0.1.5)
-- This provides consistent visibility semantics between orchestrator_queue and worker_queue.
-- abandon_work_item now uses visible_at for delay instead of keeping locked_until.

-- Add visible_at to worker_queue
-- Matches the pattern used by orchestrator_queue for delayed visibility
ALTER TABLE worker_queue ADD COLUMN IF NOT EXISTS visible_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP;

-- Create index for visible_at queries (matches idx_orch_visible pattern)
CREATE INDEX IF NOT EXISTS idx_worker_visible ON worker_queue(visible_at, lock_token);

DO $$
DECLARE
    v_schema_name TEXT := current_schema();
BEGIN
    -- ============================================================================
    -- Update fetch_work_item to check visible_at
    -- Items are available if: visible_at <= now AND (unlocked OR lock expired)
    -- ============================================================================
    EXECUTE format('DROP FUNCTION IF EXISTS %I.fetch_work_item(BIGINT, BIGINT)', v_schema_name);

    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.fetch_work_item(
            p_now_ms BIGINT,
            p_lock_timeout_ms BIGINT
        )
        RETURNS TABLE(
            out_work_item TEXT,
            out_lock_token TEXT,
            out_attempt_count INTEGER
        ) AS $fetch_worker$
        DECLARE
            v_id BIGINT;
        BEGIN
            -- Item is available if:
            -- 1. visible_at <= now (not delayed)
            -- 2. AND (lock_token IS NULL OR locked_until <= now) (not locked or lock expired)
            SELECT q.id INTO v_id
            FROM %I.worker_queue q
            WHERE q.visible_at <= TO_TIMESTAMP(p_now_ms / 1000.0)
              AND (q.lock_token IS NULL OR q.locked_until <= p_now_ms)
            ORDER BY q.id
            LIMIT 1
            FOR UPDATE OF q SKIP LOCKED;

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

            RETURN NEXT;
        END;
        $fetch_worker$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name, v_schema_name, v_schema_name);

    -- ============================================================================
    -- Update abandon_work_item to use visible_at for delay
    -- Sets visible_at to future time AND clears lock (cleaner semantics)
    -- ============================================================================
    EXECUTE format('DROP FUNCTION IF EXISTS %I.abandon_work_item(TEXT, BIGINT, BOOLEAN)', v_schema_name);

    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.abandon_work_item(
            p_lock_token TEXT,
            p_delay_ms BIGINT DEFAULT NULL,
            p_ignore_attempt BOOLEAN DEFAULT FALSE
        )
        RETURNS VOID AS $abandon_worker$
        DECLARE
            v_rows_affected INTEGER;
            v_visible_at TIMESTAMPTZ;
        BEGIN
            -- Calculate visible_at based on delay
            IF p_delay_ms IS NOT NULL AND p_delay_ms > 0 THEN
                v_visible_at := NOW() + (p_delay_ms || '' milliseconds'')::INTERVAL;
            ELSE
                v_visible_at := NOW();
            END IF;

            -- Always clear lock_token and locked_until when abandoning
            -- Use visible_at to control when item becomes available again
            IF p_ignore_attempt THEN
                UPDATE %I.worker_queue
                SET lock_token = NULL,
                    locked_until = NULL,
                    visible_at = v_visible_at,
                    attempt_count = GREATEST(0, attempt_count - 1)
                WHERE lock_token = p_lock_token;
            ELSE
                UPDATE %I.worker_queue
                SET lock_token = NULL,
                    locked_until = NULL,
                    visible_at = v_visible_at
                WHERE lock_token = p_lock_token;
            END IF;

            GET DIAGNOSTICS v_rows_affected = ROW_COUNT;

            IF v_rows_affected = 0 THEN
                RAISE EXCEPTION ''Invalid lock token or already acked'';
            END IF;
        END;
        $abandon_worker$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name, v_schema_name);

    -- ============================================================================
    -- Update enqueue_worker_work to set visible_at
    -- ============================================================================
    EXECUTE format('DROP FUNCTION IF EXISTS %I.enqueue_worker_work(TEXT)', v_schema_name);

    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.enqueue_worker_work(p_work_item TEXT)
        RETURNS VOID AS $enq_worker$
        BEGIN
            INSERT INTO %I.worker_queue (work_item, visible_at, created_at)
            VALUES (p_work_item, NOW(), NOW());
        END;
        $enq_worker$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name);

    -- ============================================================================
    -- Update get_queue_depths to check visible_at for worker_queue
    -- ============================================================================
    EXECUTE format('DROP FUNCTION IF EXISTS %I.get_queue_depths(BIGINT)', v_schema_name);

    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.get_queue_depths(p_now_ms BIGINT)
        RETURNS TABLE(
            orchestrator_queue BIGINT,
            worker_queue BIGINT
        ) AS $get_queue_depths$
        BEGIN
            RETURN QUERY
            SELECT 
                (SELECT COUNT(*)::BIGINT FROM %I.orchestrator_queue 
                 WHERE visible_at <= TO_TIMESTAMP(p_now_ms / 1000.0)
                   AND (lock_token IS NULL OR locked_until <= p_now_ms)) as orchestrator_queue,
                (SELECT COUNT(*)::BIGINT FROM %I.worker_queue 
                 WHERE visible_at <= TO_TIMESTAMP(p_now_ms / 1000.0)
                   AND (lock_token IS NULL OR locked_until <= p_now_ms)) as worker_queue;
        END;
        $get_queue_depths$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name, v_schema_name);

END;
$$;
