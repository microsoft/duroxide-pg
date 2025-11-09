# Migration Plan: PostgreSQL Provider Instance Creation Changes

## Overview

This plan documents the changes required to migrate the PostgreSQL provider to the new instance creation pattern where:
- Instances are created via `ack_orchestration_item()` metadata (not on enqueue)
- `orchestration_version` becomes nullable
- Error handling uses `ProviderError` instead of `String`
- All instance creation logic is removed from enqueue operations

## Key Changes Summary

1. **Schema Update**: Make `orchestration_version` nullable in initial schema
2. **Remove Instance Creation from Enqueue**: Remove all instance creation from `enqueue_orchestrator_work()` and stored procedures
3. **Add Instance Creation to Ack**: Create instances using `ExecutionMetadata` in `ack_orchestration_item()`
4. **Update Fetch Logic**: Handle NULL versions and search all work items
5. **Update Error Handling**: Convert all methods to return `ProviderError`
6. **Update Management APIs**: Handle NULL versions in `get_instance_info()`
7. **Update Tests**: Provide proper `ExecutionMetadata` in all test calls

---

## Step 1: Update Initial Schema

### 1.1 Update Initial Schema

**File**: `migrations/0001_initial_schema.sql`

**Change**: Make `orchestration_version` nullable directly in the initial schema

Change line 9 from:
```sql
orchestration_version TEXT NOT NULL,
```

To:
```sql
orchestration_version TEXT, -- NULLable, set by runtime via ack_orchestration_item metadata
```

**Note**: Since we're not in production yet, we can update the initial schema directly instead of creating a separate migration.

---

## Step 2: Remove Instance Creation from Enqueue Operations

### 2.1 Update `enqueue_orchestrator_work()` Method

**File**: `src/provider.rs`

**Current behavior** (lines 1209-1219):
- Extracts `orchestration_name`, `orchestration_version`, `execution_id` from `StartOrchestration`
- Passes these to stored procedure

**Required change**:
- Remove extraction of orchestration metadata
- Pass `NULL` for `orchestration_name`, `orchestration_version`, `execution_id` parameters
- Update stored procedure call to not create instances

**Location**: Lines 1209-1232

### 2.2 Update Stored Procedure `enqueue_orchestrator_work`

**File**: `migrations/0002_create_stored_procedures.sql`

**Current behavior**:
- Creates instance if `StartOrchestration` detected
- Creates execution row

**Required change**:
- Remove all instance creation logic
- Remove all execution creation logic
- Only enqueue the work item to `orchestrator_queue`

**Location**: Find the `enqueue_orchestrator_work` function definition

---

## Step 3: Remove Instance Creation from `ack_orchestration_item()` Loop

### 3.1 Remove Instance Creation for Sub-Orchestrations

**File**: `src/provider.rs`

**Current behavior** (lines 695-726):
- When processing `orchestrator_items`, if `StartOrchestration` is detected, creates instance and execution

**Required change**:
- Remove entire block (lines 695-726)
- Only enqueue the work item, no instance creation

---

## Step 4: Add Instance Creation to `ack_orchestration_item()` Using Metadata

### 4.1 Add Instance Creation Logic

**File**: `src/provider.rs`

**Location**: After lock validation (after line 531), before execution creation (before line 544)

**Pattern**:
```rust
// Step 2: Create or update instance metadata from runtime-provided metadata
if let (Some(name), Some(version)) = (
    &metadata.orchestration_name,
    &metadata.orchestration_version,
) {
    // Step 2a: Ensure instance exists (creates if new, ignores if exists)
    sqlx::query(&format!(
        "INSERT INTO {} (instance_id, orchestration_name, orchestration_version, current_execution_id)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (instance_id) DO NOTHING",
        self.table_name("instances")
    ))
    .bind(&instance_id)
    .bind(name)
    .bind(version)
    .bind(execution_id as i64)
    .execute(&mut *tx)
    .await
    .map_err(|e| Self::sqlx_to_provider_error("ack_orchestration_item", e))?;

    // Step 2b: Update instance with resolved version
    sqlx::query(&format!(
        "UPDATE {} 
         SET orchestration_name = $1, orchestration_version = $2
         WHERE instance_id = $3",
        self.table_name("instances")
    ))
    .bind(name)
    .bind(version)
    .bind(&instance_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| Self::sqlx_to_provider_error("ack_orchestration_item", e))?;
}
```

