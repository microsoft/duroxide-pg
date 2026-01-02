#!/bin/bash
# Generate a diff markdown file for a specific migration
# Usage: ./scripts/generate_migration_diff.sh <migration_number>
# Example: ./scripts/generate_migration_diff.sh 9
#
# This script:
# 1. Creates two temp schemas (before/after the target migration)
# 2. Extracts schema DDL using SQL queries (no pg_dump required)
# 3. Generates a diff in markdown format
# 4. Cleans up temp schemas

set -e

if [ -z "$1" ]; then
    echo "Usage: $0 <migration_number>"
    echo "Example: $0 9"
    exit 1
fi

MIGRATION_NUM=$1
MIGRATION_NUM_PADDED=$(printf "%04d" $MIGRATION_NUM)

# Load DATABASE_URL from .env if it exists
if [ -f .env ]; then
    set -a
    source .env
    set +a
fi

if [ -z "$DATABASE_URL" ]; then
    echo "Error: DATABASE_URL not set. Create a .env file or export DATABASE_URL."
    exit 1
fi

# Generate unique schema names
TIMESTAMP=$(date +%s)
BEFORE_SCHEMA="diff_before_${MIGRATION_NUM_PADDED}_${TIMESTAMP}"
AFTER_SCHEMA="diff_after_${MIGRATION_NUM_PADDED}_${TIMESTAMP}"

# Temp files
BEFORE_DUMP=$(mktemp)
AFTER_DUMP=$(mktemp)
DIFF_OUTPUT=$(mktemp)

cleanup() {
    echo "Cleaning up temp schemas..."
    psql "$DATABASE_URL" -q -c "DROP SCHEMA IF EXISTS $BEFORE_SCHEMA CASCADE;" 2>/dev/null || true
    psql "$DATABASE_URL" -q -c "DROP SCHEMA IF EXISTS $AFTER_SCHEMA CASCADE;" 2>/dev/null || true
    rm -f "$BEFORE_DUMP" "$AFTER_DUMP" "$DIFF_OUTPUT"
}

# Only trap on EXIT, not on errors during diff (diff returns 1 when files differ)
trap cleanup EXIT
set +e  # Disable exit on error for the rest of the script

# Function to extract schema DDL using SQL queries
extract_schema_ddl() {
    local schema_name=$1
    local output_file=$2
    
    # Extract tables
    echo "-- TABLES" >> "$output_file"
    psql "$DATABASE_URL" -t -A -c "
        SELECT 'CREATE TABLE ' || table_name || ' (' || 
            string_agg(column_name || ' ' || 
                CASE 
                    WHEN data_type = 'character varying' THEN 'VARCHAR(' || character_maximum_length || ')'
                    WHEN data_type = 'numeric' THEN 'NUMERIC'
                    ELSE UPPER(data_type)
                END ||
                CASE WHEN is_nullable = 'NO' THEN ' NOT NULL' ELSE '' END ||
                CASE WHEN column_default IS NOT NULL THEN ' DEFAULT ' || column_default ELSE '' END
            , ', ' ORDER BY ordinal_position) || ');'
        FROM information_schema.columns 
        WHERE table_schema = '$schema_name' 
        GROUP BY table_name 
        ORDER BY table_name;
    " >> "$output_file" 2>/dev/null
    
    # Extract indexes
    echo "" >> "$output_file"
    echo "-- INDEXES" >> "$output_file"
    psql "$DATABASE_URL" -t -A -c "
        SELECT indexdef || ';'
        FROM pg_indexes 
        WHERE schemaname = '$schema_name' 
        AND indexname NOT LIKE '%_pkey'
        ORDER BY indexname;
    " >> "$output_file" 2>/dev/null
    
    # Extract functions (stored procedures)
    echo "" >> "$output_file"
    echo "-- FUNCTIONS" >> "$output_file"
    psql "$DATABASE_URL" -t -A -c "
        SELECT pg_get_functiondef(p.oid) || ';'
        FROM pg_proc p
        JOIN pg_namespace n ON p.pronamespace = n.oid
        WHERE n.nspname = '$schema_name'
        ORDER BY p.proname;
    " >> "$output_file" 2>/dev/null
}

