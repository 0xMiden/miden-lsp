use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path as FsPath, PathBuf},
    sync::Arc,
};

use miden_assembly_syntax::{
    Path as MasmPath,
    ast::ProcedureName,
    debuginfo::{DefaultSourceManager, SourceManagerExt},
};
use miden_core::serde::Deserializable;
use miden_mast_package::{
    Package as MastPackage, PackageExport, ProcedureExport, SectionId,
    debug_info::{DebugFunctionsSection, DebugSourcesSection},
};
use miden_package_registry::{
    InMemoryPackageRegistry, PackageId, PackageProvider, PackageStore, Version,
};
use miden_project::{
    Package, Project, ProjectDependencyGraphBuilder, ProjectDependencyNodeProvenance, Target,
};
use serde::Deserialize;
use tower_lsp::lsp_types::{
    Diagnostic, DiagnosticSeverity, DocumentSymbol, DocumentSymbolResponse, Hover, HoverContents,
    Location, MarkupContent, MarkupKind, Position, Range, SymbolInformation, SymbolKind, Url,
};
use tree_sitter::{Node, Tree};

use crate::document::{byte_range_to_lsp_range, compute_line_offsets, parse_text};

type OverlayMap = BTreeMap<PathBuf, String>;

pub fn normalize_path(path: &FsPath) -> PathBuf {
    fs::canonicalize(path)
        .or_else(|_| {
            let parent = path
                .parent()
                .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))?;
            let file_name = path
                .file_name()
                .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))?;
            fs::canonicalize(parent).map(|parent| parent.join(file_name))
        })
        .unwrap_or_else(|_| path.to_path_buf())
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct InitializationOptions {
    pub registry_artifacts: Vec<PathBuf>,
    pub git_cache_root: Option<PathBuf>,
}

#[derive(Default)]
pub struct RegistryState {
    registry: InMemoryPackageRegistry,
    artifacts: BTreeMap<(PackageId, Version), PathBuf>,
}

impl RegistryState {
    pub fn from_options(options: &InitializationOptions) -> Result<Self, String> {
        let mut state = Self::default();
        if options.registry_artifacts.is_empty() {
            return Ok(state);
        }

        let mut pending = Vec::new();
        for artifact_path in &options.registry_artifacts {
            let bytes = fs::read(artifact_path)
                .map_err(|error| format!("failed to read '{}': {error}", artifact_path.display()))?;
            let package = Arc::new(
                MastPackage::read_from_bytes(&bytes)
                    .map_err(|error| format!("failed to decode '{}': {error}", artifact_path.display()))?,
            );
            pending.push((artifact_path.clone(), package));
        }

        while !pending.is_empty() {
            let mut remaining = Vec::new();
            let mut published = 0usize;

            for (artifact_path, package) in pending {
                match state.registry.publish_package(package.clone()) {
                    Ok(version) => {
                        state
                            .artifacts
                            .insert((package.name.clone(), version), artifact_path);
                        published += 1;
                    },
                    Err(_) => remaining.push((artifact_path, package)),
                }
            }

            if published == 0 {
                let stuck = remaining
                    .into_iter()
                    .map(|(path, package)| format!("{} ({})", path.display(), package.name))
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(format!(
                    "failed to publish registry artifacts due to unresolved dependencies: {stuck}"
                ));
            }

            pending = remaining;
        }

        Ok(state)
    }

    pub fn registry(&self) -> &InMemoryPackageRegistry {
        &self.registry
    }

