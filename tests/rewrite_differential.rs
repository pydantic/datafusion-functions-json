//! Differential tests for the expression folding done by `JsonFunctionRewriter`.
//!
//! The rewriter folds `CAST(json_get(x, 'k') AS BIGINT)` down to `json_get_int(x, 'k')` so the
//! JSON union is never materialized just to be cast away, and it flattens nested `json_get`
//! calls into a single multi-argument call. Both are optimizations, so both are only correct
//! if they are invisible to the caller. These tests generate input and check that.
//!
//! # Running the same query with and without the rewrite
//!
//! Function rewrites run in the *analyzer*, before the optimizer merges projections. A cast
//! applied to a column of a subquery therefore never sees a `json_get` underneath it and is
//! not folded — and then `merge_projection` inlines the `json_get` back under the cast. So
//! [`unfolded_sql`] gives the plan we would have had with no rewriter registered at all,
//! without needing a second `SessionContext`. [`barrier_really_blocks_folding`] keeps that
//! trick honest.
//!
//! For nested `json_get` flattening this gives a straightforward invariant, checked by
//! [`prop_unnest_is_invisible`]: the flattened and un-flattened plans must agree.
//!
//! For cast folding it does **not**: casting the JSON union is much weaker than the typed
//! accessors (`CAST(union AS Float64)` is NULL even for `42` — see
//! [`unfolded_union_cast_is_weaker`]), so the rewrite is deliberately *better* than the
//! expression it replaces and equality is the wrong assertion. What the fold does promise is
//!
//! ```text
//! CAST(json_get(x, 'k') AS T)  ==  CAST(json_get_<T>(x, 'k') AS T)
//! ```
//!
//! i.e. "use the typed accessor, then apply the cast the user actually wrote". The
//! right-hand side is not itself folded — the rewriter only folds casts over `json_get` — so
//! it serves as the reference implementation. That is [`prop_fold_is_invisible`].

use std::sync::Arc;

use datafusion::arrow::array::{RecordBatch, StringViewArray};
use datafusion::arrow::datatypes::{DataType, Field, Schema};
use datafusion::arrow::util::display::{ArrayFormatter, FormatOptions};
use datafusion::error::Result;
use datafusion::execution::context::SessionContext;
use datafusion::prelude::SessionConfig;
use datafusion_functions_json::register_all;
use proptest::prelude::*;

// ---------------------------------------------------------------------------------------
// the cast matrix
// ---------------------------------------------------------------------------------------

/// What the rewriter does with a given cast of a `json_get` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fold {
    /// Left alone: the cast is applied to the JSON union.
    None,
    /// Folded to the typed accessor, whose return type *is* the type that was asked for.
    Exact,
    /// Folded to the typed accessor, whose return type is **not** the type that was asked
    /// for. The expression silently changes type and the narrowing never happens.
    Widening,
}

/// A cast target: how it is spelled in SQL, the exact Arrow type `arrow_cast` names for it,
/// the typed accessor the rewriter folds it to, and what each of the two folding paths does
/// with it.
#[derive(Debug, Clone, Copy)]
struct Target {
    sql: &'static str,
    arrow: &'static str,
    accessor: &'static str,
    sql_cast: Fold,
    arrow_cast: Fold,
}

