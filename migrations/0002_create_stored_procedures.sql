-- Migration 0002: Create stored procedures for PostgreSQL provider
-- This migration creates schema-qualified stored procedures to replace inline SQL queries
-- Note: This migration runs with SET LOCAL search_path TO {schema_name}, so procedures
-- will be created in the target schema automatically. However, procedures need to use
-- schema-qualified table names to work correctly when called from different contexts.

-- Get the current schema name (set by migration runner)
DO $$
DECLARE
    v_schema_name TEXT := current_schema();
BEGIN
    -- ============================================================================
    -- Schema Management Procedures
    -- ============================================================================

    -- Procedure: cleanup_schema
    -- Drops all tables AND stored procedures in the schema (for testing only)
    -- SAFETY: Never drops the "public" schema itself, only objects within it
    -- NOTE: Function drops are essential for public schema cleanup since
    -- DROP SCHEMA CASCADE only runs for non-public schemas
    EXECUTE format('DROP FUNCTION IF EXISTS %I.cleanup_schema()', v_schema_name);

    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.cleanup_schema()
        RETURNS VOID AS $cleanup$
        BEGIN
            -- Drop tables first
            DROP TABLE IF EXISTS %I.instances CASCADE;
            DROP TABLE IF EXISTS %I.executions CASCADE;
            DROP TABLE IF EXISTS %I.history CASCADE;
            DROP TABLE IF EXISTS %I.orchestrator_queue CASCADE;
            DROP TABLE IF EXISTS %I.worker_queue CASCADE;
            DROP TABLE IF EXISTS %I.instance_locks CASCADE;
            DROP TABLE IF EXISTS %I._duroxide_migrations CASCADE;
            
            -- Drop all stored procedures (required for public schema cleanup)
            DROP FUNCTION IF EXISTS %I.cleanup_schema();
            DROP FUNCTION IF EXISTS %I.list_instances();
            DROP FUNCTION IF EXISTS %I.list_executions(TEXT);
            DROP FUNCTION IF EXISTS %I.latest_execution_id(TEXT);
            DROP FUNCTION IF EXISTS %I.list_instances_by_status(TEXT);
            DROP FUNCTION IF EXISTS %I.get_instance_info(TEXT);
            DROP FUNCTION IF EXISTS %I.get_execution_info(TEXT, BIGINT);
            DROP FUNCTION IF EXISTS %I.get_system_metrics();
            DROP FUNCTION IF EXISTS %I.get_queue_depths(BIGINT);
            DROP FUNCTION IF EXISTS %I.enqueue_worker_work(TEXT);
            DROP FUNCTION IF EXISTS %I.ack_worker(TEXT, TEXT, TEXT);
            DROP FUNCTION IF EXISTS %I.renew_work_item_lock(TEXT, BIGINT, BIGINT);
            DROP FUNCTION IF EXISTS %I.fetch_work_item(BIGINT, BIGINT);
            DROP FUNCTION IF EXISTS %I.abandon_work_item(TEXT, BIGINT, BOOLEAN);
            DROP FUNCTION IF EXISTS %I.enqueue_orchestrator_work(TEXT, TEXT, TIMESTAMPTZ, TEXT, TEXT, BIGINT);
            DROP FUNCTION IF EXISTS %I.fetch_orchestration_item(BIGINT, BIGINT);
            DROP FUNCTION IF EXISTS %I.ack_orchestration_item(TEXT, BIGINT, JSONB, JSONB, JSONB, JSONB);
            DROP FUNCTION IF EXISTS %I.abandon_orchestration_item(TEXT, BIGINT);
            DROP FUNCTION IF EXISTS %I.abandon_orchestration_item(TEXT, BIGINT, BOOLEAN);
            DROP FUNCTION IF EXISTS %I.renew_orchestration_item_lock(TEXT, BIGINT, BIGINT);
            DROP FUNCTION IF EXISTS %I.fetch_history(TEXT);
            DROP FUNCTION IF EXISTS %I.fetch_history_with_execution(TEXT, BIGINT);
            DROP FUNCTION IF EXISTS %I.append_history(TEXT, BIGINT, JSONB);
        END;
        $cleanup$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name, v_schema_name, v_schema_name, 
       v_schema_name, v_schema_name, v_schema_name, v_schema_name,
       v_schema_name, v_schema_name, v_schema_name, v_schema_name,
       v_schema_name, v_schema_name, v_schema_name, v_schema_name,
       v_schema_name, v_schema_name, v_schema_name, v_schema_name,
       v_schema_name, v_schema_name, v_schema_name, v_schema_name,
       v_schema_name, v_schema_name, v_schema_name, v_schema_name,
       v_schema_name, v_schema_name, v_schema_name);

    -- ============================================================================
    -- Simple Query Procedures (Phase 3)
    -- ============================================================================

    -- Procedure: list_instances
    -- Returns all instance IDs ordered by creation date (newest first)
    EXECUTE format('DROP FUNCTION IF EXISTS %I.list_instances()', v_schema_name);

    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.list_instances()
        RETURNS TABLE(instance_id TEXT) AS $list_inst$
        BEGIN
            RETURN QUERY
            SELECT i.instance_id
            FROM %I.instances i
            ORDER BY i.created_at DESC;
        END;
        $list_inst$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name);

    -- Procedure: list_executions
    -- Returns all execution IDs for a given instance, ordered by execution_id
    EXECUTE format('DROP FUNCTION IF EXISTS %I.list_executions(TEXT)', v_schema_name);

    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.list_executions(p_instance_id TEXT)
        RETURNS TABLE(execution_id BIGINT) AS $list_exec$
        BEGIN
            RETURN QUERY
            SELECT e.execution_id
            FROM %I.executions e
            WHERE e.instance_id = p_instance_id
            ORDER BY e.execution_id;
        END;
        $list_exec$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name);

    -- Procedure: latest_execution_id
    -- Returns the current execution ID for a given instance
    EXECUTE format('DROP FUNCTION IF EXISTS %I.latest_execution_id(TEXT)', v_schema_name);

    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.latest_execution_id(p_instance_id TEXT)
        RETURNS BIGINT AS $latest_exec$
        DECLARE
            v_execution_id BIGINT;
        BEGIN
            SELECT i.current_execution_id INTO v_execution_id
            FROM %I.instances i
            WHERE i.instance_id = p_instance_id;
            
            RETURN v_execution_id;
        END;
        $latest_exec$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name);

    -- Procedure: list_instances_by_status
    -- Returns instance IDs filtered by execution status
    EXECUTE format('DROP FUNCTION IF EXISTS %I.list_instances_by_status(TEXT)', v_schema_name);

    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.list_instances_by_status(p_status TEXT)
        RETURNS TABLE(instance_id TEXT) AS $list_by_status$
        BEGIN
            RETURN QUERY
            SELECT i.instance_id
            FROM %I.instances i
            JOIN %I.executions e ON i.instance_id = e.instance_id 
              AND i.current_execution_id = e.execution_id
            WHERE e.status = p_status
            ORDER BY i.created_at DESC;
        END;
        $list_by_status$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name, v_schema_name);

    -- ============================================================================
    -- JOIN and Aggregate Query Procedures (Phase 4)
    -- ============================================================================

    -- Procedure: get_instance_info
    -- Returns comprehensive instance information with execution status
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
            output TEXT
        ) AS $get_inst_info$
        BEGIN
            RETURN QUERY
            SELECT i.instance_id, i.orchestration_name, 
                   COALESCE(i.orchestration_version, ''unknown'') as orchestration_version,
                   i.current_execution_id, i.created_at, i.updated_at,
                   e.status, e.output
            FROM %I.instances i
            LEFT JOIN %I.executions e ON i.instance_id = e.instance_id 
              AND i.current_execution_id = e.execution_id
            WHERE i.instance_id = p_instance_id;
        END;
        $get_inst_info$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name, v_schema_name);

    -- Procedure: get_execution_info
    -- Returns execution information with event count
    EXECUTE format('DROP FUNCTION IF EXISTS %I.get_execution_info(TEXT, BIGINT)', v_schema_name);

    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.get_execution_info(
            p_instance_id TEXT,
            p_execution_id BIGINT
        )
        RETURNS TABLE(
            execution_id BIGINT,
            status TEXT,
            output TEXT,
            started_at TIMESTAMPTZ,
            completed_at TIMESTAMPTZ,
            event_count BIGINT
        ) AS $get_exec_info$
        BEGIN
            RETURN QUERY
            SELECT e.execution_id, e.status, e.output, 
                   e.started_at, e.completed_at,
                   COALESCE(COUNT(h.event_id), 0)::BIGINT as event_count
            FROM %I.executions e
            LEFT JOIN %I.history h ON e.instance_id = h.instance_id 
              AND e.execution_id = h.execution_id
            WHERE e.instance_id = p_instance_id AND e.execution_id = p_execution_id
            GROUP BY e.execution_id, e.status, e.output, e.started_at, e.completed_at;
        END;
        $get_exec_info$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name, v_schema_name);

    -- Procedure: get_system_metrics
    -- Returns system-wide statistics
    EXECUTE format('DROP FUNCTION IF EXISTS %I.get_system_metrics()', v_schema_name);

    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.get_system_metrics()
        RETURNS TABLE(
            total_instances BIGINT,
            total_executions BIGINT,
            running_instances BIGINT,
            completed_instances BIGINT,
            failed_instances BIGINT,
            total_events BIGINT
        ) AS $get_metrics$
        BEGIN
            RETURN QUERY
            SELECT 
                (SELECT COUNT(*)::BIGINT FROM %I.instances) as total_instances,
                (SELECT COUNT(*)::BIGINT FROM %I.executions) as total_executions,
                (SELECT COUNT(DISTINCT i.instance_id)::BIGINT
                 FROM %I.instances i
                 JOIN %I.executions e ON i.instance_id = e.instance_id 
                   AND i.current_execution_id = e.execution_id
                 WHERE e.status = ''Running'') as running_instances,
                (SELECT COUNT(DISTINCT i.instance_id)::BIGINT
                 FROM %I.instances i
                 JOIN %I.executions e ON i.instance_id = e.instance_id 
                   AND i.current_execution_id = e.execution_id
                 WHERE e.status = ''Completed'') as completed_instances,
                (SELECT COUNT(DISTINCT i.instance_id)::BIGINT
                 FROM %I.instances i
                 JOIN %I.executions e ON i.instance_id = e.instance_id 
                   AND i.current_execution_id = e.execution_id
                 WHERE e.status = ''Failed'') as failed_instances,
                (SELECT COUNT(*)::BIGINT FROM %I.history) as total_events;
        END;
        $get_metrics$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, 
       v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name);

    -- Procedure: get_queue_depths
    -- Returns current queue depths (available items)
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

    -- ============================================================================
    -- Queue Operation Procedures (Phase 5)
    -- ============================================================================

    -- Procedure: enqueue_worker_work
    -- Inserts a work item into the worker queue
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

    -- Procedure: ack_worker
    -- Atomically deletes worker queue item and optionally enqueues completion to orchestrator queue
    -- If p_completion_json is NULL, just deletes without enqueueing (for cancelled activities)
    EXECUTE format('DROP FUNCTION IF EXISTS %I.ack_worker(TEXT, TEXT, TEXT)', v_schema_name);

    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.ack_worker(
            p_lock_token TEXT,
            p_instance_id TEXT DEFAULT NULL,
            p_completion_json TEXT DEFAULT NULL
        )
        RETURNS VOID AS $ack_worker$
        DECLARE
            v_rows_affected INTEGER;
        BEGIN
            -- Delete the worker queue item
            DELETE FROM %I.worker_queue WHERE lock_token = p_lock_token;
            GET DIAGNOSTICS v_rows_affected = ROW_COUNT;

            IF v_rows_affected = 0 THEN
                RAISE EXCEPTION ''Worker queue item not found or already processed'';
            END IF;

            -- Only enqueue completion if provided (NULL means cancelled activity)
            IF p_completion_json IS NOT NULL THEN
                INSERT INTO %I.orchestrator_queue (instance_id, work_item, visible_at, created_at)
                VALUES (p_instance_id, p_completion_json, NOW(), NOW());
            END IF;
        END;
        $ack_worker$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name, v_schema_name);

    -- Procedure: renew_work_item_lock
    -- Renews (extends) the lock timeout for a worker queue item
    -- Note: DROP first because migration 0007 changes the return type
    EXECUTE format('DROP FUNCTION IF EXISTS %I.renew_work_item_lock(TEXT, BIGINT, BIGINT)', v_schema_name);

    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.renew_work_item_lock(
            p_lock_token TEXT,
            p_now_ms BIGINT,
            p_extend_secs BIGINT
        )
        RETURNS VOID AS $renew_lock$
        DECLARE
            v_locked_until BIGINT;
            v_rows_affected INTEGER;
        BEGIN
            -- Calculate new locked_until timestamp
            v_locked_until := p_now_ms + (p_extend_secs * 1000);
            
            -- Update lock timeout only if lock is still valid
            -- Use p_now_ms (from application) for consistent time reference
            UPDATE %I.worker_queue
            SET locked_until = v_locked_until
            WHERE lock_token = p_lock_token
              AND locked_until > p_now_ms;
            
            GET DIAGNOSTICS v_rows_affected = ROW_COUNT;
            
            IF v_rows_affected = 0 THEN
                RAISE EXCEPTION ''Lock token invalid, expired, or already acked'';
            END IF;
        END;
        $renew_lock$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name);

    -- Procedure: fetch_work_item
    -- Fetches and locks a worker queue item in a single roundtrip
    -- Returns attempt_count for poison message detection (duroxide 0.1.2)
    -- Updated in duroxide 0.1.5: checks visible_at for delayed visibility
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

    -- Procedure: abandon_work_item
    -- Releases work item lock without completing, with optional delay and attempt count handling
    -- Added in duroxide 0.1.2 for explicit work item abandonment
    -- Updated in duroxide 0.1.5: uses visible_at for delay instead of keeping locked_until
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

    -- Procedure: enqueue_orchestrator_work
    -- Enqueues work to orchestrator queue
    -- ⚠️ CRITICAL: DO NOT create instance here - instance creation happens via ack_orchestration_item metadata
    EXECUTE format('DROP FUNCTION IF EXISTS %I.enqueue_orchestrator_work(TEXT, TEXT, TIMESTAMPTZ, TEXT, TEXT, BIGINT)', v_schema_name);

    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.enqueue_orchestrator_work(
            p_instance_id TEXT,
            p_work_item TEXT,
            p_visible_at TIMESTAMPTZ,
            p_orchestration_name TEXT DEFAULT NULL,
            p_orchestration_version TEXT DEFAULT NULL,
            p_execution_id BIGINT DEFAULT NULL
        )
        RETURNS VOID AS $enq_orch$
        BEGIN
            -- ⚠️ CRITICAL: Parameters p_orchestration_name, p_orchestration_version, p_execution_id are ignored
            -- Instance creation happens ONLY via ack_orchestration_item metadata
            
            -- Insert into orchestrator queue
            INSERT INTO %I.orchestrator_queue (instance_id, work_item, visible_at, created_at)
            VALUES (p_instance_id, p_work_item, p_visible_at, NOW());
        END;
        $enq_orch$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name);

    -- Procedure: fetch_orchestration_item
    -- Fetches and locks an orchestration item in a single database roundtrip
    -- Uses out_ prefix for return columns to avoid ambiguity with table columns
    -- Returns attempt_count for poison message detection (duroxide 0.1.2)
    -- Drop first since we changed return type signature
    EXECUTE format('DROP FUNCTION IF EXISTS %I.fetch_orchestration_item(BIGINT, BIGINT)', v_schema_name);
    
    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.fetch_orchestration_item(
            p_now_ms BIGINT,
            p_lock_timeout_ms BIGINT
        )
        RETURNS TABLE(
            out_instance_id TEXT,
            out_orchestration_name TEXT,
            out_orchestration_version TEXT,
            out_execution_id BIGINT,
            out_history JSONB,
            out_messages JSONB,
            out_lock_token TEXT,
            out_attempt_count INTEGER
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
            v_lock_acquired INTEGER;
            v_max_attempt_count INTEGER;
        BEGIN
            -- Two-phase locking to prevent deadlocks while preserving parallelism.
            -- 
            -- Background: INSERT ... ON CONFLICT on instance_locks can deadlock at the
            -- B-tree index level when concurrent transactions modify different rows.
            -- A global advisory lock would prevent deadlocks but serialize ALL fetches.
            --
            -- Solution: Instance-level advisory locks acquired BEFORE any row locks.
            -- Phase 1: Peek (no lock) to find candidate instance
            -- Phase 2: Acquire advisory lock on that specific instance  
            -- Phase 3: Re-verify with FOR UPDATE (another tx may have taken it)
            --
            -- This allows parallel processing of different instances while preventing
            -- deadlocks on the same instance.

            -- Phase 1: Find a candidate instance (no FOR UPDATE yet)
            SELECT q.instance_id INTO v_instance_id
            FROM %I.orchestrator_queue q
            WHERE q.visible_at <= TO_TIMESTAMP(p_now_ms / 1000.0)
              AND NOT EXISTS (
                SELECT 1 FROM %I.instance_locks il
                WHERE il.instance_id = q.instance_id AND il.locked_until > p_now_ms
              )
            ORDER BY q.visible_at, q.id
            LIMIT 1;

            IF NOT FOUND THEN
                RETURN;
            END IF;

            -- Phase 2: Acquire instance-level advisory lock BEFORE any row locks
            -- This prevents deadlocks by ensuring all operations on same instance
            -- are serialized, while different instances can proceed in parallel.
            PERFORM pg_advisory_xact_lock(hashtext(v_instance_id));

            -- Phase 3: Re-verify the instance is still available with FOR UPDATE
            -- Another transaction may have taken it between Phase 1 and Phase 2
            SELECT q.instance_id INTO v_instance_id
            FROM %I.orchestrator_queue q
            WHERE q.instance_id = v_instance_id
              AND q.visible_at <= TO_TIMESTAMP(p_now_ms / 1000.0)
              AND NOT EXISTS (
                SELECT 1 FROM %I.instance_locks il
                WHERE il.instance_id = q.instance_id AND il.locked_until > p_now_ms
              )
            FOR UPDATE OF q SKIP LOCKED;

            IF NOT FOUND THEN
                -- Instance was taken by another transaction, return empty
                RETURN;
            END IF;

            -- Step 2: Generate lock token and acquire instance lock
            v_lock_token := ''lock_'' || gen_random_uuid()::TEXT;
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
                RETURN;
            END IF;

            -- Step 3: Mark all visible messages for this instance with our lock and increment attempt_count
            UPDATE %I.orchestrator_queue q
            SET lock_token = v_lock_token,
                locked_until = v_locked_until,
                attempt_count = q.attempt_count + 1
            WHERE q.instance_id = v_instance_id
              AND q.visible_at <= TO_TIMESTAMP(p_now_ms / 1000.0)
              AND (q.lock_token IS NULL OR q.locked_until <= p_now_ms);

            -- Step 4: Fetch all locked messages as JSONB array and get max attempt_count
            SELECT COALESCE(JSONB_AGG(q.work_item::JSONB ORDER BY q.id), ''[]''::JSONB),
                   COALESCE(MAX(q.attempt_count), 1)
            INTO v_messages, v_max_attempt_count
            FROM %I.orchestrator_queue q
            WHERE q.lock_token = v_lock_token;

            -- Step 5: Load instance metadata (if exists)
            SELECT i.orchestration_name, i.orchestration_version, i.current_execution_id
            INTO v_orchestration_name, v_orchestration_version, v_current_execution_id
            FROM %I.instances i
            WHERE i.instance_id = v_instance_id;

            -- Step 6: Load history or implement fallback
            IF FOUND THEN
                -- Instance exists, load its history for current execution
                SELECT COALESCE(JSONB_AGG(h.event_data::JSONB ORDER BY h.event_id), ''[]''::JSONB)
                INTO v_history
                FROM %I.history h
                WHERE h.instance_id = v_instance_id AND h.execution_id = v_current_execution_id;
                
                v_orchestration_version := COALESCE(v_orchestration_version, ''unknown'');
            ELSE
                -- Fallback: instance doesn''t exist, try to extract from history
                SELECT COALESCE(JSONB_AGG(h.event_data::JSONB ORDER BY h.execution_id, h.event_id), ''[]''::JSONB)
                INTO v_history
                FROM %I.history h
                WHERE h.instance_id = v_instance_id;

                -- Try to extract metadata from first OrchestrationStarted event in history
                IF JSONB_ARRAY_LENGTH(v_history) > 0 AND v_history->0 ? ''OrchestrationStarted'' THEN
                    v_orchestration_name := v_history->0->''OrchestrationStarted''->>''name'';
                    v_orchestration_version := v_history->0->''OrchestrationStarted''->>''version'';
                    v_current_execution_id := 1;
                ELSIF JSONB_ARRAY_LENGTH(v_messages) > 0 AND v_messages->0 ? ''StartOrchestration'' THEN
                    v_orchestration_name := v_messages->0->''StartOrchestration''->>''orchestration'';
                    v_orchestration_version := COALESCE(v_messages->0->''StartOrchestration''->>''version'', ''unknown'');
                    v_current_execution_id := COALESCE((v_messages->0->''StartOrchestration''->>''execution_id'')::BIGINT, 1);
                ELSIF JSONB_ARRAY_LENGTH(v_messages) > 0 AND v_messages->0 ? ''ContinueAsNew'' THEN
                    v_orchestration_name := v_messages->0->''ContinueAsNew''->>''orchestration'';
                    v_orchestration_version := COALESCE(v_messages->0->''ContinueAsNew''->>''version'', ''unknown'');
                    v_current_execution_id := 1;
                ELSE
                    v_orchestration_name := ''Unknown'';
                    v_orchestration_version := ''unknown'';
                    v_current_execution_id := 1;
                END IF;
            END IF;

            -- Return single row with all data (no ambiguity with out_ prefix)
            RETURN QUERY SELECT
                v_instance_id,
                v_orchestration_name,
                v_orchestration_version,
                v_current_execution_id,
                v_history,
                v_messages,
                v_lock_token,
                v_max_attempt_count;
        END;
        $fetch_orch$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name, v_schema_name, v_schema_name, 
       v_schema_name, v_schema_name, v_schema_name, v_schema_name, 
       v_schema_name, v_schema_name, v_schema_name, v_schema_name);

    -- Procedure: ack_orchestration_item
    -- Acknowledges orchestration item in a single atomic operation
    -- Combines 8-9 queries into one roundtrip
    EXECUTE format('DROP FUNCTION IF EXISTS %I.ack_orchestration_item(TEXT, BIGINT, JSONB, JSONB, JSONB, JSONB)', v_schema_name);

    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.ack_orchestration_item(
            p_lock_token TEXT,
            p_execution_id BIGINT,
            p_history_delta JSONB,
            p_worker_items JSONB,
            p_orchestrator_items JSONB,
            p_metadata JSONB
        )
        RETURNS VOID AS $ack_orch$
        DECLARE
            v_instance_id TEXT;
            v_now_ms BIGINT;
            v_orchestration_name TEXT;
            v_orchestration_version TEXT;
            v_status TEXT;
            v_output TEXT;
            v_completed_at TIMESTAMPTZ;
            v_elem JSONB;
            v_visible_at TIMESTAMPTZ;
            v_fire_at_ms BIGINT;
            v_item_instance_id TEXT;
        BEGIN
            v_now_ms := (EXTRACT(EPOCH FROM NOW()) * 1000)::BIGINT;

            -- Step 1: Validate lock token
            SELECT il.instance_id INTO v_instance_id
            FROM %I.instance_locks il
            WHERE il.lock_token = p_lock_token AND il.locked_until > v_now_ms;

            IF NOT FOUND THEN
                RAISE EXCEPTION ''Invalid lock token'';
            END IF;

            -- Step 2: Extract metadata from JSONB
            v_orchestration_name := p_metadata->>''orchestration_name'';
            v_orchestration_version := p_metadata->>''orchestration_version'';
            v_status := p_metadata->>''status'';
            v_output := p_metadata->>''output'';

            -- Step 3: Create or update instance metadata
            IF v_orchestration_name IS NOT NULL AND v_orchestration_version IS NOT NULL THEN
                INSERT INTO %I.instances (instance_id, orchestration_name, orchestration_version, current_execution_id)
                VALUES (v_instance_id, v_orchestration_name, v_orchestration_version, p_execution_id)
                ON CONFLICT (instance_id) DO NOTHING;

                UPDATE %I.instances i
                SET orchestration_name = v_orchestration_name,
                    orchestration_version = v_orchestration_version
                WHERE i.instance_id = v_instance_id;
            END IF;

            -- Step 4: Create execution record (idempotent)
            INSERT INTO %I.executions (instance_id, execution_id, status, started_at)
            VALUES (v_instance_id, p_execution_id, ''Running'', NOW())
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
                    WHEN v_status IN (''Completed'', ''Failed'') THEN NOW() 
                    ELSE NULL 
                END;
                
                UPDATE %I.executions e
                SET status = v_status, output = v_output, completed_at = v_completed_at
                WHERE e.instance_id = v_instance_id AND e.execution_id = p_execution_id;
            END IF;

            -- Step 8: Enqueue worker items (batch)
            IF p_worker_items IS NOT NULL AND JSONB_ARRAY_LENGTH(p_worker_items) > 0 THEN
                INSERT INTO %I.worker_queue (work_item, created_at)
                SELECT elem::TEXT, NOW()
                FROM JSONB_ARRAY_ELEMENTS(p_worker_items) AS elem;
            END IF;

            -- Step 9: Enqueue orchestrator items (batch with instance extraction and visible_at handling)
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
                        v_visible_at := NOW();
                    END IF;

                    INSERT INTO %I.orchestrator_queue (instance_id, work_item, visible_at, created_at)
                    VALUES (v_item_instance_id, v_elem::TEXT, v_visible_at, NOW());
                    
                    v_fire_at_ms := NULL; -- Reset for next iteration
                END LOOP;
            END IF;

            -- Step 10: Delete locked messages
            DELETE FROM %I.orchestrator_queue q WHERE q.lock_token = p_lock_token;

            -- Step 11: Remove instance lock
            DELETE FROM %I.instance_locks il
            WHERE il.instance_id = v_instance_id AND il.lock_token = p_lock_token;
        END;
        $ack_orch$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name, v_schema_name, v_schema_name,
       v_schema_name, v_schema_name, v_schema_name, v_schema_name,
       v_schema_name, v_schema_name, v_schema_name, v_schema_name);

    -- Procedure: abandon_orchestration_item
    -- Updated in duroxide 0.1.2 to add ignore_attempt parameter for poison message handling
    -- Fixed in duroxide 0.1.3: don't update visible_at when no delay (for consistent timing with fetch)
    EXECUTE format('DROP FUNCTION IF EXISTS %I.abandon_orchestration_item(TEXT, BIGINT)', v_schema_name);
    EXECUTE format('DROP FUNCTION IF EXISTS %I.abandon_orchestration_item(TEXT, BIGINT, BOOLEAN)', v_schema_name);

    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.abandon_orchestration_item(
            p_lock_token TEXT,
            p_delay_ms BIGINT DEFAULT NULL,
            p_ignore_attempt BOOLEAN DEFAULT FALSE
        )
        RETURNS TEXT AS $abandon_orch$
        DECLARE
            v_instance_id TEXT;
            v_visible_at TIMESTAMPTZ;
        BEGIN
            SELECT il.instance_id INTO v_instance_id
            FROM %I.instance_locks il
            WHERE il.lock_token = p_lock_token;

            IF NOT FOUND THEN
                RAISE EXCEPTION ''Invalid lock token'';
            END IF;

            IF p_delay_ms IS NOT NULL AND p_delay_ms > 0 THEN
                -- Delay provided: set visible_at to future time
                v_visible_at := NOW() + (p_delay_ms::DOUBLE PRECISION / 1000.0) * INTERVAL ''1 second'';
                
                IF p_ignore_attempt THEN
                    UPDATE %I.orchestrator_queue
                    SET lock_token = NULL,
                        locked_until = NULL,
                        visible_at = v_visible_at,
                        attempt_count = GREATEST(0, attempt_count - 1)
                    WHERE lock_token = p_lock_token;
                ELSE
                    UPDATE %I.orchestrator_queue
                    SET lock_token = NULL,
                        locked_until = NULL,
                        visible_at = v_visible_at
                    WHERE lock_token = p_lock_token;
                END IF;
            ELSE
                -- No delay: just clear lock, don''t update visible_at
                IF p_ignore_attempt THEN
                    UPDATE %I.orchestrator_queue
                    SET lock_token = NULL,
                        locked_until = NULL,
                        attempt_count = GREATEST(0, attempt_count - 1)
                    WHERE lock_token = p_lock_token;
                ELSE
                    UPDATE %I.orchestrator_queue
                    SET lock_token = NULL,
                        locked_until = NULL
                    WHERE lock_token = p_lock_token;
                END IF;
            END IF;

            DELETE FROM %I.instance_locks
            WHERE lock_token = p_lock_token;

            RETURN v_instance_id;
        END;
        $abandon_orch$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name);

    -- Procedure: renew_orchestration_item_lock
    -- Extends lock timeout for in-flight orchestration turns
    -- Added in duroxide 0.1.2 for long-running orchestration support
    EXECUTE format('DROP FUNCTION IF EXISTS %I.renew_orchestration_item_lock(TEXT, BIGINT, BIGINT)', v_schema_name);

    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.renew_orchestration_item_lock(
            p_lock_token TEXT,
            p_now_ms BIGINT,
            p_extend_secs BIGINT
        )
        RETURNS VOID AS $renew_orch_lock$
        DECLARE
            v_locked_until BIGINT;
            v_rows_affected INTEGER;
        BEGIN
            -- Calculate new locked_until timestamp
            v_locked_until := p_now_ms + (p_extend_secs * 1000);
            
            -- Update instance lock timeout only if lock is still valid
            UPDATE %I.instance_locks
            SET locked_until = v_locked_until
            WHERE lock_token = p_lock_token
              AND locked_until > p_now_ms;
            
            GET DIAGNOSTICS v_rows_affected = ROW_COUNT;
            
            IF v_rows_affected = 0 THEN
                RAISE EXCEPTION ''Lock token invalid, expired, or already released'';
            END IF;

            -- Also update the locked_until on orchestrator_queue messages
            UPDATE %I.orchestrator_queue
            SET locked_until = v_locked_until
            WHERE lock_token = p_lock_token;
        END;
        $renew_orch_lock$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name, v_schema_name);

    -- Procedure: fetch_history (latest execution)
    EXECUTE format('DROP FUNCTION IF EXISTS %I.fetch_history(TEXT)', v_schema_name);

    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.fetch_history(
            p_instance_id TEXT
        )
        RETURNS TABLE(out_event_data TEXT) AS $fetch_history$
        DECLARE
            v_execution_id BIGINT;
        BEGIN
            SELECT COALESCE(MAX(execution_id), 1)
            INTO v_execution_id
            FROM %I.executions
            WHERE instance_id = p_instance_id;

            RETURN QUERY
            SELECT h.event_data
            FROM %I.history h
            WHERE h.instance_id = p_instance_id
              AND h.execution_id = v_execution_id
            ORDER BY h.event_id;
        END;
        $fetch_history$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name, v_schema_name);

    -- Procedure: fetch_history_with_execution
    EXECUTE format('DROP FUNCTION IF EXISTS %I.fetch_history_with_execution(TEXT, BIGINT)', v_schema_name);

    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.fetch_history_with_execution(
            p_instance_id TEXT,
            p_execution_id BIGINT
        )
        RETURNS TABLE(out_event_data TEXT) AS $fetch_history_exec$
        BEGIN
            RETURN QUERY
            SELECT h.event_data
            FROM %I.history h
            WHERE h.instance_id = p_instance_id
              AND h.execution_id = p_execution_id
            ORDER BY h.event_id;
        END;
        $fetch_history_exec$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name, v_schema_name);

    -- Procedure: append_history
    EXECUTE format('DROP FUNCTION IF EXISTS %I.append_history(TEXT, BIGINT, JSONB)', v_schema_name);

    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.append_history(
            p_instance_id TEXT,
            p_execution_id BIGINT,
            p_events JSONB
        )
        RETURNS VOID AS $append_hist$
        BEGIN
            IF p_events IS NULL OR JSONB_ARRAY_LENGTH(p_events) = 0 THEN
                RETURN;
            END IF;

            IF EXISTS (
                SELECT 1
                FROM JSONB_ARRAY_ELEMENTS(p_events) elem
                WHERE COALESCE((elem->>''event_id'')::BIGINT, 0) <= 0
            ) THEN
                RAISE EXCEPTION ''Invalid event_id in append_history'';
            END IF;

            INSERT INTO %I.history (instance_id, execution_id, event_id, event_type, event_data)
            SELECT
                p_instance_id,
                p_execution_id,
                (elem->>''event_id'')::BIGINT,
                elem->>''event_type'',
                elem->>''event_data''
            FROM JSONB_ARRAY_ELEMENTS(p_events) AS elem
            ON CONFLICT (instance_id, execution_id, event_id) DO NOTHING;
        END;
        $append_hist$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name, v_schema_name, v_schema_name);
END $$;



