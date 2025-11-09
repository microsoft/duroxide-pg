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
    -- Drops all tables in the schema (for testing only)
    -- SAFETY: Never drops the "public" schema itself, only tables within it
    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.cleanup_schema()
        RETURNS VOID AS $cleanup$
        BEGIN
            DROP TABLE IF EXISTS %I.instances CASCADE;
            DROP TABLE IF EXISTS %I.executions CASCADE;
            DROP TABLE IF EXISTS %I.history CASCADE;
            DROP TABLE IF EXISTS %I.orchestrator_queue CASCADE;
            DROP TABLE IF EXISTS %I.worker_queue CASCADE;
            DROP TABLE IF EXISTS %I.instance_locks CASCADE;
            DROP TABLE IF EXISTS %I._duroxide_migrations CASCADE;
        END;
        $cleanup$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name, v_schema_name, v_schema_name, 
       v_schema_name, v_schema_name, v_schema_name, v_schema_name);

    -- ============================================================================
    -- Simple Query Procedures (Phase 3)
    -- ============================================================================

    -- Procedure: list_instances
    -- Returns all instance IDs ordered by creation date (newest first)
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
                 WHERE lock_token IS NULL OR locked_until <= p_now_ms) as orchestrator_queue,
                (SELECT COUNT(*)::BIGINT FROM %I.worker_queue 
                 WHERE lock_token IS NULL OR locked_until <= p_now_ms) as worker_queue;
        END;
        $get_queue_depths$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name, v_schema_name);

    -- ============================================================================
    -- Queue Operation Procedures (Phase 5)
    -- ============================================================================

    -- Procedure: enqueue_worker_work
    -- Inserts a work item into the worker queue
    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.enqueue_worker_work(p_work_item TEXT)
        RETURNS VOID AS $enq_worker$
        BEGIN
            INSERT INTO %I.worker_queue (work_item, created_at)
            VALUES (p_work_item, NOW());
        END;
        $enq_worker$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name);

    -- Note: dequeue_worker_peek_lock is kept in Rust code because SELECT FOR UPDATE SKIP LOCKED
    -- requires careful transaction handling that's better managed in application code

    -- Procedure: ack_worker
    -- Atomically deletes worker queue item and enqueues completion to orchestrator queue
    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.ack_worker(
            p_lock_token TEXT,
            p_instance_id TEXT,
            p_completion_json TEXT
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

            -- Enqueue completion to orchestrator queue
            INSERT INTO %I.orchestrator_queue (instance_id, work_item, visible_at, created_at)
            VALUES (p_instance_id, p_completion_json, NOW(), NOW());
        END;
        $ack_worker$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name, v_schema_name);

    -- Procedure: enqueue_orchestrator_work
    -- Enqueues work to orchestrator queue
    -- ⚠️ CRITICAL: DO NOT create instance here - instance creation happens via ack_orchestration_item metadata
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
END $$;