const TARGETS: &[Target] = &[
    Target {
        sql: "bigint",
        arrow: "Int64",
        accessor: "json_get_int",
        sql_cast: Fold::Exact,
        arrow_cast: Fold::Exact,
    },
    Target {
        sql: "double",
        arrow: "Float64",
        accessor: "json_get_float",
        sql_cast: Fold::Exact,
        arrow_cast: Fold::Exact,
    },
    Target {
        sql: "boolean",
        arrow: "Boolean",
        accessor: "json_get_bool",
        sql_cast: Fold::Exact,
        arrow_cast: Fold::Exact,
    },
    // SQL `VARCHAR` is `Utf8View` in DataFusion, but `json_get_str` returns `Utf8`.
    Target {
        sql: "varchar",
        arrow: "Utf8",
        accessor: "json_get_str",
        sql_cast: Fold::Widening,
        arrow_cast: Fold::Exact,
    },
    // Narrowing targets: the type asked for is narrower than what the accessor returns. The
    // `CAST` table folds them anyway; the `arrow_*` handler deliberately does not.
    Target {
        sql: "int",
        arrow: "Int32",
        accessor: "json_get_int",
        sql_cast: Fold::Widening,
        arrow_cast: Fold::None,
    },
    Target {
        sql: "real",
        arrow: "Float32",
        accessor: "json_get_float",
        sql_cast: Fold::Widening,
        arrow_cast: Fold::None,
    },
    Target {
        sql: "decimal(10,2)",
        arrow: "Decimal128(10, 2)",
        accessor: "json_get_float",
        sql_cast: Fold::Widening,
        arrow_cast: Fold::None,
    },
    // A type neither path has an accessor for, so neither folds it.
    Target {
        sql: "smallint",
        arrow: "Int16",
        accessor: "json_get_int",
        sql_cast: Fold::None,
        arrow_cast: Fold::None,
    },
];

/// The spellings of a cast that all mean the same thing to a user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Spelling {
    Cast,
    DoubleColon,
    TryCast,
    ArrowCast,
    ArrowTryCast,
}

const SPELLINGS: &[Spelling] = &[
    Spelling::Cast,
    Spelling::DoubleColon,
    Spelling::TryCast,
    Spelling::ArrowCast,
    Spelling::ArrowTryCast,
];

impl Spelling {
    /// Write `inner` cast to `target`.
    fn apply(self, inner: &str, target: Target) -> String {
        match self {
            Spelling::Cast => format!("cast({inner} as {})", target.sql),
            Spelling::DoubleColon => format!("({inner})::{}", target.sql),
            Spelling::TryCast => format!("try_cast({inner} as {})", target.sql),
            Spelling::ArrowCast => format!("arrow_cast({inner}, '{}')", target.arrow),
            Spelling::ArrowTryCast => format!("arrow_try_cast({inner}, '{}')", target.arrow),
        }
    }

    fn names_an_arrow_type(self) -> bool {
        matches!(self, Spelling::ArrowCast | Spelling::ArrowTryCast)
    }

    /// What the rewriter does with this spelling of this target.
    fn fold(self, target: Target) -> Fold {
        if self.names_an_arrow_type() {
            target.arrow_cast
        } else {
            target.sql_cast
        }
    }
}

// ---------------------------------------------------------------------------------------
// documents and SQL
// ---------------------------------------------------------------------------------------

/// A JSON document and a lookup path into it that reaches the interesting value.
#[derive(Debug, Clone)]
struct Doc {
    json: String,
    path: Vec<String>,
}

impl Doc {
    /// `json_get(<json>, <path...>)`, the expression the rewriter folds.
    fn json_get(&self) -> String {
        self.call("json_get")
    }

    /// The same lookup through a typed accessor, which the rewriter leaves alone.
    fn accessor(&self, name: &str) -> String {
        self.call(name)
    }

    fn call(&self, func: &str) -> String {
        let args = std::iter::once(JSON_COLUMN.to_string())
            .chain(self.path.iter().map(|p| path_arg(p)))
            .collect::<Vec<_>>();
        format!("{func}({})", args.join(", "))
    }

    /// `json_get(j, k1, .., k(n-1))`: the lookup for everything but the last path segment,
    /// which is what an outer accessor call gets nested around. `None` for a one-element
    /// path, where there is nothing to nest.
    fn prefix_json_get(&self) -> Option<String> {
        let (_, prefix) = self.path.split_last()?;
        if prefix.is_empty() {
            return None;
        }
        let args = std::iter::once(JSON_COLUMN.to_string())
            .chain(prefix.iter().map(|p| path_arg(p)))
            .collect::<Vec<_>>();
        Some(format!("json_get({})", args.join(", ")))
    }
}

