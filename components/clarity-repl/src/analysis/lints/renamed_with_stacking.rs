//! Lint to explain that `with-stacking` was renamed to `with-staking`
//! starting in Clarity 6.

use clarity::vm::analysis::analysis_db::AnalysisDatabase;
use clarity::vm::analysis::types::ContractAnalysis;
use clarity::vm::diagnostic::{Diagnostic, Level};
use clarity::vm::{ClarityName, ClarityVersion, SymbolicExpression};

use crate::analysis::annotation::{get_index_of_span, Annotation, AnnotationKind, WarningKind};
use crate::analysis::ast_visitor::{traverse, ASTVisitor};
use crate::analysis::cache::AnalysisCache;
use crate::analysis::linter::Lint;
use crate::analysis::{self, AnalysisPass, AnalysisResult, LintName};

pub fn check_renamed_with_stacking(
    expressions: &[SymbolicExpression],
    clarity_version: ClarityVersion,
    annotations: &[Annotation],
    level: Level,
) -> Vec<Diagnostic> {
    let checker = RenamedWithStacking::new(clarity_version, annotations, level);
    checker.run_expressions(expressions)
}

pub struct RenamedWithStacking<'a> {
    clarity_version: ClarityVersion,
    diagnostics: Vec<Diagnostic>,
    annotations: &'a [Annotation],
    level: Level,
    active_annotation: Option<usize>,
}

impl<'a> RenamedWithStacking<'a> {
    fn new(clarity_version: ClarityVersion, annotations: &'a [Annotation], level: Level) -> Self {
        Self {
            clarity_version,
            diagnostics: Vec::new(),
            annotations,
            level,
            active_annotation: None,
        }
    }

    fn run(mut self, contract_analysis: &'a ContractAnalysis) -> AnalysisResult {
        if contract_analysis.clarity_version < ClarityVersion::Clarity6 {
            return Ok(self.diagnostics);
        }

        traverse(&mut self, &contract_analysis.expressions);
        Ok(self.diagnostics)
    }

    fn run_expressions(mut self, expressions: &'a [SymbolicExpression]) -> Vec<Diagnostic> {
        if self.clarity_version < ClarityVersion::Clarity6 {
            return self.diagnostics;
        }

        traverse(&mut self, expressions);
        self.diagnostics
    }

    fn allow(&self) -> bool {
        self.active_annotation
            .map(|idx| Self::match_allow_annotation(&self.annotations[idx]))
            .unwrap_or(false)
    }
}

impl<'a> ASTVisitor<'a> for RenamedWithStacking<'a> {
    fn get_clarity_version(&self) -> &ClarityVersion {
        &self.clarity_version
    }

    fn visit_call_user_defined(
        &mut self,
        expr: &'a SymbolicExpression,
        name: &'a ClarityName,
        _args: &'a [SymbolicExpression],
    ) -> bool {
        if name.as_str() != "with-stacking" {
            return true;
        }

        self.active_annotation = get_index_of_span(self.annotations, &expr.span);
        if self.allow() {
            return true;
        }

        self.diagnostics.push(Diagnostic {
            level: self.level.clone(),
            message:
                "`with-stacking` was renamed to `with-staking` in Clarity 6. Replace this call with `with-staking`."
                    .to_string(),
            spans: vec![expr.span.clone()],
            suggestion: Some("Replace `with-stacking` with `with-staking`.".to_string()),
        });

        true
    }
}

impl AnalysisPass for RenamedWithStacking<'_> {
    fn run_pass(
        _analysis_db: &mut AnalysisDatabase,
        analysis_cache: &mut AnalysisCache,
        level: Level,
        _settings: &analysis::Settings,
    ) -> AnalysisResult {
        let checker = RenamedWithStacking::new(
            analysis_cache.contract_analysis.clarity_version,
            analysis_cache.annotations,
            level,
        );
        checker.run(analysis_cache.contract_analysis)
    }
}

