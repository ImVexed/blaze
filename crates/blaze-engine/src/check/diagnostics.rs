//! Complete CHECK results, memoized with the semantic and source-map reads that produced them.

use std::sync::Arc;

use hir::db::HirDatabase;
use ide_db::{RootDatabase, base_db::Crate};
use rustc_hash::FxHashSet;

// The diagnostics API takes RootDatabase. Returning the same database keeps its semantic
// reads inside this query's dependency tracking, without storing database-borrowed values.
#[salsa::db]
trait CheckDatabase: HirDatabase {
    fn root_database(&self) -> &RootDatabase;
}

#[salsa::db]
impl CheckDatabase for RootDatabase {
    fn root_database(&self) -> &RootDatabase {
        self
    }
}

pub(super) fn errors_of(
    db: &RootDatabase,
    _config: &ide_diagnostics::DiagnosticsConfig,
    file: vfs::FileId,
    within: Option<Arc<FxHashSet<Crate>>>,
    (lints, refuse_borrowck): (bool, bool),
) -> crate::FileErrors {
    let within = within.map(|crates| {
        let mut crates: Vec<_> = crates.iter().copied().collect();
        crates.sort_unstable();
        crates.dedup();
        crates
    });
    file_errors(db, file, within, lints, refuse_borrowck)
}

