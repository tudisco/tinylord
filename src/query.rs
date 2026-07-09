//! Query language compiler (§8).
//!
//! Compiles the deliberately-minimal Mongo-ish filter into parameterized SQL.
//! JSON paths are validated to a safe charset and inlined as string literals;
//! all *values* are bound parameters — client input never reaches SQL as text.
//! Supported operators: `$eq $ne $gt $gte $lt $lte $in`, plus a single
//! top-level `$or`. Nested boolean trees are rejected.

use crate::errors::{ApiError, ApiResult};
use crate::ids::field_to_json_path;
use rusqlite::types::Value;
use serde::Deserialize;

/// A compiled WHERE clause: an SQL boolean expression plus its bound values.
#[derive(Debug)]
pub struct WhereClause {
    /// SQL boolean expression (never empty; `"1"` means match-all).
    pub sql: String,
    pub params: Vec<Value>,
}

/// The query request body (§8).
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct QueryRequest {
    pub filter: Option<serde_json::Value>,
    pub sort: Vec<(String, String)>,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
    pub cursor: Option<String>,
    pub projection: Option<Vec<String>>,
}

/// The count request body (§7.5).
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct CountRequest {
    pub filter: Option<serde_json::Value>,
}

/// Compile a filter object into a WHERE clause. `None`/empty → match all.
pub fn compile_filter(filter: Option<&serde_json::Value>) -> ApiResult<WhereClause> {
    let mut params = Vec::new();
    let Some(filter) = filter else {
        return Ok(WhereClause {
            sql: "1".to_string(),
            params,
        });
    };
    if filter.is_null() {
        return Ok(WhereClause {
            sql: "1".to_string(),
            params,
        });
    }
    let obj = filter
        .as_object()
        .ok_or_else(|| ApiError::validation("filter must be a JSON object"))?;

    let mut parts: Vec<String> = Vec::new();
    for (key, val) in obj {
        // Keep SQL fragments and their params in lockstep by compiling each key
        // fully before moving on.
        match key.as_str() {
            "$or" => parts.push(compile_or(val, &mut params)?),
            "$and" => {
                return Err(ApiError::validation(
                    "$and is not supported; multiple top-level keys are already ANDed",
                ))
            }
            k if k.starts_with('$') => {
                return Err(ApiError::validation(format!("unsupported top-level operator {k}")))
            }
            _ => parts.push(compile_field(key, val, &mut params)?),
        }
    }

    let sql = if parts.is_empty() {
        "1".to_string()
    } else {
        parts.join(" AND ")
    };
    Ok(WhereClause { sql, params })
}

/// Compile the single top-level `$or`: an array of flat AND-maps, joined by OR.
fn compile_or(val: &serde_json::Value, params: &mut Vec<Value>) -> ApiResult<String> {
    let arr = val
        .as_array()
        .ok_or_else(|| ApiError::validation("$or must be an array of sub-filters"))?;
    let mut subs = Vec::new();
    for sub in arr {
        let obj = sub
            .as_object()
            .ok_or_else(|| ApiError::validation("$or entries must be objects"))?;
        let mut and_parts = Vec::new();
        for (k, v) in obj {
            if k == "$or" || k == "$and" {
                return Err(ApiError::validation("nested boolean operators are not allowed"));
            }
            if k.starts_with('$') {
                return Err(ApiError::validation(format!(
                    "unexpected operator {k} inside $or entry"
                )));
            }
            and_parts.push(compile_field(k, v, params)?);
        }
        let joined = if and_parts.is_empty() {
            "1".to_string()
        } else {
            and_parts.join(" AND ")
        };
        subs.push(format!("({joined})"));
    }
    if subs.is_empty() {
        // An empty $or matches nothing.
        Ok("0".to_string())
    } else {
        Ok(format!("({})", subs.join(" OR ")))
    }
}

/// Compile a single field condition: either a scalar equality or an operator map.
fn compile_field(field: &str, spec: &serde_json::Value, params: &mut Vec<Value>) -> ApiResult<String> {
    let path = field_to_json_path(field)?;
    let extract = format!("json_extract(doc, '{path}')");

    match spec {
        serde_json::Value::Object(map) => {
            let mut parts = Vec::new();
            for (op, arg) in map {
                if !op.starts_with('$') {
                    return Err(ApiError::validation(format!(
                        "operator map for {field} may only contain operators; got {op}"
                    )));
                }
                parts.push(compile_operator(&extract, op, arg, params)?);
            }
            if parts.is_empty() {
                Ok("1".to_string())
            } else {
                Ok(format!("({})", parts.join(" AND ")))
            }
        }
        serde_json::Value::Array(_) => Err(ApiError::validation(format!(
            "array equality is not supported for {field}; use $in"
        ))),
        scalar => compile_eq(&extract, scalar, params),
    }
}

