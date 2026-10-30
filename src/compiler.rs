use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
};

use miden_assembly_syntax::{
    ModuleParser,
    debuginfo::{SourceManager, Spanned},
};
use miden_core::serde::Deserializable;
use miden_mast_package::Package as MastPackage;
use miden_package_registry::{InMemoryPackageRegistry, PackageProvider};
use miden_project::{
    Package, Project, ProjectDependencyGraphBuilder, ProjectDependencyNodeProvenance,
    ProjectSource, Target,
};
use midenc_dialect_hir::analyses::{AdviceTaintAnalysis, RangeAnalysisStatus};
use midenc_frontend_masm::{
    DisassemblerConfig,
    project::{ExternalMetadata, ProjectTargetInput, load_target_modules},
};
use midenc_hir::{Context, Op, Report, pass::AnalysisManager};
use midenc_hir_analysis::DataFlowConfig;
use midenc_session::{Options, Session, diagnostics::NullEmitter};
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity};

use crate::{analysis::normalize_path, diagnostics, project::OverlaySourceManager};

pub(crate) fn analyze(
    package: &Package,
    target: &Target,
    document: &Path,
    overlays: &BTreeMap<PathBuf, String>,
    cache: &InMemoryPackageRegistry,
    git_cache_root: Option<&Path>,
) -> Vec<Diagnostic> {
    let text = overlays
        .get(&normalize_path(document))
        .cloned()
        .or_else(|| std::fs::read_to_string(document).ok())
        .unwrap_or_default();
    let mut output = Vec::new();
    if let Err(error) =
        analyze_into(package, target, document, overlays, cache, git_cache_root, &mut output)
    {
        let previous = output.len();
        diagnostics::append(error.as_ref(), document, &text, "miden-compiler", &mut output);
        if output.len() == previous {
            output.push(diagnostics::notice(
                format!("Compiler analysis unavailable: {error}"),
                DiagnosticSeverity::WARNING,
                "miden-compiler",
            ));
        }
    }
    output
}

