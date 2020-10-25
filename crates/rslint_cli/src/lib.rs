mod cli;
mod config;
mod files;
mod panic_hook;

pub use self::{
    cli::{show_all_rules, ExplanationRunner},
    config::*,
    files::*,
    panic_hook::*,
};
pub use rslint_core::Outcome;
pub use rslint_errors::{
    file, file::Files, Diagnostic, Emitter, Formatter, LongFormatter, Severity, ShortFormatter,
};

use colored::*;
use rayon::prelude::*;
use rslint_core::autofix::recursively_apply_fixes;
use rslint_core::{lint_file, util::find_best_match_for_name, CstRuleStore, LintResult, RuleLevel};
use std::fs::write;

pub(crate) const REPO_LINK: &str = "https://github.com/RDambrosio016/RSLint";

#[allow(unused_must_use)]
pub fn run(glob: String, verbose: bool, fix: bool, dirty: bool, formatter: Option<String>) {
    let res = glob::glob(&glob);
    if let Err(err) = res {
        lint_err!("Invalid glob pattern: {}", err);
        return;
    }

    let handle = config::Config::new_threaded();
    let mut walker = FileWalker::from_glob(res.unwrap());
    let joined = handle.join();
    let config = joined.expect("config thread paniced");

    let store = if let Some(cfg) = config.as_ref().and_then(|cfg| cfg.rules.as_ref()) {
        cfg.store()
    } else {
        CstRuleStore::new().builtins()
    };
    let mut formatter = formatter
        .or_else(|| config.as_ref().map(|c| c.errors.formatter.clone()))
        .unwrap_or_else(|| String::from("long"));

    verify_formatter(&mut formatter);

    if walker.files.is_empty() {
        lint_err!("No matching files found");
        return;
    }

    let mut results = walker
        .files
        .par_keys()
        .map(|id| {
            let file = walker.files.get(id).unwrap();
            lint_file(
                *id,
                &file.source.clone(),
                file.kind == JsFileKind::Module,
                &store,
                verbose,
            )
        })
        .filter_map(|res| {
            if let Err(diagnostic) = res {
                emit_diagnostic(&diagnostic, &walker);
                None
            } else {
                res.ok()
            }
        })
        .collect::<Vec<_>>();

    let fix_count = if fix {
        apply_fixes(&mut results, &mut walker, dirty)
    } else {
        0
    };
    print_results(
        &mut results,
        &walker,
        config.as_ref(),
        fix_count,
        &formatter,
    );
}

pub fn apply_fixes(results: &mut Vec<LintResult>, walker: &mut FileWalker, dirty: bool) -> usize {
    let mut fix_count = 0;
    // TODO: should we aquire a file lock if we know we need to run autofix?
    for res in results {
        let file = walker.files.get_mut(&res.file_id).unwrap();
        // skip virtual files
        if file.path.is_none() {
            continue;
        }
        if res
            .parser_diagnostics
            .iter()
            .any(|x| x.severity == Severity::Error)
            && !dirty
        {
            lint_note!(
                "skipping autofix for `{}` because it contains syntax errors",
                file.path.as_ref().unwrap().to_string_lossy()
            );
            continue;
        }
        let original_problem_num = res
            .rule_results
            .iter()
            .filter(|(_, x)| x.outcome() == Outcome::Warning || x.outcome() == Outcome::Failure)
            .map(|(_, res)| res.diagnostics.len())
            .sum::<usize>();
        let fixed = recursively_apply_fixes(res);
        let new_problem_num = res
            .rule_results
            .iter()
            .filter(|(_, x)| x.outcome() == Outcome::Warning || x.outcome() == Outcome::Failure)
            .map(|(_, res)| res.diagnostics.len())
            .sum::<usize>();
        let path = file.path.as_ref().unwrap();
        if let Err(err) = write(path, fixed.clone()) {
            lint_err!("failed to write to `{:#?}`: {}", path, err.to_string());
        } else {
            file.update_src(fixed);
            fix_count += original_problem_num.saturating_sub(new_problem_num);
        }
    }
    fix_count
}

