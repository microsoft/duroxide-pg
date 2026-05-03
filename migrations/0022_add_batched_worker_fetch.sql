-- Migration: 0022_add_batched_worker_fetch.sql
-- Description: Adds provider-native batched worker fetch with bounded session claims.

DO $$
DECLARE
    v_schema_name TEXT := current_schema();
BEGIN
    EXECUTE format('
        CREATE OR REPLACE FUNCTION %I.fetch_work_items(
            p_now_ms BIGINT,
            p_lock_timeout_ms BIGINT,
            p_owner_id TEXT DEFAULT NULL,
            p_session_lock_timeout_ms BIGINT DEFAULT NULL,
            p_tag_filter TEXT[] DEFAULT NULL,
            p_tag_mode TEXT DEFAULT ''default_only'',
            p_max_items INTEGER DEFAULT 1,
            p_max_new_sessions INTEGER DEFAULT 0
        )
        RETURNS TABLE(
            out_work_item TEXT,
            out_lock_token TEXT,
            out_attempt_count INTEGER
        ) AS $fetch_workers$
        DECLARE
            v_candidate RECORD;
            v_lock_token TEXT;
            v_session_locked_until BIGINT;
            v_rows_affected INTEGER;
            v_returned INTEGER := 0;
            v_new_sessions_claimed INTEGER := 0;
            v_claimed_session_ids TEXT[] := ARRAY[]::TEXT[];
            v_result_ids BIGINT[] := ARRAY[]::BIGINT[];
            v_result_work_items TEXT[] := ARRAY[]::TEXT[];
            v_result_lock_tokens TEXT[] := ARRAY[]::TEXT[];
            v_result_attempt_counts INTEGER[] := ARRAY[]::INTEGER[];
            v_scan_limit INTEGER;
        BEGIN
            IF current_setting(''transaction_isolation'') <> ''read committed'' THEN
                RAISE EXCEPTION ''fetch_work_items requires READ COMMITTED isolation'';
            END IF;

            IF p_max_items IS NULL OR p_max_items <= 0 THEN
                RETURN;
            END IF;

            -- none mode: return immediately with no results
            IF p_tag_mode = ''none'' THEN
                RETURN;
            END IF;

            v_scan_limit := LEAST(GREATEST(p_max_items * 4, p_max_items), 1024);

            FOR v_candidate IN
                SELECT q.id, q.session_id, s.worker_id AS active_worker_id
                FROM %I.worker_queue q
                LEFT JOIN %I.sessions s ON s.session_id = q.session_id AND s.locked_until > p_now_ms
                WHERE q.visible_at <= TO_TIMESTAMP(p_now_ms / 1000.0)
                  AND (q.lock_token IS NULL OR q.locked_until <= p_now_ms)
                  AND (
                    (p_owner_id IS NOT NULL AND (
                        q.session_id IS NULL
                        OR s.worker_id = p_owner_id
                        OR s.session_id IS NULL
                    ))
                    OR (p_owner_id IS NULL AND q.session_id IS NULL)
                  )
                  AND (
                    CASE p_tag_mode
                        WHEN ''default_only'' THEN q.tag IS NULL
                        WHEN ''tags'' THEN q.tag = ANY(p_tag_filter)
                        WHEN ''default_and'' THEN (q.tag IS NULL OR q.tag = ANY(p_tag_filter))
                        WHEN ''any'' THEN TRUE
                        ELSE FALSE
                    END
                  )
                ORDER BY q.session_id NULLS FIRST, q.id
                LIMIT v_scan_limit
                FOR UPDATE OF q SKIP LOCKED
            LOOP
                EXIT WHEN v_returned >= p_max_items;

                IF v_candidate.session_id IS NOT NULL THEN
                    IF p_owner_id IS NULL THEN
                        CONTINUE;
                    END IF;

                    IF v_candidate.active_worker_id IS DISTINCT FROM p_owner_id
                       AND NOT (v_candidate.session_id = ANY(v_claimed_session_ids)) THEN
                        IF v_new_sessions_claimed >= COALESCE(p_max_new_sessions, 0) THEN
                            CONTINUE;
                        END IF;
                        v_new_sessions_claimed := v_new_sessions_claimed + 1;
                    END IF;

                    v_session_locked_until := p_now_ms + COALESCE(p_session_lock_timeout_ms, p_lock_timeout_ms);

                    INSERT INTO %I.sessions (session_id, worker_id, locked_until, last_activity_at)
                    VALUES (v_candidate.session_id, p_owner_id, v_session_locked_until, p_now_ms)
                    ON CONFLICT (session_id) DO UPDATE
                    SET worker_id = p_owner_id,
                        locked_until = v_session_locked_until,
                        last_activity_at = p_now_ms
                    WHERE %I.sessions.locked_until <= p_now_ms OR %I.sessions.worker_id = p_owner_id;

                    GET DIAGNOSTICS v_rows_affected = ROW_COUNT;
                    IF v_rows_affected = 0 THEN
                        IF v_candidate.active_worker_id IS DISTINCT FROM p_owner_id
                           AND NOT (v_candidate.session_id = ANY(v_claimed_session_ids)) THEN
                            v_new_sessions_claimed := GREATEST(0, v_new_sessions_claimed - 1);
                        END IF;
                        CONTINUE;
                    END IF;
                    IF NOT (v_candidate.session_id = ANY(v_claimed_session_ids)) THEN
                        v_claimed_session_ids := array_append(v_claimed_session_ids, v_candidate.session_id);
                    END IF;
                END IF;

                v_lock_token := ''lock_'' || gen_random_uuid()::TEXT;

                UPDATE %I.worker_queue q
                SET lock_token = v_lock_token,
                    locked_until = p_now_ms + p_lock_timeout_ms,
                    attempt_count = q.attempt_count + 1
                WHERE q.id = v_candidate.id
                  AND (q.lock_token IS NULL OR q.locked_until <= p_now_ms)
                RETURNING q.work_item, q.attempt_count
                INTO out_work_item, out_attempt_count;

                GET DIAGNOSTICS v_rows_affected = ROW_COUNT;
                IF v_rows_affected = 0 THEN
                    CONTINUE;
                END IF;

                v_result_ids := array_append(v_result_ids, v_candidate.id);
                v_result_work_items := array_append(v_result_work_items, out_work_item);
                v_result_lock_tokens := array_append(v_result_lock_tokens, v_lock_token);
                v_result_attempt_counts := array_append(v_result_attempt_counts, out_attempt_count);
                v_returned := v_returned + 1;
            END LOOP;

            RETURN QUERY
            SELECT u.work_item, u.lock_token, u.attempt_count
            FROM unnest(
                v_result_ids,
                v_result_work_items,
                v_result_lock_tokens,
                v_result_attempt_counts
            ) AS u(id, work_item, lock_token, attempt_count)
            ORDER BY u.id;
        END;
        $fetch_workers$ LANGUAGE plpgsql;
    ', v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name, v_schema_name);

    EXECUTE format('
        CREATE INDEX IF NOT EXISTS idx_worker_queue_fetch_default_unlocked
        ON %I.worker_queue (visible_at, id)
        WHERE session_id IS NULL AND lock_token IS NULL
    ', v_schema_name);

    EXECUTE format('
        CREATE INDEX IF NOT EXISTS idx_worker_queue_fetch_session_unlocked
        ON %I.worker_queue (session_id, visible_at, id)
        WHERE lock_token IS NULL
    ', v_schema_name);

    EXECUTE format('
        CREATE INDEX IF NOT EXISTS idx_worker_queue_fetch_tag_unlocked
        ON %I.worker_queue (tag, visible_at, id)
        WHERE lock_token IS NULL
    ', v_schema_name);

    EXECUTE format('
        CREATE INDEX IF NOT EXISTS idx_worker_queue_lock_expiry
        ON %I.worker_queue (locked_until)
        WHERE lock_token IS NOT NULL
    ', v_schema_name);
END;
$$;