### 4.2 Remove Existing Metadata Update

**File**: `src/provider.rs`

**Current behavior** (lines 566-581):
- Updates instance metadata if present in metadata

**Required change**:
- Remove this block (it's now handled by Step 4.1 above)

---

## Step 5: Update `fetch_orchestration_item()` to Handle NULL Versions

### 5.1 Update Version Reading

**File**: `src/provider.rs`

**Current behavior** (line 400):
- Reads `orchestration_version` as `String` (non-nullable)

**Required change**:
- Change to `Option<String>`
- Handle NULL with "unknown" fallback
- Add debug assertion for data consistency

**Location**: Lines 398-409

**New code**:
```rust
let instance_info: Option<(String, Option<String>, i64)> = sqlx::query_as(&format!(
    r#"
    SELECT orchestration_name, orchestration_version, current_execution_id
    FROM {}
    WHERE instance_id = $1
    "#,
    self.table_name("instances")
))
.bind(&instance_id)
.fetch_optional(&mut *tx)
.await
.ok()?;

let (orchestration_name, orchestration_version, current_execution_id, history) =
    if let Some((name, version_opt, exec_id)) = instance_info {
        // Handle NULL version
        let version = version_opt.unwrap_or_else(|| {
            debug_assert!(
                false, // We'll check history below
                "Instance exists with NULL version - should be set by metadata"
            );
            "unknown".to_string()
        });
        
        // Load history for current execution
        // ... existing history loading code ...
        
        (name, version, exec_id as u64, history)
    } else {
        // New instance - derive from work items
        // ... existing fallback logic ...
    };
```

### 5.2 Update Fallback Logic to Search All Work Items

**File**: `src/provider.rs`

**Current behavior** (lines 432-449):
- Only checks `messages.first()` for `StartOrchestration`
- Uses "1.0.0" as default version

**Required change**:
- Search ALL work items for `StartOrchestration` or `ContinueAsNew`
- Use "unknown" instead of "1.0.0" as fallback version

**New code**:
```rust
} else {
    // New instance - find StartOrchestration or ContinueAsNew in all work items
    if let Some(start_item) = messages.iter().find(|item| {
        matches!(item, WorkItem::StartOrchestration { .. } | WorkItem::ContinueAsNew { .. })
    }) {
        let (orchestration, version) = match start_item {
            WorkItem::StartOrchestration { orchestration, version, execution_id, .. }
            | WorkItem::ContinueAsNew { orchestration, version, execution_id, .. } => {
                (orchestration.clone(), version.clone())
            }
            _ => unreachable!(),
        };
        let version = version.unwrap_or_else(|| "unknown".to_string());
        let exec_id = match start_item {
            WorkItem::StartOrchestration { execution_id, .. }
            | WorkItem::ContinueAsNew { execution_id, .. } => *execution_id,
            _ => unreachable!(),
        };
        (orchestration, version, exec_id, Vec::new())
    } else {
        // Shouldn't happen - no instance metadata and not StartOrchestration
        tx.rollback().await.ok();
        return None;
    }
};
```

---

## Step 6: Update `get_instance_info()` to Handle NULL Versions

### 6.1 Update Stored Procedure Return Type

**File**: `migrations/0002_create_stored_procedures.sql`

**Current behavior**:
- `get_instance_info()` returns `orchestration_version` as `TEXT NOT NULL`

**Required change**:
- Change return type to allow NULL
- Handle NULL in function body with "unknown" fallback

### 6.2 Update Rust Code

**File**: `src/provider.rs`

**Current behavior** (lines 1400-1443):
- Reads `orchestration_version` as `String` (non-nullable)

**Required change**:
- Change to `Option<String>`
- Use "unknown" as fallback

**Location**: Lines 1402-1443

---

## Step 7: Update Error Handling to Use ProviderError

### 7.1 Add ProviderError Import

**File**: `src/provider.rs`

Add to imports:
```rust
use duroxide::providers::ProviderError;
```

### 7.2 Add Error Conversion Helper

**File**: `src/provider.rs`

Add helper function:
```rust
impl PostgresProvider {
    // ... existing methods ...
    
    /// Convert sqlx::Error to ProviderError with proper classification
    fn sqlx_to_provider_error(operation: &str, e: SqlxError) -> ProviderError {
        match e {
            SqlxError::Database(db_err) => {
                // PostgreSQL error codes
                if db_err.code().as_deref() == Some("40P01") {
                    // Deadlock detected
                    ProviderError::retryable(operation, format!("Deadlock detected: {}", e))
                } else if db_err.code().as_deref() == Some("40001") {
                    // Serialization failure
                    ProviderError::retryable(operation, format!("Serialization failure: {}", e))
                } else if db_err.code().as_deref() == Some("23505") {
                    // Unique constraint violation (duplicate event)
                    ProviderError::permanent(operation, format!("Duplicate detected: {}", e))
                } else if db_err.code().as_deref() == Some("23503") {
                    // Foreign key constraint violation
                    ProviderError::permanent(operation, format!("Foreign key violation: {}", e))
                } else {
                    ProviderError::permanent(operation, format!("Database error: {}", e))
                }
            }
            SqlxError::PoolClosed | SqlxError::PoolTimedOut => {
                ProviderError::retryable(operation, format!("Connection pool error: {}", e))
            }
            SqlxError::Io(_) => {
                ProviderError::retryable(operation, format!("I/O error: {}", e))
            }
            _ => ProviderError::permanent(operation, format!("Unexpected error: {}", e)),
        }
    }
}
```

### 7.3 Update All Method Signatures

**File**: `src/provider.rs`

Change return types from `Result<..., String>` to `Result<..., ProviderError>` for:

1. `ack_orchestration_item()` - Line 490
2. `abandon_orchestration_item()` - Line 804
3. `append_with_execution()` - Line 921
4. `enqueue_worker_work()` - Line 993
5. `ack_worker()` - Line 1095
6. `enqueue_orchestrator_work()` - Line 1154

### 7.4 Update All Error Returns

Replace all `.map_err(|e| format!(...))` with `.map_err(|e| Self::sqlx_to_provider_error(...))`

Replace all `Err(...)` with `Err(ProviderError::permanent(...))` or `Err(ProviderError::retryable(...))`

---

## Step 8: Update ManagementCapability Methods

### 8.1 Update Return Types

**File**: `src/provider.rs`

Change all `ManagementCapability` methods from `Result<..., String>` to `Result<..., ProviderError>`:

1. `list_instances()` - Line 1325
2. `list_instances_by_status()` - Line 1336
3. `list_executions()` - Line 1348
4. `read_execution()` - Line 1362
5. `latest_execution_id()` - Line 1387
6. `get_instance_info()` - Line 1401
7. `get_execution_info()` - Line 1446
8. `get_system_metrics()` - Line 1482
9. `get_queue_depths()` - Line 1518

### 8.2 Update Error Handling

Replace all `.map_err(|e| format!(...))` with `.map_err(|e| Self::sqlx_to_provider_error(...))`

---

## Step 9: Update Tests

### 9.1 Find All Test Files

**Files to check**:
- `tests/basic_tests.rs`
- `tests/e2e_samples.rs`
- `tests/postgres_provider_test.rs`

### 9.2 Update `ack_orchestration_item()` Calls

**Pattern to find**:
```rust
.ack_orchestration_item(
    &lock_token,
    execution_id,
    vec![],
    vec![],
    vec![],
    ExecutionMetadata::default(),  // ❌ Missing orchestration_name and version
)
```

**Replace with**:
```rust
.ack_orchestration_item(
    &lock_token,
    execution_id,
    vec![Event::OrchestrationStarted {
        event_id: duroxide::INITIAL_EVENT_ID,
        name: "TestOrch".to_string(),
        version: "1.0.0".to_string(),
        input: "".to_string(),
        parent_instance: None,
        parent_id: None,
    }],
    vec![],
    vec![],
    ExecutionMetadata {
        orchestration_name: Some("TestOrch".to_string()),
        orchestration_version: Some("1.0.0".to_string()),
        ..Default::default()
    },
)
```

### 9.3 Update Error Assertions

**Pattern to find**:
```rust
assert!(result.unwrap_err().contains("duplicate"));
```

**Replace with**:
```rust
assert!(result.unwrap_err().message.contains("duplicate"));
```

---

## Step 10: Update Stored Procedures

### 10.1 Remove Instance Creation from `enqueue_orchestrator_work`

**File**: `migrations/0002_create_stored_procedures.sql`

Find the function and remove:
- All `INSERT INTO instances` statements
- All `INSERT INTO executions` statements
- Parameters `p_orchestration_name`, `p_orchestration_version`, `p_execution_id` (or make them unused)

### 10.2 Update `get_instance_info()` to Handle NULL

**File**: `migrations/0002_create_stored_procedures.sql`

Update return type and handle NULL:
```sql
-- Change return type to allow NULL
-- In function body:
COALESCE(i.orchestration_version, 'unknown') as orchestration_version
```

---

## Verification Checklist

After completing all steps:

- [ ] Schema: `orchestration_version` is nullable (initial schema updated)
- [ ] `enqueue_orchestrator_work()`: No instance creation (stored procedure updated)
- [ ] `ack_orchestration_item()` loop: No instance creation for sub-orchestrations
- [ ] `ack_orchestration_item()`: Instance creation using metadata (INSERT OR IGNORE + UPDATE pattern)
- [ ] `fetch_orchestration_item()`: Handles NULL versions, uses "unknown" fallback
- [ ] `fetch_orchestration_item()`: Searches all work items for StartOrchestration/ContinueAsNew
- [ ] `get_instance_info()`: Handles NULL versions, uses "unknown" fallback
- [ ] All methods return `Result<..., ProviderError>` instead of `Result<..., String>`
- [ ] Error classification is correct (retryable vs non-retryable)
- [ ] All tests pass with proper `ExecutionMetadata`
- [ ] Tests updated to handle `ProviderError` (access `.message` field)

---

## Testing

Run provider validation tests:

```bash
cargo test --features provider-test
```

**Key tests to verify**:
- `test_instance_creation_via_metadata` - Instances created via ack metadata
- `test_no_instance_creation_on_enqueue` - No instance created on enqueue
- `test_null_version_handling` - NULL version handled correctly
- `test_sub_orchestration_instance_creation` - Sub-orchestrations follow same pattern

---

## Implementation Order

1. **Step 1**: Update initial schema (make orchestration_version nullable)
2. **Step 7**: Error handling conversion (foundation for other changes)
3. **Step 2**: Remove instance creation from enqueue
4. **Step 3**: Remove instance creation from ack loop
5. **Step 4**: Add instance creation to ack using metadata
6. **Step 5**: Update fetch logic
7. **Step 6**: Update management APIs
8. **Step 8**: Update ManagementCapability methods
9. **Step 9**: Update tests
10. **Step 10**: Update stored procedures

---

## Notes

- All changes must maintain backward compatibility where possible
- NULL version handling is critical - use "unknown" as fallback, don't extract from history
- Error classification is important for runtime retry logic
- Instance creation pattern: INSERT OR IGNORE + UPDATE ensures version is always set correctly
- Tests must provide proper `ExecutionMetadata` to create instances

