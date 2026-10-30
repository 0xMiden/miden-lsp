use std::path::{Path, PathBuf};

use miden_assembly_syntax::diagnostics::{
    Diagnostic as MidenDiagnostic, Severity, miette::SourceCode,
};
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range, Url};

use crate::{
    analysis::normalize_path,
    document::{byte_range_to_lsp_range, compute_line_offsets},
};

pub(crate) fn append(
    diagnostic: &dyn MidenDiagnostic,
    document: &Path,
    text: &str,
    source: &str,
    output: &mut Vec<Diagnostic>,
) {
    visit(diagnostic, None, None, document, text, source, output);
}

fn visit<'a>(
    diagnostic: &'a dyn MidenDiagnostic,
    inherited_code: Option<&'a dyn SourceCode>,
    inherited_severity: Option<Severity>,
    document: &Path,
    text: &str,
    source: &str,
    output: &mut Vec<Diagnostic>,
) {
    let code = diagnostic.source_code().or(inherited_code);
    let severity = diagnostic.severity().or(inherited_severity).unwrap_or(Severity::Error);
    if let (Some(code), Some(labels)) = (code, diagnostic.labels()) {
        for label in labels {
            let Ok(contents) = code.read_span(label.inner(), 0, 0) else {
                continue;
            };
            let Some(name) = contents.name() else {
                continue;
            };
            let path = Url::parse(name)
                .ok()
                .and_then(|uri| uri.to_file_path().ok())
                .unwrap_or_else(|| PathBuf::from(name));
            if normalize_path(&path) != normalize_path(document) {
                continue;
            }
            let bounds = label.offset().min(text.len())
                ..label.offset().saturating_add(label.len()).min(text.len());
            let mut message = diagnostic.to_string();
            if let Some(label) = label.label() {
                message.push_str(&format!(": {label}"));
            }
            if let Some(help) = diagnostic.help() {
                message.push_str(&format!("\n{help}"));
            }
            output.push(Diagnostic {
                range: byte_range_to_lsp_range(text, &compute_line_offsets(text), bounds),
                severity: Some(match severity {
                    Severity::Error => DiagnosticSeverity::ERROR,
                    Severity::Warning => DiagnosticSeverity::WARNING,
                    Severity::Advice => DiagnosticSeverity::INFORMATION,
                }),
                code: diagnostic.code().map(|code| NumberOrString::String(code.to_string())),
                source: Some(source.into()),
                message,
                ..Diagnostic::default()
            });
        }
    }
    if let Some(related) = diagnostic.related() {
        for diagnostic in related {
            visit(diagnostic, code, Some(severity), document, text, source, output);
        }
    }
}

pub(crate) fn notice(
    message: impl Into<String>,
    severity: DiagnosticSeverity,
    source: &str,
) -> Diagnostic {
    Diagnostic {
        range: Range::new(Position::new(0, 0), Position::new(0, 0)),
        severity: Some(severity),
        source: Some(source.into()),
        message: message.into(),
        ..Diagnostic::default()
    }
}

#[cfg(test)]
mod tests {
    use miden_assembly_syntax::diagnostics::{
        LabeledSpan, NamedSource, Report, miette::MietteDiagnostic,
    };

    use super::*;

    #[test]
    fn structured_diagnostics_preserve_severity_codes_help_and_utf16_ranges() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("diagnostic with spaces.masm");
        let text = "pub proc main\n# 😀 target\nend\n";
        std::fs::write(&path, text).unwrap();
        let offset = text.find("target").unwrap();
        for (severity, expected) in [
            (Severity::Warning, DiagnosticSeverity::WARNING),
            (Severity::Advice, DiagnosticSeverity::INFORMATION),
            (Severity::Error, DiagnosticSeverity::ERROR),
        ] {
            let report = Report::new(
                MietteDiagnostic::new("structured finding")
                    .with_severity(severity)
                    .with_code("analysis.test")
                    .with_help("check this value")
                    .with_labels([LabeledSpan::new_with_span(
                        Some("value used here".into()),
                        (offset, 6),
                    )]),
            )
            .with_source_code(NamedSource::new(
                Url::from_file_path(&path).unwrap(),
                text.to_string(),
            ));
            let mut output = Vec::new();
            append(report.as_ref(), &path, text, "miden-compiler", &mut output);
            assert_eq!(output.len(), 1);
            assert_eq!(output[0].severity, Some(expected));
            assert_eq!(output[0].range, Range::new(Position::new(1, 5), Position::new(1, 11)));
            assert_eq!(output[0].code, Some(NumberOrString::String("analysis.test".into())));
            assert!(output[0].message.contains("check this value"));
            assert!(output[0].message.contains("value used here"));
        }
    }
}