/// Render a path segment: integers stay integers so array indexing is exercised.
fn path_arg(segment: &str) -> String {
    if segment.chars().all(|c| c.is_ascii_digit()) {
        segment.to_string()
    } else {
        sql_str(segment)
    }
}

/// Quote a string as a SQL literal.
fn sql_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

const JSON_TABLE: &str = "t";
const JSON_COLUMN: &str = "j";

/// Point the single-row table `t` at `json`.
///
/// The document has to arrive as column data rather than as a SQL literal: `DataFusion`
/// const-evaluates a projection over literals down to a single scalar, which erases the very
/// plan shape these tests assert on.
fn set_doc(ctx: &SessionContext, json: &str) {
    let schema = Schema::new(vec![Field::new(JSON_COLUMN, DataType::Utf8View, false)]);
    let batch = RecordBatch::try_new(Arc::new(schema), vec![Arc::new(StringViewArray::from(vec![json]))]).unwrap();
    let _ = ctx.deregister_table(JSON_TABLE);
    ctx.register_batch(JSON_TABLE, batch).unwrap();
}

fn select(expr: &str) -> String {
    format!("select {expr} as v from {JSON_TABLE}")
}

/// Wrap `expr` so the rewriter cannot see through to the `json_get` underneath.
///
/// See the module docs: the analyzer sees a cast over a subquery column and leaves it alone,
/// then the optimizer inlines the `json_get` back underneath. The result evaluates as if no
/// rewrite had been registered.
fn unfolded_sql(inner: &str, wrap: impl FnOnce(&str) -> String) -> String {
    format!("select {} as v from (select {inner} as x from {JSON_TABLE})", wrap("x"))
}

// ---------------------------------------------------------------------------------------
// running
// ---------------------------------------------------------------------------------------

/// The result of a query: coarse enough not to be brittle about error text, precise enough
/// to catch a changed type, a changed value, or a cast that should have failed but did not.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    Value(DataType, String),
    Error,
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Outcome::Error => write!(f, "ERROR"),
            Outcome::Value(dt, v) if v.is_empty() => write!(f, "{dt}=NULL"),
            Outcome::Value(dt, v) => write!(f, "{dt}={v}"),
        }
    }
}

async fn outcome(ctx: &SessionContext, sql: &str) -> Outcome {
    let Ok(df) = ctx.sql(sql).await else {
        return Outcome::Error;
    };
    let Ok(batches) = df.collect().await else {
        return Outcome::Error;
    };
    assert_eq!(batches.len(), 1, "expected one batch from {sql}");
    let batch = &batches[0];
    assert_eq!(batch.num_rows(), 1, "expected one row from {sql}");
    let column = batch.column(0);
    let options = FormatOptions::default().with_display_error(true);
    let formatter = ArrayFormatter::try_new(column.as_ref(), &options).unwrap();
    Outcome::Value(
        batch.schema().field(0).data_type().clone(),
        formatter.value(0).try_to_string().unwrap(),
    )
}

async fn logical_plan(ctx: &SessionContext, sql: &str) -> String {
    let batches = ctx
        .sql(&format!("explain {sql}"))
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    let options = FormatOptions::default();
    let formatter = ArrayFormatter::try_new(batches[0].column(1).as_ref(), &options).unwrap();
    formatter.value(0).try_to_string().unwrap()
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Runtime::new().unwrap()
}

fn create_context() -> Result<SessionContext> {
    let config = SessionConfig::new().set_str("datafusion.sql_parser.dialect", "postgres");
    let mut ctx = SessionContext::new_with_config(config);
    register_all(&mut ctx)?;
    Ok(ctx)
}

// ---------------------------------------------------------------------------------------
// generators
// ---------------------------------------------------------------------------------------