// The checker fixes process-wide macro, borrow and skipped-check policies for its lifetime.
// Scope and diagnostic rules remain explicit keys. Every miss asks for ALL bodies; CLEAN is
// neither consulted nor updated, so dependency edits invalidate the owned result via Salsa.
#[salsa::tracked(returns(clone))]
fn file_errors(
    db: &dyn CheckDatabase,
    file: vfs::FileId,
    within: Option<Vec<Crate>>,
    lints: bool,
    refuse_borrowck: bool,
) -> crate::FileErrors {
    let within = within.map(|crates| Arc::new(FxHashSet::from_iter(crates)));
    crate::refused_file_errors(
        ide_diagnostics::semantic_diagnostics_of_bodies(
            db.root_database(),
            &ide_diagnostics::DiagnosticsConfig::test_sample(),
            &ide_db::assists::AssistResolveStrategy::None,
            file,
            within,
            &|_| true,
        ),
        (lints, refuse_borrowck),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ide_db::base_db::SourceDatabase as _;
    use test_fixture::WithFixture as _;

    fn checked(
        db: &RootDatabase,
        file: vfs::FileId,
        within: Option<Arc<FxHashSet<Crate>>>,
        rules: (bool, bool),
    ) -> crate::FileErrors {
        crate::prepare_macro_sessions(db);
        hir::attach_db(db, || {
            errors_of(db, &ide_diagnostics::DiagnosticsConfig::test_sample(), file, within, rules)
        })
    }

    fn codes(found: &crate::FileErrors) -> Vec<&'static str> {
        let mut codes: Vec<_> = found.iter().map(|(_, code, _)| *code).collect();
        codes.sort_unstable();
        codes
    }

    // Run with RUSTD_TRACE=1 cargo test -p blaze-engine --lib
    // check::diagnostics::tests::file_result_reuses_after_unrelated_edit -- --ignored --test-threads=1
    #[test]
    #[ignore = "requires RUSTD_TRACE=1 and --test-threads=1 for the existing query counter"]
    fn file_result_reuses_after_unrelated_edit() {
        assert!(std::env::var_os("RUSTD_TRACE").is_some());
        let (mut db, files) = RootDatabase::with_many_files(
            r#"
//- /lib.rs crate:app
mod other;
pub fn caller() -> u32 { 1 }
//- /other.rs
pub fn unrelated() -> u32 { 1 }
"#,
        );
        let [caller, other] = [0, 1].map(|it| files[it].file_id(&db));
        let count = |db: &RootDatabase| {
            ide_db::executions::take(db);
            assert!(checked(db, caller, None, (true, true)).is_empty());
            ide_db::executions::take(db)
                .into_iter()
                .filter(|(name, _)| name == "file_errors")
                .map(|(_, count)| count)
                .sum::<u32>()
        };
        assert_eq!(count(&db), 1);
        assert_eq!(count(&db), 0);
        db.set_file_text(other, "pub fn unrelated() -> u32 { 2 }");
        assert_eq!(count(&db), 0);
    }

    #[test]
    fn file_result_tracks_cross_file_signature_and_restore() {
        let (mut db, files) = RootDatabase::with_many_files(
            r#"
//- /lib.rs crate:app deps:dep
pub fn caller() -> u32 { dep::value() }
//- /dep.rs crate:dep
pub fn value() -> u32 { 1 }
"#,
        );
        let [caller, dep] = [0, 1].map(|it| files[it].file_id(&db));
        let original = db.file_text(dep).text(&db).to_owned();
        assert!(checked(&db, caller, None, (true, true)).is_empty());
        db.set_file_text(dep, "pub fn value() -> bool { true }");
        assert_eq!(codes(&checked(&db, caller, None, (true, true))), ["E0308"]);
        db.set_file_text(dep, &original);
        assert!(checked(&db, caller, None, (true, true)).is_empty());
    }

    #[test]
    fn file_result_tracks_cross_file_macro_and_restore() {
        let (mut db, files) = RootDatabase::with_many_files(
            r#"
//- /lib.rs crate:app deps:dep
pub fn caller() -> u32 { dep::value!() }
//- /dep.rs crate:dep
#[macro_export]
macro_rules! value { () => { 1u32 }; }
"#,
        );
        let [caller, dep] = [0, 1].map(|it| files[it].file_id(&db));
        let original = db.file_text(dep).text(&db).to_owned();
        assert!(checked(&db, caller, None, (true, true)).is_empty());
        db.set_file_text(dep, &original.replace("1u32", "true"));
        assert_eq!(codes(&checked(&db, caller, None, (true, true))), ["E0308"]);
        db.set_file_text(dep, &original);
        assert!(checked(&db, caller, None, (true, true)).is_empty());
    }

    #[test]
    fn file_result_tracks_body_edits_and_fresh_spans() {
        let original = "pub fn caller() -> u32 { true }\n";
        let (mut db, file) = RootDatabase::with_single_file(original);
        let file = file.file_id(&db);
        let before = checked(&db, file, None, (true, true));
        assert_eq!(codes(&before), ["E0308"]);
        db.set_file_text(file, &format!("\n\n{original}"));
        let shifted = checked(&db, file, None, (true, true));
        assert_eq!(codes(&shifted), ["E0308"]);
        assert_eq!(shifted[0].0.file_id, before[0].0.file_id);
        assert_eq!(shifted[0].0.range, before[0].0.range + syntax::TextSize::from(2));
        assert_eq!((&shifted[0].1, &shifted[0].2), (&before[0].1, &before[0].2));
        db.set_file_text(file, &original.replace("true", "1u32"));
        assert!(checked(&db, file, None, (true, true)).is_empty());
        db.set_file_text(file, original);
        assert_eq!(checked(&db, file, None, (true, true)), before);
    }

    #[test]
    fn file_result_keys_the_selected_crate_scope() {
        let (db, files) = RootDatabase::with_many_files(
            r#"
//- /app.rs crate:app
#[path = "shared.rs"] mod shared;
fn value() -> u32 { 1 }
//- /shared.rs
pub fn caller() { let _: u32 = crate::value(); }
//- /other.rs crate:other
#[path = "shared.rs"] mod shared;
fn value() -> bool { true }
"#,
        );
        let [app, shared, other] = [0, 1, 2].map(|it| files[it].file_id(&db));
        let within = |file| {
            let crates: FxHashSet<_> = ide_db::base_db::relevant_crates(&db, file)
                .iter()
                .copied()
                .filter(|krate| krate.data(&db).root_file_id == file)
                .collect();
            assert_eq!(crates.len(), 1);
            Some(Arc::new(crates))
        };
        let (app, other) = (within(app), within(other));
        assert_ne!(app, other);
        assert!(checked(&db, shared, app.clone(), (true, true)).is_empty());
        assert_eq!(codes(&checked(&db, shared, other, (true, true))), ["E0308"]);
        assert!(checked(&db, shared, app, (true, true)).is_empty());
    }

    #[test]
    fn file_result_keys_lint_and_borrow_refusal_rules() {
        let (db, file) = RootDatabase::with_single_file(
            r#"
#![deny(non_snake_case)]
fn BadName() {}
fn caller<'a>() -> &'a u32 { let value = 1u32; &value }
"#,
        );
        let file = file.file_id(&db);
        for _ in 0..2 {
            assert!(checked(&db, file, None, (false, false)).is_empty());
            assert_eq!(codes(&checked(&db, file, None, (true, false))), ["non_snake_case"]);
            assert_eq!(codes(&checked(&db, file, None, (false, true))), ["E0515"]);
            assert_eq!(codes(&checked(&db, file, None, (true, true))), ["E0515", "non_snake_case"],);
        }
    }
}
