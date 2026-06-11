use crate::error::Error;
use crate::io::StatementId;
use crate::query_as::query_as;
use crate::statement::PgStatementMetadata;
use crate::types::Json;
use crate::PgConnection;
use smallvec::SmallVec;
use sqlx_core::query_builder::QueryBuilder;
use sqlx_core::sql_str::AssertSqlSafe;
use std::collections::BTreeSet;

impl PgConnection {
    /// Check whether EXPLAIN statements are supported by the current connection
    fn is_explain_available(&self) -> bool {
        let parameter_statuses = &self.inner.stream.parameter_statuses;
        let is_cockroachdb = parameter_statuses.contains_key("crdb_version");
        let is_materialize = parameter_statuses.contains_key("mz_version");
        let is_questdb = parameter_statuses.contains_key("questdb_version");
        !is_cockroachdb && !is_materialize && !is_questdb
    }

    pub(crate) async fn get_nullable_for_columns(
        &mut self,
        stmt_id: StatementId,
        meta: &PgStatementMetadata,
    ) -> Result<Vec<Option<bool>>, Error> {
        if meta.columns.is_empty() {
            return Ok(vec![]);
        }

        if meta.columns.len() * 3 > 65535 {
            tracing::debug!(
                ?stmt_id,
                num_columns = meta.columns.len(),
                "number of columns in query is too large to pull nullability for"
            );
        }

        // Query for NOT NULL constraints for each column in the query.
        //
        // This will include columns that don't have a `relation_id` (are not from a table);
        // assuming those are a minority of columns, it's less code to _not_ work around it
        // and just let Postgres return `NULL`.
        //
        // Use `UNION ALL` syntax instead of `VALUES` due to frequent lack of
        // support for `VALUES` in pgwire supported databases.
        let mut nullable_query = QueryBuilder::new("SELECT NOT attnotnull FROM ( ");
        let mut separated = nullable_query.separated("UNION ALL ");

        let mut column_iter = meta.columns.iter().zip(0i32..);
        if let Some((column, i)) = column_iter.next() {
            separated.push("( SELECT ");
            separated
                .push_bind_unseparated(i)
                .push_unseparated("::int4 AS idx, ");
            separated
                .push_bind_unseparated(column.relation_id)
                .push_unseparated("::int4 AS table_id, ");
            separated
                .push_bind_unseparated(column.relation_attribute_no)
                .push_unseparated("::int2 AS col_idx ) ");
        }

        for (column, i) in column_iter {
            separated.push("( SELECT ");
            separated
                .push_bind_unseparated(i)
                .push_unseparated("::int4, ");
            separated
                .push_bind_unseparated(column.relation_id)
                .push_unseparated("::int4, ");
            separated
                .push_bind_unseparated(column.relation_attribute_no)
                .push_unseparated("::int2 ) ");
        }

        nullable_query.push(
            ") AS col LEFT JOIN pg_catalog.pg_attribute \
                ON table_id IS NOT NULL \
               AND attrelid = table_id \
               AND attnum = col_idx \
            ORDER BY idx",
        );

        let mut nullables: Vec<Option<bool>> = nullable_query
            .build_query_scalar()
            .fetch_all(&mut *self)
            .await
            .map_err(|e| {
                err_protocol!(
                    "error from nullables query: {e}; query: {:?}",
                    nullable_query.sql()
                )
            })?;

        // If the server doesn't support EXPLAIN statements, skip this step (#1248).
        if self.is_explain_available() {
            // patch up our null inference with data from EXPLAIN
            let nullable_patch = self
                .nullables_from_explain(stmt_id, meta.parameters.len())
                .await?;

            for (nullable, patch) in nullables.iter_mut().zip(nullable_patch) {
                *nullable = patch.or(*nullable);
            }
        }

        Ok(nullables)
    }

    /// Infer nullability for columns of this statement using EXPLAIN VERBOSE.
    ///
    /// This currently only marks columns that are on the inner half of an outer join
    /// and returns `None` for all others.
    async fn nullables_from_explain(
        &mut self,
        stmt_id: StatementId,
        params_len: usize,
    ) -> Result<Vec<Option<bool>>, Error> {
        let stmt_id_display = stmt_id
            .display()
            .ok_or_else(|| err_protocol!("cannot EXPLAIN unnamed statement: {stmt_id:?}"))?;

        let mut explain = format!("EXPLAIN (VERBOSE, FORMAT JSON) EXECUTE {stmt_id_display}");
        let mut comma = false;

        if params_len > 0 {
            explain += "(";

            // fill the arguments list with NULL, which should theoretically be valid
            for _ in 0..params_len {
                if comma {
                    explain += ", ";
                }

                explain += "NULL";
                comma = true;
            }

            explain += ")";
        }

        let (Json(explains),): (Json<SmallVec<[Explain; 1]>>,) =
            query_as(AssertSqlSafe(explain)).fetch_one(self).await?;

        let mut nullables = Vec::new();

        if let Some(Explain::Plan {
            plan:
                plan @ Plan {
                    output: Some(ref outputs),
                    ..
                },
        }) = explains.first()
        {
            nullables.resize(outputs.len(), None);
            visit_plan(plan, false, outputs, &mut nullables);
        }

        Ok(nullables)
    }
}

/// Outer-join types from a plan node's `Join Type` field. Other values
/// (`Inner`, `Anti`, `Semi`, …) don't introduce nullability and don't get
/// parsed — they fall through to `None`.
#[derive(Copy, Clone)]
enum JoinType {
    Left,
    Right,
    Full,
}

impl JoinType {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "Left" => Some(Self::Left),
            "Right" => Some(Self::Right),
            "Full" => Some(Self::Full),
            _ => None,
        }
    }

    /// Whether a child with the given `Parent Relationship` is on this
    /// join's nullable side.
    fn child_is_nullable(self, child_rel: Option<ParentRelation>) -> bool {
        match self {
            Self::Full => true,
            Self::Left => child_rel == Some(ParentRelation::Inner),
            Self::Right => child_rel == Some(ParentRelation::Outer),
        }
    }
}

/// A child's `Parent Relationship` field from the EXPLAIN plan.
#[derive(Copy, Clone, PartialEq, Eq)]
enum ParentRelation {
    Outer,
    Inner,
}

impl ParentRelation {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "Outer" => Some(Self::Outer),
            "Inner" => Some(Self::Inner),
            _ => None,
        }
    }
}

