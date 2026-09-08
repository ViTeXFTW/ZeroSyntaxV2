//! One-shot diagnostics for shells, CI, and coding agents.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{bail, Context, Result};
use clap::{value_parser, Arg, ArgAction, Command};
use serde::Serialize;
use tower_lsp::lsp_types::Url;
use zerosyntax_analysis::diagnostics;
use zerosyntax_analysis::index::{
    definitions_in, module_tags_in, object_models_in, object_parents_in, references_in,
};
use zerosyntax_analysis::{Analyzer, Diagnostic, Severity, WorkspaceIndex};
use zerosyntax_syntax::Parse;

use crate::scan::{
    collect_scan_paths_checked, load_sibling_str_keys, read_lossy, scan_files_checked, ScanEntry,
};

pub(crate) fn run(args: impl IntoIterator<Item = OsString>) -> ExitCode {
    let matches = match command().try_get_matches_from(args) {
        Ok(matches) => matches,
        Err(error) => {
            let code = error.exit_code();
            let _ = error.print();
            return ExitCode::from(code as u8);
        }
    };

    let Some(("check", check)) = matches.subcommand() else {
        return ExitCode::from(2);
    };
    match check_command(check) {
        Ok(true) => ExitCode::from(1),
        Ok(false) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("zerosyntax-lsp: {error:#}");
            ExitCode::from(2)
        }
    }
}

fn command() -> Command {
    Command::new("zerosyntax-lsp")
        .version(env!("CARGO_PKG_VERSION"))
        .about("Language server and diagnostics checker for Generals INI files")
        .subcommand(
            Command::new("check")
                .about("Check INI files and directories")
                .arg(
                    Arg::new("base-root")
                        .long("base-root")
                        .value_name("PATH")
                        .value_parser(value_parser!(PathBuf))
                        .action(ArgAction::Append)
                        .help(
                            "INI and game assets directory or .big archive loaded before targets",
                        ),
                )
                .arg(
                    Arg::new("stdin-filename")
                        .long("stdin-filename")
                        .value_name("PATH")
                        .help("Display and index name for stdin (default: <stdin>)"),
                )
                .arg(
                    Arg::new("json")
                        .long("json")
                        .action(ArgAction::SetTrue)
                        .help("Emit a JSON diagnostic array"),
                )
                .arg(
                    Arg::new("fail-on")
                        .long("fail-on")
                        .value_name("SEVERITY")
                        .value_parser(["error", "warning", "hint"])
                        .default_value("error")
                        .help("Lowest diagnostic severity that produces exit 1"),
                )
                .arg(
                    Arg::new("targets")
                        .value_name("PATH|-")
                        .value_parser(value_parser!(OsString))
                        .num_args(1..)
                        .required(true),
                ),
        )
}

