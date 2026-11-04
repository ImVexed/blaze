//! Incremental contracts for the independently tracked diagnostic passes.

use base_db::{FileId, SourceDatabase};
use hir_def::{DefWithBodyId, ModuleDefId, ModuleId, attrs::AttrFlags, expr_store::Body};

use crate::{
    diagnostics::{BodyValidationDiagnostic, UnsafetyReason, missing_unsafe},
    test_db::TestDB,
};
use test_fixture::WithFixture as _;

fn named_body(db: &TestDB, module: ModuleId, name: &str) -> DefWithBodyId {
    module
        .def_map(db)
        .module(db, module)
        .scope(db)
        .declarations()
        .find_map(|def| {
            let ModuleDefId::FunctionId(function) = def else { return None };
            (hir_def::signatures::FunctionSignature::of(db, function).name.as_str() == name)
                .then_some(function.into())
        })
        .unwrap()
}

fn caller(db: &TestDB, file: FileId) -> DefWithBodyId {
    named_body(db, db.module_for_file(file), "caller")
}

fn macro_call(db: &TestDB, owner: DefWithBodyId) -> hir_expand::MacroCallId {
    Body::with_source_map(db, owner).1.macro_calls().next().unwrap().1
}

#[test]
fn diagnostic_queries_reuse_after_unrelated_body_revision() {
    let (mut db, files) = TestDB::with_many_files(
        r#"
//- /lib.rs crate:app
mod other;
macro_rules! value { () => { 1u32 }; }
fn caller() -> u32 { value!() }
//- /other.rs
pub fn unrelated() -> u32 { 1 }
"#,
    );
    let [file, other] = [0, 1].map(|it| files[it].file_id(&db));
    let owner = caller(&db, file);
    let call = macro_call(&db, owner);
    let queries = ["missing_unsafe", "body_validation_diagnostics", "parse_macro_expansion_error"];
    let count = |db: &TestDB| {
        let (executed, _) = db.log_executed(|| {
            crate::attach_db(db, || {
                assert!(missing_unsafe(db, owner).unsafe_exprs.is_empty());
                assert!(BodyValidationDiagnostic::collect(db, owner, false).is_empty());
                assert!(call.parse_macro_expansion_error(db).is_none());
            })
        });
        queries.map(|query| executed.iter().filter(|name| name.contains(query)).count())
    };
    assert_eq!(count(&db), [1, 1, 1]);
    assert_eq!(count(&db), [0, 0, 0]);
    db.set_file_text(other, "pub fn unrelated() -> u32 { 2 }");
    assert_eq!(count(&db), [0, 0, 0]);
}

#[test]
fn diagnostic_queries_track_cross_file_unsafety_and_fresh_spans() {
    let (mut db, files) = TestDB::with_many_files(
        r#"
//- /lib.rs crate:app deps:dep
pub fn caller() { dep::callee(); }
//- /dep.rs crate:dep
pub fn callee() {}
"#,
    );
    let [file, dep] = [0, 1].map(|it| files[it].file_id(&db));
    let owner = caller(&db, file);
    let original = db.file_text(file).text(&db).to_owned();
    assert!(missing_unsafe(&db, owner).unsafe_exprs.is_empty());
    db.set_file_text(dep, "pub unsafe fn callee() {}");
    let unsafe_call = missing_unsafe(&db, owner);
    assert_eq!(unsafe_call.unsafe_exprs.len(), 1);
    assert_eq!(unsafe_call.unsafe_exprs[0].1, UnsafetyReason::UnsafeFnCall);
    let range = |db: &TestDB| {
        Body::with_source_map(db, owner)
            .1
            .expr_or_pat_syntax(unsafe_call.unsafe_exprs[0].0)
            .unwrap()
            .value
            .text_range()
    };
    let before = range(&db);
    db.set_file_text(file, &format!("\n\n{original}"));
    assert_eq!(missing_unsafe(&db, owner), unsafe_call);
    assert_eq!(range(&db), before + syntax::TextSize::from(2));
    db.set_file_text(dep, "pub fn callee() {}");
    db.set_file_text(file, &original);
    assert!(missing_unsafe(&db, owner).unsafe_exprs.is_empty());
}

#[test]
fn diagnostic_queries_track_macro_syntax_errors_and_restore() {
    let (mut db, files) = TestDB::with_many_files(
        r#"
//- /lib.rs crate:app deps:dep
pub fn caller() -> u32 { dep::value!() }
//- /dep.rs crate:dep
#[macro_export]
macro_rules! value { () => { 1u32 }; }
"#,
    );
    let [file, dep] = [0, 1].map(|it| files[it].file_id(&db));
    let owner = caller(&db, file);
    let call = macro_call(&db, owner);
    let original = db.file_text(dep).text(&db).to_owned();
    assert!(call.parse_macro_expansion_error(&db).is_none());
    db.set_file_text(dep, &original.replace("1u32", "let = ;"));
    let error = macro_call(&db, caller(&db, file)).parse_macro_expansion_error(&db).unwrap();
    assert!(!error.value.is_empty());
    db.set_file_text(dep, &original);
    assert!(macro_call(&db, caller(&db, file)).parse_macro_expansion_error(&db).is_none());
}