/// Walk the EXPLAIN plan tree marking each root output that may be NULL due
/// to an outer join above it.
///
///   * In a nullable subtree (ancestor outer join already nulled this
///     branch), every output is treated as potentially NULL.
///   * At an outer-join node (`Left` / `Right` / `Full`), the join's own
///     `Output` is scanned for qualified column refs (`alias.col`) drawn
///     from the nullable-side subtree's leaves; any output that mentions
///     one is potentially NULL. Outputs that reference neither side
///     (subplan refs like `(SubPlan N)`, parameter refs, constants) stay
///     unmarked — their nullability is genuinely unknown to this pass.
///
/// False positives — marking a column nullable when a downstream filter
/// eliminates the NULLs — only leave a column wrapped in `Option<T>`. False
/// negatives would cause runtime `unexpected null` decode panics.
fn visit_plan(
    plan: &Plan,
    in_nullable: bool,
    outputs: &[String],
    nullables: &mut Vec<Option<bool>>,
) {
    let join = plan.join_type.as_deref().and_then(JoinType::parse);

    if in_nullable {
        // Ancestor outer join already null-extended this whole subtree.
        for output in plan.output.iter().flatten() {
            if let Some(i) = outputs.iter().position(|o| outputs_match(o, output)) {
                nullables[i] = Some(true);
            }
        }
    } else if let Some(j) = join {
        let mut nullable_cols: BTreeSet<&str> = BTreeSet::new();
        for child in plan.plans.iter().flatten() {
            let child_rel = child
                .parent_relation
                .as_deref()
                .and_then(ParentRelation::parse);
            if j.child_is_nullable(child_rel) {
                collect_qualified_col_refs(child, &mut nullable_cols);
            }
        }
        for output in plan.output.iter().flatten() {
            if !qualified_col_refs(output).any(|c| nullable_cols.contains(c)) {
                continue;
            }
            if let Some(i) = outputs.iter().position(|o| outputs_match(o, output)) {
                nullables[i] = Some(true);
            }
        }
    }

    for child in plan.plans.iter().flatten() {
        let child_rel = child
            .parent_relation
            .as_deref()
            .and_then(ParentRelation::parse);
        let child_in_nullable = in_nullable || join.is_some_and(|j| j.child_is_nullable(child_rel));
        visit_plan(child, child_in_nullable, outputs, nullables);
    }
}

/// Collect `<ident>.<ident>` tokens from a plan's `Output` and from the
/// `Output`s of its main-join descendants.
///
/// Only `Outer` / `Inner` children are descended into. `SubPlan` / `InitPlan`
/// children are computed independently of the surrounding outer join, so the
/// columns they expose are not what makes the join's own outputs nullable.
fn collect_qualified_col_refs<'a>(plan: &'a Plan, into: &mut BTreeSet<&'a str>) {
    for output in plan.output.iter().flatten() {
        for col in qualified_col_refs(output) {
            into.insert(col);
        }
    }
    for child in plan.plans.iter().flatten() {
        if child
            .parent_relation
            .as_deref()
            .and_then(ParentRelation::parse)
            .is_some()
        {
            collect_qualified_col_refs(child, into);
        }
    }
}

/// Iterate over `<ident>.<ident>` tokens in `s`, where each `ident` is
/// either:
///
///   * an unquoted identifier — a Unicode-letter-or-underscore start
///     followed by Unicode-letters, underscores, digits, or `$`, or
///   * a double-quoted identifier `"…"`, with `""` as the inner escape
///     for a literal double-quote.
///
/// Both shapes per PG manual §4.1.1 "Identifiers and Key Words":
/// <https://www.postgresql.org/docs/current/sql-syntax-lexical.html#SQL-SYNTAX-IDENTIFIERS>
///
/// The leading and trailing quote bytes are part of the returned slice so
/// a leaf `Output` of `"my col".x` and a join `Output` of `"my col".x`
/// match each other as the same token.
///
/// Unicode-escape identifiers (`U&"…"`) are not recognized as a distinct
/// shape: the `U&` prefix is skipped over and the quoted body is tokenized
/// like an ordinary `"…"` ident. That's symmetric across all `Output`
/// occurrences of the same expression, so matching still works.
fn qualified_col_refs(s: &str) -> impl Iterator<Item = &str> + '_ {
    let mut pos = 0;
    std::iter::from_fn(move || {
        while pos < s.len() {
            let first_start = pos;
            let first_end = scan_ident(s, pos);
            if first_end == pos {
                // No identifier here — advance one char and retry.
                pos += s[pos..].chars().next().map_or(1, char::len_utf8);
                continue;
            }
            if s.as_bytes().get(first_end) != Some(&b'.') {
                pos = first_end;
                continue;
            }
            let dot = first_end;
            let second_start = dot + 1;
            let second_end = scan_ident(s, second_start);
            // Resume after the dot so chained `schema.table.col` yields
            // both `schema.table` and `table.col`.
            pos = second_start;
            if second_end == second_start {
                continue;
            }
            return Some(&s[first_start..second_end]);
        }
        None
    })
}

/// Scan one identifier starting at byte offset `start`. Returns the byte
/// offset just past it, or `start` if no identifier is present.
fn scan_ident(s: &str, start: usize) -> usize {
    let bytes = s.as_bytes();
    if bytes.get(start) == Some(&b'"') {
        let mut i = start + 1;
        while i < s.len() {
            if bytes[i] != b'"' {
                i += 1;
                continue;
            }
            if bytes.get(i + 1) == Some(&b'"') {
                i += 2; // `""` escape inside a quoted identifier
            } else {
                return i + 1; // closing quote
            }
        }
        return start; // unterminated — decline
    }
    let mut iter = s[start..].char_indices();
    match iter.next() {
        Some((_, c)) if c.is_alphabetic() || c == '_' => {}
        _ => return start,
    }
    let mut end = start + s[start..].chars().next().unwrap().len_utf8();
    for (off, c) in iter {
        if c.is_alphanumeric() || c == '_' || c == '$' {
            end = start + off + c.len_utf8();
        } else {
            break;
        }
    }
    end
}

/// Compare two `Output` entries from an EXPLAIN plan, tolerating differences
/// in redundant outer-paren wrapping.
///
/// Postgres deparses the same computed expression with a different number of
/// outer paren pairs at different plan levels: empirically (PG 17) a Limit's
/// root target list emits `((b.x || 'y'::text))` while the underlying join
/// node's `Output` for the same expression is `(b.x || 'y'::text)`. The
/// two come from different deparse paths (`ruleutils.c` target-list rendering
/// vs. plan-output rendering in `explain.c`). Without normalization,
/// exact-string matching misses these and leaves nullability unset — and
/// because the choice of join algorithm can shift with table statistics,
/// `cargo sqlx prepare --check` can flip between runs on populated vs.
/// unpopulated databases.
fn outputs_match(a: &str, b: &str) -> bool {
    a == b || strip_redundant_outer_parens(a) == strip_redundant_outer_parens(b)
}

/// Strip balanced outer paren pairs from `s` (`((x))` → `x`).
///
/// A pair is balanced when the leading `(` matches the trailing `)` — verified
/// by walking the interior and ensuring paren depth never goes negative and
/// ends at zero. Postgres lexical literals (standard `'…'`, quoted identifier
/// `"…"`, escape `E'…'`, dollar-quoted `$tag$…$tag$`) are skipped as opaque
/// so parens inside them don't affect the count. If the interior can't be
/// tokenized cleanly (e.g. an unterminated literal), we conservatively decline
/// to strip.
fn strip_redundant_outer_parens(s: &str) -> &str {
    let mut s = s;
    while let Some(inner) = try_strip_one_paren_pair(s) {
        s = inner;
    }
    s
}