    pub fn artifact_path(&self, package: &PackageId, version: &Version) -> Option<&PathBuf> {
        self.artifacts.get(&(package.clone(), version.clone()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ContextKey {
    package: String,
    target: String,
    executable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ItemKind {
    Module,
    Procedure,
    Constant,
    Type,
}

#[derive(Clone, Debug)]
enum DefinitionLocation {
    Source { path: PathBuf, selection_range: Range },
    Artifact { path: PathBuf },
}

#[derive(Clone, Debug)]
struct Definition {
    context: ContextKey,
    path: String,
    module_path: String,
    name: String,
    kind: ItemKind,
    symbol_kind: SymbolKind,
    hover: String,
    location: Option<DefinitionLocation>,
    visible_outside_context: bool,
    selection_range: Range,
}

impl Definition {
    fn location(&self) -> Option<Location> {
        match self.location.as_ref()? {
            DefinitionLocation::Source { path, selection_range } => Some(Location {
                uri: Url::from_file_path(path).ok()?,
                range: *selection_range,
            }),
            DefinitionLocation::Artifact { path } => Some(Location {
                uri: Url::from_file_path(path).ok()?,
                range: Range::new(Position::new(0, 0), Position::new(0, 0)),
            }),
        }
    }

    fn workspace_symbol(&self) -> Option<SymbolInformation> {
        #[allow(deprecated)]
        Some(SymbolInformation {
            name: self.name.clone(),
            kind: self.symbol_kind,
            tags: None,
            deprecated: None,
            location: self.location()?,
            container_name: Some(self.module_path.clone()),
        })
    }
}

#[derive(Clone, Debug)]
enum AliasTarget {
    Path(String),
    MastRoot,
}

#[derive(Clone, Debug)]
struct ImportAlias {
    target: AliasTarget,
}

#[derive(Clone, Debug)]
enum ReferenceKind {
    Invoke,
    ImportAlias,
    ImportTarget,
    Constant,
    Type,
}

#[derive(Clone, Debug)]
struct RawOccurrence {
    range: Range,
    raw_path: String,
    kind: ReferenceKind,
}

#[derive(Clone, Debug)]
struct ResolvedOccurrence {
    range: Range,
    definitions: Vec<usize>,
}

#[derive(Clone, Debug)]
struct ParsedFile {
    text: String,
    line_offsets: Vec<usize>,
    tree: Tree,
}

#[derive(Clone, Debug)]
struct ModuleAnalysis {
    context: ContextKey,
    file_path: PathBuf,
    module_path: String,
    priority: usize,
    local_names: BTreeMap<String, String>,
    imports: BTreeMap<String, ImportAlias>,
    definitions: Vec<Definition>,
    raw_occurrences: Vec<RawOccurrence>,
    resolved_occurrences: Vec<ResolvedOccurrence>,
    document_symbols: Vec<DocumentSymbol>,
}

impl ModuleAnalysis {
    fn resolve_occurrences(&mut self, index: &ResolutionIndex) {
        self.resolved_occurrences = self
            .raw_occurrences
            .iter()
            .filter_map(|occurrence| {
                let resolved = resolve_reference(
                    &self.context,
                    &self.module_path,
                    &self.local_names,
                    &self.imports,
                    occurrence,
                    index,
                );
                if resolved.is_empty() {
                    None
                } else {
                    Some(ResolvedOccurrence { range: occurrence.range, definitions: resolved })
                }
            })
            .collect();
    }
}

#[derive(Debug, Default)]
pub struct ProjectSnapshot {
    modules_by_file: BTreeMap<PathBuf, Vec<ModuleAnalysis>>,
    definitions: Vec<Definition>,
    definitions_by_context: BTreeMap<ContextKey, BTreeMap<String, Vec<usize>>>,
    public_definitions: BTreeMap<String, Vec<usize>>,
}

impl ProjectSnapshot {
    pub fn load_for_document(
        document_path: &FsPath,
        overlays: &OverlayMap,
        registry: &RegistryState,
        git_cache_root: Option<&FsPath>,
    ) -> Result<Self, String> {
        let document_path = normalize_path(document_path);
        let root = if document_path.file_name().is_some_and(|name| name == "miden-project.toml") {
            RootAnalysis::for_manifest(&document_path, registry, git_cache_root)?
        } else {
            RootAnalysis::for_document(&document_path, registry, git_cache_root)?
        };
        build_snapshot(root, overlays, registry)
    }

    pub fn workspace_symbols(
        workspace_folders: &[PathBuf],
        overlays: &OverlayMap,
        registry: &RegistryState,
        git_cache_root: Option<&FsPath>,
        query: &str,
    ) -> Vec<SymbolInformation> {
        let needle = query.to_lowercase();
        let mut snapshots = Vec::new();
        let mut seen = BTreeSet::new();

        for folder in workspace_folders {
            let manifest_path = normalize_path(&folder.join("miden-project.toml"));
            if !manifest_path.exists() || !seen.insert(manifest_path.clone()) {
                continue;
            }
            if let Ok(root) = RootAnalysis::for_manifest(&manifest_path, registry, git_cache_root)
                && let Ok(snapshot) = build_snapshot(root, overlays, registry)
            {
                snapshots.push(snapshot);
            }
        }

        let mut symbols = Vec::new();
        for snapshot in snapshots {
            for definition in snapshot.definitions {
                if matches!(definition.kind, ItemKind::Module) {
                    continue;
                }
                let haystack = format!("{} {}", definition.name, definition.module_path).to_lowercase();
                if haystack.contains(&needle)
                    && let Some(symbol) = definition.workspace_symbol()
                {
                    symbols.push(symbol);
                }
            }
        }
        symbols
    }

    pub fn document_symbols(
        &self,
        document_path: &FsPath,
    ) -> Option<DocumentSymbolResponse> {
        let document_path = normalize_path(document_path);
        let modules = self.modules_by_file.get(&document_path)?;
        let module = pick_primary_module(modules)?;
        Some(DocumentSymbolResponse::Nested(module.document_symbols.clone()))
    }

    pub fn definition_at(
        &self,
        document_path: &FsPath,
        position: Position,
    ) -> Option<Location> {
        let document_path = normalize_path(document_path);
        let modules = self.modules_by_file.get(&document_path)?;

        let mut ordered = modules.iter().collect::<Vec<_>>();
        ordered.sort_by_key(|module| module.priority);
        for module in ordered {
            for definition in &module.definitions {
                if contains_position(definition.selection_range, position) {
                    if let Some(location) = definition.location() {
                        return Some(location);
                    }
                }
            }

            for occurrence in &module.resolved_occurrences {
                if contains_position(occurrence.range, position) {
                    for index in &occurrence.definitions {
                        if let Some(location) = self.definitions.get(*index).and_then(Definition::location) {
                            return Some(location);
                        }
                    }
                }
            }
        }

        None
    }

    pub fn hover_at(
        &self,
        document_path: &FsPath,
        position: Position,
    ) -> Option<Hover> {
        let document_path = normalize_path(document_path);
        let modules = self.modules_by_file.get(&document_path)?;

        let mut ordered = modules.iter().collect::<Vec<_>>();
        ordered.sort_by_key(|module| module.priority);
        for module in ordered {
            for definition in &module.definitions {
                if contains_position(definition.selection_range, position) {
                    return Some(render_hover(definition));
                }
            }

            for occurrence in &module.resolved_occurrences {
                if contains_position(occurrence.range, position)
                    && let Some(definition) =
                        occurrence.definitions.iter().find_map(|index| self.definitions.get(*index))
                {
                    return Some(render_hover(definition));
                }
            }
        }

        None
    }
}

pub fn project_diagnostic(
    document_path: &FsPath,
    registry: &RegistryState,
    git_cache_root: Option<&FsPath>,
) -> Option<Diagnostic> {
    let snapshot = ProjectSnapshot::load_for_document(document_path, &OverlayMap::default(), registry, git_cache_root);
    match snapshot {
        Ok(_) => None,
        Err(message) => Some(Diagnostic {
            range: Range::new(Position::new(0, 0), Position::new(0, 1)),
            severity: Some(DiagnosticSeverity::WARNING),
            message,
            ..Diagnostic::default()
        }),
    }
}

#[derive(Clone, Debug)]
enum SourceMode {
    AllTargets,
    LibraryOnly,
}

#[derive(Clone, Debug)]
struct SourcePackageInput {
    package: Arc<Package>,
    mode: SourceMode,
}

#[derive(Clone, Debug)]
struct MetadataPackageInput {
    package: Arc<MastPackage>,
    artifact_path: Option<PathBuf>,
}

#[derive(Clone, Debug)]
struct RootAnalysis {
    source_packages: Vec<SourcePackageInput>,
    metadata_packages: Vec<MetadataPackageInput>,
}

impl RootAnalysis {
    fn for_document(
        document_path: &FsPath,
        registry: &RegistryState,
        git_cache_root: Option<&FsPath>,
    ) -> Result<Self, String> {
        let manifest_path = find_manifest_path(document_path)?;
        let source_manager = Arc::new(DefaultSourceManager::default());
        let project = Project::load(&manifest_path, source_manager.as_ref())
            .map_err(|error| error.to_string())?;
        let package_manifest_path = project
            .manifest_path()
            .ok_or_else(|| "loaded project is missing a manifest path".to_string())?
            .to_path_buf();

        let mut source_packages = Vec::new();
        let mut seen_manifests = BTreeSet::new();

        match project {
            Project::Package(package) => {
                remember_source_package(
                    &mut source_packages,
                    &mut seen_manifests,
                    package,
                    SourceMode::AllTargets,
                );
            },
            Project::WorkspacePackage { package, workspace } => {
                remember_source_package(
                    &mut source_packages,
                    &mut seen_manifests,
                    package,
                    SourceMode::AllTargets,
                );
                for member in workspace.members() {
                    remember_source_package(
                        &mut source_packages,
                        &mut seen_manifests,
                        member.clone(),
                        SourceMode::AllTargets,
                    );
                }
            },
        }

        let mut metadata_packages = Vec::new();
        let mut seen_metadata = BTreeSet::new();

        let mut builder = ProjectDependencyGraphBuilder::new(registry.registry());
        if let Some(root) = git_cache_root {
            builder = builder.with_git_cache_root(root);
        }

        let graph = builder
            .build_from_path(&package_manifest_path)
            .map_err(|error| error.to_string())?;

        for node in graph.nodes().values() {
            match &node.provenance {
                ProjectDependencyNodeProvenance::Source(source) => {
                    let manifest_path = match source {
                        miden_project::ProjectSource::Real { manifest_path, .. } => manifest_path,
                        miden_project::ProjectSource::Virtual { .. } => continue,
                    };

                    let source_manager = Arc::new(DefaultSourceManager::default());
                    let project = Project::load_project_reference(
                        node.name.as_ref(),
                        manifest_path,
                        source_manager.as_ref(),
                    )
                    .map_err(|error| error.to_string())?;

                    let package = project.package();
                    remember_source_package(
                        &mut source_packages,
                        &mut seen_manifests,
                        package,
                        SourceMode::LibraryOnly,
                    );
                },
                ProjectDependencyNodeProvenance::Preassembled { path, .. } => {
                    let bytes =
                        fs::read(path).map_err(|error| format!("failed to read '{}': {error}", path.display()))?;
                    let package = Arc::new(
                        MastPackage::read_from_bytes(&bytes)
                            .map_err(|error| format!("failed to decode '{}': {error}", path.display()))?,
                    );
                    if seen_metadata.insert(package.name.to_string()) {
                        metadata_packages.push(MetadataPackageInput {
                            package,
                            artifact_path: Some(path.clone()),
                        });
                    }
                },
                ProjectDependencyNodeProvenance::Registry { selected, .. } => {
                    let package = registry
                        .registry()
                        .load_package(&node.name, selected)
                        .map_err(|error| error.to_string())?;
                    if seen_metadata.insert(format!("{}@{}", node.name, selected)) {
                        metadata_packages.push(MetadataPackageInput {
                            artifact_path: registry.artifact_path(&node.name, selected).cloned(),
                            package,
                        });
                    }
                },
            }
        }

        Ok(Self { source_packages, metadata_packages })
    }

    fn for_manifest(
        manifest_path: &FsPath,
        registry: &RegistryState,
        git_cache_root: Option<&FsPath>,
    ) -> Result<Self, String> {
        let manifest_path = normalize_path(manifest_path);
        let source_manager = Arc::new(DefaultSourceManager::default());
        let source_file = source_manager
            .load_file(&manifest_path)
            .map_err(|error| format!("failed to load '{}': {error}", manifest_path.display()))?;

        let manifest_contents = source_file.as_str();
        if manifest_contents.contains("[workspace]") {
            let workspace = miden_project::Workspace::load(source_file, source_manager.as_ref())
                .map_err(|error| error.to_string())?;

            let mut root = Self { source_packages: Vec::new(), metadata_packages: Vec::new() };
            let mut seen_manifests = BTreeSet::new();
            for member in workspace.members() {
                remember_source_package(
                    &mut root.source_packages,
                    &mut seen_manifests,
                    member.clone(),
                    SourceMode::AllTargets,
                );
            }
            Ok(root)
        } else {
            let package = Project::load(&manifest_path, source_manager.as_ref())
                .map_err(|error| error.to_string())?
                .package();
            let mut root = Self::for_document(&manifest_path, registry, git_cache_root)?;
            let mut seen_manifests = BTreeSet::new();
            root.source_packages.clear();
            remember_source_package(
                &mut root.source_packages,
                &mut seen_manifests,
                package,
                SourceMode::AllTargets,
            );
            Ok(root)
        }
    }
}

fn find_manifest_path(path: &FsPath) -> Result<PathBuf, String> {
    let path = normalize_path(path);
    if path.file_name().is_some_and(|name| name == "miden-project.toml") {
        return Ok(path);
    }

    let mut current = if path.is_dir() {
        path.clone()
    } else {
        path.parent()
            .ok_or_else(|| format!("'{}' has no parent directory", path.display()))?
            .to_path_buf()
    };

    loop {
        let candidate = current.join("miden-project.toml");
        if candidate.exists() {
            return Ok(normalize_path(&candidate));
        }
        let Some(parent) = current.parent() else {
            break;
        };
        current = parent.to_path_buf();
    }

    Err(format!(
        "failed to locate miden-project.toml for '{}'",
        path.display()
    ))
}

fn remember_source_package(
    source_packages: &mut Vec<SourcePackageInput>,
    seen_manifests: &mut BTreeSet<PathBuf>,
    package: Arc<Package>,
    mode: SourceMode,
) {
    if let Some(path) = package.manifest_path().map(normalize_path)
        && seen_manifests.insert(path)
    {
        source_packages.push(SourcePackageInput { package, mode });
    }
}

fn build_snapshot(
    root: RootAnalysis,
    overlays: &OverlayMap,
    registry: &RegistryState,
) -> Result<ProjectSnapshot, String> {
    let mut snapshot = ProjectSnapshot::default();

    for package in &root.source_packages {
        let contexts = target_contexts(package)?;
        let file_counts = count_context_files(&contexts)?;

        for context in contexts {
            for file_path in collect_target_files(&context)? {
                let priority = primary_priority(&context.target, &file_path, &file_counts);
                let parsed = parse_file(&file_path, overlays)?;
                let module = analyze_source_module(&context, &file_path, priority, &parsed)?;

                for definition in &module.definitions {
                    let index = snapshot.definitions.len();
                    snapshot
                        .definitions_by_context
                        .entry(definition.context.clone())
                        .or_default()
                        .entry(definition.path.clone())
                        .or_default()
                        .push(index);
                    if definition.visible_outside_context {
                        snapshot
                            .public_definitions
                            .entry(definition.path.clone())
                            .or_default()
                            .push(index);
                    }
                    snapshot.definitions.push(definition.clone());
                }

                snapshot
                    .modules_by_file
                    .entry(file_path.clone())
                    .or_default()
                    .push(module);
            }
        }
    }

    for package in &root.metadata_packages {
        index_metadata_package(&mut snapshot, package, registry)?;
    }

        let resolution_index = ResolutionIndex {
            definitions_by_context: snapshot.definitions_by_context.clone(),
            public_definitions: snapshot.public_definitions.clone(),
        };

        for modules in snapshot.modules_by_file.values_mut() {
            for module in modules.iter_mut() {
                module.resolve_occurrences(&resolution_index);
            }
            modules.sort_by_key(|module| module.priority);
        }

    Ok(snapshot)
}

#[derive(Clone, Debug)]
struct TargetContext {
    package_name: String,
    target: String,
    executable: bool,
    namespace: Arc<MasmPath>,
    root_file: PathBuf,
    root_dir: PathBuf,
    sibling_exec_roots: BTreeSet<PathBuf>,
}

fn target_contexts(package: &SourcePackageInput) -> Result<Vec<TargetContext>, String> {
    let manifest_path = package
        .package
        .manifest_path()
        .ok_or_else(|| format!("package '{}' has no manifest path", package.package.name().inner()))?;
    let manifest_dir = manifest_path
        .parent()
        .ok_or_else(|| format!("manifest '{}' has no parent", manifest_path.display()))?;

    let exec_roots = package
        .package
        .executable_targets()
        .iter()
        .filter_map(|target| target.path.as_ref())
        .map(|path| normalize_path(&manifest_dir.join(path.path())))
        .collect::<BTreeSet<_>>();

    let mut contexts = Vec::new();

    if let Some(target) = package.package.library_target()
        && target.path.is_some()
    {
        contexts.push(build_target_context(
            package.package.name().inner().to_string(),
            target.inner(),
            manifest_dir,
            &exec_roots,
        )?);
    }

    if matches!(package.mode, SourceMode::AllTargets) {
        for target in package.package.executable_targets() {
            if target.path.is_some() {
                contexts.push(build_target_context(
                    package.package.name().inner().to_string(),
                    target.inner(),
                    manifest_dir,
                    &exec_roots,
                )?);
            }
        }
    }

    Ok(contexts)
}

fn build_target_context(
    package_name: String,
    target: &Target,
    manifest_dir: &FsPath,
    exec_roots: &BTreeSet<PathBuf>,
) -> Result<TargetContext, String> {
    let root_path = target
        .path
        .as_ref()
        .ok_or_else(|| format!("target '{}' has no source path", target.name.inner()))?;
    let root_file = normalize_path(&manifest_dir.join(root_path.path()));
    let root_dir = root_file
        .parent()
        .ok_or_else(|| format!("target root '{}' has no parent", root_file.display()))?
        .to_path_buf();

    let sibling_exec_roots = exec_roots
        .iter()
        .filter(|path| *path != &root_file && path.parent() == Some(root_dir.as_path()))
        .cloned()
        .collect();

    Ok(TargetContext {
        executable: target.is_executable(),
        namespace: target.namespace.inner().clone(),
        package_name,
        root_dir,
        root_file,
        sibling_exec_roots,
        target: target.name.inner().to_string(),
    })
}

fn count_context_files(contexts: &[TargetContext]) -> Result<BTreeMap<PathBuf, usize>, String> {
    let mut counts = BTreeMap::new();
    for context in contexts {
        for file_path in collect_target_files(context)? {
            *counts.entry(file_path).or_default() += 1;
        }
    }
    Ok(counts)
}

fn primary_priority(
    context: &str,
    file_path: &FsPath,
    counts: &BTreeMap<PathBuf, usize>,
) -> usize {
    let shared = counts.get(file_path).copied().unwrap_or_default() > 1;
    if !shared {
        0
    } else if context == "lib" {
        0
    } else {
        1
    }
}

fn collect_target_files(context: &TargetContext) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    collect_masm_files(&context.root_dir, &mut files)?;
    files.sort();
    if context.executable {
        files.retain(|path| !context.sibling_exec_roots.contains(path));
    }
    Ok(files)
}

fn collect_masm_files(dir: &FsPath, files: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = fs::read_dir(dir)
        .map_err(|error| format!("failed to read directory '{}': {error}", dir.display()))?;

    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        let path = entry.path();
        if path.is_dir() {
            collect_masm_files(&path, files)?;
        } else if path.extension().is_some_and(|ext| ext == "masm") {
            files.push(normalize_path(&path));
        }
    }

    Ok(())
}

fn parse_file(path: &FsPath, overlays: &OverlayMap) -> Result<ParsedFile, String> {
    let text = match overlays.get(path) {
        Some(text) => text.clone(),
        None => fs::read_to_string(path)
            .map_err(|error| format!("failed to read '{}': {error}", path.display()))?,
    };
    let (tree, _) = parse_text(&text)?;
    let line_offsets = compute_line_offsets(&text);
    Ok(ParsedFile { line_offsets, text, tree })
}

fn analyze_source_module(
    context: &TargetContext,
    file_path: &FsPath,
    priority: usize,
    parsed: &ParsedFile,
) -> Result<ModuleAnalysis, String> {
    let module_path = module_path_for_file(context, file_path)?;
    let mut module = ModuleAnalysis {
        context: ContextKey {
            package: context.package_name.clone(),
            target: context.target.clone(),
            executable: context.executable,
        },
        definitions: Vec::new(),
        document_symbols: Vec::new(),
        file_path: file_path.to_path_buf(),
        imports: BTreeMap::new(),
        local_names: BTreeMap::new(),
        module_path: module_path.clone(),
        priority,
        raw_occurrences: Vec::new(),
        resolved_occurrences: Vec::new(),
    };

    let module_selection = Range::new(Position::new(0, 0), Position::new(0, 0));
    module.definitions.push(Definition {
        context: module.context.clone(),
        hover: format!("```masm\nmodule {module_path}\n```"),
        kind: ItemKind::Module,
        location: Some(DefinitionLocation::Source {
            path: file_path.to_path_buf(),
            selection_range: module_selection,
        }),
        module_path: module_path.clone(),
        name: module_path
            .rsplit("::")
            .next()
            .unwrap_or(module_path.as_str())
            .to_string(),
        path: module_path.clone(),
        selection_range: module_selection,
        symbol_kind: SymbolKind::MODULE,
        visible_outside_context: !context.executable,
    });

    let mut pending_docs = Vec::<String>::new();
    let mut cursor = parsed.tree.root_node().walk();
    for child in parsed.tree.root_node().named_children(&mut cursor) {
        match child.kind() {
            "doc_comment" => pending_docs.push(node_text(child, &parsed.text)?.trim().to_string()),
            "import" => {
                parse_import(&mut module, child, parsed)?;
                pending_docs.clear();
            },
            "constant" => {
                parse_constant(&mut module, child, parsed, take_docs(&mut pending_docs))?;
            },
            "type_alias" | "enum_declaration" => {
                parse_type_definition(
                    &mut module,
                    child,
                    parsed,
                    take_docs(&mut pending_docs),
                    child.kind() == "enum_declaration",
                )?;
            },
            "procedure" => {
                parse_procedure(&mut module, child, parsed, take_docs(&mut pending_docs), false)?;
            },
            "entrypoint" => {
                parse_procedure(&mut module, child, parsed, take_docs(&mut pending_docs), true)?;
            },
            _ => pending_docs.clear(),
        }
    }

    Ok(module)
}

fn take_docs(pending_docs: &mut Vec<String>) -> Option<String> {
    if pending_docs.is_empty() {
        None
    } else {
        Some(std::mem::take(pending_docs).join("\n"))
    }
}

fn parse_import(
    module: &mut ModuleAnalysis,
    node: Node<'_>,
    parsed: &ParsedFile,
) -> Result<(), String> {
    let target = node
        .child_by_field_name("target")
        .ok_or_else(|| "import without a target".to_string())?;
    let target_text = node_text(target, &parsed.text)?.trim().to_string();
    let alias_name = if let Some(alias) = node.child_by_field_name("alias") {
        let name = alias
            .child_by_field_name("name")
            .ok_or_else(|| "import alias without a name".to_string())?;
        node_text(name, &parsed.text)?.to_string()
    } else {
        target_text
            .rsplit("::")
            .next()
            .unwrap_or(target_text.as_str())
            .trim_matches('"')
            .to_string()
    };

    let import_target = match target.kind() {
        "mast_root" => AliasTarget::MastRoot,
        _ => AliasTarget::Path(canonicalize_path_text(&target_text)?),
    };
    module
        .imports
        .insert(alias_name.clone(), ImportAlias { target: import_target });

    module.raw_occurrences.push(RawOccurrence {
        kind: ReferenceKind::ImportTarget,
        range: node_range(target, parsed),
        raw_path: target_text,
    });

    if let Some(alias) = node.child_by_field_name("alias")
        && let Some(name) = alias.child_by_field_name("name")
    {
        module.raw_occurrences.push(RawOccurrence {
            kind: ReferenceKind::ImportAlias,
            range: node_range(name, parsed),
            raw_path: alias_name,
        });
    }

    Ok(())
}

fn parse_constant(
    module: &mut ModuleAnalysis,
    node: Node<'_>,
    parsed: &ParsedFile,
    docs: Option<String>,
) -> Result<(), String> {
    let name = node
        .child_by_field_name("name")
        .ok_or_else(|| "constant without a name".to_string())?;
    let ident = node_text(name, &parsed.text)?.to_string();
    let path = format!("{}::{}", module.module_path, ident);
    module.local_names.insert(ident.clone(), path.clone());

    let selection_range = node_range(name, parsed);
    let full_range = node_range(node, parsed);
    let hover = render_definition_block("const", &path, None, docs.as_deref());

    module.definitions.push(Definition {
        context: module.context.clone(),
        hover,
        kind: ItemKind::Constant,
        location: Some(DefinitionLocation::Source {
            path: module.file_path.clone(),
            selection_range,
        }),
        module_path: module.module_path.clone(),
        name: ident.clone(),
        path: path.clone(),
        selection_range,
        symbol_kind: SymbolKind::CONSTANT,
        visible_outside_context: !module.context.executable,
    });

    #[allow(deprecated)]
    module.document_symbols.push(DocumentSymbol {
        detail: None,
        kind: SymbolKind::CONSTANT,
        name: ident,
        range: full_range,
        selection_range,
        children: None,
        tags: None,
        deprecated: None,
    });

    record_nested_references(module, node, parsed, false, false);
    Ok(())
}

fn parse_type_definition(
    module: &mut ModuleAnalysis,
    node: Node<'_>,
    parsed: &ParsedFile,
    docs: Option<String>,
    is_enum: bool,
) -> Result<(), String> {
    let name = node
        .child_by_field_name("name")
        .ok_or_else(|| "type definition without a name".to_string())?;
    let ident = node_text(name, &parsed.text)?.to_string();
    let path = format!("{}::{}", module.module_path, ident);
    module.local_names.insert(ident.clone(), path.clone());

    let selection_range = node_range(name, parsed);
    let full_range = node_range(node, parsed);
    let keyword = if is_enum { "enum" } else { "type" };
    let hover = render_definition_block(keyword, &path, None, docs.as_deref());

    module.definitions.push(Definition {
        context: module.context.clone(),
        hover,
        kind: ItemKind::Type,
        location: Some(DefinitionLocation::Source {
            path: module.file_path.clone(),
            selection_range,
        }),
        module_path: module.module_path.clone(),
        name: ident.clone(),
        path: path.clone(),
        selection_range,
        symbol_kind: SymbolKind::CLASS,
        visible_outside_context: !module.context.executable,
    });

    #[allow(deprecated)]
    module.document_symbols.push(DocumentSymbol {
        children: None,
        detail: None,
        kind: SymbolKind::CLASS,
        name: ident,
        range: full_range,
        selection_range,
        tags: None,
        deprecated: None,
    });

    record_nested_references(module, node, parsed, true, false);
    Ok(())
}

fn parse_procedure(
    module: &mut ModuleAnalysis,
    node: Node<'_>,
    parsed: &ParsedFile,
    docs: Option<String>,
    entrypoint: bool,
) -> Result<(), String> {
    let (name_node, ident) = if entrypoint {
        (
            None,
            ProcedureName::MAIN_PROC_NAME.to_string(),
        )
    } else {
        let name = node
            .child_by_field_name("name")
            .ok_or_else(|| "procedure without a name".to_string())?;
        let ident = first_named_child(name).unwrap_or(name);
        (Some(ident), node_text(ident, &parsed.text)?.to_string())
    };

    let path = format!("{}::{}", module.module_path, ident);
    module.local_names.insert(ident.clone(), path.clone());

    let signature = node
        .child_by_field_name("signature")
        .map(|signature| node_text(signature, &parsed.text).map(str::to_string))
        .transpose()?;

    let selection_range = match name_node {
        Some(name_node) => node_range(name_node, parsed),
        None => node_range(node, parsed),
    };
    let full_range = node_range(node, parsed);
    let hover = render_definition_block("proc", &path, signature.as_deref(), docs.as_deref());

    module.definitions.push(Definition {
        context: module.context.clone(),
        hover,
        kind: ItemKind::Procedure,
        location: Some(DefinitionLocation::Source {
            path: module.file_path.clone(),
            selection_range,
        }),
        module_path: module.module_path.clone(),
        name: ident.clone(),
        path: path.clone(),
        selection_range,
        symbol_kind: SymbolKind::FUNCTION,
        visible_outside_context: !module.context.executable,
    });

    #[allow(deprecated)]
    module.document_symbols.push(DocumentSymbol {
        children: None,
        detail: signature.clone(),
        kind: SymbolKind::FUNCTION,
        name: ident,
        range: full_range,
        selection_range,
        tags: None,
        deprecated: None,
    });

    if let Some(signature) = node.child_by_field_name("signature") {
        record_nested_references(module, signature, parsed, true, false);
    }
    if let Some(body) = node.child_by_field_name("body") {
        record_body_references(module, body, parsed);
    }
    Ok(())
}

fn record_body_references(module: &mut ModuleAnalysis, node: Node<'_>, parsed: &ParsedFile) {
    let mut stack = vec![node];
    while let Some(node) = stack.pop() {
        if node.kind() == "invoke" {
            if let Some(path) = node.child_by_field_name("path")
                && let Ok(raw_path) = node_text(path, &parsed.text)
            {
                module.raw_occurrences.push(RawOccurrence {
                    kind: ReferenceKind::Invoke,
                    range: node_range(path, parsed),
                    raw_path: raw_path.to_string(),
                });
            }
            continue;
        }

        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }
}

fn record_nested_references(
    module: &mut ModuleAnalysis,
    root: Node<'_>,
    parsed: &ParsedFile,
    include_type_paths: bool,
    include_const_paths: bool,
) {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "path" if include_type_paths => {
                if let Ok(raw_path) = node_text(node, &parsed.text) {
                    module.raw_occurrences.push(RawOccurrence {
                        kind: ReferenceKind::Type,
                        range: node_range(node, parsed),
                        raw_path: raw_path.to_string(),
                    });
                }
            },
            "const_path" if include_const_paths || !include_type_paths => {
                if let Ok(raw_path) = node_text(node, &parsed.text) {
                    module.raw_occurrences.push(RawOccurrence {
                        kind: ReferenceKind::Constant,
                        range: node_range(node, parsed),
                        raw_path: raw_path.to_string(),
                    });
                }
            },
            _ => (),
        }

        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }
}

fn resolve_reference(
    context: &ContextKey,
    module_path: &str,
    local_names: &BTreeMap<String, String>,
    imports: &BTreeMap<String, ImportAlias>,
    occurrence: &RawOccurrence,
    index: &ResolutionIndex,
) -> Vec<usize> {
    let resolved_path = match occurrence.kind {
        ReferenceKind::Invoke => {
            resolve_path_reference(local_names, imports, module_path, &occurrence.raw_path, true)
        },
        ReferenceKind::ImportAlias => imports
            .get(&occurrence.raw_path)
            .and_then(|import| alias_target_to_path(import, imports)),
        ReferenceKind::ImportTarget => {
            resolve_import_target(imports, &occurrence.raw_path)
        },
        ReferenceKind::Constant => {
            resolve_path_reference(local_names, imports, module_path, &occurrence.raw_path, false)
        },
        ReferenceKind::Type => {
            resolve_path_reference(local_names, imports, module_path, &occurrence.raw_path, false)
        },
    };

    let Some(resolved_path) = resolved_path else {
        return Vec::new();
    };

    let mut matches = index
        .definitions_by_context
        .get(context)
        .and_then(|definitions| definitions.get(&resolved_path))
        .cloned()
        .unwrap_or_default();

    if matches.is_empty() {
        matches.extend(index.public_definitions.get(&resolved_path).cloned().unwrap_or_default());
    }

    matches
}

fn resolve_path_reference(
    local_names: &BTreeMap<String, String>,
    imports: &BTreeMap<String, ImportAlias>,
    module_path: &str,
    raw_path: &str,
    invoke: bool,
) -> Option<String> {
    if invoke && raw_path == ProcedureName::MAIN_PROC_NAME {
        return Some(format!("{module_path}::{}", ProcedureName::MAIN_PROC_NAME));
    }

    let path_text = canonicalize_path_text(raw_path).ok()?;
    let path = MasmPath::new(&path_text);
    if path.is_absolute() {
        return Some(path.to_string());
    }

    if let Some(ident) = path.as_ident() {
        let ident = ident.as_str().to_string();
        if let Some(local) = local_names.get(&ident) {
            return Some(local.clone());
        }
        if let Some(import) = imports.get(&ident) {
            return alias_target_to_path(import, imports);
        }
        return if invoke { Some(format!("{module_path}::{ident}")) } else { None };
    }

    let (head, rest) = path.split_first()?;
    if let Some(import) = imports.get(head)
        && let Some(expanded) = alias_target_to_path(import, imports)
    {
        let expanded = MasmPath::new(expanded.as_str());
        return Some(expanded.join(rest).to_string());
    }

    Some(path.to_absolute().to_string())
}

fn resolve_import_target(
    imports: &BTreeMap<String, ImportAlias>,
    raw_path: &str,
) -> Option<String> {
    let path_text = canonicalize_path_text(raw_path).ok()?;
    let path = MasmPath::new(&path_text);
    if path.is_absolute() {
        Some(path.to_string())
    } else if let Some(ident) = path.as_ident() {
        imports.get(ident.as_str()).and_then(|import| alias_target_to_path(import, imports))
    } else {
        Some(path.to_absolute().to_string())
    }
}

fn alias_target_to_path(
    import: &ImportAlias,
    imports: &BTreeMap<String, ImportAlias>,
) -> Option<String> {
    match &import.target {
        AliasTarget::Path(path) => {
            let path_text = canonicalize_path_text(path).ok()?;
            let path = MasmPath::new(&path_text);
            if path.is_absolute() {
                Some(path.to_string())
            } else if let Some(ident) = path.as_ident() {
                imports
                    .get(ident.as_str())
                    .and_then(|next| alias_target_to_path(next, imports))
                    .or_else(|| Some(path.to_absolute().to_string()))
            } else if let Some((head, rest)) = path.split_first() {
                if let Some(next) = imports.get(head)
                    && let Some(expanded) = alias_target_to_path(next, imports)
                {
                    let expanded = MasmPath::new(expanded.as_str());
                    Some(expanded.join(rest).to_string())
                } else {
                    Some(path.to_absolute().to_string())
                }
            } else {
                None
            }
        },
        AliasTarget::MastRoot => None,
    }
}

fn index_metadata_package(
    snapshot: &mut ProjectSnapshot,
    input: &MetadataPackageInput,
    registry: &RegistryState,
) -> Result<(), String> {
    let context = ContextKey {
        package: input.package.name.to_string(),
        target: "metadata".to_string(),
        executable: input.package.is_program(),
    };

    let debug_sources = read_debug_sources(&input.package);
    let debug_functions = read_debug_functions(&input.package);
    let mut module_locations = BTreeMap::<String, DefinitionLocation>::new();

    for export in input.package.manifest.exports() {
        let path = export.path().as_str().to_string();
        let module_path = export.namespace().as_str().to_string();
        let name = export.name().to_string();
        let (kind, symbol_kind, signature) = match export {
            PackageExport::Procedure(ProcedureExport { signature, .. }) => (
                ItemKind::Procedure,
                SymbolKind::FUNCTION,
                signature.as_ref().map(ToString::to_string),
            ),
            PackageExport::Constant(_) => (ItemKind::Constant, SymbolKind::CONSTANT, None),
            PackageExport::Type(_) => (ItemKind::Type, SymbolKind::CLASS, None),
        };

        let location = match export {
            PackageExport::Procedure(procedure) => debug_location_for_procedure(
                procedure,
                &debug_sources,
                &debug_functions,
            )
            .map(DefinitionLocation::from)
            .or_else(|| {
                input
                    .artifact_path
                    .clone()
                    .map(|path| DefinitionLocation::Artifact { path })
            }),
            PackageExport::Constant(_) | PackageExport::Type(_) => input
                .artifact_path
                .clone()
                .map(|path| DefinitionLocation::Artifact { path }),
        };

        let hover = render_definition_block(
            match kind {
                ItemKind::Procedure => "proc",
                ItemKind::Constant => "const",
                ItemKind::Type => "type",
                ItemKind::Module => "module",
            },
            &path,
            signature.as_deref(),
            Some(&format!("package: {}@{}", input.package.name, input.package.version)),
        );

        let definition = Definition {
            context: context.clone(),
            hover,
            kind,
            location: location.clone(),
            module_path: module_path.clone(),
            name: name.clone(),
            path: path.clone(),
            selection_range: Range::new(Position::new(0, 0), Position::new(0, 0)),
            symbol_kind,
            visible_outside_context: true,
        };

    if let Some(location) = location {
        module_locations.entry(module_path.clone()).or_insert(location);
    }

        let index = snapshot.definitions.len();
        snapshot
            .definitions_by_context
            .entry(context.clone())
            .or_default()
            .entry(path.clone())
            .or_default()
            .push(index);
        snapshot.public_definitions.entry(path).or_default().push(index);
        snapshot.definitions.push(definition);
    }

    for (module_path, location) in module_locations {
        let index = snapshot.definitions.len();
        snapshot
            .definitions_by_context
            .entry(context.clone())
            .or_default()
            .entry(module_path.clone())
            .or_default()
            .push(index);
        snapshot
            .public_definitions
            .entry(module_path.clone())
            .or_default()
            .push(index);
        snapshot.definitions.push(Definition {
            context: context.clone(),
            hover: format!("```masm\nmodule {module_path}\n```"),
            kind: ItemKind::Module,
            location: Some(location),
            module_path: module_path.clone(),
            name: module_path
                .rsplit("::")
                .next()
                .unwrap_or(module_path.as_str())
                .to_string(),
            path: module_path,
            selection_range: Range::new(Position::new(0, 0), Position::new(0, 0)),
            symbol_kind: SymbolKind::MODULE,
            visible_outside_context: true,
        });
    }

    let _ = registry;
    Ok(())
}

#[derive(Clone, Debug)]
struct SourceDebugLocation {
    path: PathBuf,
    range: Range,
}

impl From<SourceDebugLocation> for DefinitionLocation {
    fn from(value: SourceDebugLocation) -> Self {
        Self::Source {
            path: value.path,
            selection_range: value.range,
        }
    }
}

fn read_debug_sources(package: &MastPackage) -> Option<DebugSourcesSection> {
    let section = package.sections.iter().find(|section| section.id == SectionId::DEBUG_SOURCES)?;
    DebugSourcesSection::read_from_bytes(section.data.as_ref()).ok()
}

fn read_debug_functions(package: &MastPackage) -> Option<DebugFunctionsSection> {
    let section =
        package.sections.iter().find(|section| section.id == SectionId::DEBUG_FUNCTIONS)?;
    DebugFunctionsSection::read_from_bytes(section.data.as_ref()).ok()
}

fn debug_location_for_procedure(
    export: &ProcedureExport,
    debug_sources: &Option<DebugSourcesSection>,
    debug_functions: &Option<DebugFunctionsSection>,
) -> Option<SourceDebugLocation> {
    let debug_sources = debug_sources.as_ref()?;
    let debug_functions = debug_functions.as_ref()?;

    let function = debug_functions
        .functions
        .iter()
        .find(|function| function.mast_root.is_some_and(|root| root == export.digest))?;
    let file = debug_sources.get_file(function.file_idx)?;
    let file_path = debug_sources.get_string(file.path_idx)?;

    Some(SourceDebugLocation {
        path: PathBuf::from(file_path.as_ref()),
        range: Range::new(
            Position::new(function.line.to_u32().saturating_sub(1), function.column.to_u32().saturating_sub(1)),
            Position::new(function.line.to_u32().saturating_sub(1), function.column.to_u32()),
        ),
    })
}

fn module_path_for_file(context: &TargetContext, file_path: &FsPath) -> Result<String, String> {
    if file_path == context.root_file {
        return Ok(context.namespace.as_str().to_string());
    }

    let relative = file_path
        .strip_prefix(&context.root_dir)
        .map_err(|error| error.to_string())?;
    let mut module_path = context.namespace.to_path_buf();
    let mut components = relative.components().collect::<Vec<_>>();
    if components.is_empty() {
        return Ok(context.namespace.as_str().to_string());
    }

    let file_component = components.pop().unwrap();
    for component in components {
        let segment = component.as_os_str().to_string_lossy();
        module_path = module_path.join(segment.as_ref());
    }

    let file_component = file_component.as_os_str().to_os_string();
    let stem = FsPath::new(&file_component)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| format!("failed to derive module path for '{}'", file_path.display()))?;
    if stem != "mod" {
        module_path = module_path.join(stem);
    }
    Ok(module_path.as_str().to_string())
}

