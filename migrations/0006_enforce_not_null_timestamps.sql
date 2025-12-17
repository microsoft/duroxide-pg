-- Migration 0006: Enforce NOT NULL timestamps without defaults
-- All timestamp columns must be explicitly provided by the Rust provider.
-- This migration removes DEFAULT clauses and adds NOT NULL constraints.

-- Step 1: Update any existing NULL values to NOW() (safety for existing data)
UPDATE instances SET created_at = NOW() WHERE created_at IS NULL;
UPDATE instances SET updated_at = NOW() WHERE updated_at IS NULL;
UPDATE executions SET started_at = NOW() WHERE started_at IS NULL;
UPDATE history SET created_at = NOW() WHERE created_at IS NULL;
UPDATE orchestrator_queue SET visible_at = NOW() WHERE visible_at IS NULL;
UPDATE orchestrator_queue SET created_at = NOW() WHERE created_at IS NULL;
UPDATE worker_queue SET created_at = NOW() WHERE created_at IS NULL;

-- Step 2: Alter columns to NOT NULL and drop defaults

-- instances table
ALTER TABLE instances 
    ALTER COLUMN created_at SET NOT NULL,
    ALTER COLUMN created_at DROP DEFAULT,
    ALTER COLUMN updated_at SET NOT NULL,
    ALTER COLUMN updated_at DROP DEFAULT;

-- executions table
ALTER TABLE executions 
    ALTER COLUMN started_at SET NOT NULL,
    ALTER COLUMN started_at DROP DEFAULT;
-- Note: completed_at remains nullable (only set when status is Completed/Failed)

-- history table
ALTER TABLE history 
    ALTER COLUMN created_at SET NOT NULL,
    ALTER COLUMN created_at DROP DEFAULT;

-- orchestrator_queue table
ALTER TABLE orchestrator_queue 
    ALTER COLUMN visible_at SET NOT NULL,
    ALTER COLUMN visible_at DROP DEFAULT,
    ALTER COLUMN created_at SET NOT NULL,
    ALTER COLUMN created_at DROP DEFAULT;

-- worker_queue table
ALTER TABLE worker_queue 
    ALTER COLUMN created_at SET NOT NULL,
    ALTER COLUMN created_at DROP DEFAULT;

-- Note: _duroxide_migrations.applied_at keeps its default since migrations
-- are tracked by the migration runner itself, not the provider.
