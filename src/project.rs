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
        SourceId, SourceManager, SourceManagerError, SourceSpan, Uri,
    },
};
use miden_assembly_syntax_cst::ast::Item;
use miden_package_registry::InMemoryPackageRegistry;
use miden_project::Project;
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range, Url};

use crate::analysis::{find_manifest_path, normalize_path};

pub(crate) fn source_files(
    root: &Path,
    namespace: &MasmPath,
    kind: ModuleKind,
    overlays: &BTreeMap<PathBuf, String>,
) -> Result<Vec<(PathBuf, String)>, String> {
    if root.extension().is_none_or(|extension| extension != "masm") {
        return Ok(Vec::new());
    }
    let manager = Arc::new(OverlaySourceManager::new(overlays.clone()));
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
pub(crate) struct OverlaySourceManager {
    inner: DefaultSourceManager,
    overlays: BTreeMap<PathBuf, String>,
    git_cache_root: PathBuf,
}

impl OverlaySourceManager {
    pub(crate) fn new(overlays: BTreeMap<PathBuf, String>) -> Self {
        Self {
            inner: DefaultSourceManager::default(),
            overlays,
            git_cache_root: crate::dependencies::default_git_cache_root(),
        }
    }

    pub(crate) fn with_git_cache_root(mut self, root: Option<&Path>) -> Self {
        if let Some(root) = root {
            self.git_cache_root = std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf());
        }
        self
    }
}