fn try_strip_one_paren_pair(s: &str) -> Option<&str> {
    let inner = s.strip_prefix('(')?.strip_suffix(')')?;
    let bytes = inner.as_bytes();
    let mut i = 0;
    let mut depth: i32 = 0;
    // Lexical forms below mirror Postgres's scanner (src/backend/parser/scan.l)
    // and the manual chapter on lexical structure:
    // https://www.postgresql.org/docs/current/sql-syntax-lexical.html
    while i < bytes.len() {
        i = match bytes[i..] {
            // Standard string constant — §4.1.2.1. `''` is the only in-string
            // escape (assuming `standard_conforming_strings = on`, the default,
            // and what `ruleutils.c` deparses against).
            [b'\'', ..] => skip_quoted_doubled(bytes, i + 1, b'\'')?,
            // Quoted identifier — §4.1.1. `""` is the in-identifier escape.
            [b'"', ..] => skip_quoted_doubled(bytes, i + 1, b'"')?,
            // C-style escape string `E'…'` — §4.1.2.2. Both `\'` and `''`
            // escape the quote; `\\` produces a literal backslash.
            [b'E' | b'e', b'\'', ..] => skip_e_string(bytes, i + 2)?,
            // Bit / hex string constant `B'…'` / `X'…'` — §4.1.2.5.
            [b'B' | b'b' | b'X' | b'x', b'\'', ..] => skip_quoted_doubled(bytes, i + 2, b'\'')?,
            // Unicode-escape string `U&'…'` (§4.1.2.3) or quoted identifier
            // `U&"…"` (§4.1.1). The scanner finds the terminator via the
            // doubled-quote rule; `\nnnn` / `\+nnnnnn` are expanded in a
            // post-pass and don't affect termination.
            [b'U' | b'u', b'&', q @ (b'\'' | b'"'), ..] => skip_quoted_doubled(bytes, i + 3, q)?,
            // Dollar-quoted string constant `$tag$…$tag$` — §4.1.2.4.
            // Also disambiguates parameter refs (`$1`) and stray `$`.
            [b'$', ..] => skip_dollar_or_pass(bytes, i + 1)?,
            [b'(', ..] => {
                depth += 1;
                i + 1
            }
            [b')', ..] => {
                depth -= 1;
                if depth < 0 {
                    return None;
                }
                i + 1
            }
            _ => i + 1,
        };
    }
    (depth == 0).then_some(inner)
}

/// Skip past the closing `quote` of a literal that uses doubled-quote
/// (`''` or `""`) as the in-literal escape. Returns the byte index AFTER
/// the closing quote, or `None` if the literal never terminates.
fn skip_quoted_doubled(bytes: &[u8], start: usize, quote: u8) -> Option<usize> {
    let mut i = start;
    loop {
        match bytes.get(i..)? {
            [q, q2, ..] if *q == quote && *q2 == quote => i += 2,
            [q, ..] if *q == quote => return Some(i + 1),
            [_, ..] => i += 1,
            [] => return None,
        }
    }
}

/// Skip past the closing `'` of an `E'…'` escape-string literal. Inside an
/// E-string both `\'` and `''` produce a literal quote; `\\` produces a
/// literal backslash (so the byte after `\\` does NOT start a new escape).
fn skip_e_string(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start;
    loop {
        match bytes.get(i..)? {
            [b'\\', _, ..] => i += 2,
            [b'\'', b'\'', ..] => i += 2,
            [b'\'', ..] => return Some(i + 1),
            [_, ..] => i += 1,
            [] => return None,
        }
    }
}

/// Handle the byte just after a `$`. If it opens a dollar-quoted string
/// (`$tag$…$tag$`, tag empty or `[A-Za-z_][A-Za-z0-9_]*`), skip past the
/// closing tag. Otherwise (parameter ref like `$1`, stray `$`, unterminated
/// `$tag$…`), just advance past the `$`.
fn skip_dollar_or_pass(bytes: &[u8], start: usize) -> Option<usize> {
    let suffix = &bytes[start..];
    let tag_len = suffix
        .iter()
        .position(|&b| !(b.is_ascii_alphanumeric() || b == b'_'))
        .unwrap_or(suffix.len());

    // Tag must end on a `$`; if non-empty, first char must be a letter or `_`.
    if suffix.get(tag_len) != Some(&b'$')
        || (tag_len > 0 && !(suffix[0].is_ascii_alphabetic() || suffix[0] == b'_'))
    {
        return Some(start);
    }

    let tag = &suffix[..tag_len];
    let body_start = start + tag_len + 1;
    find_dollar_close(bytes, body_start, tag).or(Some(start))
}

fn find_dollar_close(bytes: &[u8], start: usize, tag: &[u8]) -> Option<usize> {
    let needle_len = tag.len() + 2; // $ + tag + $
    bytes
        .get(start..)?
        .windows(needle_len)
        .position(|w| w[0] == b'$' && w[needle_len - 1] == b'$' && &w[1..needle_len - 1] == tag)
        .map(|p| start + p + needle_len)
}

#[derive(serde::Deserialize, Debug)]
#[serde(untagged)]
enum Explain {
    // NOTE: the returned JSON may not contain a `plan` field, for example, with `CALL` statements:
    // https://github.com/launchbadge/sqlx/issues/1449
    //
    // In this case, we should just fall back to assuming all is nullable.
    //
    // It may also contain additional fields we don't care about, which should not break parsing:
    // https://github.com/launchbadge/sqlx/issues/2587
    // https://github.com/launchbadge/sqlx/issues/2622
    Plan {
        #[serde(rename = "Plan")]
        plan: Plan,
    },

    // This ensures that parsing never technically fails.
    //
    // We don't want to specifically expect `"Utility Statement"` because there might be other cases
    // and we don't care unless it contains a query plan anyway.
    Other(serde::de::IgnoredAny),
}

#[derive(serde::Deserialize, Debug)]
struct Plan {
    #[serde(rename = "Join Type")]
    join_type: Option<String>,
    #[serde(rename = "Parent Relationship")]
    parent_relation: Option<String>,
    #[serde(rename = "Output")]
    output: Option<Vec<String>>,
    #[serde(rename = "Plans")]
    plans: Option<Vec<Plan>>,
}

#[test]
fn explain_parsing() {
    let normal_plan = r#"[
   {
     "Plan": {
       "Node Type": "Result",
       "Parallel Aware": false,
       "Async Capable": false,
       "Startup Cost": 0.00,
       "Total Cost": 0.01,
       "Plan Rows": 1,
       "Plan Width": 4,
       "Output": ["1"]
     }
   }
]"#;

    // https://github.com/launchbadge/sqlx/issues/2622
    let extra_field = r#"[
   {                                        
     "Plan": {                              
       "Node Type": "Result",               
       "Parallel Aware": false,             
       "Async Capable": false,              
       "Startup Cost": 0.00,                
       "Total Cost": 0.01,                  
       "Plan Rows": 1,                      
       "Plan Width": 4,                     
       "Output": ["1"]                      
     },                                     
     "Query Identifier": 1147616880456321454
   }                                        
]"#;

    // https://github.com/launchbadge/sqlx/issues/1449
    let utility_statement = r#"["Utility Statement"]"#;

    let normal_plan_parsed = serde_json::from_str::<[Explain; 1]>(normal_plan).unwrap();
    let extra_field_parsed = serde_json::from_str::<[Explain; 1]>(extra_field).unwrap();
    let utility_statement_parsed = serde_json::from_str::<[Explain; 1]>(utility_statement).unwrap();

    assert!(
        matches!(normal_plan_parsed, [Explain::Plan { plan: Plan { .. } }]),
        "unexpected parse from {normal_plan:?}: {normal_plan_parsed:?}"
    );

    assert!(
        matches!(extra_field_parsed, [Explain::Plan { plan: Plan { .. } }]),
        "unexpected parse from {extra_field:?}: {extra_field_parsed:?}"
    );

    assert!(
        matches!(utility_statement_parsed, [Explain::Other(_)]),
        "unexpected parse from {utility_statement:?}: {utility_statement_parsed:?}"
    )
}