fn canonicalize_path_text(text: &str) -> Result<String, String> {
    MasmPath::validate(text)
        .map_err(|error| format!("invalid MASM path '{text}': {error}"))
        .map(|path| {
            path.canonicalize()
                .expect("validated path should canonicalize")
                .as_path()
                .as_str()
                .to_string()
        })
}

fn node_text<'a>(node: Node<'_>, source: &'a str) -> Result<&'a str, String> {
    node.utf8_text(source.as_bytes())
        .map_err(|error| format!("invalid UTF-8 in syntax tree: {error}"))
}

fn node_range(node: Node<'_>, parsed: &ParsedFile) -> Range {
    byte_range_to_lsp_range(&parsed.text, &parsed.line_offsets, node.start_byte()..node.end_byte())
}

fn first_named_child(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).next()
}

fn contains_position(range: Range, position: Position) -> bool {
    (position.line > range.start.line
        || (position.line == range.start.line && position.character >= range.start.character))
        && (position.line < range.end.line
            || (position.line == range.end.line && position.character <= range.end.character))
}

fn render_definition_block(
    keyword: &str,
    path: &str,
    signature: Option<&str>,
    docs: Option<&str>,
) -> String {
    let mut value = format!("```masm\n{keyword} {path}");
    if let Some(signature) = signature {
        value.push(' ');
        value.push_str(signature);
    }
    value.push_str("\n```");
    if let Some(docs) = docs
        && !docs.is_empty()
    {
        value.push_str("\n\n");
        value.push_str(docs);
    }
    value
}

