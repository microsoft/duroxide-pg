-- Migration: 0007_add_execution_state_support.sql
-- Description: Updates fetch_work_item and renew_work_item_lock to return ExecutionState
-- Required for duroxide 0.1.7 activity cancellation support
-- ExecutionState values: "Running", "Terminal:<status>", "Missing"

DO $$
DECLARE
    v_schema_name TEXT := current_schema();
BEGIN
    -- ============================================================================
    -- Update fetch_work_item to return ExecutionState
    -- ExecutionState indicates the parent orchestration's state:
    -- - "Running": Orchestration is active
    -- - "Terminal:<status>": Orchestration completed/failed/continued
    -- - "Missing": Instance or execution not found
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
            out_attempt_count INTEGER,
            out_execution_state TEXT
        ) AS $fetch_worker$
        DECLARE
            v_id BIGINT;
            v_instance_id TEXT;
            v_current_execution_id BIGINT;
            v_status TEXT;
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

            -- Extract instance_id and execution_id from the work_item JSON
            -- WorkItem is a tagged enum: {"ActivityExecute": {...}}
            -- Note: Only ActivityExecute goes to worker queue (SubOrchCompleted/Failed go to orchestrator queue)
            v_instance_id := out_work_item::jsonb->''ActivityExecute''->>''instance'';
            v_current_execution_id := (out_work_item::jsonb->''ActivityExecute''->>''execution_id'')::BIGINT;

            -- Look up the execution state
            IF v_instance_id IS NULL OR v_current_execution_id IS NULL THEN
                out_execution_state := ''Missing'';
            ELSE
                -- Get execution status directly from executions table
                SELECT e.status INTO v_status
                FROM %I.executions e
                WHERE e.instance_id = v_instance_id
                  AND e.execution_id = v_current_execution_id;

                IF v_status IS NULL THEN
                    out_execution_state := ''Missing'';
                ELSIF v_status = ''Running'' THEN
                    out_execution_state := ''Running'';
                ELSE
                    out_execution_state := ''Terminal:'' || v_status;
                END IF;
            END IF;

            RETURN NEXT;
        END;
        $fetch_worker$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name);

    -- ============================================================================
    -- Update renew_work_item_lock to return ExecutionState
    -- Allows runtime to detect state changes during long-running activities
    -- ============================================================================
    EXECUTE format('DROP FUNCTION IF EXISTS %I.renew_work_item_lock(TEXT, BIGINT, BIGINT)', v_schema_name);

    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.renew_work_item_lock(
            p_lock_token TEXT,
            p_now_ms BIGINT,
            p_extend_secs BIGINT
        )
        RETURNS TEXT AS $renew_lock$
        DECLARE
            v_locked_until BIGINT;
            v_work_item TEXT;
            v_instance_id TEXT;
            v_current_execution_id BIGINT;
            v_status TEXT;
            v_execution_state TEXT;
        BEGIN
            -- Step 1: Verify lock is valid and get work_item (no update yet)
            SELECT work_item INTO v_work_item
            FROM %I.worker_queue
            WHERE lock_token = p_lock_token
              AND locked_until > p_now_ms;
            
            IF v_work_item IS NULL THEN
                RAISE EXCEPTION ''Lock token invalid, expired, or already acked'';
            END IF;

            -- Step 2: Extract instance_id and execution_id from the work_item JSON
            -- WorkItem is a tagged enum: {"ActivityExecute": {...}}
            -- Note: Only ActivityExecute goes to worker queue (SubOrchCompleted/Failed go to orchestrator queue)
            v_instance_id := v_work_item::jsonb->''ActivityExecute''->>''instance'';
            v_current_execution_id := (v_work_item::jsonb->''ActivityExecute''->>''execution_id'')::BIGINT;

            -- Step 3: Determine execution state
            IF v_instance_id IS NULL OR v_current_execution_id IS NULL THEN
                v_execution_state := ''Missing'';
            ELSE
                -- Get execution status directly from executions table
                SELECT e.status INTO v_status
                FROM %I.executions e
                WHERE e.instance_id = v_instance_id
                  AND e.execution_id = v_current_execution_id;

                IF v_status IS NULL THEN
                    v_execution_state := ''Missing'';
                ELSIF v_status = ''Running'' THEN
                    v_execution_state := ''Running'';
                ELSE
                    v_execution_state := ''Terminal:'' || v_status;
                END IF;
            END IF;

            -- Step 4: Only extend lock if orchestration is Running
            -- Per duroxide contract: lock is NOT extended for Terminal/Missing states
            IF v_execution_state = ''Running'' THEN
                v_locked_until := p_now_ms + (p_extend_secs * 1000);
                
                UPDATE %I.worker_queue
                SET locked_until = v_locked_until
                WHERE lock_token = p_lock_token;
            END IF;

            RETURN v_execution_state;
        END;
        $renew_lock$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name, v_schema_name, v_schema_name);

END $$;