impl SourceManager for OverlaySourceManager {
    fn load_from_raw_parts(&self, uri: Uri, content: SourceContent) -> Arc<SourceFile> {
        let path = uri.to_path();
        let text = path
            .as_ref()
            .and_then(|path| self.overlays.get(&normalize_path(path)))
            .map(String::as_str)
            .unwrap_or(content.as_str());
        let replacement = if path
            .as_ref()
            .is_some_and(|path| path.file_name().is_some_and(|name| name == "miden-project.toml"))
        {
            crate::dependencies::cached_manifest(text, &self.git_cache_root)
        } else {
            None
        };
        let content = if replacement.is_some() || text != content.as_str() {
            SourceContent::new(
                content.language(),
                uri.clone(),
                replacement.as_deref().unwrap_or(text).to_owned(),
            )
        } else {
            content
        };
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
    git_cache_root: Option<&Path>,
) -> Vec<Diagnostic> {
    let Ok(manifest) = find_manifest_path(document) else {
        return Vec::new();
    };
    let manager =
        Arc::new(OverlaySourceManager::new(overlays.clone()).with_git_cache_root(git_cache_root));
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
    for selector in targets {
        let target = match &selector {
            ProjectTargetSelector::Library => project.library_target(),
            ProjectTargetSelector::Executable(name) => project
                .executable_targets()
                .iter()
                .find(|target| target.name.inner().as_ref() == *name),
        };
        if let Some(target) = target {
            let root = project
                .manifest_path()
                .and_then(|manifest| manifest.parent())
                .zip(target.path.to_path())
                .map(|(directory, path)| directory.join(path));
            let kind = if target.is_executable() {
                ModuleKind::Executable
            } else if target.is_kernel() {
                ModuleKind::Kernel
            } else {
                ModuleKind::Library
            };
            if let Some(root) = root
                && let Ok(files) = source_files(&root, target.namespace.inner(), kind, overlays)
                && !files.iter().any(|(path, _)| *path == normalize_path(document))
            {
                continue;
            }
        }
        let result = Assembler::new(manager.clone())
            .for_project(project.clone(), &mut cache)
            .and_then(|mut assembler| assembler.assemble(selector, "dev"));
        if let Err(error) = result {
            let previous = diagnostics.len();
            crate::diagnostics::append(
                error.as_ref(),
                document,
                &text,
                "miden-assembler",
                &mut diagnostics,
            );
            if diagnostics.len() == previous {
                diagnostics.push(Diagnostic {
                    range: Range::new(Position::new(0, 0), Position::new(0, 0)),
                    severity: Some(DiagnosticSeverity::ERROR),
                    source: Some("miden-assembler".into()),
                    message: error.to_string(),
                    ..Diagnostic::default()
                });
            }
        } else if let Some(target) = target {
            diagnostics.extend(crate::compiler::analyze(
                &project,
                target.inner(),
                document,
                overlays,
                &cache,
                git_cache_root,
            ));
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

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn project(text: &str) -> PathBuf {
        let unique = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("miden-lsp-project-validation-{unique}"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("miden-project.toml"),
            "[package]\nname = \"app\"\nversion = \"1.0.0\"\n[lib]\npath = \
             \"mod.masm\"\nnamespace = \"app\"\n",
        )
        .unwrap();
        let source = root.join("mod.masm");
        std::fs::write(&source, text).unwrap();
        normalize_path(&source)
    }

    #[test]
    fn project_assembler_reports_unresolved_calls_at_the_source_span() {
        let source = project("pub proc helper\n    exec.missing\nend\n");
        let errors =
            diagnostics(&source, &BTreeMap::new(), InMemoryPackageRegistry::default(), None);
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
        let errors = diagnostics(&source, &overlays, InMemoryPackageRegistry::default(), None);
        assert!(errors.is_empty(), "{errors:?}");
        std::fs::write(&source, valid).unwrap();
        let overlays =
            BTreeMap::from([(source.clone(), "pub proc helper\nexec.unknown\nend\n".into())]);
        let errors = diagnostics(&source, &overlays, InMemoryPackageRegistry::default(), None);
        assert!(!errors.is_empty());
        assert!(std::fs::read_to_string(source).unwrap().contains("push.1"));
    }

    #[test]
    fn submodules_are_checked_with_the_same_overlay_source_manager() {
        let source = project("pub mod child\npub proc helper\nexec.child::target\nend\n");
        let child = source.parent().unwrap().join("child.masm");
        std::fs::write(&child, "pub proc target\nexec.missing\nend\n").unwrap();
        let errors =
            diagnostics(&child, &BTreeMap::new(), InMemoryPackageRegistry::default(), None);
        assert!(!errors.is_empty());
        let overlays =
            BTreeMap::from([(child.clone(), "pub proc target\npush.1 drop\nend\n".into())]);
        let manager = Arc::new(OverlaySourceManager::new(overlays.clone()));
        let package =
            Project::load(source.parent().unwrap().join("miden-project.toml"), manager.as_ref())
                .unwrap()
                .package();
        let mut cache = InMemoryPackageRegistry::default();
        let assembled = Assembler::new(manager)
            .for_project(package, &mut cache)
            .and_then(|mut assembler| assembler.assemble(ProjectTargetSelector::Library, "dev"));
        assert!(assembled.is_ok(), "{:?}", assembled.err());
        let errors = diagnostics(&child, &overlays, InMemoryPackageRegistry::default(), None);
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn compiler_dataflow_reports_unconstrained_advice_after_successful_assembly() {
        let source = project("pub proc entry() -> u32\nadv_push\npush.1\nu32wrapping_add\nend\n");
        let findings =
            diagnostics(&source, &BTreeMap::new(), InMemoryPackageRegistry::default(), None);
        assert!(
            findings
                .iter()
                .any(|finding| finding.source.as_deref() == Some("miden-compiler")
                    && finding.severity == Some(DiagnosticSeverity::WARNING)
                    && finding.message.contains("unconstrained")
                    && finding.range.start.line == 3),
            "{findings:#?}"
        );
        assert!(
            !findings
                .iter()
                .any(|finding| finding.severity == Some(DiagnosticSeverity::ERROR)),
            "{findings:#?}"
        );
        assert!(!source.parent().unwrap().join("target").exists());
    }

    #[test]
    fn compiler_dataflow_uses_unsaved_buffers_and_clears_fixed_findings() {
        let unsafe_text = "pub proc entry() -> u32\nadv_push\npush.1\nu32wrapping_add\nend\n";
        let source = project("pub proc entry() -> u32\npush.1\nend\n");
        let overlays = BTreeMap::from([(source.clone(), unsafe_text.into())]);
        let findings = diagnostics(&source, &overlays, InMemoryPackageRegistry::default(), None);
        assert!(
            findings
                .iter()
                .any(|finding| finding.source.as_deref() == Some("miden-compiler")
                    && finding.message.contains("unconstrained")),
            "{findings:#?}"
        );
        let overlays = BTreeMap::from([(
            source.clone(),
            "pub proc entry() -> u32\nadv_push\nu32assert\npush.1\nu32wrapping_add\nend\n".into(),
        )]);
        let fixed = diagnostics(&source, &overlays, InMemoryPackageRegistry::default(), None);
        assert!(fixed.is_empty(), "{fixed:#?}");
        assert!(!std::fs::read_to_string(source).unwrap().contains("adv_push"));
    }

    #[test]
    fn compiler_dataflow_follows_calls_into_source_dependencies() {
        let source =
            project("use dep\npub proc entry() -> u32\nadv_push\nexec.dep::consume\nend\n");
        let root = source.parent().unwrap();
        let manifest = root.join("miden-project.toml");
        let mut text = std::fs::read_to_string(&manifest).unwrap();
        text.push_str("[dependencies]\ndep = { path = \"dep\" }\n");
        std::fs::write(manifest, text).unwrap();
        std::fs::create_dir_all(root.join("dep")).unwrap();
        std::fs::write(
            root.join("dep/miden-project.toml"),
            "[package]\nname = \"dep\"\nversion = \"1.0.0\"\n[lib]\npath = \
             \"mod.masm\"\nnamespace = \"dep\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join("dep/mod.masm"),
            "pub proc consume\npush.1\nu32wrapping_add\nend\n",
        )
        .unwrap();
        let findings =
            diagnostics(&source, &BTreeMap::new(), InMemoryPackageRegistry::default(), None);
        assert!(
            findings
                .iter()
                .any(|finding| finding.source.as_deref() == Some("miden-compiler")
                    && finding.message.contains("unconstrained")),
            "{findings:#?}"
        );
        assert!(
            !findings.iter().any(|finding| finding.message.contains("unavailable")),
            "{findings:#?}"
        );
    }

    #[test]
    fn compiler_dataflow_uses_registry_package_parameter_contracts() {
        use miden_package_registry::PackageCache;
        let source =
            project("use dep\npub proc entry() -> u32\nadv_push\nexec.dep::consume\nend\n");
        let manifest = source.parent().unwrap().join("miden-project.toml");
        let mut text = std::fs::read_to_string(&manifest).unwrap();
        text.push_str("[dependencies]\ndep = \"1.0.0\"\n");
        std::fs::write(manifest, text).unwrap();
        let mut package = Assembler::default()
            .assemble_library(
                "dep",
                "namespace dep\npub proc consume(value: u32) -> \
                 u32\npush.1\nu32wrapping_add\nend\n",
                core::iter::empty::<&str>(),
            )
            .unwrap();
        package.version = "1.0.0".parse().unwrap();
        let mut cache = InMemoryPackageRegistry::default();
        cache.cache_package(Arc::from(package)).unwrap();
        let findings = diagnostics(&source, &BTreeMap::new(), cache, None);
        assert!(
            findings
                .iter()
                .any(|finding| finding.source.as_deref() == Some("miden-compiler")
                    && finding.message.contains("unconstrained")
                    && finding.range.start.line == 3),
            "{findings:#?}"
        );
        assert!(
            !findings.iter().any(|finding| finding.message.contains("unavailable")),
            "{findings:#?}"
        );
    }

    #[test]
    fn successful_projects_preserve_semantic_warnings() {
        let source = project(
            "pub mod child\nuse {helper} from ::app::child\npub proc main\npush.1 drop\nend\n",
        );
        std::fs::write(
            source.parent().unwrap().join("child.masm"),
            "pub proc helper\npush.1 drop\nend\n",
        )
        .unwrap();
        let findings =
            diagnostics(&source, &BTreeMap::new(), InMemoryPackageRegistry::default(), None);
        assert!(
            findings
                .iter()
                .any(|finding| finding.source.as_deref() == Some("miden-assembler")
                    && finding.severity == Some(DiagnosticSeverity::WARNING)
                    && finding.message.contains("unused")),
            "{findings:#?}"
        );
        assert!(
            !findings
                .iter()
                .any(|finding| finding.severity == Some(DiagnosticSeverity::ERROR)),
            "{findings:#?}"
        );
    }

    #[test]
    fn unsupported_lifting_is_reported_instead_of_silently_skipped() {
        let source = project("pub proc entry\ndynexec\nend\n");
        let findings =
            diagnostics(&source, &BTreeMap::new(), InMemoryPackageRegistry::default(), None);
        assert!(
            findings
                .iter()
                .any(|finding| finding.source.as_deref() == Some("miden-compiler")
                    && finding.severity == Some(DiagnosticSeverity::WARNING)
                    && (finding.message.contains("skipped")
                        || finding.message.contains("unavailable"))),
            "{findings:#?}"
        );
    }

    #[test]
    fn diagnostics_do_not_assign_an_unrelated_targets_errors_to_the_open_document() {
        let source = project("pub proc main\npush.1 drop\nend\n");
        let root = source.parent().unwrap();
        let manifest = root.join("miden-project.toml");
        let mut text = std::fs::read_to_string(&manifest).unwrap();
        text.push_str(
            "[[bin]]\nname = \"other\"\npath = \"other.masm\"\nnamespace = \"app::other\"\n",
        );
        std::fs::write(manifest, text).unwrap();
        std::fs::write(root.join("other.masm"), "begin\nexec.missing\nend\n").unwrap();
        let findings =
            diagnostics(&source, &BTreeMap::new(), InMemoryPackageRegistry::default(), None);
        assert!(findings.is_empty(), "{findings:#?}");
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