fn render_hover(definition: &Definition) -> Hover {
    Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: definition.hover.clone(),
        }),
        range: Some(definition.selection_range),
    }
}

fn pick_primary_module(modules: &[ModuleAnalysis]) -> Option<&ModuleAnalysis> {
    modules.iter().min_by_key(|module| module.priority)
}

#[derive(Clone)]
struct ResolutionIndex {
    definitions_by_context: BTreeMap<ContextKey, BTreeMap<String, Vec<usize>>>,
    public_definitions: BTreeMap<String, Vec<usize>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("miden-lsp-{name}-{suffix}"));
        fs::create_dir_all(&dir).unwrap();
        normalize_path(&dir)
    }

    #[test]
    fn resolves_local_definition_and_hover() {
        let root = temp_dir("local-def");
        fs::write(
            root.join("miden-project.toml"),
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\n[lib]\npath = \"mod.masm\"\nnamespace = \"app\"\n",
        )
        .unwrap();
        fs::write(
            root.join("mod.masm"),
            "pub proc foo\n    call.foo\nend\n",
        )
        .unwrap();

        let snapshot = ProjectSnapshot::load_for_document(
            &root.join("mod.masm"),
            &OverlayMap::default(),
            &RegistryState::default(),
            None,
        )
        .unwrap();

        let definition = snapshot
            .definition_at(&root.join("mod.masm"), Position::new(1, 9))
            .unwrap();
        assert_eq!(definition.uri, Url::from_file_path(root.join("mod.masm")).unwrap());

        let hover = snapshot
            .hover_at(&root.join("mod.masm"), Position::new(1, 9))
            .unwrap();
        match hover.contents {
            HoverContents::Markup(content) => assert!(content.value.contains("proc ::app::foo")),
            _ => panic!("expected markdown hover"),
        }
    }

    #[test]
    fn indexes_workspace_members_for_workspace_symbols() {
        let root = temp_dir("workspace");
        let util_dir = root.join("util");
        let app_dir = root.join("app");
        fs::create_dir_all(&util_dir).unwrap();
        fs::create_dir_all(&app_dir).unwrap();

        fs::write(
            root.join("miden-project.toml"),
            "[workspace]\nmembers=[\"util\",\"app\"]\n[workspace.package]\nversion=\"0.1.0\"\n",
        )
        .unwrap();
        fs::write(
            util_dir.join("miden-project.toml"),
            "[package]\nname=\"util\"\nversion.workspace=true\n[lib]\npath=\"mod.masm\"\nnamespace=\"util\"\n",
        )
        .unwrap();
        fs::write(util_dir.join("mod.masm"), "pub proc helper\n    push.1\nend\n").unwrap();
        fs::write(
            app_dir.join("miden-project.toml"),
            "[package]\nname=\"app\"\nversion.workspace=true\n[lib]\npath=\"mod.masm\"\nnamespace=\"app\"\n",
        )
        .unwrap();
        fs::write(app_dir.join("mod.masm"), "pub proc main\n    push.1\nend\n").unwrap();

        let symbols = ProjectSnapshot::workspace_symbols(
            std::slice::from_ref(&root),
            &OverlayMap::default(),
            &RegistryState::default(),
            None,
            "helper",
        );
        assert!(symbols.iter().any(|symbol| symbol.name == "helper"));
    }
}
