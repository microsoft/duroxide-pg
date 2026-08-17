-- Copyright (c) Microsoft Corporation.
-- Licensed under the MIT License.

-- Migration 0023: make mostly-NULL secondary indexes partial
--
-- NOTE: 0022 is reserved by the concurrent worker_queue dequeue PR
-- (perf/worker-queue-partial-index-dequeue). This migration is independent of it
-- and applies cleanly whether or not 0022 is present; the runner applies pending
-- migrations by version and tolerates the gap.
--
-- Three secondary indexes cover columns that are NULL for the majority of rows,
-- yet a plain B-tree still stores an entry for every row (including the NULLs).
-- Each is only ever probed by a concrete non-NULL value, so restricting the index
-- to the non-NULL rows keeps it serving those lookups while shrinking it and
-- removing index maintenance / autovacuum work for the common (NULL) insert path:
--
--   worker_queue.session_id      -- NULL for every non-session work item
--   worker_queue.tag             -- NULL for every untagged (default) work item
--   orchestrator_queue.lock_token -- NULL for every unclaimed row
--
-- Usage that keeps working (all match only non-NULL values):
--   session routing/takeover:  WHERE session_id = <id>, JOIN ON s.session_id = q.session_id
--   tag routing:               WHERE tag = ANY(<tags>)
--   orchestrator ack:          DELETE ... WHERE lock_token = <token>

DO $$
DECLARE
    v_schema_name TEXT := current_schema();
BEGIN
    EXECUTE format('DROP INDEX IF EXISTS %1$I.idx_worker_queue_session_id', v_schema_name);
    EXECUTE format(
        'CREATE INDEX idx_worker_queue_session_id ON %1$I.worker_queue (session_id) WHERE session_id IS NOT NULL',
        v_schema_name
    );

    EXECUTE format('DROP INDEX IF EXISTS %1$I.idx_worker_queue_tag', v_schema_name);
    EXECUTE format(
        'CREATE INDEX idx_worker_queue_tag ON %1$I.worker_queue (tag) WHERE tag IS NOT NULL',
        v_schema_name
    );

    EXECUTE format('DROP INDEX IF EXISTS %1$I.idx_orch_lock', v_schema_name);
    EXECUTE format(
        'CREATE INDEX idx_orch_lock ON %1$I.orchestrator_queue (lock_token) WHERE lock_token IS NOT NULL',
        v_schema_name
    );

    RAISE NOTICE 'Migration 0023: session_id, tag, and orchestrator lock_token indexes are now partial (WHERE NOT NULL)';
END $$;