#[cfg(test)]
fn nullables_from_plan(plan_json: &str) -> Vec<Option<bool>> {
    let [Explain::Plan { plan }] = serde_json::from_str::<[Explain; 1]>(plan_json).unwrap() else {
        panic!("expected Explain::Plan, got something else");
    };
    let outputs = plan.output.clone().unwrap_or_default();
    let mut nullables = vec![None; outputs.len()];
    visit_plan(&plan, false, &outputs, &mut nullables);
    nullables
}

#[test]
fn strip_redundant_outer_parens_cases() {
    assert_eq!(strip_redundant_outer_parens(""), "");
    assert_eq!(strip_redundant_outer_parens("a.id"), "a.id");
    assert_eq!(
        strip_redundant_outer_parens("(b.x || 'y'::text)"),
        "b.x || 'y'::text"
    );
    assert_eq!(
        strip_redundant_outer_parens("((b.x || 'y'::text))"),
        "b.x || 'y'::text"
    );
    assert_eq!(strip_redundant_outer_parens("(((x)))"), "x");
    // Not a single outer pair — leave alone.
    assert_eq!(strip_redundant_outer_parens("(a) + (b)"), "(a) + (b)");
    assert_eq!(
        strip_redundant_outer_parens("(a) + (b) + (c)"),
        "(a) + (b) + (c)"
    );
    // One outer pair around an inner sum of parenthesized terms — strip just one.
    assert_eq!(strip_redundant_outer_parens("((a) + (b))"), "(a) + (b)");
    // Quoted literal containing an unbalanced paren should not be miscounted.
    assert_eq!(
        strip_redundant_outer_parens("('foo(' || b.x)"),
        "'foo(' || b.x"
    );
    assert_eq!(
        strip_redundant_outer_parens("('it''s' || b.x)"),
        "'it''s' || b.x"
    );
    // Double-quoted identifier containing an unbalanced paren.
    assert_eq!(
        strip_redundant_outer_parens(r#"("we(ird" || b.x)"#),
        r#""we(ird" || b.x"#
    );
    assert_eq!(
        strip_redundant_outer_parens(r#"("a""b" || b.x)"#),
        r#""a""b" || b.x"#
    );
    // Unterminated literal: decline to strip rather than mis-count.
    assert_eq!(
        strip_redundant_outer_parens("('unterminated"),
        "('unterminated"
    );

    // E-strings: backslash escape, `\'` is escaped, `\\` is literal backslash
    // (so the following `'` is the terminator).
    assert_eq!(
        strip_redundant_outer_parens(r"(E'foo\nbar')"),
        r"E'foo\nbar'"
    );
    assert_eq!(
        strip_redundant_outer_parens(r"(E'a\'b' || x)"),
        r"E'a\'b' || x"
    );
    assert_eq!(
        strip_redundant_outer_parens(r"(E'a\\' || x)"),
        r"E'a\\' || x"
    );
    // Lowercase `e` prefix is also valid.
    assert_eq!(strip_redundant_outer_parens(r"(e'(' || x)"), r"e'(' || x");
    // An unprefixed `E` followed by anything other than `'` is just an identifier.
    assert_eq!(strip_redundant_outer_parens("(E + x)"), "E + x");

    // Dollar-quoted strings: empty tag and named tag.
    assert_eq!(
        strip_redundant_outer_parens("($$weird('$$ || x)"),
        "$$weird('$$ || x"
    );
    assert_eq!(
        strip_redundant_outer_parens("($tag$has $$nested$$ stuff$tag$ || x)"),
        "$tag$has $$nested$$ stuff$tag$ || x"
    );
    // Parameter reference `$1` must NOT be treated as a dollar-quote start.
    assert_eq!(strip_redundant_outer_parens("($1 + $2)"), "$1 + $2");
    // Dollar-tag with invalid first char falls through to plain `$`.
    assert_eq!(strip_redundant_outer_parens("($9$ + 1)"), "$9$ + 1");
    // Dollar-quoted body containing parens must not affect depth count
    // (PG docs §4.1.2.4).
    assert_eq!(
        strip_redundant_outer_parens("($tag$ has ) and ( in body $tag$ || x)"),
        "$tag$ has ) and ( in body $tag$ || x"
    );
    // Unterminated dollar-quote: tokenizer falls back to advancing past the
    // single `$`. Inner unbalanced parens then prevent stripping.
    assert_eq!(
        strip_redundant_outer_parens("($tag$ has ( unclosed)"),
        "($tag$ has ( unclosed)"
    );

    // Bit-string and hex-string constants (PG docs §4.1.2.5).
    assert_eq!(
        strip_redundant_outer_parens("(B'10(1' || x)"),
        "B'10(1' || x"
    );
    assert_eq!(
        strip_redundant_outer_parens("(X'1F)F' || x)"),
        "X'1F)F' || x"
    );
    // Lowercase prefixes are also valid.
    assert_eq!(strip_redundant_outer_parens("(b'(' || x)"), "b'(' || x");
    assert_eq!(strip_redundant_outer_parens("(x')' || y)"), "x')' || y");
    // `B` / `X` not followed by `'` are ordinary identifiers/keywords.
    assert_eq!(strip_redundant_outer_parens("(B + 1)"), "B + 1");
    assert_eq!(strip_redundant_outer_parens("(X + 1)"), "X + 1");

    // Unicode escape strings and identifiers (PG docs §§4.1.1, 4.1.2.3).
    assert_eq!(
        strip_redundant_outer_parens(r"(U&'\0028' || x)"),
        r"U&'\0028' || x"
    );
    assert_eq!(
        strip_redundant_outer_parens(r#"(U&"weird(col" || x)"#),
        r#"U&"weird(col" || x"#
    );
    // `U&` not followed by `'` or `"` is not a Unicode-escape prefix.
    assert_eq!(strip_redundant_outer_parens("(U & 1)"), "U & 1");
    assert_eq!(strip_redundant_outer_parens("(U + 1)"), "U + 1");
}

#[test]
fn outputs_match_cases() {
    assert!(outputs_match("a.id", "a.id"));
    assert!(outputs_match("(b.x)", "b.x"));
    assert!(outputs_match("((b.x || 'y'::text))", "(b.x || 'y'::text)"));
    assert!(!outputs_match("a.id", "b.id"));
    assert!(!outputs_match("(a + b)", "(a - b)"));
}

#[test]
fn qualified_col_refs_cases() {
    let collect = |s| qualified_col_refs(s).collect::<Vec<_>>();

    // Bare qualified column.
    assert_eq!(collect("a.id"), vec!["a.id"]);
    // Wrapped in parens / part of an expression.
    assert_eq!(collect("(b.x || 'y'::text)"), vec!["b.x"]);
    // Multiple refs.
    assert_eq!(collect("(a.id = b.id)"), vec!["a.id", "b.id"]);
    // Casts and unqualified identifiers don't match.
    assert!(collect("'y'::text").is_empty());
    assert!(collect("count(*)").is_empty());
    // Subplan / initplan refs — Postgres deparses these without qualifying
    // the identifier, so the SubPlan-as-output case stays unmarked.
    assert!(collect("(SubPlan 1)").is_empty());
    assert!(collect("(InitPlan 1).col1").is_empty());
    // Function calls.
    assert_eq!(collect("lpad(b.x, 10, ' '::text)"), vec!["b.x"]);
    // Parameter refs.
    assert!(collect("$1").is_empty());
    assert!(collect("$1 + $2").is_empty());
    // Identifier starting with underscore.
    assert_eq!(collect("_t.col"), vec!["_t.col"]);
    // Dollar sign allowed in continuation positions (not as a start).
    assert_eq!(collect("t.my$col"), vec!["t.my$col"]);
    assert!(collect("$1.x").is_empty());
    // Schema-qualified `schema.table.col` yields both `schema.table` and
    // `table.col` so the latter still matches a leaf scan's bare `table.col`.
    assert_eq!(collect("public.b.name"), vec!["public.b", "b.name"]);
    // Non-ASCII letters and digits in unquoted identifiers.
    assert_eq!(collect("café.id"), vec!["café.id"]);
    assert_eq!(collect("(t.数据 = u.x)"), vec!["t.数据", "u.x"]);
    // Quoted identifier with a space, with `""` as the inner escape.
    assert_eq!(collect(r#""my col".x"#), vec![r#""my col".x"#]);
    assert_eq!(collect(r#"t."a""b""#), vec![r#"t."a""b""#]);
    // Bare closing-quote in the middle is just opaque body, not a token end.
    assert_eq!(collect(r#""we""ird".x"#), vec![r#""we""ird".x"#]);
    // Unterminated quoted identifier is declined.
    assert!(collect(r#""unterminated"#).is_empty());
}

// https://github.com/launchbadge/sqlx/issues/3202
//
// PostgreSQL rewrites `A LEFT JOIN B` as `B RIGHT JOIN A` to put the smaller
// relation on the hash-build side. After the swap, the SQL right operand (the
// nullable side) appears as the plan's `Outer` child, not the `Inner`.
//
// Plan is verbatim EXPLAIN (VERBOSE, FORMAT JSON) output of (the only SET
// here, `plan_cache_mode`, is what `sqlx-macros-core` itself runs on each
// connection used for describe):
//
//     CREATE TABLE a (id uuid NOT NULL);
//     CREATE TABLE b (id uuid NOT NULL, name text NOT NULL);
//     INSERT INTO a SELECT gen_random_uuid() FROM generate_series(1, 1000);
//     INSERT INTO b SELECT gen_random_uuid(), 'b' FROM generate_series(1, 50000);
//     ANALYZE a; ANALYZE b;
//     SET plan_cache_mode = force_generic_plan;
//     PREPARE q(int) AS
//       SELECT a.id, b.name FROM a LEFT JOIN b ON a.id = b.id LIMIT $1;
//     EXPLAIN (VERBOSE, FORMAT JSON) EXECUTE q(NULL);
#[test]
fn nullable_inference_left_join_rewritten_as_right() {
    let plan = r#"
        [
          {
            "Plan": {
              "Node Type": "Limit",
              "Parallel Aware": false,
              "Async Capable": false,
              "Startup Cost": 28.50,
              "Total Cost": 130.15,
              "Plan Rows": 100,
              "Plan Width": 18,
              "Output": ["a.id", "b.name"],
              "Plans": [
                {
                  "Node Type": "Hash Join",
                  "Parent Relationship": "Outer",
                  "Parallel Aware": false,
                  "Async Capable": false,
                  "Join Type": "Right",
                  "Startup Cost": 28.50,
                  "Total Cost": 1045.00,
                  "Plan Rows": 1000,
                  "Plan Width": 18,
                  "Output": ["a.id", "b.name"],
                  "Inner Unique": false,
                  "Hash Cond": "(b.id = a.id)",
                  "Plans": [
                    {
                      "Node Type": "Seq Scan",
                      "Parent Relationship": "Outer",
                      "Parallel Aware": false,
                      "Async Capable": false,
                      "Relation Name": "b",
                      "Schema": "public",
                      "Alias": "b",
                      "Startup Cost": 0.00,
                      "Total Cost": 819.00,
                      "Plan Rows": 50000,
                      "Plan Width": 18,
                      "Output": ["b.id", "b.name"]
                    },
                    {
                      "Node Type": "Hash",
                      "Parent Relationship": "Inner",
                      "Parallel Aware": false,
                      "Async Capable": false,
                      "Startup Cost": 16.00,
                      "Total Cost": 16.00,
                      "Plan Rows": 1000,
                      "Plan Width": 16,
                      "Output": ["a.id"],
                      "Plans": [
                        {
                          "Node Type": "Seq Scan",
                          "Parent Relationship": "Outer",
                          "Parallel Aware": false,
                          "Async Capable": false,
                          "Relation Name": "a",
                          "Schema": "public",
                          "Alias": "a",
                          "Startup Cost": 0.00,
                          "Total Cost": 16.00,
                          "Plan Rows": 1000,
                          "Plan Width": 16,
                          "Output": ["a.id"]
                        }
                      ]
                    }
                  ]
                }
              ]
            }
          }
        ]
    "#;
    // a.id (Inner branch, SQL left operand): preserved
    // b.name (Outer branch, SQL right operand): nullable
    assert_eq!(nullables_from_plan(plan), vec![None, Some(true)]);
}

// Two nested LEFT JOINs both rewritten as Hash Right Join. Exercises
// (a) recursion through a non-join `Hash` node sitting between two join
// nodes, and (b) the rewrite being handled at every level.
//
// Plan is verbatim EXPLAIN (VERBOSE, FORMAT JSON) output of:
//
//     CREATE TABLE c (id uuid NOT NULL, x text NOT NULL);
//     INSERT INTO c SELECT gen_random_uuid(), 'c' FROM generate_series(1, 50000);
//     ANALYZE c;
//     -- `a` and `b` are seeded as in the test above
//     SET plan_cache_mode = force_generic_plan;
//     PREPARE q(int) AS
//       SELECT a.id, b.name, c.x
//       FROM a
//       LEFT JOIN b ON a.id = b.id
//       LEFT JOIN c ON a.id = c.id
//       LIMIT $1;
//     EXPLAIN (VERBOSE, FORMAT JSON) EXECUTE q(NULL);
#[test]
fn nullable_inference_nested_left_joins_rewritten() {
    let plan = r#"
        [
          {
            "Plan": {
              "Node Type": "Limit",
              "Parallel Aware": false,
              "Async Capable": false,
              "Startup Cost": 1057.50,
              "Total Cost": 1159.15,
              "Plan Rows": 100,
              "Plan Width": 20,
              "Output": ["a.id", "b.name", "c.x"],
              "Plans": [
                {
                  "Node Type": "Hash Join",
                  "Parent Relationship": "Outer",
                  "Parallel Aware": false,
                  "Async Capable": false,
                  "Join Type": "Right",
                  "Startup Cost": 1057.50,
                  "Total Cost": 2074.00,
                  "Plan Rows": 1000,
                  "Plan Width": 20,
                  "Output": ["a.id", "b.name", "c.x"],
                  "Inner Unique": false,
                  "Hash Cond": "(c.id = a.id)",
                  "Plans": [
                    {
                      "Node Type": "Seq Scan",
                      "Parent Relationship": "Outer",
                      "Parallel Aware": false,
                      "Async Capable": false,
                      "Relation Name": "c",
                      "Schema": "public",
                      "Alias": "c",
                      "Startup Cost": 0.00,
                      "Total Cost": 819.00,
                      "Plan Rows": 50000,
                      "Plan Width": 18,
                      "Output": ["c.id", "c.x"]
                    },
                    {
                      "Node Type": "Hash",
                      "Parent Relationship": "Inner",
                      "Parallel Aware": false,
                      "Async Capable": false,
                      "Startup Cost": 1045.00,
                      "Total Cost": 1045.00,
                      "Plan Rows": 1000,
                      "Plan Width": 18,
                      "Output": ["a.id", "b.name"],
                      "Plans": [
                        {
                          "Node Type": "Hash Join",
                          "Parent Relationship": "Outer",
                          "Parallel Aware": false,
                          "Async Capable": false,
                          "Join Type": "Right",
                          "Startup Cost": 28.50,
                          "Total Cost": 1045.00,
                          "Plan Rows": 1000,
                          "Plan Width": 18,
                          "Output": ["a.id", "b.name"],
                          "Inner Unique": false,
                          "Hash Cond": "(b.id = a.id)",
                          "Plans": [
                            {
                              "Node Type": "Seq Scan",
                              "Parent Relationship": "Outer",
                              "Parallel Aware": false,
                              "Async Capable": false,
                              "Relation Name": "b",
                              "Schema": "public",
                              "Alias": "b",
                              "Startup Cost": 0.00,
                              "Total Cost": 819.00,
                              "Plan Rows": 50000,
                              "Plan Width": 18,
                              "Output": ["b.id", "b.name"]
                            },
                            {
                              "Node Type": "Hash",
                              "Parent Relationship": "Inner",
                              "Parallel Aware": false,
                              "Async Capable": false,
                              "Startup Cost": 16.00,
                              "Total Cost": 16.00,
                              "Plan Rows": 1000,
                              "Plan Width": 16,
                              "Output": ["a.id"],
                              "Plans": [
                                {
                                  "Node Type": "Seq Scan",
                                  "Parent Relationship": "Outer",
                                  "Parallel Aware": false,
                                  "Async Capable": false,
                                  "Relation Name": "a",
                                  "Schema": "public",
                                  "Alias": "a",
                                  "Startup Cost": 0.00,
                                  "Total Cost": 16.00,
                                  "Plan Rows": 1000,
                                  "Plan Width": 16,
                                  "Output": ["a.id"]
                                }
                              ]
                            }
                          ]
                        }
                      ]
                    }
                  ]
                }
              ]
            }
          }
        ]
    "#;
    // a.id (driving table) preserved through both LEFT JOINs.
    // b.name and c.x become NULL when their respective JOIN finds no match.
    assert_eq!(
        nullables_from_plan(plan),
        vec![None, Some(true), Some(true)]
    );
}

// https://github.com/transact-rs/sqlx/pull/4285#issuecomment-4572525414
//
// Postgres deparses the same computed expression with a different number of
// outer paren pairs at different plan levels: the root target list emits
// `((<expr>))` while the underlying join/projection node emits `(<expr>)`.
// The exact-string match in `visit_plan` then misses, leaving the column's
// nullability unset.
//
// Hand-built plan models the Right-join (build/probe-swapped) shape with
// the computed expression pushed onto a nullable-side child node so the
// paren-mismatch path is exercised directly. The plan is loosely modeled
// on:
//
//     CREATE TABLE a (id uuid NOT NULL);
//     CREATE TABLE b (id uuid NOT NULL, x text NOT NULL);
//     SELECT a.id, b.x || 'y' FROM a LEFT JOIN b ON a.id = b.id LIMIT 100;
#[test]
fn nullable_inference_root_output_has_extra_outer_parens() {
    let plan = r#"
        [
          {
            "Plan": {
              "Node Type": "Limit",
              "Parallel Aware": false,
              "Async Capable": false,
              "Startup Cost": 0.00,
              "Total Cost": 130.15,
              "Plan Rows": 100,
              "Plan Width": 36,
              "Output": ["a.id", "((b.x || 'y'::text))"],
              "Plans": [
                {
                  "Node Type": "Hash Join",
                  "Parent Relationship": "Outer",
                  "Parallel Aware": false,
                  "Async Capable": false,
                  "Join Type": "Right",
                  "Startup Cost": 28.50,
                  "Total Cost": 1045.00,
                  "Plan Rows": 1000,
                  "Plan Width": 36,
                  "Output": ["a.id", "(b.x || 'y'::text)"],
                  "Inner Unique": false,
                  "Hash Cond": "(b.id = a.id)",
                  "Plans": [
                    {
                      "Node Type": "Seq Scan",
                      "Parent Relationship": "Outer",
                      "Parallel Aware": false,
                      "Async Capable": false,
                      "Relation Name": "b",
                      "Schema": "public",
                      "Alias": "b",
                      "Startup Cost": 0.00,
                      "Total Cost": 819.00,
                      "Plan Rows": 50000,
                      "Plan Width": 36,
                      "Output": ["b.id", "(b.x || 'y'::text)"]
                    },
                    {
                      "Node Type": "Hash",
                      "Parent Relationship": "Inner",
                      "Parallel Aware": false,
                      "Async Capable": false,
                      "Startup Cost": 16.00,
                      "Total Cost": 16.00,
                      "Plan Rows": 1000,
                      "Plan Width": 16,
                      "Output": ["a.id"],
                      "Plans": [
                        {
                          "Node Type": "Seq Scan",
                          "Parent Relationship": "Outer",
                          "Parallel Aware": false,
                          "Async Capable": false,
                          "Relation Name": "a",
                          "Schema": "public",
                          "Alias": "a",
                          "Startup Cost": 0.00,
                          "Total Cost": 16.00,
                          "Plan Rows": 1000,
                          "Plan Width": 16,
                          "Output": ["a.id"]
                        }
                      ]
                    }
                  ]
                }
              ]
            }
          }
        ]
    "#;
    // a.id (Inner branch, SQL left operand): preserved.
    // The b.x-derived expression (Outer branch, SQL right operand): nullable.
    assert_eq!(nullables_from_plan(plan), vec![None, Some(true)]);
}

// Verbatim EXPLAIN (VERBOSE, FORMAT JSON) output captured from PG 17 against
// an unpopulated DB (no ANALYZE, `plan_cache_mode = force_generic_plan`) for:
//
//     CREATE TABLE a (id uuid NOT NULL);
//     CREATE TABLE b (id uuid NOT NULL, x text NOT NULL);
//     PREPARE q(int) AS
//       SELECT a.id, b.x || 'y' FROM a LEFT JOIN b ON a.id = b.id LIMIT $1;
//     EXPLAIN (VERBOSE, FORMAT JSON) EXECUTE q(NULL);
//
// The computed expression `(b.x || 'y'::text)` lives on the Hash Join node
// itself; its nullable child (Hash) only carries the raw column outputs
// `b.x` / `b.id`. So the children-only walk inside `visit_plan` never sees
// the computed expression, and the root output `((b.x || 'y'::text))`
// stays unmarked. Expected: the second column is nullable.
#[test]
fn nullable_inference_left_join_computed_expression() {
    let plan = r#"
        [
          {
            "Plan": {
              "Node Type": "Limit",
              "Parallel Aware": false,
              "Async Capable": false,
              "Startup Cost": 34.08,
              "Total Cost": 74.51,
              "Plan Rows": 990,
              "Plan Width": 48,
              "Output": ["a.id", "((b.x || 'y'::text))"],
              "Plans": [
                {
                  "Node Type": "Hash Join",
                  "Parent Relationship": "Outer",
                  "Parallel Aware": false,
                  "Async Capable": false,
                  "Join Type": "Left",
                  "Startup Cost": 34.08,
                  "Total Cost": 438.36,
                  "Plan Rows": 9898,
                  "Plan Width": 48,
                  "Output": ["a.id", "(b.x || 'y'::text)"],
                  "Inner Unique": false,
                  "Hash Cond": "(a.id = b.id)",
                  "Plans": [
                    {
                      "Node Type": "Seq Scan",
                      "Parent Relationship": "Outer",
                      "Parallel Aware": false,
                      "Async Capable": false,
                      "Relation Name": "a",
                      "Schema": "public",
                      "Alias": "a",
                      "Startup Cost": 0.00,
                      "Total Cost": 28.50,
                      "Plan Rows": 1850,
                      "Plan Width": 16,
                      "Output": ["a.id"]
                    },
                    {
                      "Node Type": "Hash",
                      "Parent Relationship": "Inner",
                      "Parallel Aware": false,
                      "Async Capable": false,
                      "Startup Cost": 20.70,
                      "Total Cost": 20.70,
                      "Plan Rows": 1070,
                      "Plan Width": 48,
                      "Output": ["b.x", "b.id"],
                      "Plans": [
                        {
                          "Node Type": "Seq Scan",
                          "Parent Relationship": "Outer",
                          "Parallel Aware": false,
                          "Async Capable": false,
                          "Relation Name": "b",
                          "Schema": "public",
                          "Alias": "b",
                          "Startup Cost": 0.00,
                          "Total Cost": 20.70,
                          "Plan Rows": 1070,
                          "Plan Width": 48,
                          "Output": ["b.x", "b.id"]
                        }
                      ]
                    }
                  ]
                }
              ]
            }
          }
        ]
    "#;
    assert_eq!(nullables_from_plan(plan), vec![None, Some(true)]);
}

// Same scenario as above but with a function call instead of `||`:
//
//     SELECT a.id, lpad(b.x, 10) FROM a LEFT JOIN b ON a.id = b.id LIMIT $1;
//
// Verifies the join-node-output classification isn't peculiar to operator
// expressions.
#[test]
fn nullable_inference_left_join_function_call() {
    let plan = r#"
        [
          {
            "Plan": {
              "Node Type": "Limit",
              "Parallel Aware": false,
              "Async Capable": false,
              "Startup Cost": 34.08,
              "Total Cost": 74.51,
              "Plan Rows": 990,
              "Plan Width": 48,
              "Output": ["a.id", "(lpad(b.x, 10, ' '::text))"],
              "Plans": [
                {
                  "Node Type": "Hash Join",
                  "Parent Relationship": "Outer",
                  "Parallel Aware": false,
                  "Async Capable": false,
                  "Join Type": "Left",
                  "Startup Cost": 34.08,
                  "Total Cost": 438.36,
                  "Plan Rows": 9898,
                  "Plan Width": 48,
                  "Output": ["a.id", "lpad(b.x, 10, ' '::text)"],
                  "Inner Unique": false,
                  "Hash Cond": "(a.id = b.id)",
                  "Plans": [
                    {
                      "Node Type": "Seq Scan",
                      "Parent Relationship": "Outer",
                      "Parallel Aware": false,
                      "Async Capable": false,
                      "Relation Name": "a",
                      "Schema": "public",
                      "Alias": "a",
                      "Startup Cost": 0.00,
                      "Total Cost": 28.50,
                      "Plan Rows": 1850,
                      "Plan Width": 16,
                      "Output": ["a.id"]
                    },
                    {
                      "Node Type": "Hash",
                      "Parent Relationship": "Inner",
                      "Parallel Aware": false,
                      "Async Capable": false,
                      "Startup Cost": 20.70,
                      "Total Cost": 20.70,
                      "Plan Rows": 1070,
                      "Plan Width": 48,
                      "Output": ["b.x", "b.id"],
                      "Plans": [
                        {
                          "Node Type": "Seq Scan",
                          "Parent Relationship": "Outer",
                          "Parallel Aware": false,
                          "Async Capable": false,
                          "Relation Name": "b",
                          "Schema": "public",
                          "Alias": "b",
                          "Startup Cost": 0.00,
                          "Total Cost": 20.70,
                          "Plan Rows": 1070,
                          "Plan Width": 48,
                          "Output": ["b.x", "b.id"]
                        }
                      ]
                    }
                  ]
                }
              ]
            }
          }
        ]
    "#;
    assert_eq!(nullables_from_plan(plan), vec![None, Some(true)]);
}

// Captured from PG 17 for:
//
//     SELECT a.id, b.x || 'y' FROM a LEFT JOIN b ON a.id = b.id
//     ORDER BY a.id LIMIT $1;
//
// The planner picks Merge Join over Hash Join here. Same bug pattern: the
// `(b.x || 'y'::text)` expression lives on the Merge Join node itself; its
// Inner (nullable) Sort child only forwards raw `b.x` / `b.id`.
#[test]
fn nullable_inference_merge_join_left_computed_expression() {
    let plan = r#"
        [
          {
            "Plan": {
              "Node Type": "Limit",
              "Parallel Aware": false,
              "Async Capable": false,
              "Startup Cost": 203.43,
              "Total Cost": 221.68,
              "Plan Rows": 990,
              "Plan Width": 48,
              "Output": ["a.id", "((b.x || 'y'::text))"],
              "Plans": [
                {
                  "Node Type": "Merge Join",
                  "Parent Relationship": "Outer",
                  "Parallel Aware": false,
                  "Async Capable": false,
                  "Join Type": "Left",
                  "Startup Cost": 203.43,
                  "Total Cost": 385.90,
                  "Plan Rows": 9898,
                  "Plan Width": 48,
                  "Output": ["a.id", "(b.x || 'y'::text)"],
                  "Inner Unique": false,
                  "Merge Cond": "(a.id = b.id)",
                  "Plans": [
                    {
                      "Node Type": "Sort",
                      "Parent Relationship": "Outer",
                      "Parallel Aware": false,
                      "Async Capable": false,
                      "Startup Cost": 128.89,
                      "Total Cost": 133.52,
                      "Plan Rows": 1850,
                      "Plan Width": 16,
                      "Output": ["a.id"],
                      "Sort Key": ["a.id"],
                      "Plans": [
                        {
                          "Node Type": "Seq Scan",
                          "Parent Relationship": "Outer",
                          "Parallel Aware": false,
                          "Async Capable": false,
                          "Relation Name": "a",
                          "Schema": "public",
                          "Alias": "a",
                          "Startup Cost": 0.00,
                          "Total Cost": 28.50,
                          "Plan Rows": 1850,
                          "Plan Width": 16,
                          "Output": ["a.id"]
                        }
                      ]
                    },
                    {
                      "Node Type": "Sort",
                      "Parent Relationship": "Inner",
                      "Parallel Aware": false,
                      "Async Capable": false,
                      "Startup Cost": 74.54,
                      "Total Cost": 77.21,
                      "Plan Rows": 1070,
                      "Plan Width": 48,
                      "Output": ["b.x", "b.id"],
                      "Sort Key": ["b.id"],
                      "Plans": [
                        {
                          "Node Type": "Seq Scan",
                          "Parent Relationship": "Outer",
                          "Parallel Aware": false,
                          "Async Capable": false,
                          "Relation Name": "b",
                          "Schema": "public",
                          "Alias": "b",
                          "Startup Cost": 0.00,
                          "Total Cost": 20.70,
                          "Plan Rows": 1070,
                          "Plan Width": 48,
                          "Output": ["b.x", "b.id"]
                        }
                      ]
                    }
                  ]
                }
              ]
            }
          }
        ]
    "#;
    assert_eq!(nullables_from_plan(plan), vec![None, Some(true)]);
}

// https://github.com/transact-rs/sqlx/pull/4285#issuecomment-4587339482
//
// When a join's `Output` contains references to sibling subplans (Postgres
// deparses them as `(SubPlan N)` / `(InitPlan N)`), those references must
// not be marked nullable by the join above: subplans are computed
// independently of the join's NULL extension, so their nullability is
// genuinely unknown to this pass.
//
// Verbatim EXPLAIN (VERBOSE, FORMAT JSON) output captured from PG 17 for:
//
//     CREATE TABLE a (id uuid NOT NULL);
//     CREATE TABLE b (id uuid NOT NULL, name text NOT NULL);
//     CREATE TABLE c (a_id uuid NOT NULL, val int NOT NULL);
//     INSERT INTO a SELECT gen_random_uuid() FROM generate_series(1, 1000);
//     INSERT INTO b SELECT gen_random_uuid(), 'b' FROM generate_series(1, 1000);
//     INSERT INTO c SELECT gen_random_uuid(), s FROM generate_series(1, 5000) s;
//     ANALYZE a; ANALYZE b; ANALYZE c;
//     SET plan_cache_mode = force_generic_plan;
//     PREPARE q AS
//       SELECT a.id,
//              (SELECT count(*) FROM c WHERE c.a_id = a.id) AS cnt1,
//              (SELECT max(val)  FROM c WHERE c.a_id = a.id) AS cnt2,
//              b.name
//       FROM a LEFT JOIN b ON a.id = b.id;
//     EXPLAIN (VERBOSE, FORMAT JSON) EXECUTE q;
#[test]
fn nullable_inference_subplan_outputs_not_marked_nullable() {
    let plan = r#"
        [
          {
            "Plan": {
              "Node Type": "Hash Join",
              "Parallel Aware": false,
              "Async Capable": false,
              "Join Type": "Right",
              "Startup Cost": 28.50,
              "Total Cost": 189084.25,
              "Plan Rows": 1000,
              "Plan Width": 30,
              "Output": ["a.id", "(SubPlan 1)", "(SubPlan 2)", "b.name"],
              "Inner Unique": false,
              "Hash Cond": "(b.id = a.id)",
              "Plans": [
                {
                  "Node Type": "Seq Scan",
                  "Parent Relationship": "Outer",
                  "Parallel Aware": false,
                  "Async Capable": false,
                  "Relation Name": "b",
                  "Schema": "public",
                  "Alias": "b",
                  "Startup Cost": 0.00,
                  "Total Cost": 17.00,
                  "Plan Rows": 1000,
                  "Plan Width": 18,
                  "Output": ["b.id", "b.name"]
                },
                {
                  "Node Type": "Hash",
                  "Parent Relationship": "Inner",
                  "Parallel Aware": false,
                  "Async Capable": false,
                  "Startup Cost": 16.00,
                  "Total Cost": 16.00,
                  "Plan Rows": 1000,
                  "Plan Width": 16,
                  "Output": ["a.id"],
                  "Plans": [
                    {
                      "Node Type": "Seq Scan",
                      "Parent Relationship": "Outer",
                      "Parallel Aware": false,
                      "Async Capable": false,
                      "Relation Name": "a",
                      "Schema": "public",
                      "Alias": "a",
                      "Startup Cost": 0.00,
                      "Total Cost": 16.00,
                      "Plan Rows": 1000,
                      "Plan Width": 16,
                      "Output": ["a.id"]
                    }
                  ]
                },
                {
                  "Node Type": "Aggregate",
                  "Strategy": "Plain",
                  "Partial Mode": "Simple",
                  "Parent Relationship": "SubPlan",
                  "Subplan Name": "SubPlan 1",
                  "Parallel Aware": false,
                  "Async Capable": false,
                  "Startup Cost": 94.50,
                  "Total Cost": 94.51,
                  "Plan Rows": 1,
                  "Plan Width": 8,
                  "Output": ["count(*)"],
                  "Plans": [
                    {
                      "Node Type": "Seq Scan",
                      "Parent Relationship": "Outer",
                      "Parallel Aware": false,
                      "Async Capable": false,
                      "Relation Name": "c",
                      "Schema": "public",
                      "Alias": "c",
                      "Startup Cost": 0.00,
                      "Total Cost": 94.50,
                      "Plan Rows": 1,
                      "Plan Width": 0,
                      "Output": ["c.a_id", "c.val"],
                      "Filter": "(c.a_id = a.id)"
                    }
                  ]
                },
                {
                  "Node Type": "Aggregate",
                  "Strategy": "Plain",
                  "Partial Mode": "Simple",
                  "Parent Relationship": "SubPlan",
                  "Subplan Name": "SubPlan 2",
                  "Parallel Aware": false,
                  "Async Capable": false,
                  "Startup Cost": 94.50,
                  "Total Cost": 94.51,
                  "Plan Rows": 1,
                  "Plan Width": 4,
                  "Output": ["max(c_1.val)"],
                  "Plans": [
                    {
                      "Node Type": "Seq Scan",
                      "Parent Relationship": "Outer",
                      "Parallel Aware": false,
                      "Async Capable": false,
                      "Relation Name": "c",
                      "Schema": "public",
                      "Alias": "c_1",
                      "Startup Cost": 0.00,
                      "Total Cost": 94.50,
                      "Plan Rows": 1,
                      "Plan Width": 4,
                      "Output": ["c_1.a_id", "c_1.val"],
                      "Filter": "(c_1.a_id = a.id)"
                    }
                  ]
                }
              ]
            }
          }
        ]
    "#;
    assert_eq!(
        nullables_from_plan(plan),
        vec![None, None, None, Some(true)]
    );
}