impl Lint for RenamedWithStacking<'_> {
    fn get_name() -> LintName {
        LintName::RenamedWithStacking
    }

    fn match_allow_annotation(annotation: &Annotation) -> bool {
        match &annotation.kind {
            AnnotationKind::Allow(warning_kinds) => {
                warning_kinds.contains(&WarningKind::RenamedWithStacking)
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use clarity::types::StacksEpochId;
    use clarity::vm::diagnostic::{Diagnostic, Level};
    use clarity::vm::ClarityVersion;
    use indoc::indoc;

    use super::{check_renamed_with_stacking, RenamedWithStacking};
    use crate::analysis::annotation::Annotation;
    use crate::analysis::linter::Lint;
    use crate::repl::session::Session;
    use crate::repl::SessionSettings;
    use crate::test_fixtures::clarity_contract::ClarityContractBuilder;

    /// Parse `snippet` at the given epoch/version and run the lint over the raw AST.
    ///
    /// `build_ast` only parses, so this exercises the visitor in isolation: the
    /// result cannot be influenced by the type checker accepting or rejecting the
    /// contract.
    fn lint_ast_at(
        snippet: String,
        epoch: StacksEpochId,
        clarity_version: ClarityVersion,
    ) -> Vec<Diagnostic> {
        let mut session = Session::new_without_boot_contracts(SessionSettings::default());
        session.update_epoch(epoch);

        let contract = ClarityContractBuilder::new()
            .code_source(snippet)
            .name("checker")
            .epoch(epoch)
            .clarity_version(clarity_version)
            .build();
        let (ast, _, _) = session.interpreter.build_ast(&contract);

        check_renamed_with_stacking(
            &ast.expressions,
            clarity_version,
            &Vec::<Annotation>::new(),
            Level::Warning,
        )
    }

    /// Run the lint over `snippet` as a Clarity 6 contract.
    fn lint_ast(snippet: String) -> Vec<Diagnostic> {
        lint_ast_at(snippet, StacksEpochId::Epoch40, ClarityVersion::Clarity6)
    }

    fn run_snippet(
        snippet: String,
    ) -> Result<
        crate::repl::session::AnnotatedExecutionResult,
        Vec<clarity::vm::diagnostic::Diagnostic>,
    > {
        let mut settings = SessionSettings::default();
        settings
            .repl_settings
            .analysis
            .enable_lint(RenamedWithStacking::get_name(), Level::Warning);

        let mut session = Session::new_without_boot_contracts(settings);
        session.update_epoch(StacksEpochId::Epoch40);

        match session.formatted_interpretation(snippet, Some("checker".to_string()), false, None) {
            Ok((_, result)) => Ok(result),
            Err((_, diagnostics)) => Err(diagnostics),
        }
    }

    #[test]
    fn warn_with_stacking_usage_in_clarity6() {
        #[rustfmt::skip]
        let snippet = indoc!("
            (define-public (test)
                (as-contract? ((with-stacking u1)) true))
        ").to_string();

        let result = run_snippet(snippet);
        match result {
            Ok(_) => panic!("expected analysis error for unresolved `with-stacking`"),
            Err(diagnostics) => {
                assert!(diagnostics
                    .iter()
                    .any(|diagnostic| { diagnostic.message.contains("with-staking") }));
            }
        }
    }

    #[test]
    fn check_pre_analysis_directly() {
        #[rustfmt::skip]
        let snippet = indoc!("
            (define-public (test)
                (as-contract? ((with-stacking u1)) true))
        ").to_string();

        let mut session = Session::new_without_boot_contracts(SessionSettings::default());
        session.update_epoch(StacksEpochId::Epoch40);

        let contract = ClarityContractBuilder::new()
            .code_source(snippet)
            .name("checker")
            .epoch(clarity::types::StacksEpochId::Epoch40)
            .clarity_version(clarity::vm::ClarityVersion::Clarity6)
            .build();
        let (ast, _, _) = session.interpreter.build_ast(&contract);

        let diagnostics = check_renamed_with_stacking(
            &ast.expressions,
            clarity::vm::ClarityVersion::Clarity6,
            &Vec::<Annotation>::new(),
            Level::Warning,
        );

        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("with-staking"));
    }

    /// `with-stacking` is still a builtin in Clarity 5, so using it must not warn.
    ///
    /// This is the realistic regression test: it pins the behaviour every existing
    /// Clarity 4/5 contract depends on. Paired with `check_pre_analysis_directly`,
    /// which flags the identical snippet at Clarity 6, the Clarity version is the
    /// only variable between warning and silence.
    ///
    /// Note what this does *not* prove. At Clarity 5 the lint is silent for two
    /// independent reasons, and only one is the version gate in `run_expressions`:
    /// `lookup_by_name_at_version("with-stacking", Clarity5)` resolves to
    /// `AllowanceWithStacking`, so `traverse_list` dispatches to
    /// `traverse_allowance` and `visit_call_user_defined` is never reached. This
    /// test therefore still passes with the version gate deleted — see
    /// `no_warning_before_with_stacking_was_introduced` for the case that pins it.
    #[test]
    fn no_warning_in_clarity5() {
        #[rustfmt::skip]
        let snippet = indoc!("
            (define-public (test)
                (as-contract? ((with-stacking u1)) true))
        ").to_string();

        let diagnostics = lint_ast_at(snippet, StacksEpochId::Epoch34, ClarityVersion::Clarity5);

        assert!(
            diagnostics.is_empty(),
            "`with-stacking` is a valid builtin in Clarity 5 and must not be \
             reported as renamed; got {diagnostics:?}"
        );
    }

    /// The Clarity 1-3 case, which is what actually pins the version gate.
    ///
    /// `with-stacking` was introduced in Clarity 4, so at Clarity 3 it is neither a
    /// builtin nor the post-rename spelling: `lookup_by_name_at_version` returns
    /// `None`, `traverse_list` falls through to `traverse_call_user_defined`, and
    /// `visit_call_user_defined` *does* fire. The only thing standing between that
    /// and a nonsensical "renamed in Clarity 6" warning on a Clarity 3 contract is
    /// the `clarity_version < Clarity6` early return.
    ///
    /// Delete that gate and this test fails while `no_warning_in_clarity5` keeps
    /// passing.
    #[test]
    fn no_warning_before_with_stacking_was_introduced() {
        #[rustfmt::skip]
        let snippet = indoc!("
            (define-public (test)
                (ok (with-stacking u1)))
        ").to_string();

        let diagnostics = lint_ast_at(snippet, StacksEpochId::Epoch30, ClarityVersion::Clarity3);

        assert!(
            diagnostics.is_empty(),
            "`with-stacking` did not exist before Clarity 4, so a Clarity 3 contract \
             must not be told it was renamed; got {diagnostics:?}"
        );
    }

    /// FAILING (review finding #1): the lint never fires for `restrict-assets?`.
    ///
    /// `with-stacking` is an allowance, valid in both `as-contract?` and
    /// `restrict-assets?`. Only the former is covered:
    ///
    /// - `as-contract?` dispatches to `traverse_as_contract_safe`, which calls
    ///   `traverse_expr` on each element of the allowance list, so the
    ///   `(with-stacking u1)` call reaches `visit_call_user_defined`.
    /// - `restrict-assets?` dispatches to the `RestrictAssets` arm of
    ///   `traverse_list` (ast_visitor.rs), which passes `args[0]` — the *owner* —
    ///   to `match_pairs` instead of `args[1]`. `match_pairs` calls `match_list()`
    ///   on an atom, gets `None`, and `unwrap_or_default()` yields an empty map, so
    ///   the allowance list is dropped entirely.
    ///
    /// Fixing the index alone is not enough: `traverse_restrict_assets` only
    /// descends into each binding's *value* (`u1`), never the `(with-stacking u1)`
    /// call expression itself.
    #[test]
    fn warn_with_stacking_in_restrict_assets() {
        #[rustfmt::skip]
        let snippet = indoc!("
            (define-public (test)
                (restrict-assets? tx-sender ((with-stacking u1)) (ok true)))
        ").to_string();

        let diagnostics = lint_ast(snippet);

        assert_eq!(
            diagnostics.len(),
            1,
            "expected `with-stacking` in a `restrict-assets?` allowance list to be \
             flagged, but the allowance list is never traversed; got {diagnostics:?}"
        );
    }

    /// FAILING (review finding #2): the lint fires on a user-defined function that
    /// happens to be named `with-stacking`.
    ///
    /// `is_reserved_word` reserves only `block-height`, so once `with-stacking`
    /// stopped being a builtin in Clarity 6 it became a legal user identifier.
    /// `visit_call_user_defined` cannot tell the two apart, so a contract that
    /// defines and calls its own `with-stacking` is told to rename it.
    ///
    /// The repo already settled this question the other way for the removed
    /// `at-block` builtin — see `lints/at_block.rs`, which disables itself at
    /// Clarity 5+ with the comment "`at-block` is no longer part of the language,
    /// so allow it to be used as a user-defined function".
    #[test]
    fn no_warning_for_user_defined_with_stacking() {
        #[rustfmt::skip]
        let snippet = indoc!("
            (define-private (with-stacking (amount uint))
                amount)
            (define-public (test)
                (ok (with-stacking u1)))
        ").to_string();

        let diagnostics = lint_ast(snippet);

        assert!(
            diagnostics.is_empty(),
            "`with-stacking` is a legal user-defined function name in Clarity 6, so \
             calling it must not be reported as the renamed builtin; got {diagnostics:?}"
        );
    }

    /// FAILING (review finding #2, end-to-end): same false positive, but reached
    /// through the normal lint pass rather than the pre-analysis hook.
    ///
    /// This contract type-checks cleanly, so `clarity::vm::analysis::run_analysis`
    /// succeeds and `RenamedWithStacking::run_pass` runs as a regular lint. The bad
    /// diagnostic is therefore user-visible from `clarinet check`, not just an
    /// artifact of calling the checker directly.
    #[test]
    fn no_warning_for_user_defined_with_stacking_end_to_end() {
        #[rustfmt::skip]
        let snippet = indoc!("
            (define-private (with-stacking (amount uint))
                amount)
            (define-public (test)
                (ok (with-stacking u1)))
        ").to_string();

        let result = run_snippet(snippet)
            .expect("contract defining its own `with-stacking` should type-check");

        let renamed: Vec<&str> = result
            .lint_diagnostics
            .iter()
            .map(|ld| ld.diagnostic.message.as_str())
            .filter(|message| message.contains("was renamed to `with-staking`"))
            .collect();

        assert!(
            renamed.is_empty(),
            "calling a user-defined `with-stacking` must not be reported as the \
             renamed builtin; got {renamed:?}"
        );
    }

    #[test]
    fn allow_with_annotation() {
        #[rustfmt::skip]
        let snippet = indoc!("
            (define-public (test)
                ;; #[allow(renamed_with_stacking)]
                (as-contract? ((with-stacking u1)) true))
        ").to_string();

        let result = run_snippet(snippet);
        match result {
            Ok(_) => panic!("expected analysis error for unresolved `with-stacking`"),
            Err(diagnostics) => {
                assert!(!diagnostics.iter().any(|diagnostic| {
                    diagnostic.message.contains("was renamed to `with-staking`")
                }));
            }
        }
    }
}
