-- Copyright (c) Microsoft Corporation.
-- Licensed under the MIT License.

-- Migration 0030: notification hints for optional long-polling.
-- Table OIDs keep channels short and independent of identifier case/truncation.

CREATE OR REPLACE FUNCTION duroxide_notify_queue_work()
RETURNS TRIGGER
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $notify$
BEGIN
    IF TG_TABLE_NAME = 'sessions' THEN
        PERFORM pg_catalog.pg_notify(
            'duroxide_pg_' || pg_catalog.format('%I.worker_queue', TG_TABLE_SCHEMA)::pg_catalog.regclass::pg_catalog.oid::TEXT,
            NEW.locked_until::TEXT
        );
        RETURN NEW;
    ELSIF TG_OP = 'DELETE' THEN
        -- Acknowledgment consumes claimed rows and releases the instance lock
        -- in one transaction; NOTIFY is delivered only after that commit.
        PERFORM pg_catalog.pg_notify('duroxide_pg_' || TG_RELID::TEXT, '0');
        RETURN OLD;
    ELSIF TG_OP = 'UPDATE' AND TG_TABLE_NAME = 'orchestrator_queue' THEN
        IF OLD.lock_token IS NOT NULL AND NEW.lock_token IS NULL THEN
            -- Other visible messages can run even when this message is delayed.
            PERFORM pg_catalog.pg_notify('duroxide_pg_' || TG_RELID::TEXT, '0');
        END IF;
    END IF;

    PERFORM pg_catalog.pg_notify(
        'duroxide_pg_' || TG_RELID::TEXT,
        CASE WHEN NEW.lock_token IS NOT NULL THEN NEW.locked_until::TEXT
             ELSE pg_catalog.ceil(EXTRACT(EPOCH FROM NEW.visible_at) * 1000)::BIGINT::TEXT
        END
    );
    RETURN NEW;
END;
$notify$;

-- Match normal acknowledgments' worker-before-orchestration table lock order.
DROP TRIGGER IF EXISTS trg_notify_worker_work ON worker_queue;
CREATE TRIGGER trg_notify_worker_work
    AFTER INSERT ON worker_queue
    FOR EACH ROW EXECUTE FUNCTION duroxide_notify_queue_work();
ALTER TABLE worker_queue DISABLE TRIGGER trg_notify_worker_work;

DROP TRIGGER IF EXISTS trg_notify_worker_work_released ON worker_queue;
CREATE TRIGGER trg_notify_worker_work_released
    AFTER UPDATE OF visible_at, lock_token, locked_until ON worker_queue
    FOR EACH ROW
    WHEN (
        NEW.visible_at IS DISTINCT FROM OLD.visible_at
        OR (OLD.lock_token IS NOT NULL AND NEW.lock_token IS NULL)
        OR (NEW.lock_token IS NOT NULL AND (
            NEW.lock_token IS DISTINCT FROM OLD.lock_token
            OR NEW.locked_until IS DISTINCT FROM OLD.locked_until
        ))
    )
    EXECUTE FUNCTION duroxide_notify_queue_work();
ALTER TABLE worker_queue DISABLE TRIGGER trg_notify_worker_work_released;

DROP TRIGGER IF EXISTS trg_notify_orch_work ON orchestrator_queue;
CREATE TRIGGER trg_notify_orch_work
    AFTER INSERT ON orchestrator_queue
    FOR EACH ROW EXECUTE FUNCTION duroxide_notify_queue_work();
ALTER TABLE orchestrator_queue DISABLE TRIGGER trg_notify_orch_work;

DROP TRIGGER IF EXISTS trg_notify_orch_work_released ON orchestrator_queue;
CREATE TRIGGER trg_notify_orch_work_released
    AFTER UPDATE OF visible_at, lock_token, locked_until ON orchestrator_queue
    FOR EACH ROW
    WHEN (
        NEW.visible_at IS DISTINCT FROM OLD.visible_at
        OR (OLD.lock_token IS NOT NULL AND NEW.lock_token IS NULL)
        OR (NEW.lock_token IS NOT NULL AND (
            NEW.lock_token IS DISTINCT FROM OLD.lock_token
            OR NEW.locked_until IS DISTINCT FROM OLD.locked_until
        ))
    )
    EXECUTE FUNCTION duroxide_notify_queue_work();
ALTER TABLE orchestrator_queue DISABLE TRIGGER trg_notify_orch_work_released;

DROP TRIGGER IF EXISTS trg_notify_orch_work_consumed ON orchestrator_queue;
CREATE TRIGGER trg_notify_orch_work_consumed
    AFTER DELETE ON orchestrator_queue
    FOR EACH ROW
    WHEN (OLD.lock_token IS NOT NULL)
    EXECUTE FUNCTION duroxide_notify_queue_work();
ALTER TABLE orchestrator_queue DISABLE TRIGGER trg_notify_orch_work_consumed;

DROP TRIGGER IF EXISTS trg_notify_session_lease ON sessions;
CREATE TRIGGER trg_notify_session_lease
    AFTER INSERT OR UPDATE OF locked_until ON sessions
    FOR EACH ROW EXECUTE FUNCTION duroxide_notify_queue_work();
ALTER TABLE sessions DISABLE TRIGGER trg_notify_session_lease;

DO $$
DECLARE
    v_schema_name TEXT := current_schema();