echo "=== Generating diff for migration $MIGRATION_NUM_PADDED ==="

# Find the migration file to get its name
MIGRATION_FILE=$(ls migrations/${MIGRATION_NUM_PADDED}_*.sql 2>/dev/null | head -1)
if [ -z "$MIGRATION_FILE" ]; then
    echo "Error: Migration file migrations/${MIGRATION_NUM_PADDED}_*.sql not found"
    exit 1
fi
MIGRATION_NAME=$(basename "$MIGRATION_FILE")
echo "Migration file: $MIGRATION_NAME"

# Create the "before" schema (migrations 1 to N-1)
echo "Creating 'before' schema with migrations 1 to $((MIGRATION_NUM - 1))..."
psql "$DATABASE_URL" -q -c "CREATE SCHEMA $BEFORE_SCHEMA;"

# Run migrations up to N-1
for i in $(seq 1 $((MIGRATION_NUM - 1))); do
    PADDED=$(printf "%04d" $i)
    SQL_FILE=$(ls migrations/${PADDED}_*.sql 2>/dev/null | head -1)
    if [ -n "$SQL_FILE" ]; then
        echo "  Applying migration $PADDED to before schema..."
        psql "$DATABASE_URL" -q -v ON_ERROR_STOP=1 <<EOF
SET search_path TO $BEFORE_SCHEMA;
\i $SQL_FILE
EOF
    fi
done

# Create the "after" schema (migrations 1 to N)
echo "Creating 'after' schema with migrations 1 to $MIGRATION_NUM..."
psql "$DATABASE_URL" -q -c "CREATE SCHEMA $AFTER_SCHEMA;"

# Run migrations up to N
for i in $(seq 1 $MIGRATION_NUM); do
    PADDED=$(printf "%04d" $i)
    SQL_FILE=$(ls migrations/${PADDED}_*.sql 2>/dev/null | head -1)
    if [ -n "$SQL_FILE" ]; then
        echo "  Applying migration $PADDED to after schema..."
        psql "$DATABASE_URL" -q -v ON_ERROR_STOP=1 <<EOF
SET search_path TO $AFTER_SCHEMA;
\i $SQL_FILE
EOF
    fi
done

# Extract schema DDL
echo "Extracting schema DDL..."
extract_schema_ddl "$BEFORE_SCHEMA" "$BEFORE_DUMP"
extract_schema_ddl "$AFTER_SCHEMA" "$AFTER_DUMP"

# Normalize schema names for comparison
sed -i.bak "s/$BEFORE_SCHEMA/SCHEMA/g" "$BEFORE_DUMP"
sed -i.bak "s/$AFTER_SCHEMA/SCHEMA/g" "$AFTER_DUMP"
rm -f "$BEFORE_DUMP.bak" "$AFTER_DUMP.bak"

# Generate diff
echo "Generating diff..."
diff -u "$BEFORE_DUMP" "$AFTER_DUMP" > "$DIFF_OUTPUT" || true

# Count lines
DIFF_LINES=$(wc -l < "$DIFF_OUTPUT" | tr -d ' ')

# Output file path
OUTPUT_FILE="migrations/${MIGRATION_NUM_PADDED}_diff.md"

# Generate markdown
cat > "$OUTPUT_FILE" << EOF
# Diff for migration $MIGRATION_NUM_PADDED

**Migration file:** \`$MIGRATION_NAME\`

This diff was auto-generated by comparing schema state before and after applying migration $MIGRATION_NUM_PADDED.

## Schema Diff

\`\`\`diff
$(cat "$DIFF_OUTPUT")
\`\`\`

---
*Generated on $(date -u +"%Y-%m-%d %H:%M:%S UTC")*
EOF

echo ""
echo "=== Done! ==="
echo "Output written to: $OUTPUT_FILE"
echo "Lines in diff: $DIFF_LINES"