fn check_command(matches: &clap::ArgMatches) -> Result<bool> {
    let target_args: Vec<OsString> = matches
        .get_many::<OsString>("targets")
        .expect("required by clap")
        .cloned()
        .collect();
    let stdin_count = target_args.iter().filter(|arg| *arg == "-").count();
    if stdin_count > 1 {
        bail!("stdin target '-' may only be specified once");
    }
    if matches.get_one::<String>("stdin-filename").is_some() && stdin_count == 0 {
        bail!("--stdin-filename requires the '-' target");
    }

    let base_roots = canonical_roots(
        matches
            .get_many::<PathBuf>("base-root")
            .into_iter()
            .flatten()
            .cloned(),
        RootKind::Base,
    )?;
    let target_roots = canonical_roots(
        target_args
            .iter()
            .filter(|arg| *arg != "-")
            .map(PathBuf::from),
        RootKind::Target,
    )?;

    let analyzer = Analyzer::embedded();
    let mut index = WorkspaceIndex::new();

    let mut base_paths = collect_scan_paths_checked(&base_roots)?;
    sort_dedup_paths(&mut base_paths);
    apply_entries(&mut index, scan_files_checked(&analyzer, &base_paths)?);

    let mut target_scan_paths = collect_scan_paths_checked(&target_roots)?;
    sort_dedup_paths(&mut target_scan_paths);
    apply_entries(
        &mut index,
        scan_files_checked(&analyzer, &target_scan_paths)?,
    );

    let cwd = std::env::current_dir().context("failed to determine current directory")?;
    let mut documents = Vec::new();
    for path in target_scan_paths
        .iter()
        .filter(|path| has_extension(path, "ini"))
    {
        let document = TargetDocument::from_path(&analyzer, path, &cwd)?;
        let uri = Url::parse(&document.file).expect("created from a valid file path");
        index.set_ini_string_keys(&document.file, load_sibling_str_keys(&uri));
        documents.push(document);
    }

    if stdin_count == 1 {
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .context("failed to read stdin")?;
        let name = matches
            .get_one::<String>("stdin-filename")
            .cloned()
            .unwrap_or_else(|| "<stdin>".to_string());
        let parse = analyzer.parse(&text);
        index_document(&mut index, &analyzer, &parse, &name);
        documents.push(TargetDocument {
            display: name.clone(),
            file: name,
            text,
            parse,
        });
    }

    documents.sort_by(|a, b| a.display.cmp(&b.display));
    let mut output = Vec::new();
    for document in &documents {
        let line_index = LineIndex::new(&document.text);
        output.extend(
            diagnostics::diagnose(
                &analyzer,
                &document.parse,
                Some(&index),
                Some(&document.file),
            )
            .into_iter()
            .map(|diagnostic| OutputDiagnostic::new(document, &line_index, diagnostic)),
        );
    }
    output.sort_by(|a, b| {
        (
            &a.file,
            a.range.start.line,
            a.range.start.column,
            a.range.end.line,
            a.range.end.column,
            &a.code,
        )
            .cmp(&(
                &b.file,
                b.range.start.line,
                b.range.start.column,
                b.range.end.line,
                b.range.end.column,
                &b.code,
            ))
    });

    if matches.get_flag("json") {
        let stdout = std::io::stdout();
        let mut writer = stdout.lock();
        serde_json::to_writer(&mut writer, &output).context("failed to write JSON output")?;
        writeln!(writer).context("failed to write JSON output")?;
    } else {
        write_human(&output)?;
    }

    let threshold = match matches
        .get_one::<String>("fail-on")
        .map(String::as_str)
        .expect("defaulted by clap")
    {
        "error" => 3,
        "warning" => 2,
        "hint" => 1,
        _ => unreachable!("validated by clap"),
    };
    Ok(output
        .iter()
        .any(|diagnostic| diagnostic.severity.rank() >= threshold))
}

#[derive(Clone, Copy)]
enum RootKind {
    Base,
    Target,
}

fn canonical_roots(
    roots: impl IntoIterator<Item = PathBuf>,
    kind: RootKind,
) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for root in roots {
        let canonical = std::fs::canonicalize(&root)
            .with_context(|| format!("cannot access {}", root.display()))?;
        let metadata = std::fs::metadata(&canonical)
            .with_context(|| format!("cannot inspect {}", root.display()))?;
        let supported = if metadata.is_dir() {
            true
        } else {
            match kind {
                RootKind::Base => has_extension(&canonical, "big"),
                RootKind::Target => has_extension(&canonical, "ini"),
            }
        };
        if !supported {
            let expected = match kind {
                RootKind::Base => "a directory or .big archive",
                RootKind::Target => "a directory or .ini file",
            };
            bail!("{} is not {expected}", root.display());
        }
        out.push(canonical);
    }
    sort_dedup_paths(&mut out);
    Ok(out)
}

fn sort_dedup_paths(paths: &mut Vec<PathBuf>) {
    paths.sort();
    paths.dedup();
}

fn has_extension(path: &Path, extension: &str) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case(extension))
}

fn apply_entries(index: &mut WorkspaceIndex, entries: Vec<ScanEntry>) {
    for (
        file,
        definitions,
        references,
        tags,
        object_models,
        object_parents,
        models,
        assets,
        _,
        animations,
    ) in entries
    {
        index.set_file(&file, definitions);
        index.set_file_refs(&file, references);
        index.set_file_tags(&file, tags);
        index.set_file_object_models(&file, object_models);
        index.set_file_object_parents(&file, object_parents);
        index.set_file_models(&file, models);
        index.set_file_animations(&file, animations);
        index.set_file_assets(&file, assets);
    }
}

