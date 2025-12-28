-- Migration: 0006_use_rust_timestamps.sql
-- Description: Updates all stored procedures to use Rust-provided timestamps (p_now_ms)
-- instead of database NOW(). This ensures consistent time handling between the application
-- and database, avoiding clock skew and precision mismatch issues.

DO $$
DECLARE
    v_schema_name TEXT := current_schema();
BEGIN
    -- ============================================================================
    -- Update enqueue_worker_work to accept p_now_ms
    -- ============================================================================
    EXECUTE format('DROP FUNCTION IF EXISTS %I.enqueue_worker_work(TEXT)', v_schema_name);

    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.enqueue_worker_work(
            p_work_item TEXT,
            p_now_ms BIGINT
        )
        RETURNS VOID AS $enq_worker$
        DECLARE
            v_now_ts TIMESTAMPTZ;
        BEGIN
            v_now_ts := TO_TIMESTAMP(p_now_ms / 1000.0);
            INSERT INTO %I.worker_queue (work_item, visible_at, created_at)
            VALUES (p_work_item, v_now_ts, v_now_ts);
        END;
        $enq_worker$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name);

    -- ============================================================================
    -- Update abandon_work_item to accept p_now_ms
    -- ============================================================================
    EXECUTE format('DROP FUNCTION IF EXISTS %I.abandon_work_item(TEXT, BIGINT, BOOLEAN)', v_schema_name);

    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.abandon_work_item(
            p_lock_token TEXT,
            p_now_ms BIGINT,
            p_delay_ms BIGINT DEFAULT NULL,
            p_ignore_attempt BOOLEAN DEFAULT FALSE
        )
        RETURNS VOID AS $abandon_worker$
        DECLARE
            v_rows_affected INTEGER;
            v_visible_at TIMESTAMPTZ;
        BEGIN
            -- Calculate visible_at based on delay using Rust-provided time
            IF p_delay_ms IS NOT NULL AND p_delay_ms > 0 THEN
                v_visible_at := TO_TIMESTAMP((p_now_ms + p_delay_ms) / 1000.0);
            ELSE
                v_visible_at := TO_TIMESTAMP(p_now_ms / 1000.0);
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
    -- Update abandon_orchestration_item to accept p_now_ms
    -- ============================================================================
    EXECUTE format('DROP FUNCTION IF EXISTS %I.abandon_orchestration_item(TEXT, BIGINT, BOOLEAN)', v_schema_name);

    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.abandon_orchestration_item(
            p_lock_token TEXT,
            p_now_ms BIGINT,
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

            -- Calculate visible_at based on delay using Rust-provided time
            IF p_delay_ms IS NOT NULL AND p_delay_ms > 0 THEN
                v_visible_at := TO_TIMESTAMP((p_now_ms + p_delay_ms) / 1000.0);
            ELSE
                v_visible_at := TO_TIMESTAMP(p_now_ms / 1000.0);
            END IF;

            IF p_delay_ms IS NOT NULL AND p_delay_ms > 0 THEN
                -- Delay provided: set visible_at to future time
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

    -- ============================================================================
    -- Update enqueue_orchestration_work to accept p_now_ms
    -- ============================================================================
    EXECUTE format('DROP FUNCTION IF EXISTS %I.enqueue_orchestration_work(TEXT, TEXT)', v_schema_name);

    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.enqueue_orchestration_work(
            p_instance_id TEXT,
            p_work_item TEXT,
            p_now_ms BIGINT
        )
        RETURNS VOID AS $enq_orch$
        DECLARE
            v_now_ts TIMESTAMPTZ;
        BEGIN
            v_now_ts := TO_TIMESTAMP(p_now_ms / 1000.0);
            INSERT INTO %I.orchestrator_queue (instance_id, work_item, visible_at, created_at)
            VALUES (p_instance_id, p_work_item, v_now_ts, v_now_ts);
        END;
        $enq_orch$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name);

    -- ============================================================================
    -- Update enqueue_completion to accept p_now_ms
    -- ============================================================================
    EXECUTE format('DROP FUNCTION IF EXISTS %I.enqueue_completion(TEXT, TEXT)', v_schema_name);

    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.enqueue_completion(
            p_instance_id TEXT,
            p_completion_json TEXT,
            p_now_ms BIGINT
        )
        RETURNS VOID AS $enq_comp$
        DECLARE
            v_now_ts TIMESTAMPTZ;
        BEGIN
            v_now_ts := TO_TIMESTAMP(p_now_ms / 1000.0);
            INSERT INTO %I.orchestrator_queue (instance_id, work_item, visible_at, created_at)
            VALUES (p_instance_id, p_completion_json, v_now_ts, v_now_ts);
        END;
        $enq_comp$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name);

    -- ============================================================================
    -- Update enqueue_message to accept p_now_ms
    -- ============================================================================
    EXECUTE format('DROP FUNCTION IF EXISTS %I.enqueue_message(TEXT, TEXT, TIMESTAMPTZ)', v_schema_name);

    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.enqueue_message(
            p_instance_id TEXT,
            p_work_item TEXT,
            p_visible_at_ms BIGINT,
            p_now_ms BIGINT
        )
        RETURNS VOID AS $enq_msg$
        DECLARE
            v_visible_at TIMESTAMPTZ;
            v_now_ts TIMESTAMPTZ;
        BEGIN
            v_visible_at := TO_TIMESTAMP(p_visible_at_ms / 1000.0);
            v_now_ts := TO_TIMESTAMP(p_now_ms / 1000.0);
            INSERT INTO %I.orchestrator_queue (instance_id, work_item, visible_at, created_at)
            VALUES (p_instance_id, p_work_item, v_visible_at, v_now_ts);
        END;
        $enq_msg$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name);

    -- ============================================================================
    -- Update ack_orchestration to accept p_now_ms
    -- ============================================================================
    EXECUTE format('DROP FUNCTION IF EXISTS %I.ack_orchestration(TEXT, JSONB)', v_schema_name);

    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.ack_orchestration(
            p_lock_token TEXT,
            p_state_json JSONB,
            p_now_ms BIGINT
        )
        RETURNS VOID AS $ack_orch$
        DECLARE
            v_instance_id TEXT;
            v_execution_id BIGINT;
            v_status TEXT;
            v_now_ts TIMESTAMPTZ;
            elem JSONB;
            v_event JSONB;
            v_visible_at_ms BIGINT;
            v_visible_at TIMESTAMPTZ;
            v_item_instance_id TEXT;
            v_existing_name TEXT;
            v_existing_version TEXT;
        BEGIN
            v_now_ts := TO_TIMESTAMP(p_now_ms / 1000.0);

            SELECT il.instance_id, il.locked_at
            INTO v_instance_id, v_execution_id
            FROM %I.instance_locks il
            WHERE il.lock_token = p_lock_token;

            IF NOT FOUND THEN
                RAISE EXCEPTION ''Invalid lock token'';
            END IF;

            v_status := p_state_json ->> ''status'';

            SELECT i.orchestration_name, i.orchestration_version
            INTO v_existing_name, v_existing_version
            FROM %I.instances i
            WHERE i.instance_id = v_instance_id;

            IF FOUND THEN
                UPDATE %I.instances
                SET status = v_status,
                    current_execution_id = v_execution_id,
                    completed_at = CASE
                        WHEN v_status IN (''Completed'', ''Failed'') THEN v_now_ts
                        ELSE completed_at
                    END
                WHERE instance_id = v_instance_id;
            ELSE
                INSERT INTO %I.instances (instance_id, orchestration_name, orchestration_version, status, current_execution_id, created_at)
                VALUES (v_instance_id, p_state_json ->> ''name'', COALESCE(p_state_json ->> ''version'', ''unknown''), v_status, v_execution_id, v_now_ts)
                ON CONFLICT (instance_id) DO UPDATE
                SET status = EXCLUDED.status,
                    current_execution_id = EXCLUDED.current_execution_id;
            END IF;

            DELETE FROM %I.orchestrator_queue
            WHERE lock_token = p_lock_token;

            -- Insert history events
            FOR elem IN SELECT * FROM jsonb_array_elements(p_state_json -> ''history'')
            LOOP
                INSERT INTO %I.history (instance_id, execution_id, event_data, created_at)
                SELECT v_instance_id, v_execution_id, elem::TEXT, v_now_ts;
            END LOOP;

            -- Insert orchestrator messages
            FOR v_event IN SELECT * FROM jsonb_array_elements(p_state_json -> ''orchestrator_messages'')
            LOOP
                -- Extract visible_at if present, otherwise use now
                IF v_event ? ''visible_at'' THEN
                    v_visible_at_ms := (v_event ->> ''visible_at'')::BIGINT;
                    v_visible_at := TO_TIMESTAMP(v_visible_at_ms / 1000.0);
                ELSE
                    v_visible_at := v_now_ts;
                END IF;

                -- Extract target instance_id if present
                v_item_instance_id := v_event ->> ''instance_id'';
                IF v_item_instance_id IS NULL THEN
                    v_item_instance_id := v_instance_id;
                END IF;

                INSERT INTO %I.orchestrator_queue (instance_id, work_item, visible_at, created_at)
                VALUES (v_item_instance_id, v_event::TEXT, v_visible_at, v_now_ts);
            END LOOP;

            -- Insert worker messages
            FOR v_event IN SELECT * FROM jsonb_array_elements(p_state_json -> ''worker_messages'')
            LOOP
                INSERT INTO %I.worker_queue (work_item, visible_at, created_at)
                VALUES (v_event::TEXT, v_now_ts, v_now_ts);
            END LOOP;

            DELETE FROM %I.instance_locks
            WHERE lock_token = p_lock_token;
        END;
        $ack_orch$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name);

    -- ============================================================================
    -- Update ack_worker to accept p_now_ms and support nullable completion
    -- ============================================================================
    EXECUTE format('DROP FUNCTION IF EXISTS %I.ack_worker(TEXT, TEXT, TEXT)', v_schema_name);
    EXECUTE format('DROP FUNCTION IF EXISTS %I.ack_worker(TEXT, TEXT, TEXT, BIGINT)', v_schema_name);

    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.ack_worker(
            p_lock_token TEXT,
            p_instance_id TEXT DEFAULT NULL,
            p_completion_json TEXT DEFAULT NULL,
            p_now_ms BIGINT DEFAULT NULL
        )
        RETURNS VOID AS $ack_worker$
        DECLARE
            v_rows_affected INTEGER;
            v_now_ts TIMESTAMPTZ;
        BEGIN
            -- Delete the worker queue item
            DELETE FROM %I.worker_queue WHERE lock_token = p_lock_token;
            GET DIAGNOSTICS v_rows_affected = ROW_COUNT;

            IF v_rows_affected = 0 THEN
                RAISE EXCEPTION ''Worker queue item not found or already processed'';
            END IF;

            -- Only enqueue completion if provided (NULL means cancelled activity)
            IF p_completion_json IS NOT NULL THEN
                -- Validate required parameters for completion
                IF p_instance_id IS NULL THEN
                    RAISE EXCEPTION ''p_instance_id is required when p_completion_json is provided'';
                END IF;
                IF p_now_ms IS NULL THEN
                    RAISE EXCEPTION ''p_now_ms is required when p_completion_json is provided'';
                END IF;
                
                v_now_ts := TO_TIMESTAMP(p_now_ms / 1000.0);
                INSERT INTO %I.orchestrator_queue (instance_id, work_item, visible_at, created_at)
                VALUES (p_instance_id, p_completion_json, v_now_ts, v_now_ts);
            END IF;
        END;
        $ack_worker$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name, v_schema_name);

END;
$$;