#[test]
fn diagnostic_queries_track_must_use_messages_and_lint_keys() {
    let (mut db, files) = TestDB::with_many_files(
        r#"
//- /lib.rs crate:app deps:dep
pub fn caller() { dep::value(); }
//- /dep.rs crate:dep
#[must_use = "type"]
pub struct Value;
#[must_use = "first"]
pub fn value() -> Value { Value }
"#,
    );
    let [file, dep] = [0, 1].map(|it| files[it].file_id(&db));
    let owner = caller(&db, file);
    let original = db.file_text(dep).text(&db).to_owned();
    let messages = |db: &TestDB| {
        BodyValidationDiagnostic::collect(db, owner, true)
            .into_iter()
            .filter_map(|diagnostic| match diagnostic {
                BodyValidationDiagnostic::UnusedMustUse { owner, .. } => {
                    Some(AttrFlags::must_use_message(db, owner).unwrap().map(str::to_owned))
                }
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert!(BodyValidationDiagnostic::collect(&db, owner, false).is_empty());
    assert_eq!(messages(&db), [Some("first".to_owned())]);
    db.set_file_text(dep, &original.replace("first", "second"));
    assert_eq!(messages(&db), [Some("second".to_owned())]);
    assert!(BodyValidationDiagnostic::collect(&db, owner, false).is_empty());
    let fallback = original.replace(r#"#[must_use = "first"]"#, "");
    db.set_file_text(dep, &fallback);
    assert_eq!(messages(&db), [Some("type".to_owned())]);
    let bare = original.replace(r#"#[must_use = "first"]"#, "#[must_use]");
    db.set_file_text(dep, &bare);
    assert_eq!(messages(&db), [None]);
    db.set_file_text(dep, &fallback.replace(r#"#[must_use = "type"]"#, ""));
    assert!(messages(&db).is_empty());
    db.set_file_text(dep, &original);
    assert_eq!(messages(&db), [Some("first".to_owned())]);
}

#[test]
fn diagnostic_queries_track_cross_file_exhaustiveness_and_restore() {
    let (mut db, files) = TestDB::with_many_files(
        r#"
//- /lib.rs crate:app deps:dep
pub fn caller(value: dep::E) -> u32 { match value { dep::E::A => 0 } }
//- /dep.rs crate:dep
pub enum E { A }
"#,
    );
    let [file, dep] = [0, 1].map(|it| files[it].file_id(&db));
    let owner = caller(&db, file);
    assert!(BodyValidationDiagnostic::collect(&db, owner, false).is_empty());
    db.set_file_text(dep, "pub enum E { A, B }");
    let diagnostics = BodyValidationDiagnostic::collect(&db, owner, false);
    assert_eq!(diagnostics.len(), 1);
    assert!(matches!(diagnostics[0], BodyValidationDiagnostic::MissingMatchArms { .. }));
    db.set_file_text(dep, "pub enum E { A }");
    assert!(BodyValidationDiagnostic::collect(&db, owner, false).is_empty());
}

#[test]
fn diagnostic_queries_keep_shared_body_crate_variants_distinct() {
    let (db, files) = TestDB::with_many_files(
        r#"
//- /safe.rs crate:safe
#[path = "shared.rs"] mod shared;
pub fn callee() {}
//- /shared.rs
pub fn caller() { crate::callee(); }
//- /unsafe.rs crate:other
#[path = "shared.rs"] mod shared;
pub unsafe fn callee() {}
"#,
    );
    let [safe, other] = [0, 2].map(|it| files[it].file_id(&db));
    let variant = |file| {
        let root = db.module_for_file(file);
        let shared = root
            .def_map(&db)
            .module(&db, root)
            .scope(&db)
            .declarations()
            .find_map(|def| match def {
                ModuleDefId::ModuleId(module) => Some(module),
                _ => None,
            })
            .unwrap();
        named_body(&db, shared, "caller")
    };
    let (safe, other) = (variant(safe), variant(other));
    assert_ne!(safe, other);
    assert!(missing_unsafe(&db, safe).unsafe_exprs.is_empty());
    let unsafe_call = missing_unsafe(&db, other);
    assert_eq!(unsafe_call.unsafe_exprs.len(), 1);
    assert_eq!(unsafe_call.unsafe_exprs[0].1, UnsafetyReason::UnsafeFnCall);
    assert!(missing_unsafe(&db, safe).unsafe_exprs.is_empty());
}