BEGIN
    EXECUTE format($fmt$
        CREATE OR REPLACE FUNCTION %1$I.cleanup_schema()
        RETURNS VOID AS $cleanup$
        BEGIN
            DROP TABLE IF EXISTS %1$I.sessions CASCADE;
            DROP TABLE IF EXISTS %1$I.kv_delta CASCADE;
            DROP TABLE IF EXISTS %1$I.kv_store CASCADE;
            DROP TABLE IF EXISTS %1$I.instances CASCADE;
            DROP TABLE IF EXISTS %1$I.executions CASCADE;
            DROP TABLE IF EXISTS %1$I.history CASCADE;
            DROP TABLE IF EXISTS %1$I.orchestrator_queue CASCADE;
            DROP TABLE IF EXISTS %1$I.worker_queue CASCADE;
            DROP TABLE IF EXISTS %1$I.instance_locks CASCADE;
            DROP TABLE IF EXISTS %1$I._duroxide_migrations CASCADE;

            DROP FUNCTION IF EXISTS %1$I.duroxide_notify_queue_work();
            DROP FUNCTION IF EXISTS %1$I.cleanup_schema();
            DROP FUNCTION IF EXISTS %1$I.list_instances();
            DROP FUNCTION IF EXISTS %1$I.list_executions(TEXT);
            DROP FUNCTION IF EXISTS %1$I.latest_execution_id(TEXT);
            DROP FUNCTION IF EXISTS %1$I.list_instances_by_status(TEXT);
            DROP FUNCTION IF EXISTS %1$I.get_instance_info(TEXT);
            DROP FUNCTION IF EXISTS %1$I.get_execution_info(TEXT, BIGINT);
            DROP FUNCTION IF EXISTS %1$I.get_system_metrics();
            DROP FUNCTION IF EXISTS %1$I.get_queue_depths(BIGINT);
            DROP FUNCTION IF EXISTS %1$I.enqueue_worker_work(TEXT, BIGINT, TEXT, BIGINT, BIGINT, TEXT);
            DROP FUNCTION IF EXISTS %1$I.enqueue_worker_work(TEXT, BIGINT, TEXT, BIGINT, BIGINT, TEXT, TEXT);
            DROP FUNCTION IF EXISTS %1$I.ack_worker(TEXT, TEXT, TEXT, BIGINT);
            DROP FUNCTION IF EXISTS %1$I.renew_work_item_lock(TEXT, BIGINT, BIGINT);
            DROP FUNCTION IF EXISTS %1$I.fetch_work_item(BIGINT, BIGINT, TEXT, BIGINT);
            DROP FUNCTION IF EXISTS %1$I.fetch_work_item(BIGINT, BIGINT, TEXT, BIGINT, TEXT[], TEXT);
            DROP FUNCTION IF EXISTS %1$I.abandon_work_item(TEXT, BIGINT, BIGINT, BOOLEAN);
            DROP FUNCTION IF EXISTS %1$I.enqueue_orchestrator_work(TEXT, TEXT, TIMESTAMPTZ, TEXT, TEXT, BIGINT);
            DROP FUNCTION IF EXISTS %1$I.fetch_orchestration_item(BIGINT, BIGINT);
            DROP FUNCTION IF EXISTS %1$I.fetch_orchestration_item(BIGINT, BIGINT, BIGINT, BIGINT);
            DROP FUNCTION IF EXISTS %1$I.ack_orchestration_item(TEXT, BIGINT, BIGINT, JSONB, JSONB, JSONB, JSONB, JSONB);
            DROP FUNCTION IF EXISTS %1$I.abandon_orchestration_item(TEXT, BIGINT, BIGINT, BOOLEAN);
            DROP FUNCTION IF EXISTS %1$I.renew_orchestration_item_lock(TEXT, BIGINT, BIGINT);
            DROP FUNCTION IF EXISTS %1$I.fetch_history(TEXT);
            DROP FUNCTION IF EXISTS %1$I.fetch_history_with_execution(TEXT, BIGINT);
            DROP FUNCTION IF EXISTS %1$I.append_history(TEXT, BIGINT, JSONB);
            DROP FUNCTION IF EXISTS %1$I.list_children(TEXT);
            DROP FUNCTION IF EXISTS %1$I.get_parent_id(TEXT);
            DROP FUNCTION IF EXISTS %1$I.delete_instances_atomic(TEXT[], BOOLEAN);
            DROP FUNCTION IF EXISTS %1$I.prune_executions(TEXT, INTEGER, BIGINT);
            DROP FUNCTION IF EXISTS %1$I.prune_executions_batch(TEXT, BIGINT, BIGINT, INTEGER);
            DROP FUNCTION IF EXISTS %1$I.renew_session_lock(TEXT[], BIGINT, BIGINT, BIGINT);
            DROP FUNCTION IF EXISTS %1$I.cleanup_orphaned_sessions(BIGINT);
            DROP FUNCTION IF EXISTS %1$I.get_custom_status(TEXT, BIGINT);
            DROP FUNCTION IF EXISTS %1$I.get_kv_value(TEXT, TEXT);
            DROP FUNCTION IF EXISTS %1$I.get_kv_all_values(TEXT);
            DROP FUNCTION IF EXISTS %1$I.get_instance_stats(TEXT);
        END;
        $cleanup$ LANGUAGE plpgsql;
    $fmt$, v_schema_name);
END $$;
