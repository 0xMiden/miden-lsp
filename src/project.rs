use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
};

use miden_assembly::{Assembler, ProjectTargetSelector};
use miden_assembly_syntax::{
    Path as MasmPath,
    ast::ModuleKind,
    debuginfo::{
        DefaultSourceManager, FileLineCol, Location, Selection, SourceContent, SourceFile,
        SourceId, SourceLanguage, SourceManager, SourceManagerError, SourceSpan, Uri,
    },
    diagnostics::Diagnostic as MidenDiagnostic,
};
use miden_assembly_syntax_cst::ast::Item;
use miden_package_registry::InMemoryPackageRegistry;
use miden_project::Project;
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range, Url};

use crate::{
    analysis::{find_manifest_path, normalize_path},
    document::{byte_range_to_lsp_range, compute_line_offsets},
};

pub(crate) fn source_files(
    root: &Path,
    namespace: &MasmPath,
    kind: ModuleKind,
    overlays: &BTreeMap<PathBuf, String>,
) -> Result<Vec<(PathBuf, String)>, String> {
    if root.extension().is_none_or(|extension| extension != "masm") {
        return Ok(Vec::new());
    }
    let manager = Arc::new(OverlaySourceManager {
        inner: DefaultSourceManager::default(),
        overlays: overlays.clone(),
    });
    if let Ok((module, support)) = miden_assembly_syntax::parser::read_modules_from_root(
        root,
        Some(namespace.into()),
        Some(kind),
        manager.clone(),
        false,
    ) {
        use miden_assembly_syntax::debuginfo::Spanned;
        return std::iter::once(module)
            .chain(support)
            .map(|module| {
                let source =
                    manager.get(module.span().source_id()).map_err(|error| error.to_string())?;
                let path = source
                    .uri()
                    .to_path()
                    .ok_or_else(|| "module source is not a file".to_string())?;
                Ok((normalize_path(&path), module.path().as_str().to_string()))
            })
            .collect();
    }
    let mut modules = Vec::new();
    let mut pending = vec![(
        normalize_path(root),
        namespace.as_str().to_string(),
        root.parent().unwrap_or(Path::new(".")).to_path_buf(),
    )];
    let mut visited = BTreeSet::new();
    while let Some((path, namespace, directory)) = pending.pop() {
        if !visited.insert(path.clone()) {
            continue;
        }
        let text = overlays
            .get(&path)
            .cloned()
            .map(Ok)
            .unwrap_or_else(|| std::fs::read_to_string(&path))
            .map_err(|error| format!("cannot read '{}': {error}", path.display()))?;
        let uri =
            Url::from_file_path(&path).map_err(|_| "invalid module source path".to_string())?;
        let (parse, _) = crate::document::parse_document(&uri, &text)?;
        modules.push((path.clone(), namespace.clone()));
        for item in parse.root().items() {
            let Item::Submodule(module) = item else {
                continue;
            };
            let Some(name) = module.name_token() else {
                continue;
            };
            let basename = name.text().trim_matches('"').replace('-', "_");
            if basename.is_empty()
                || basename.contains(['/', '\\'])
                || !matches!(
                    Path::new(&basename).components().next(),
                    Some(std::path::Component::Normal(_))
                )
            {
                continue;
            }
            let child_dir = directory.join(&basename);
            let child_file = child_dir.with_extension("masm");
            let child_root = child_dir.join("mod.masm");
            let candidates = [child_file, child_root]
                .into_iter()
                .filter(|candidate| {
                    candidate.is_file() || overlays.contains_key(&normalize_path(candidate))
                })
                .collect::<Vec<_>>();
            if candidates.len() != 1 {
                continue;
            }
            let child = normalize_path(&candidates[0]);
            if child == path {
                continue;
            }
            let child_namespace = format!("{namespace}::{}", name.text());
            pending.push((child, child_namespace, child_dir));
        }
    }
    modules.sort();
    Ok(modules)
}

#[derive(Debug)]
struct OverlaySourceManager {
    inner: DefaultSourceManager,
    overlays: BTreeMap<PathBuf, String>,
}

impl SourceManager for OverlaySourceManager {
    fn load_from_raw_parts(&self, uri: Uri, content: SourceContent) -> Arc<SourceFile> {
        let content = uri
            .to_path()
            .and_then(|path| self.overlays.get(&normalize_path(&path)))
            .map(|text| SourceContent::new(SourceLanguage::Masm, uri.clone(), text.clone()))
            .unwrap_or(content);
        self.inner.load_from_raw_parts(uri, content)
    }

