-- Copyright (c) Microsoft Corporation.
-- Licensed under the MIT License.

-- A deferral must move behind already-ready work even if its delay expires
-- before the backlog has been drained.
DO $migration$
DECLARE
    v_schema_name TEXT := current_schema();
BEGIN
    EXECUTE format(
        'CREATE INDEX IF NOT EXISTS idx_worker_ready_order ON %I.worker_queue(visible_at, id)',
        v_schema_name
    );

    EXECUTE format($ddl$
        CREATE OR REPLACE FUNCTION %1$I.fetch_work_item(
            p_now_ms BIGINT,
            p_lock_timeout_ms BIGINT,
            p_owner_id TEXT DEFAULT NULL,
            p_session_lock_timeout_ms BIGINT DEFAULT NULL,
            p_tag_filter TEXT[] DEFAULT NULL,
            p_tag_mode TEXT DEFAULT 'default_only'
        )
        RETURNS TABLE(
            out_work_item TEXT,
            out_lock_token TEXT,
            out_attempt_count INTEGER
        ) AS $fetch_worker$
        DECLARE
            v_id BIGINT;
            v_session_id TEXT;
            v_session_locked_until BIGINT;
        BEGIN
            IF p_tag_mode = 'none' THEN
                RETURN;
            END IF;

            IF p_owner_id IS NOT NULL THEN
                SELECT q.id, q.session_id INTO v_id, v_session_id
                FROM %1$I.worker_queue q
                LEFT JOIN %1$I.sessions s ON s.session_id = q.session_id AND s.locked_until > p_now_ms
                WHERE q.visible_at <= TO_TIMESTAMP(p_now_ms / 1000.0)
                  AND (q.lock_token IS NULL OR q.locked_until <= p_now_ms)
                  AND (
                    q.session_id IS NULL
                    OR s.worker_id = p_owner_id
                    OR s.session_id IS NULL
                  )
                  AND (
                    CASE p_tag_mode
                        WHEN 'default_only' THEN q.tag IS NULL
                        WHEN 'tags' THEN q.tag = ANY(p_tag_filter)
                        WHEN 'default_and' THEN (q.tag IS NULL OR q.tag = ANY(p_tag_filter))
                        WHEN 'any' THEN TRUE
                        ELSE FALSE
                    END
                  )
                ORDER BY q.visible_at, q.id
                LIMIT 1
                FOR UPDATE OF q SKIP LOCKED;
            ELSE
                SELECT q.id, q.session_id INTO v_id, v_session_id
                FROM %1$I.worker_queue q
                WHERE q.visible_at <= TO_TIMESTAMP(p_now_ms / 1000.0)
                  AND (q.lock_token IS NULL OR q.locked_until <= p_now_ms)
                  AND q.session_id IS NULL
                  AND (
                    CASE p_tag_mode
                        WHEN 'default_only' THEN q.tag IS NULL
                        WHEN 'tags' THEN q.tag = ANY(p_tag_filter)
                        WHEN 'default_and' THEN (q.tag IS NULL OR q.tag = ANY(p_tag_filter))
                        WHEN 'any' THEN TRUE
                        ELSE FALSE
                    END
                  )
                ORDER BY q.visible_at, q.id
                LIMIT 1
                FOR UPDATE OF q SKIP LOCKED;
            END IF;

            IF NOT FOUND THEN
                RETURN;
            END IF;

            out_lock_token := 'lock_' || gen_random_uuid()::TEXT;
            UPDATE %1$I.worker_queue
            SET lock_token = out_lock_token,
                locked_until = p_now_ms + p_lock_timeout_ms,
                attempt_count = attempt_count + 1
            WHERE id = v_id;

            SELECT work_item, attempt_count
            INTO out_work_item, out_attempt_count
            FROM %1$I.worker_queue
            WHERE id = v_id;

            IF v_session_id IS NOT NULL AND p_owner_id IS NOT NULL THEN
                v_session_locked_until := p_now_ms + COALESCE(p_session_lock_timeout_ms, p_lock_timeout_ms);
                INSERT INTO %1$I.sessions (session_id, worker_id, locked_until, last_activity_at)
                VALUES (v_session_id, p_owner_id, v_session_locked_until, p_now_ms)
                ON CONFLICT (session_id) DO UPDATE
                SET worker_id = p_owner_id,
                    locked_until = v_session_locked_until,
                    last_activity_at = p_now_ms
                WHERE %1$I.sessions.locked_until <= p_now_ms OR %1$I.sessions.worker_id = p_owner_id;

                IF NOT FOUND THEN
                    UPDATE %1$I.worker_queue
                    SET lock_token = NULL,
                        locked_until = NULL,
                        attempt_count = attempt_count - 1
                    WHERE id = v_id;
                    RETURN;
                END IF;
            END IF;

            RETURN NEXT;
        END;
        $fetch_worker$ LANGUAGE plpgsql;
    $ddl$, v_schema_name);
END;
$migration$;
