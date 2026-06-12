# Support compile-time validation of INSERT statements for NOT NULL constraints

Fixes #4206

## The Problem

Right now, if you write an INSERT statement that forgets a NOT NULL column without a default, the sqlx macros happily compile — and then you get a runtime error when you first try to execute it.

```rust
conn.query_as!(
    SessionGroup,
    "INSERT INTO session_group (prop_a, prop_b) VALUES (?, ?)"  // missing prop_c
)
```

That's a runtime surprise that breaks the whole point of compile-time verification.

## The Solution

The fix leverages SQLite's `PRAGMA table_info()` to inspect the schema at compile time. When describing an INSERT statement, we now:

1. Parse the INSERT to extract the table name and any explicit column list
2. Query the schema for the table's columns and NOT NULL constraints
3. Cross-check: are all NOT NULL columns (without defaults) being inserted?
4. Error at compile time if any are missing

**The approach is graceful:** If we can't parse the INSERT (complex cases like INSERT...SELECT), or if the table doesn't exist yet, validation silently skips. The whole thing degrades beautifully — edge cases still compile.

## Implementation Details

Added to `sqlx-sqlite/src/connection/describe.rs`:
- `TableColumnInfo` struct to hold parsed column metadata
- `is_insert_statement()` to detect INSERT queries
- `extract_insert_info()` to parse table name and column list (handles backticks, quotes, brackets)
- `get_table_columns()` to run PRAGMA and fetch NOT NULL/default info
- `validate_insert_statement()` to cross-check columns
- Modified `describe()` to call validation before the normal flow

## Tests

Added 10 regression tests covering:
- Happy path: all required columns provided
- Unhappy path: missing NOT NULL columns (single and multiple)
- Edge cases: INSERT without column list (deferred to runtime), defaults, case sensitivity, quoted identifiers
- Exact reproducer from issue #4206

All existing tests pass.

## Trade-offs

**What this does catch:**
- Missing NOT NULL columns in explicit INSERT statements → compile error ✓

**What it doesn't (by design):**
- INSERT...SELECT (can't statically know what columns are returned)
- INSERT...DEFAULT VALUES (no columns to check)
- Schema-qualified names like `INSERT INTO schema.table` (parsing isn't exhaustive)

For those cases, validation is skipped and SQLite's runtime validation takes over. That's the right choice — catching 80% of cases at compile time is huge, and trying to be perfect would make the code fragile.

## Why This Matters

This is a quality-of-life fix for anyone using sqlx macros with SQLite. It moves a class of errors from "find it in testing" to "catch it in CI," which is where they belong.

Thanks for considering this.