/// A single JSON value, weighted towards the values where the typed accessors and Arrow's
/// cast kernels are most likely to disagree.
fn json_scalar() -> impl Strategy<Value = String> {
    prop_oneof![
        1 => Just("null".to_string()),
        2 => any::<bool>().prop_map(|b| b.to_string()),
        4 => any::<i64>().prop_map(|i| i.to_string()),
        3 => any::<f64>()
            .prop_filter("JSON has no inf or nan", |f| f.is_finite())
            .prop_map(|f| format!("{f:?}")),
        2 => "[a-zA-Z0-9 ._+-]{0,8}".prop_map(|s| format!("\"{s}\"")),
        // numeric- and boolean-looking strings: the accessors are lenient here, Arrow is not
        2 => any::<i64>().prop_map(|i| format!("\"{i}\"")),
        1 => Just("\"true\"".to_string()),
        // integers wider than i64, which take jiter's slow path
        1 => Just("123456789012345678901234567890".to_string()),
        1 => Just("-123456789012345678901234567890".to_string()),
        // nested values, which the accessors report as absent rather than as a value
        1 => Just("[1, 2]".to_string()),
        1 => Just(r#"{"c": 1}"#.to_string()),
    ]
}

/// A JSON value nested somewhere, together with the path that reaches it.
fn doc() -> impl Strategy<Value = Doc> {
    (json_scalar(), 0usize..4).prop_map(|(value, shape)| match shape {
        0 => Doc {
            json: format!(r#"{{"a": {value}}}"#),
            path: vec!["a".to_string()],
        },
        1 => Doc {
            json: format!(r#"{{"a": {{"b": {value}}}}}"#),
            path: vec!["a".to_string(), "b".to_string()],
        },
        2 => Doc {
            json: format!("[{value}]"),
            path: vec!["0".to_string()],
        },
        _ => Doc {
            json: format!(r#"{{"a": [1, {value}]}}"#),
            path: vec!["a".to_string(), "1".to_string()],
        },
    })
}

/// A `(spelling, target)` pair whose fold is type-exact, so the fold must be invisible.
fn exact_cast() -> impl Strategy<Value = (Spelling, Target)> {
    let pairs: Vec<(Spelling, Target)> = SPELLINGS
        .iter()
        .flat_map(|&s| TARGETS.iter().map(move |&t| (s, t)))
        .filter(|&(s, t)| s.fold(t) == Fold::Exact)
        .collect();
    prop::sample::select(pairs)
}

// ---------------------------------------------------------------------------------------
// properties
// ---------------------------------------------------------------------------------------

/// Folding a type-exact cast must be indistinguishable from applying that same cast to the
/// typed accessor.
#[test]
fn prop_fold_is_invisible() {
    let rt = runtime();
    let ctx = create_context().unwrap();

    proptest!(|(doc in doc(), (spelling, target) in exact_cast())| {
        set_doc(&ctx, &doc.json);
        let folded = select(&spelling.apply(&doc.json_get(), target));
        let reference = select(&spelling.apply(&doc.accessor(target.accessor), target));

        let got = rt.block_on(outcome(&ctx, &folded));
        let want = rt.block_on(outcome(&ctx, &reference));

        prop_assert_eq!(
            &got, &want,
            "\n  folded:    {} => {}\n  reference: {} => {}\n", folded, got, reference, want
        );
    });
}

/// Flattening `json_get(json_get(j, 'a'), 'b')` into `json_get(j, 'a', 'b')` must not change
/// the answer. Here the plan without the rewrite really is the right reference, so this is
/// the plain with/without-optimizer differential the barrier was built for.
#[test]
fn prop_unnest_is_invisible() {
    let rt = runtime();
    let ctx = create_context().unwrap();

    proptest!(|(doc in doc())| {
        let Some(prefix) = doc.prefix_json_get() else {
            return Ok(());
        };
        set_doc(&ctx, &doc.json);

        // `json_get` returns the JSON union, which formats as an opaque value, so read the
        // result out through `json_as_text`. The rewriter flattens the `json_get` underneath
        // it into the outer call; the barrier stops it from seeing the nesting at all.
        let last = path_arg(doc.path.last().unwrap());
        let unnested = select(&format!("json_as_text({prefix}, {last})"));
        let not_unnested = unfolded_sql(&prefix, |x| format!("json_as_text({x}, {last})"));

        // the comparison only means something while each side is the plan it claims to be
        let plan = rt.block_on(logical_plan(&ctx, &not_unnested));
        prop_assert!(
            plan.contains("json_as_text(json_get("),
            "barrier stopped blocking the unnest:\n{}\n{}", not_unnested, plan
        );
        let unnested_plan = rt.block_on(logical_plan(&ctx, &unnested));
        prop_assert!(
            !unnested_plan.contains("json_as_text(json_get("),
            "expected the nested call to be flattened:\n{}\n{}", unnested, unnested_plan
        );

        let got = rt.block_on(outcome(&ctx, &unnested));
        let want = rt.block_on(outcome(&ctx, &not_unnested));
        prop_assert_eq!(&got, &want,
            "\n  unnested:     {} => {}\n  not unnested: {} => {}\n",
            unnested, got, not_unnested, want);
    });
}

// ---------------------------------------------------------------------------------------
// plan shape: which spellings fold
// ---------------------------------------------------------------------------------------

/// Every spelling of a cast a user might reach for folds to the same accessor — that is what
/// PR #127 added — except `arrow_cast`/`arrow_try_cast` of a type no accessor returns
/// exactly, which are deliberately left alone.
#[test]
fn every_cast_spelling_folds_as_documented() {
    let rt = runtime();
    let ctx = create_context().unwrap();
    let doc = Doc {
        json: r#"{"a": 42}"#.to_string(),
        path: vec!["a".to_string()],
    };
    set_doc(&ctx, &doc.json);

    for &target in TARGETS {
        for &spelling in SPELLINGS {
            let sql = select(&spelling.apply(&doc.json_get(), target));
            let plan = rt.block_on(logical_plan(&ctx, &sql));
            let folded = plan.contains(target.accessor);
            let expected = spelling.fold(target) != Fold::None;
            assert_eq!(
                folded, expected,
                "expected folded={expected} for {spelling:?} / {}\n{plan}",
                target.sql
            );
            if folded {
                assert!(
                    !plan.contains("json_get(t."),
                    "fold left the union accessor behind for {spelling:?} / {}\n{plan}",
                    target.sql
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------------------
// characterization: what folding a narrowing cast currently does
// ---------------------------------------------------------------------------------------

/// The `CAST`/`TRY_CAST` path folds *every* type in its table down to the bare accessor,
/// including types the accessor does not return. The result is that the expression's type is
/// not the type the user asked for, and the narrowing the cast promised never happens.
///
/// This test pins the current behaviour rather than asserting the desired behaviour, so that
/// it stays green while the divergence is open. **If this test starts failing because the
/// fold became type-preserving, delete it** — [`prop_fold_is_invisible`] covers the fixed
/// behaviour once these targets' `sql_cast` becomes `Fold::Exact`.
///
/// The fix that makes all of this go away is to keep the cast the user wrote and fold only
/// its input, `CAST(json_get_int(x, 'k') AS Int32)` instead of `json_get_int(x, 'k')`. That
/// keeps the whole point of the rewrite — the union is never materialized — while leaving
/// the declared type and the narrowing intact, and `SimplifyExpressions` drops the cast
/// where it is a no-op.
#[test]
fn narrowing_cast_fold_is_not_type_preserving() {
    let rt = runtime();
    let ctx = create_context().unwrap();

    // (json value, sql type, folded outcome, what the same cast over the typed accessor gives)
    let cases = [
        // the type of the expression is the accessor's, not the one the user asked for
        ("42", "int", "Int64=42", "Int32=42"),
        ("42", "real", "Float64=42.0", "Float32=42.0"),
        ("42", "decimal(10,2)", "Float64=42.0", "Decimal128(10, 2)=42.00"),
        (r#""abc""#, "varchar", "Utf8=abc", "Utf8View=abc"),
        // and the narrowing the cast promised never happens
        ("3000000000", "int", "Int64=3000000000", "ERROR"),
        ("9223372036854775807", "int", "Int64=9223372036854775807", "ERROR"),
        ("3000000000", "decimal(10,2)", "Float64=3000000000.0", "ERROR"),
    ];

    for (value, sql_type, want_folded, want_reference) in cases {
        let target = TARGETS.iter().find(|t| t.sql == sql_type).unwrap();
        let doc = Doc {
            json: format!(r#"{{"a": {value}}}"#),
            path: vec!["a".to_string()],
        };
        set_doc(&ctx, &doc.json);
        let folded = select(&Spelling::Cast.apply(&doc.json_get(), *target));
        let reference = select(&Spelling::Cast.apply(&doc.accessor(target.accessor), *target));

        assert_eq!(rt.block_on(outcome(&ctx, &folded)).to_string(), want_folded, "{folded}");
        assert_eq!(
            rt.block_on(outcome(&ctx, &reference)).to_string(),
            want_reference,
            "{reference}"
        );
    }
}

/// `TRY_CAST`, which PR #127 routes through the same type table, inherits the same gap: it
/// returns a value where the cast it stands for would have returned NULL.
#[test]
fn narrowing_try_cast_fold_returns_value_instead_of_null() {
    let rt = runtime();
    let ctx = create_context().unwrap();
    let target = TARGETS.iter().find(|t| t.sql == "int").unwrap();
    let doc = Doc {
        json: r#"{"a": 3000000000}"#.to_string(),
        path: vec!["a".to_string()],
    };

    set_doc(&ctx, &doc.json);
    let folded = select(&Spelling::TryCast.apply(&doc.json_get(), *target));
    let reference = select(&Spelling::TryCast.apply(&doc.accessor(target.accessor), *target));

    assert_eq!(rt.block_on(outcome(&ctx, &folded)).to_string(), "Int64=3000000000");
    assert_eq!(rt.block_on(outcome(&ctx, &reference)).to_string(), "Int32=NULL");
}

// ---------------------------------------------------------------------------------------
// the barrier, and why the naive differential does not work for casts
// ---------------------------------------------------------------------------------------

/// The subquery barrier must actually stop the fold, or every test that uses it as the
/// "without the rewrite" reference silently becomes a tautology.
#[test]
fn barrier_really_blocks_folding() {
    let rt = runtime();
    let ctx = create_context().unwrap();
    let doc = Doc {
        json: r#"{"a": 42}"#.to_string(),
        path: vec!["a".to_string()],
    };
    set_doc(&ctx, &doc.json);

    for &target in TARGETS {
        for &spelling in SPELLINGS {
            let sql = unfolded_sql(&doc.json_get(), |x| spelling.apply(x, target));
            let plan = rt.block_on(logical_plan(&ctx, &sql));
            // the json_get survives un-folded, and the optimizer inlined it back under the
            // cast, so this is one expression and not an extra projection
            assert!(
                plan.contains("CAST(json_get(t.") || plan.contains("TRY_CAST(json_get(t."),
                "barrier no longer blocks the fold for {spelling:?} / {}\n{plan}",
                target.sql
            );
        }
    }
}

/// Why cast folding cannot be checked by diffing against the un-folded plan: casting the JSON
/// union is much weaker than the typed accessors, so the two disagree by design.
///
/// This is the reason [`prop_fold_is_invisible`] compares against the typed accessor instead.
#[test]
fn unfolded_union_cast_is_weaker() {
    let rt = runtime();
    let ctx = create_context().unwrap();
    let doc = Doc {
        json: r#"{"a": 42}"#.to_string(),
        path: vec!["a".to_string()],
    };
    let target = TARGETS.iter().find(|t| t.sql == "double").unwrap();

    set_doc(&ctx, &doc.json);
    let folded = select(&Spelling::Cast.apply(&doc.json_get(), *target));
    let unfolded = unfolded_sql(&doc.json_get(), |x| Spelling::Cast.apply(x, *target));

    assert_eq!(rt.block_on(outcome(&ctx, &folded)).to_string(), "Float64=42.0");
    // the union has no Float64 member holding an integer, so the cast yields NULL
    assert_eq!(rt.block_on(outcome(&ctx, &unfolded)).to_string(), "Float64=NULL");
}