    fn update(
        &self,
        id: SourceId,
        text: String,
        range: Option<Selection>,
        version: i32,
    ) -> Result<(), SourceManagerError> {
        self.inner.update(id, text, range, version)
    }

    fn get(&self, id: SourceId) -> Result<Arc<SourceFile>, SourceManagerError> {
        self.inner.get(id)
    }
    fn find(&self, uri: &Uri) -> Option<SourceId> {
        self.inner.find(uri)
    }
    fn file_line_col_to_span(&self, location: FileLineCol) -> Option<SourceSpan> {
        self.inner.file_line_col_to_span(location)
    }
    fn file_line_col(&self, span: SourceSpan) -> Result<FileLineCol, SourceManagerError> {
        self.inner.file_line_col(span)
    }
    fn location_to_span(&self, location: Location) -> Option<SourceSpan> {
        self.inner.location_to_span(location)
    }
    fn location(&self, span: SourceSpan) -> Result<Location, SourceManagerError> {
        self.inner.location(span)
    }
    fn source(&self, id: SourceId) -> Result<&str, SourceManagerError> {
        self.inner.source(id)
    }
    fn source_slice(&self, span: SourceSpan) -> Result<&str, SourceManagerError> {
        self.inner.source_slice(span)
    }
}

pub(crate) fn diagnostics(
    document: &Path,
    overlays: &BTreeMap<PathBuf, String>,
    mut cache: InMemoryPackageRegistry,
) -> Vec<Diagnostic> {
    let Ok(manifest) = find_manifest_path(document) else {
        return Vec::new();
    };
    let manager = Arc::new(OverlaySourceManager {
        inner: DefaultSourceManager::default(),
        overlays: overlays.clone(),
    });
    let project = match Project::load(&manifest, manager.as_ref()) {
        Ok(project) => project.package(),
        Err(_) => return Vec::new(),
    };
    let mut targets = Vec::new();
    if project.library_target().is_some_and(|target| {
        target
            .path
            .to_path()
            .is_some_and(|path| path.extension().is_some_and(|extension| extension == "masm"))
    }) {
        targets.push(ProjectTargetSelector::Library);
    }
    for target in project.executable_targets() {
        if target
            .path
            .to_path()
            .is_some_and(|path| path.extension().is_some_and(|extension| extension == "masm"))
        {
            targets.push(ProjectTargetSelector::Executable(target.name.inner()));
        }
    }
    let text = overlays
        .get(&normalize_path(document))
        .cloned()
        .or_else(|| std::fs::read_to_string(document).ok())
        .unwrap_or_default();
    let mut diagnostics = Vec::new();
    for target in targets {
        let result = Assembler::new(manager.clone())
            .for_project(project.clone(), &mut cache)
            .and_then(|mut assembler| assembler.assemble(target, "dev"));
        if let Err(error) = result {
            let previous = diagnostics.len();
            collect_diagnostics(error.as_ref(), document, &text, &mut diagnostics);
            if diagnostics.len() == previous {
                diagnostics.push(Diagnostic {
                    range: Range::new(Position::new(0, 0), Position::new(0, 0)),
                    severity: Some(DiagnosticSeverity::ERROR),
                    source: Some("miden-assembler".into()),
                    message: error.to_string(),
                    ..Diagnostic::default()
                });
            }
        }
    }
    diagnostics.sort_by_key(|diagnostic| {
        (
            diagnostic.range.start.line,
            diagnostic.range.start.character,
            diagnostic.message.clone(),
        )
    });
    diagnostics.dedup_by(|left, right| left.range == right.range && left.message == right.message);
    diagnostics
}