pub(crate) fn print_results(
    results: &mut Vec<LintResult>,
    walker: &FileWalker,
    config: Option<&config::Config>,
    fix_count: usize,
    formatter: &str,
) {
    // Map each diagnostic to the correct level according to configured rule level
    for result in results.iter_mut() {
        for (rule_name, diagnostics) in result
            .rule_results
            .iter_mut()
            .map(|x| (x.0, &mut x.1.diagnostics))
        {
            if let Some(conf) = config.and_then(|cfg| cfg.rules.as_ref()) {
                remap_diagnostics_to_level(diagnostics, conf.rule_level_by_name(rule_name));
            }
        }
    }

    let failures = results
        .iter()
        .filter(|res| res.outcome() == Outcome::Failure)
        .count();
    let warnings = results
        .iter()
        .filter(|res| res.outcome() == Outcome::Warning)
        .count();
    let successes = results
        .iter()
        .filter(|res| res.outcome() == Outcome::Success)
        .count();

    let overall = Outcome::merge(results.iter().map(|res| res.outcome()));

    for result in results.iter_mut() {
        emit_diagnostics(
            formatter,
            &result.diagnostics().cloned().collect::<Vec<_>>(),
            walker,
        );
    }

    output_overall(failures, warnings, successes, fix_count);
    if overall == Outcome::Failure {
        println!("\nhelp: for more information about the errors try the explain command: `rslint explain <rules>`");
    }
}

pub fn verify_formatter(formatter: &mut String) {
    if !matches!(formatter.as_str(), "short" | "long") {
        if let Some(suggestion) =
            find_best_match_for_name(vec!["short", "long"].into_iter(), formatter, None)
        {
            lint_err!(
                "unknown formatter `{}`, using default formatter, did you mean `{}`?",
                formatter,
                suggestion
            );
        } else {
            lint_err!("unknown formatter `{}`, using default formatter", formatter);
        }
        *formatter = "long".to_string();
    }
}

pub fn emit_diagnostics(formatter: &str, diagnostics: &[Diagnostic], files: &dyn Files) {
    match formatter {
        "short" => {
            if let Err(err) = ShortFormatter.emit_stderr(diagnostics, files) {
                lint_err!("failed to emit diagnostic: {}", err);
            }
        }
        "long" => {
            if let Err(err) = LongFormatter.emit_stderr(diagnostics, files) {
                lint_err!("failed to emit diagnostic: {}", err);
            }
        }
        f => {
            if let Some(suggestion) =
                find_best_match_for_name(vec!["short", "long"].into_iter(), f, None)
            {
                lint_err!("unknown formatter `{}`, did you mean `{}`?", f, suggestion);
            } else {
                lint_err!("unknown formatter `{}`", f);
            }
        }
    }
}

#[allow(unused_must_use)]
fn output_overall(failures: usize, warnings: usize, successes: usize, fix_count: usize) {
    println!(
        "{}: {} fail, {} warn, {} success{}",
        "Outcome".white(),
        failures.to_string().red(),
        warnings.to_string().yellow(),
        successes.to_string().green(),
        if fix_count > 0 {
            format!(
                ", {} issue{} fixed",
                fix_count.to_string().green(),
                if fix_count == 1 { "" } else { "s" }
            )
        } else {
            "".to_string()
        }
    );
}

/// Remap each error diagnostic to a warning diagnostic based on the rule's level.
/// this leaves warnings untouched because rules should be able to emit errors and warnings for context without
/// the warnings being remapped to errors.
pub fn remap_diagnostics_to_level(diagnostics: &mut Vec<Diagnostic>, level: RuleLevel) {
    for diagnostic in diagnostics.iter_mut() {
        match diagnostic.severity {
            Severity::Error if level == RuleLevel::Warning => {
                diagnostic.severity = Severity::Warning
            }
            _ => {}
        }
    }
}

pub fn emit_diagnostic(diagnostic: &Diagnostic, walker: &dyn file::Files) {
    let mut emitter = Emitter::new(walker);
    emitter
        .emit_stderr(&diagnostic, true)
        .expect("failed to throw linter diagnostic")
}

// TODO: don't use expect because we treat panics as linter bugs
#[macro_export]
macro_rules! lint_diagnostic {
    ($severity:ident, $($format_args:tt)*) => {
    use rslint_errors::Emitter;

    let diag = $crate::Diagnostic::$severity(1, "", format!($($format_args)*));
    let file = rslint_errors::file::SimpleFile::new("".into(), "".into());
    let mut emitter = Emitter::new(&file);
    emitter
        .emit_stderr(&diag, true)
        .expect("failed to throw linter diagnostic")
    }
}

/// Construct a simple linter error and immediately throw it to stderr
#[macro_export]
macro_rules! lint_err {
    ($($format_args:tt)*) => {{
        $crate::lint_diagnostic!(error, $($format_args)*);
    }};
}

/// Construct a simple linter warning and immediately throw it to stderr
#[macro_export]
macro_rules! lint_warn {
    ($($format_args:tt)*) => {{
        $crate::lint_diagnostic!(warning, $($format_args)*);
    }};
}

/// Construct a simple linter note and immediately throw it to stderr
#[macro_export]
macro_rules! lint_note {
    ($($format_args:tt)*) => {{
        $crate::lint_diagnostic!(note, $($format_args)*);
    }};
}
