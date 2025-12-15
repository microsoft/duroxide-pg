-- Migration: 0001_initial_schema.sql
-- Description: Creates initial schema for Duroxide PostgreSQL provider
-- This migration will be executed with schema-qualified names via the migration runner

-- Instance metadata
CREATE TABLE IF NOT EXISTS instances (
    instance_id TEXT PRIMARY KEY,
    orchestration_name TEXT NOT NULL,
    orchestration_version TEXT, -- NULLable, set by runtime via ack_orchestration_item metadata
    current_execution_id BIGINT NOT NULL DEFAULT 1,
    created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
);

-- Multi-execution support
CREATE TABLE IF NOT EXISTS executions (
    instance_id TEXT NOT NULL,
    execution_id BIGINT NOT NULL,
    status TEXT NOT NULL DEFAULT 'Running',
    output TEXT,
    started_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
    completed_at TIMESTAMPTZ,
    PRIMARY KEY (instance_id, execution_id)
);

-- Event history (append-only)
CREATE TABLE IF NOT EXISTS history (
    instance_id TEXT NOT NULL,
    execution_id BIGINT NOT NULL,
    event_id BIGINT NOT NULL,
    event_type TEXT NOT NULL,
    event_data TEXT NOT NULL, -- JSON serialized Event
    created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (instance_id, execution_id, event_id)
);

-- Orchestrator queue
CREATE TABLE IF NOT EXISTS orchestrator_queue (
    id BIGSERIAL PRIMARY KEY,
    instance_id TEXT NOT NULL,
    work_item TEXT NOT NULL, -- JSON serialized WorkItem
    visible_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
    lock_token TEXT,
    locked_until BIGINT, -- Unix timestamp in milliseconds
    created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
);

-- Worker queue
CREATE TABLE IF NOT EXISTS worker_queue (
    id BIGSERIAL PRIMARY KEY,
    work_item TEXT NOT NULL, -- JSON serialized WorkItem
    lock_token TEXT,
    locked_until BIGINT, -- Unix timestamp in milliseconds
    created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
);

-- Instance-level locks for concurrent dispatcher coordination
CREATE TABLE IF NOT EXISTS instance_locks (
    instance_id TEXT PRIMARY KEY,
    lock_token TEXT NOT NULL,
    locked_until BIGINT NOT NULL, -- Unix timestamp in milliseconds
    locked_at BIGINT NOT NULL -- Unix timestamp in milliseconds
);

-- Create indexes
CREATE INDEX IF NOT EXISTS idx_orch_visible ON orchestrator_queue(visible_at, lock_token);
CREATE INDEX IF NOT EXISTS idx_orch_instance ON orchestrator_queue(instance_id);
CREATE INDEX IF NOT EXISTS idx_orch_lock ON orchestrator_queue(lock_token);
CREATE INDEX IF NOT EXISTS idx_worker_available ON worker_queue(lock_token, id);
CREATE INDEX IF NOT EXISTS idx_instance_locks_locked_until ON instance_locks(locked_until);
CREATE INDEX IF NOT EXISTS idx_history_lookup ON history(instance_id, execution_id, event_id);

-- Migration tracking table (create in each schema)
CREATE TABLE IF NOT EXISTS _duroxide_migrations (
    version BIGINT PRIMARY KEY,
    name TEXT NOT NULL,
    applied_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
);
