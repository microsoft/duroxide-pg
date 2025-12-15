-- Migration: 0003_add_attempt_count.sql
-- Description: Adds attempt_count column for poison message detection (duroxide 0.1.2)
-- This column tracks how many times a message has been fetched for processing

-- Add attempt_count to orchestrator_queue
-- Starts at 0, incremented to 1 on first fetch
ALTER TABLE orchestrator_queue ADD COLUMN IF NOT EXISTS attempt_count INTEGER NOT NULL DEFAULT 0;

-- Add attempt_count to worker_queue
-- Starts at 0, incremented to 1 on first fetch
ALTER TABLE worker_queue ADD COLUMN IF NOT EXISTS attempt_count INTEGER NOT NULL DEFAULT 0;