/// `field = value`, or `field IS NULL` when the value is JSON null.
fn compile_eq(extract: &str, scalar: &serde_json::Value, params: &mut Vec<Value>) -> ApiResult<String> {
    if scalar.is_null() {
        return Ok(format!("{extract} IS NULL"));
    }
    params.push(scalar_to_value(scalar)?);
    Ok(format!("{extract} = ?"))
}

fn compile_operator(
    extract: &str,
    op: &str,
    arg: &serde_json::Value,
    params: &mut Vec<Value>,
) -> ApiResult<String> {
    match op {
        "$eq" => compile_eq(extract, arg, params),
        "$ne" => {
            // `IS NOT` treats NULL as comparable so missing/null fields match a
            // $ne against a concrete value (Mongo-ish semantics).
            if arg.is_null() {
                Ok(format!("{extract} IS NOT NULL"))
            } else {
                params.push(scalar_to_value(arg)?);
                Ok(format!("{extract} IS NOT ?"))
            }
        }
        "$gt" | "$gte" | "$lt" | "$lte" => {
            let sql_op = match op {
                "$gt" => ">",
                "$gte" => ">=",
                "$lt" => "<",
                _ => "<=",
            };
            params.push(scalar_to_value(arg)?);
            Ok(format!("{extract} {sql_op} ?"))
        }
        "$in" => {
            let arr = arg
                .as_array()
                .ok_or_else(|| ApiError::validation("$in requires an array"))?;
            if arr.is_empty() {
                // `IN ()` is invalid SQL; an empty set matches nothing.
                return Ok("0".to_string());
            }
            let mut placeholders = Vec::with_capacity(arr.len());
            for v in arr {
                params.push(scalar_to_value(v)?);
                placeholders.push("?");
            }
            Ok(format!("{extract} IN ({})", placeholders.join(", ")))
        }
        other => Err(ApiError::validation(format!("unsupported operator {other}"))),
    }
}

/// Convert a JSON scalar to a bound SQLite value. Arrays/objects are rejected.
fn scalar_to_value(v: &serde_json::Value) -> ApiResult<Value> {
    match v {
        serde_json::Value::Null => Ok(Value::Null),
        serde_json::Value::Bool(b) => Ok(Value::Integer(*b as i64)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(Value::Integer(i))
            } else if let Some(f) = n.as_f64() {
                Ok(Value::Real(f))
            } else {
                Err(ApiError::validation("unsupported numeric value"))
            }
        }
        serde_json::Value::String(s) => Ok(Value::Text(s.clone())),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => Err(ApiError::validation(
            "cannot compare against a JSON array or object",
        )),
    }
}

/// Metadata columns that sort/filter may reference directly (not via the doc).
fn is_meta_column(field: &str) -> bool {
    matches!(field, "id" | "created_at" | "updated_at")
}

/// Compile a `sort` array into an `ORDER BY ...` clause. Default is `id ASC`
/// (ULID → chronological). Paths are validated; direction must be asc/desc.
pub fn compile_sort(sort: &[(String, String)]) -> ApiResult<String> {
    if sort.is_empty() {
        return Ok("ORDER BY id ASC".to_string());
    }
    let mut terms = Vec::new();
    for (path, dir) in sort {
        let dir_sql = match dir.to_ascii_lowercase().as_str() {
            "asc" => "ASC",
            "desc" => "DESC",
            _ => return Err(ApiError::validation("sort direction must be 'asc' or 'desc'")),
        };
        let expr = if is_meta_column(path) {
            path.clone()
        } else {
            let json_path = field_to_json_path(path)?;
            format!("json_extract(doc, '{json_path}')")
        };
        terms.push(format!("{expr} {dir_sql}"));
    }
    Ok(format!("ORDER BY {}", terms.join(", ")))
}

