#!/bin/bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

# fetch_work_item micro-benchmark: baseline vs. migration 0022 (generic-plan fix).
#
# Reproduces the defect fixed by migration 0022: inside the PL/pgSQL function the
# dequeue SELECT is planned once and cached as a GENERIC plan, which cannot
# estimate `visible_at <= $param` and therefore walks worker_queue_pkey in id
# order, filtering every future-visible row by hand. Cost grows linearly with the
# number of rows whose visible_at is still in the future.
#
# This builds two throwaway databases from this repo's own migration files -- one
# at 0021 (baseline) and one at 0022 (adds the redundant statement_timestamp()
# bound) -- loads FUTURE not-yet-visible rows plus one visible row into
# worker_queue, then times N real fetch_work_item() calls against each.
#
# The calls run in a DO loop so the inner SELECT is executed many times in one
# session: after ~5 executions PostgreSQL switches the function's cached plan to
# the generic plan, which is the steady-state production behaviour. A warmup loop
# forces that switch before timing. Each measured call re-frees the one visible
# row it claims, so every iteration scans past the FUTURE rows to reach it.
#
# Requires: psql, and a PostgreSQL the current user can create databases on.
# Connection is taken from the standard libpq environment variables
# (PGHOST, PGPORT, PGUSER, PGPASSWORD, ...); override as needed.
#
# Usage:
#   PGHOST=localhost PGPORT=5432 PGUSER=postgres scripts/bench-fetch-work-item.sh
#
# Tunables (env): FUTURE  not-yet-visible rows in the queue (default 100000)
#                 NCALLS  fetch_work_item calls timed        (default 500)
#                 MIGRATIONS_DIR (defaults to ../migrations relative to this file)
#
# Expected shape (hardware-dependent):
#   BASELINE (0021): grows ~0.00044 ms per future-visible row (~44 ms at 100k)
#   FIX (0022):      ~0.01-0.02 ms / call, flat regardless of FUTURE

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MIGRATIONS_DIR="${MIGRATIONS_DIR:-$(cd "$SCRIPT_DIR/.." && pwd)/migrations}"
FUTURE="${FUTURE:-100000}"
NCALLS="${NCALLS:-500}"

psql_c() { psql -X -q -v ON_ERROR_STOP=1 "$@"; }

FIX_MIGRATION="0022_fix_fetch_work_item_generic_plan.sql"
if [[ ! -f "$MIGRATIONS_DIR/$FIX_MIGRATION" ]]; then
  echo "ERROR: $MIGRATIONS_DIR does not contain $FIX_MIGRATION" >&2
  echo "       set MIGRATIONS_DIR to the duroxide-pg migrations/ directory." >&2
  exit 1
fi

echo "Building throwaway databases from $MIGRATIONS_DIR ..."
psql_c -d postgres -c "DROP DATABASE IF EXISTS dq_base;" -c "CREATE DATABASE dq_base;"
psql_c -d postgres -c "DROP DATABASE IF EXISTS dq_fix;"  -c "CREATE DATABASE dq_fix;"

# Baseline = every migration EXCEPT 0022; Fix = all migrations.
for f in $(ls "$MIGRATIONS_DIR"/0*.sql | sort); do
  base=$(basename "$f")
  [[ "$base" == 0022_* ]] || psql_c -d dq_base -f "$f" >/dev/null
  psql_c -d dq_fix -f "$f" >/dev/null
done

echo -n "  dq_base fetch_work_item bounds by statement_timestamp (expect f): "
psql_c -d dq_base -tA -c "SELECT pg_get_functiondef('fetch_work_item(bigint,bigint,text,bigint,text[],text)'::regprocedure) LIKE '%statement_timestamp%';"
echo -n "  dq_fix  fetch_work_item bounds by statement_timestamp (expect t): "
psql_c -d dq_fix  -tA -c "SELECT pg_get_functiondef('fetch_work_item(bigint,bigint,text,bigint,text[],text)'::regprocedure) LIKE '%statement_timestamp%';"

read -r -d '' AB_SQL <<'SQL' || true
TRUNCATE worker_queue;
-- :future rows whose visible_at is one hour in the future (never returnable) ...
INSERT INTO worker_queue (work_item, visible_at, created_at)
SELECT 'future', clock_timestamp() + interval '1 hour', clock_timestamp()
FROM generate_series(1, :future) g;
-- ... plus one row visible now, which every call will find and re-free.
INSERT INTO worker_queue (work_item, visible_at, created_at)
VALUES ('visible', clock_timestamp() - interval '1 second', clock_timestamp());
ANALYZE worker_queue;

-- Parameters are passed as a custom GUC because psql does not interpolate :vars
-- inside a dollar-quoted DO block.
SET my.ncalls = :ncalls;
DO $$
DECLARE
    t0 timestamptz; t1 timestamptz; i int; got int := 0; r record;
    v_ncalls int := current_setting('my.ncalls')::int;
    v_now    bigint;
BEGIN
    -- Warmup: force the function's cached plan to switch to the generic plan
    -- (PostgreSQL does this after ~5 executions). Not timed.
    FOR i IN 1..10 LOOP
        v_now := (extract(epoch FROM clock_timestamp()) * 1000)::bigint;
        SELECT * INTO r FROM fetch_work_item(v_now, 30000);
        UPDATE worker_queue SET lock_token = NULL, locked_until = NULL,
               attempt_count = attempt_count - 1 WHERE lock_token = r.out_lock_token;
    END LOOP;

    t0 := clock_timestamp();
    FOR i IN 1..v_ncalls LOOP
        v_now := (extract(epoch FROM clock_timestamp()) * 1000)::bigint;
        SELECT * INTO r FROM fetch_work_item(v_now, 30000);
        IF r.out_work_item IS NOT NULL THEN got := got + 1; END IF;
        -- Re-free the claimed row by exact (indexed) token match so the reset
        -- cost does not dominate the measured fetch cost.
        UPDATE worker_queue SET lock_token = NULL, locked_until = NULL,
               attempt_count = attempt_count - 1 WHERE lock_token = r.out_lock_token;
    END LOOP;
    t1 := clock_timestamp();
    RAISE NOTICE 'fetched=% of % calls  total=% ms  per_call=% ms',
        got, v_ncalls,
        round(extract(epoch FROM (t1-t0))*1000.0, 1),
        round(extract(epoch FROM (t1-t0))*1000.0/v_ncalls, 4);
END $$;
SQL

run_ab() {
  psql_c -d "$1" -v future="$FUTURE" -v ncalls="$NCALLS" <<<"$AB_SQL"
}

echo
echo "==== BASELINE (migrations 0001-0021) — $FUTURE future-visible rows, $NCALLS calls ===="
run_ab dq_base
echo
echo "==== FIX (migrations 0001-0022) — same data ===="
run_ab dq_fix
echo
echo "Done. (databases dq_base / dq_fix left in place; rerun drops+rebuilds them.)"