fn analyze_into(
    package: &Package,
    target: &Target,
    document: &Path,
    overlays: &BTreeMap<PathBuf, String>,
    cache: &InMemoryPackageRegistry,
    git_cache_root: Option<&Path>,
    output: &mut Vec<Diagnostic>,
) -> Result<(), Report> {
    let manager = Arc::new(OverlaySourceManager::new(overlays.clone()));
    let manifest = package.manifest_path().ok_or_else(|| Report::msg("project has no manifest"))?;
    let mut builder = ProjectDependencyGraphBuilder::new(cache);
    if let Some(root) = git_cache_root {
        builder = builder.with_git_cache_root(root);
    }
    let graph = builder.build_from_path(manifest)?;
    let mut metadata = ExternalMetadata::default();
    for (name, node) in graph.nodes() {
        if name == graph.root() {
            continue;
        }
        match &node.provenance {
            ProjectDependencyNodeProvenance::Source(ProjectSource::Real {
                manifest_path, ..
            }) => {
                let dependency = Project::load_project_reference(
                    name.as_ref(),
                    manifest_path,
                    manager.as_ref(),
                )?
                .package();
                if let Some(target) = dependency.library_target() {
                    let inputs = load_target_modules(&dependency, target.inner(), manager.clone())?;
                    metadata.source_modules.push(inputs.root);
                    metadata.source_modules.extend(inputs.support);
                }
            }
            ProjectDependencyNodeProvenance::Source(ProjectSource::Virtual { .. }) => {
                return Err(Report::msg(format!(
                    "cannot analyze virtual source dependency '{name}'"
                )));
            }
            ProjectDependencyNodeProvenance::Preassembled { path, .. } => {
                let bytes = std::fs::read(path).map_err(|error| Report::msg(error.to_string()))?;
                metadata.packages.push(Arc::new(
                    MastPackage::read_from_bytes(&bytes)
                        .map_err(|error| Report::msg(error.to_string()))?,
                ));
            }
            ProjectDependencyNodeProvenance::Registry { selected, .. } => {
                metadata.packages.push(
                    cache
                        .load_package(name, selected)
                        .map_err(|error| Report::msg(error.to_string()))?,
                );
            }
        }
    }
    let sources = load_target_modules(package, target, manager.clone())?;
    let text = overlays
        .get(&normalize_path(document))
        .cloned()
        .or_else(|| std::fs::read_to_string(document).ok())
        .unwrap_or_default();
    for module in std::iter::once(sources.root.as_ref())
        .chain(sources.support.iter().map(AsRef::as_ref))
        .chain(metadata.source_modules.iter().map(AsRef::as_ref))
    {
        let source = manager
            .get(module.span().source_id())
            .map_err(|error| Report::msg(error.to_string()))?;
        let mut parser = ModuleParser::new(Some(module.kind()));
        parser.set_warnings_as_errors(true);
        if let Err(report) = parser.parse(Some(module.path()), source, manager.clone()) {
            let mut warnings = Vec::new();
            diagnostics::append(report.as_ref(), document, &text, "miden-assembler", &mut warnings);
            output.extend(
                warnings
                    .into_iter()
                    .filter(|diagnostic| diagnostic.severity == Some(DiagnosticSeverity::WARNING)),
            );
        }
    }
    let temporary = tempfile::tempdir().map_err(|error| Report::msg(error.to_string()))?;
    let options = Options {
        sysroot: None,
        midenup_home: None,
        toolchain: None,
        search_paths: Vec::new(),
        target_dir: temporary.path().to_path_buf(),
        output_dir: Some(temporary.path().to_path_buf()),
        current_dir: manifest.parent().unwrap_or(Path::new(".")).to_path_buf(),
        ..Default::default()
    };
    let mut session = Session::new_project(
        "miden-lsp".into(),
        None,
        Box::new(options),
        Some(Arc::new(NullEmitter::default())),
        manager.clone(),
    );
    session.options.link_libraries.clear();
    let context = Rc::new(Context::new(Rc::new(session)));
    let world = midenc_frontend_masm::disassemble_project_target_input_for_lint(
        ProjectTargetInput::new(sources, metadata),
        &DisassemblerConfig {
            infer_missing_signatures: true,
        },
        context.clone(),
    )?;
    for skipped in &world.skipped_procedures {
        let source = manager.get(skipped.span.source_id()).ok();
        if source
            .as_ref()
            .and_then(|source| source.uri().to_path())
            .is_some_and(|path| normalize_path(&path) == normalize_path(document))
        {
            let mut diagnostic = diagnostics::notice(
                format!("Compiler analysis skipped '{}': {}", skipped.path, skipped.reason),
                DiagnosticSeverity::WARNING,
                "miden-compiler",
            );
            diagnostic.range = crate::document::byte_range_to_lsp_range(
                &text,
                &crate::document::compute_line_offsets(&text),
                skipped.span.into_slice_index(),
            );
            output.push(diagnostic);
        }
    }
    let mut config = DataFlowConfig::new();
    config.set_interprocedural(true).set_max_worklist_iterations(Some(10_000));
    let analysis = AdviceTaintAnalysis::run_with_config_allow_partial(
        world.world.borrow().as_operation(),
        AnalysisManager::new(world.world.as_operation_ref(), None),
        config,
    )?;
    for diagnostic in analysis.analysis.diagnostics(manager.as_ref()) {
        diagnostics::append(&diagnostic, document, &text, "miden-compiler", output);
    }
    append_analysis_status(
        analysis.analysis.range_analysis_status(),
        analysis.incomplete_reason.as_deref(),
        output,
    );
    Ok(())
}

fn append_analysis_status(
    ranges: &RangeAnalysisStatus,
    incomplete_reason: Option<&str>,
    output: &mut Vec<Diagnostic>,
) {
    if let RangeAnalysisStatus::BudgetExhausted { reason } = ranges {
        output.push(diagnostics::notice(
            format!(
                "Integer range inference is incomplete; using conservative operand constraints: \
                 {reason}"
            ),
            DiagnosticSeverity::WARNING,
            "miden-compiler",
        ));
    }
    if let Some(reason) = incomplete_reason {
        output.push(diagnostics::notice(
            format!("Compiler dataflow analysis is incomplete: {reason}"),
            DiagnosticSeverity::WARNING,
            "miden-compiler",
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reduced_range_precision_and_partial_taint_coverage_are_both_reported() {
        let mut output = Vec::new();
        append_analysis_status(&RangeAnalysisStatus::Complete, None, &mut output);
        assert!(output.is_empty());
        append_analysis_status(
            &RangeAnalysisStatus::BudgetExhausted {
                reason: "range budget".into(),
            },
            Some("taint budget"),
            &mut output,
        );
        assert_eq!(output.len(), 2);
        assert!(
            output
                .iter()
                .all(|diagnostic| diagnostic.severity == Some(DiagnosticSeverity::WARNING))
        );
        assert!(output[0].message.contains("conservative operand constraints"));
        assert!(output[0].message.contains("range budget"));
        assert!(output[1].message.contains("taint budget"));
    }
}
