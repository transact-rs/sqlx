use crate::error::Error;
use crate::io::StatementId;
use crate::query_as::query_as;
use crate::statement::PgStatementMetadata;
use crate::types::Json;
use crate::PgConnection;
use smallvec::SmallVec;
use sqlx_core::query_builder::QueryBuilder;
use sqlx_core::sql_str::AssertSqlSafe;

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
            visit_plan(plan, None, outputs, &mut nullables);
        }

        Ok(nullables)
    }
}

fn visit_plan(
    plan: &Plan,
    parent_join_type: Option<&str>,
    outputs: &[String],
    nullables: &mut Vec<Option<bool>>,
) {
    if let Some(plan_outputs) = &plan.output {
        // Determine whether THIS plan's outputs can be NULL due to its parent join.
        //
        // PostgreSQL may execute `A LEFT JOIN B` as a `Right` join when the planner
        // swaps the build/probe sides for hash join efficiency (e.g. when B is the
        // smaller of the two and is cheaper as the hash-build side). After that
        // swap, the operand that *was* the SQL right side (B, the nullable one)
        // appears as the "Outer" child of the plan node — not the "Inner" child.
        //
        // So the side that needs the nullable mark depends on `parent_join_type`:
        //   * Left  : Inner child is the nullable side (SQL right operand)
        //   * Right : Outer child is the nullable side (SQL right operand, after swap)
        //   * Full  : both sides nullable
        let parent_nulls_this_side = matches!(
            (parent_join_type, plan.parent_relation.as_deref()),
            (Some("Full"), _) | (Some("Left"), Some("Inner")) | (Some("Right"), Some("Outer"))
        );

        let self_is_full_join = plan.join_type.as_deref() == Some("Full");

        if parent_nulls_this_side || self_is_full_join {
            for output in plan_outputs {
                if let Some(i) = outputs.iter().position(|o| o == output) {
                    // N.B. this may produce false positives but those don't cause runtime errors
                    nullables[i] = Some(true);
                }
            }
        }
    }

    if let Some(plans) = &plan.plans {
        // Recurse into all child plans so nested LEFT/RIGHT joins are reached even
        // if intermediate nodes are not joins themselves (e.g. a `Hash` node sitting
        // between two join nodes).
        for child in plans {
            visit_plan(child, plan.join_type.as_deref(), outputs, nullables);
        }
    }
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
    visit_plan(&plan, None, &outputs, &mut nullables);
    nullables
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
