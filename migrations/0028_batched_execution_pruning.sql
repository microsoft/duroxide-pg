-- Copyright (c) Microsoft Corporation.
-- Licensed under the MIT License.

-- Migration 0028: Bounded cursor-based execution pruning.
-- The page bounds scanned execution keys, including protected and young rows.
-- Callers advance using the returned key even when no execution was deleted.
DO $$
DECLARE
    v_schema_name TEXT := current_schema();
BEGIN
    EXECUTE format($fmt$
        CREATE OR REPLACE FUNCTION %1$I.prune_executions_batch(
            p_after_instance_id TEXT,
            p_after_execution_id BIGINT,
            p_completed_before_ms BIGINT,
            p_limit INTEGER
        )
        RETURNS TABLE(
            last_instance_id TEXT,
            last_execution_id BIGINT,
            executions_scanned BIGINT,
            executions_deleted BIGINT,
            events_deleted BIGINT
        )
        LANGUAGE plpgsql
        SET search_path = pg_catalog, pg_temp
        AS $prune_batch$
        DECLARE
            v_execution RECORD;
            v_current_execution_id BIGINT;
            v_count BIGINT;
        BEGIN
            IF p_after_instance_id IS NULL OR p_after_execution_id IS NULL
               OR p_after_execution_id < 0 OR p_completed_before_ms IS NULL
               OR p_completed_before_ms < 0 OR p_limit IS NULL
               OR p_limit < 1 OR p_limit > 1000 THEN
                RAISE EXCEPTION 'Invalid execution pruning cursor, cutoff, or limit (expected 1..1000)'
                    USING ERRCODE = '22023';
            END IF;

            executions_scanned := 0;
            executions_deleted := 0;
            events_deleted := 0;
            FOR v_execution IN
                SELECT e.instance_id, e.execution_id
                FROM %1$I.executions e
                WHERE (e.instance_id, e.execution_id) >
                      (p_after_instance_id, p_after_execution_id)
                ORDER BY e.instance_id, e.execution_id
                LIMIT p_limit
            LOOP
                last_instance_id := v_execution.instance_id;
                last_execution_id := v_execution.execution_id;
                executions_scanned := executions_scanned + 1;
                -- Lock before touching history, as deletion does. The page may
                -- describe an earlier lifetime of the same instance/execution key.
                SELECT i.current_execution_id INTO v_current_execution_id
                FROM %1$I.instances i
                WHERE i.instance_id = v_execution.instance_id
                FOR NO KEY UPDATE;

                IF NOT FOUND OR v_execution.execution_id >= v_current_execution_id THEN
                    CONTINUE;
                END IF;

                -- Eligibility may have changed while waiting for the instance lock.
                PERFORM 1 FROM %1$I.executions e
                WHERE e.instance_id = v_execution.instance_id
                  AND e.execution_id = v_execution.execution_id
                  AND e.status <> 'Running'
                  AND e.completed_at < to_timestamp(p_completed_before_ms / 1000.0);
                IF NOT FOUND THEN
                    CONTINUE;
                END IF;

                -- Exact-key deletes avoid plans that scan the entire history
                -- table when joining a batch to its many history rows.
                DELETE FROM %1$I.history h
                WHERE h.instance_id = v_execution.instance_id
                  AND h.execution_id = v_execution.execution_id;
                GET DIAGNOSTICS v_count = ROW_COUNT;
                events_deleted := events_deleted + v_count;

                DELETE FROM %1$I.executions e
                WHERE e.instance_id = v_execution.instance_id
                  AND e.execution_id = v_execution.execution_id;
                GET DIAGNOSTICS v_count = ROW_COUNT;
                executions_deleted := executions_deleted + v_count;
            END LOOP;
            RETURN NEXT;
        END;
        $prune_batch$;
$fmt$, v_schema_name);
END $$;