fn index_document(index: &mut WorkspaceIndex, analyzer: &Analyzer, parse: &Parse, file: &str) {
    index.set_file(file, definitions_in(analyzer, parse, file));
    index.set_file_refs(file, references_in(analyzer, parse));
    index.set_file_tags(file, module_tags_in(analyzer, parse));
    index.set_file_object_models(file, object_models_in(analyzer, parse));
    index.set_file_object_parents(file, object_parents_in(parse));
}

struct TargetDocument {
    display: String,
    file: String,
    text: String,
    parse: Parse,
}

impl TargetDocument {
    fn from_path(analyzer: &Analyzer, path: &Path, cwd: &Path) -> Result<Self> {
        let text = read_lossy(path)?;
        let parse = analyzer.parse(&text);
        let file = Url::from_file_path(path)
            .map_err(|_| anyhow::anyhow!("cannot convert {} to a file URI", path.display()))?
            .to_string();
        let display = path
            .strip_prefix(cwd)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        Ok(Self {
            display,
            file,
            text,
            parse,
        })
    }
}

#[derive(Serialize)]
struct OutputDiagnostic {
    file: String,
    range: OutputRange,
    severity: OutputSeverity,
    code: &'static str,
    message: String,
}

impl OutputDiagnostic {
    fn new(document: &TargetDocument, lines: &LineIndex, diagnostic: Diagnostic) -> Self {
        Self {
            file: document.display.clone(),
            range: OutputRange {
                start: lines.position(&document.text, diagnostic.span.start as usize),
                end: lines.position(&document.text, diagnostic.span.end as usize),
            },
            severity: diagnostic.severity.into(),
            code: diagnostic.code,
            message: diagnostic.message,
        }
    }
}

#[derive(Serialize)]
struct OutputRange {
    start: OutputPosition,
    end: OutputPosition,
}

#[derive(Serialize)]
struct OutputPosition {
    line: usize,
    column: usize,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
enum OutputSeverity {
    Error,
    Warning,
    Hint,
}

impl OutputSeverity {
    fn rank(self) -> u8 {
        match self {
            Self::Error => 3,
            Self::Warning => 2,
            Self::Hint => 1,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Hint => "hint",
        }
    }
}

impl From<Severity> for OutputSeverity {
    fn from(value: Severity) -> Self {
        match value {
            Severity::Error => Self::Error,
            Severity::Warning => Self::Warning,
            Severity::Hint => Self::Hint,
        }
    }
}

struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    fn new(text: &str) -> Self {
        let mut starts = vec![0];
        starts.extend(
            text.match_indices('\n')
                .map(|(offset, _)| offset.saturating_add(1)),
        );
        Self { starts }
    }

    fn position(&self, text: &str, offset: usize) -> OutputPosition {
        let offset = offset.min(text.len());
        let line = self.starts.partition_point(|start| *start <= offset) - 1;
        OutputPosition {
            line: line + 1,
            column: text[self.starts[line]..offset].chars().count() + 1,
        }
    }
}

fn write_human(diagnostics: &[OutputDiagnostic]) -> Result<()> {
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();
    for diagnostic in diagnostics {
        writeln!(
            writer,
            "{}:{}:{}: {}[{}]: {}",
            diagnostic.file,
            diagnostic.range.start.line,
            diagnostic.range.start.column,
            diagnostic.severity.as_str(),
            diagnostic.code,
            diagnostic.message
        )?;
    }

    let counts = diagnostics.iter().fold([0usize; 3], |mut counts, item| {
        counts[match item.severity {
            OutputSeverity::Error => 0,
            OutputSeverity::Warning => 1,
            OutputSeverity::Hint => 2,
        }] += 1;
        counts
    });
    writeln!(
        writer,
        "{} error(s), {} warning(s), {} hint(s)",
        counts[0], counts[1], counts[2]
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_scalar_positions_are_one_based() {
        let text = "; é🚀\nWeapon X\n";
        let index = LineIndex::new(text);
        assert_eq!(index.position(text, "; é".len()).line, 1);
        assert_eq!(index.position(text, "; é".len()).column, 4);
        assert_eq!(index.position(text, text.find("Weapon").unwrap()).line, 2);
        assert_eq!(index.position(text, text.find("Weapon").unwrap()).column, 1);
    }
}
