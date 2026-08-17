#!/bin/bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

# Secondary-index size/insert benchmark: baseline vs. migration 0023.
#
# Builds two throwaway databases from this repo's migration files -- one at 0021
# (baseline, full B-tree secondary indexes) and one with 0023 (session_id / tag /
# orchestrator lock_token indexes made partial WHERE NOT NULL) -- loads a realistic
# population where most rows have NULL session_id/tag and most orchestrator rows are
# unclaimed, then reports secondary-index sizes and bulk-insert time for each.
#
# Requires: psql, and a PostgreSQL the current user can create databases on.
# Connection is taken from the standard libpq environment variables
# (PGHOST, PGPORT, PGUSER, PGPASSWORD, ...); override as needed.
#
# Usage:
#   PGHOST=localhost PGPORT=5432 PGUSER=postgres scripts/bench-secondary-indexes.sh
#
# Tunables (env): N  worker rows (default 500000, ~5% sessioned, ~5% tagged)
#                 NO orchestrator rows (default 100000, ~10% claimed)
#                 MIGRATIONS_DIR (defaults to ../migrations relative to this file)

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MIGRATIONS_DIR="${MIGRATIONS_DIR:-$(cd "$SCRIPT_DIR/.." && pwd)/migrations}"
N="${N:-500000}"
NO="${NO:-100000}"

psql_c() { psql -X -q -v ON_ERROR_STOP=1 "$@"; }

if [[ ! -f "$MIGRATIONS_DIR/0023_partial_secondary_indexes.sql" ]]; then
  echo "ERROR: $MIGRATIONS_DIR does not contain 0023_partial_secondary_indexes.sql" >&2
  exit 1
fi

echo "Building throwaway databases from $MIGRATIONS_DIR ..."
psql_c -d postgres -c "DROP DATABASE IF EXISTS sec_base;" -c "CREATE DATABASE sec_base;"
psql_c -d postgres -c "DROP DATABASE IF EXISTS sec_fix;"  -c "CREATE DATABASE sec_fix;"

for f in $(ls "$MIGRATIONS_DIR"/0*.sql | sort); do
  b=$(basename "$f")
  [[ "$b" == 0023_* ]] || psql_c -d sec_base -f "$f" >/dev/null
  psql_c -d sec_fix -f "$f" >/dev/null
done

read -r -d '' BODY <<'SQL' || true
\set now 1900000000000
TRUNCATE worker_queue;
TRUNCATE orchestrator_queue;

\echo '--- insert timing: :n worker rows (~5% sessioned, ~5% tagged) ---'
\timing on
INSERT INTO worker_queue (work_item, visible_at, created_at, session_id, tag)
SELECT 'wi', to_timestamp((:now)/1000.0), now(),
       CASE WHEN g % 20 = 0 THEN 'sess_'||(g%1000) ELSE NULL END,
       CASE WHEN g % 20 = 1 THEN 'tag_'||(g%10)   ELSE NULL END
FROM generate_series(1, :n) g;
\timing off

INSERT INTO orchestrator_queue (instance_id, work_item, visible_at, created_at, lock_token, locked_until)
SELECT 'inst_'||g, 'wi', to_timestamp((:now)/1000.0), now(),
       CASE WHEN g % 10 = 0 THEN 'lock_'||g ELSE NULL END,
       CASE WHEN g % 10 = 0 THEN :now+300000 ELSE NULL END
FROM generate_series(1, :no) g;

ANALYZE worker_queue;
ANALYZE orchestrator_queue;

\echo '--- secondary index sizes ---'
SELECT indexrelname,
       pg_size_pretty(pg_relation_size(indexrelid)) AS size,
       (SELECT reltuples::bigint FROM pg_class WHERE oid = indexrelid) AS est_entries
FROM pg_stat_user_indexes
WHERE indexrelname IN ('idx_worker_queue_session_id','idx_worker_queue_tag','idx_orch_lock')
ORDER BY indexrelname;
SQL

run() { psql_c -d "$1" -v n="$N" -v no="$NO" <<<"$BODY"; }

echo
echo "==== BASELINE (migrations 0001-0021, full indexes) ===="
run sec_base
echo
echo "==== FIX (with 0023, partial indexes) ===="
run sec_fix
echo
echo "Done. (databases sec_base / sec_fix left in place; rerun drops+rebuilds them.)"