/// Cursor pagination (`WHERE id > ?`) only applies to the default/id-ordered
/// case (§8). Custom sorts fall back to offset.
pub fn is_cursor_compatible(sort: &[(String, String)]) -> bool {
    sort.is_empty()
        || (sort.len() == 1 && sort[0].0 == "id" && sort[0].1.eq_ignore_ascii_case("asc"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn compile(v: serde_json::Value) -> WhereClause {
        compile_filter(Some(&v)).expect("should compile")
    }

    #[test]
    fn empty_filter_matches_all() {
        assert_eq!(compile_filter(None).unwrap().sql, "1");
        assert_eq!(compile(json!({})).sql, "1");
    }

    #[test]
    fn scalar_equality() {
        let w = compile(json!({"status": "active"}));
        assert_eq!(w.sql, "json_extract(doc, '$.status') = ?");
        assert_eq!(w.params, vec![Value::Text("active".into())]);
    }

    #[test]
    fn dotted_path_maps_to_json_path() {
        let w = compile(json!({"user.age": 18}));
        assert_eq!(w.sql, "json_extract(doc, '$.user.age') = ?");
        assert_eq!(w.params, vec![Value::Integer(18)]);
    }

    #[test]
    fn all_comparison_operators() {
        for (op, sym) in [("$gt", ">"), ("$gte", ">="), ("$lt", "<"), ("$lte", "<=")] {
            let w = compile(json!({"age": {op: 21}}));
            assert_eq!(w.sql, format!("(json_extract(doc, '$.age') {sym} ?)"));
            assert_eq!(w.params, vec![Value::Integer(21)]);
        }
    }

    #[test]
    fn ne_uses_is_not() {
        let w = compile(json!({"role": {"$ne": "admin"}}));
        assert_eq!(w.sql, "(json_extract(doc, '$.role') IS NOT ?)");
    }

    #[test]
    fn eq_null_uses_is_null() {
        let w = compile(json!({"deleted": null}));
        assert_eq!(w.sql, "json_extract(doc, '$.deleted') IS NULL");
        assert!(w.params.is_empty());
    }

    #[test]
    fn in_operator() {
        let w = compile(json!({"role": {"$in": ["a", "b"]}}));
        assert_eq!(w.sql, "(json_extract(doc, '$.role') IN (?, ?))");
        assert_eq!(
            w.params,
            vec![Value::Text("a".into()), Value::Text("b".into())]
        );
    }

    #[test]
    fn empty_in_matches_nothing() {
        let w = compile(json!({"role": {"$in": []}}));
        assert_eq!(w.sql, "(0)");
    }

    #[test]
    fn multiple_fields_are_anded() {
        let w = compile(json!({"a": 1, "b": 2}));
        // Order follows map iteration; both clauses present and ANDed.
        assert!(w.sql.contains(" AND "));
        assert_eq!(w.params.len(), 2);
    }

    #[test]
    fn or_group() {
        let w = compile(json!({"$or": [{"a": 1}, {"b": 2}]}));
        assert_eq!(
            w.sql,
            "((json_extract(doc, '$.a') = ?) OR (json_extract(doc, '$.b') = ?))"
        );
        assert_eq!(w.params.len(), 2);
    }

    #[test]
    fn or_anded_with_top_level_field() {
        let w = compile(json!({"status": "active", "$or": [{"a": 1}, {"b": 2}]}));
        assert_eq!(w.params.len(), 3);
        assert!(w.sql.contains(" AND "));
        assert!(w.sql.contains(" OR "));
    }

    #[test]
    fn rejects_nested_or() {
        assert!(compile_filter(Some(&json!({"$or": [{"$or": [{"a": 1}]}]}))).is_err());
    }

    #[test]
    fn rejects_and() {
        assert!(compile_filter(Some(&json!({"$and": [{"a": 1}]}))).is_err());
    }

    #[test]
    fn rejects_unknown_operator() {
        assert!(compile_filter(Some(&json!({"a": {"$regex": "x"}}))).is_err());
    }

    #[test]
    fn rejects_or_inside_field() {
        // A field whose object mixes a non-operator key is invalid.
        assert!(compile_filter(Some(&json!({"a": {"foo": 1}}))).is_err());
    }

    #[test]
    fn rejects_bad_path() {
        assert!(compile_filter(Some(&json!({"a-b": 1}))).is_err());
        assert!(compile_filter(Some(&json!({"a.": 1}))).is_err());
        assert!(compile_filter(Some(&json!({"": 1}))).is_err());
    }

    #[test]
    fn rejects_array_equality() {
        assert!(compile_filter(Some(&json!({"tags": [1, 2]}))).is_err());
    }

    #[test]
    fn sort_default_and_custom() {
        assert_eq!(compile_sort(&[]).unwrap(), "ORDER BY id ASC");
        let s = compile_sort(&[
            ("created_at".into(), "desc".into()),
            ("name".into(), "asc".into()),
        ])
        .unwrap();
        assert_eq!(
            s,
            "ORDER BY created_at DESC, json_extract(doc, '$.name') ASC"
        );
    }

    #[test]
    fn sort_rejects_bad_direction() {
        assert!(compile_sort(&[("a".into(), "sideways".into())]).is_err());
    }

    #[test]
    fn cursor_compatibility() {
        assert!(is_cursor_compatible(&[]));
        assert!(is_cursor_compatible(&[("id".into(), "asc".into())]));
        assert!(!is_cursor_compatible(&[("id".into(), "desc".into())]));
        assert!(!is_cursor_compatible(&[("name".into(), "asc".into())]));
    }
}