fn collect_diagnostics(
    error: &dyn MidenDiagnostic,
    document: &Path,
    text: &str,
    output: &mut Vec<Diagnostic>,
) {
    if let (Some(code), Some(labels)) = (error.source_code(), error.labels()) {
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
            let bounds = label.offset()..label.offset().saturating_add(label.len()).min(text.len());
            output.push(Diagnostic {
                range: byte_range_to_lsp_range(text, &compute_line_offsets(text), bounds),
                severity: Some(DiagnosticSeverity::ERROR),
                source: Some("miden-assembler".into()),
                message: match label.label() {
                    Some(label) => format!("{error}: {label}"),
                    None => error.to_string(),
                },
                ..Diagnostic::default()
            });
        }
    }
    if let Some(related) = error.related() {
        for error in related {
            collect_diagnostics(error, document, text, output);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn project(text: &str) -> PathBuf {
        let unique = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("miden-lsp-project-validation-{unique}"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("miden-project.toml"), "[package]\nname = \"app\"\nversion = \"1.0.0\"\n[lib]\npath = \"mod.masm\"\nnamespace = \"app\"\n").unwrap();
        let source = root.join("mod.masm");
        std::fs::write(&source, text).unwrap();
        normalize_path(&source)
    }

    #[test]
    fn project_assembler_reports_unresolved_calls_at_the_source_span() {
        let source = project("pub proc helper\n    exec.missing\nend\n");
        let errors = diagnostics(&source, &BTreeMap::new(), InMemoryPackageRegistry::default());
        assert!(!errors.is_empty());
        assert!(
            errors
                .iter()
                .any(|error| error.range == Range::new(Position::new(1, 9), Position::new(1, 16))
                    && error.message.contains("undefined symbol")),
            "{errors:?}"
        );
        assert!(errors.iter().all(|error| error.source.as_deref() == Some("miden-assembler")));
        assert!(!source.parent().unwrap().join("target").exists());
    }

    #[test]
    fn unsaved_overlays_replace_disk_contents_for_project_assembly() {
        let source = project("pub proc helper\n    exec.missing\nend\n");
        let valid = "pub proc helper\n    push.1 drop\nend\n";
        let overlays = BTreeMap::from([(source.clone(), valid.into())]);
        let errors = diagnostics(&source, &overlays, InMemoryPackageRegistry::default());
        assert!(errors.is_empty(), "{errors:?}");
        std::fs::write(&source, valid).unwrap();
        let overlays =
            BTreeMap::from([(source.clone(), "pub proc helper\nexec.unknown\nend\n".into())]);
        let errors = diagnostics(&source, &overlays, InMemoryPackageRegistry::default());
        assert!(!errors.is_empty());
        assert!(std::fs::read_to_string(source).unwrap().contains("push.1"));
    }

    #[test]
    fn submodules_are_checked_with_the_same_overlay_source_manager() {
        let source = project("pub mod child\npub proc helper\nexec.child::target\nend\n");
        let child = source.parent().unwrap().join("child.masm");
        std::fs::write(&child, "pub proc target\nexec.missing\nend\n").unwrap();
        let errors = diagnostics(&child, &BTreeMap::new(), InMemoryPackageRegistry::default());
        assert!(!errors.is_empty());
        let overlays =
            BTreeMap::from([(child.clone(), "pub proc target\npush.1 drop\nend\n".into())]);
        let errors = diagnostics(&child, &overlays, InMemoryPackageRegistry::default());
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn assembler_module_discovery_excludes_undeclared_files_and_preserves_names() {
        let root = project("pub mod \"math-tools\"\npub proc main\npush.1 drop\nend\n");
        let directory = root.parent().unwrap();
        let child = directory.join("math_tools.masm");
        std::fs::write(&child, "pub proc helper\npush.1\nend\n").unwrap();
        std::fs::write(directory.join("orphan.masm"), "pub proc hidden\nnop\nend\n").unwrap();
        let modules =
            source_files(&root, MasmPath::new("::app"), ModuleKind::Library, &BTreeMap::new())
                .unwrap();
        assert_eq!(modules.len(), 2);
        assert!(
            modules.contains(&(normalize_path(&child), "::app::\"math-tools\"".into())),
            "{modules:?}"
        );
        assert!(!modules.iter().any(|(path, _)| path.ends_with("orphan.masm")));
    }

    #[test]
    fn cst_recovery_keeps_declared_modules_while_an_open_buffer_is_incomplete() {
        let root = project("pub mod child\npub proc main\npush.1 drop\nend\n");
        let child = root.parent().unwrap().join("child.masm");
        std::fs::write(&child, "pub proc helper\nnop\nend\n").unwrap();
        let overlays =
            BTreeMap::from([(root.clone(), "pub mod child\npub proc main\npush.1\n".into())]);
        let modules =
            source_files(&root, MasmPath::new("::app"), ModuleKind::Library, &overlays).unwrap();
        assert_eq!(modules.len(), 2);
        assert!(modules.contains(&(normalize_path(&child), "::app::child".into())));
    }
}
