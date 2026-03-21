use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use tokio::sync::RwLock;
use tower_lsp::{
    Client, LanguageServer,
    jsonrpc::Result,
    lsp_types::{
        DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
        DidSaveTextDocumentParams, DocumentSymbolParams, GotoDefinitionParams,
        GotoDefinitionResponse, Hover, HoverParams, InitializeParams, InitializeResult,
        InitializedParams, MessageType, OneOf, ServerCapabilities, TextDocumentSyncCapability,
        TextDocumentSyncKind, Url, WorkspaceFolder, WorkspaceSymbolParams,
    },
};

use crate::{
    analysis::{InitializationOptions, ProjectSnapshot, RegistryState, normalize_path, project_diagnostic},
    document::TextDocument,
};

#[derive(Default)]
struct ServerState {
    documents: BTreeMap<Url, TextDocument>,
    git_cache_root: Option<PathBuf>,
    registry: RegistryState,
    workspace_folders: Vec<PathBuf>,
}

impl ServerState {
    fn overlays(&self) -> BTreeMap<PathBuf, String> {
        self.documents
            .values()
            .filter_map(|document| {
                document
                    .uri()
                    .to_file_path()
                    .ok()
                    .map(|path| (normalize_path(&path), document.text().to_string()))
            })
            .collect()
    }
}

pub struct Backend {
    client: Client,
    state: Arc<RwLock<ServerState>>,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Self { client, state: Arc::new(RwLock::new(ServerState::default())) }
    }

    async fn publish_diagnostics(&self, uri: Url, include_project: bool) {
        let diagnostics = {
            let state = self.state.read().await;
            let Some(document) = state.documents.get(&uri) else {
                return;
            };

            let mut diagnostics = document.syntax_diagnostics().to_vec();
            if include_project
                && let Ok(path) = uri.to_file_path()
                && let Some(diagnostic) =
                    project_diagnostic(&path, &state.registry, state.git_cache_root.as_deref())
            {
                diagnostics.push(diagnostic);
            }

            (document.version(), diagnostics)
        };

        self.client
            .publish_diagnostics(uri, diagnostics.1, Some(diagnostics.0))
            .await;
    }

    async fn snapshot_for_path(&self, path: &Path) -> Option<ProjectSnapshot> {
        let path = normalize_path(path);
        let state = self.state.read().await;
        ProjectSnapshot::load_for_document(
            &path,
            &state.overlays(),
            &state.registry,
            state.git_cache_root.as_deref(),
        )
        .ok()
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        let options = params
            .initialization_options
            .and_then(|value| serde_json::from_value::<InitializationOptions>(value).ok())
            .unwrap_or_default();

        let workspace_folders = params
            .workspace_folders
            .unwrap_or_else(|| root_workspace_folder(params.root_uri))
            .into_iter()
            .filter_map(|folder| folder.uri.to_file_path().ok())
            .map(|path| normalize_path(&path))
            .collect::<Vec<_>>();

        let registry = RegistryState::from_options(&options)
            .map_err(tower_lsp::jsonrpc::Error::invalid_params)?;

        let mut state = self.state.write().await;
        state.git_cache_root = options.git_cache_root;
        state.registry = registry;
        state.workspace_folders = workspace_folders;

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                definition_provider: Some(OneOf::Left(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                hover_provider: Some(tower_lsp::lsp_types::HoverProviderCapability::Simple(true)),
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::INCREMENTAL,
                )),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                ..ServerCapabilities::default()
            },
            ..InitializeResult::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "miden-lsp initialized")
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let document = params.text_document;
        match TextDocument::new(document.uri.clone(), document.version, document.text) {
            Ok(text_document) => {
                self.state
                    .write()
                    .await
                    .documents
                    .insert(document.uri.clone(), text_document);
                self.publish_diagnostics(document.uri, true).await;
            },
            Err(error) => {
                self.client.log_message(MessageType::ERROR, error).await;
            },
        }
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let mut state = self.state.write().await;
        let Some(document) = state.documents.get_mut(&params.text_document.uri) else {
            return;
        };

        match document.apply_changes(params.text_document.version, params.content_changes) {
            Ok(()) => drop(state),
            Err(error) => {
                drop(state);
                self.client.log_message(MessageType::ERROR, error).await;
                return;
            },
        }

        self.publish_diagnostics(params.text_document.uri, false).await;
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        self.publish_diagnostics(params.text_document.uri, true).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.state.write().await.documents.remove(&params.text_document.uri);
        self.client
            .publish_diagnostics(params.text_document.uri, Vec::new(), None)
            .await;
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<tower_lsp::lsp_types::DocumentSymbolResponse>> {
        let path = match params.text_document.uri.to_file_path() {
            Ok(path) => path,
            Err(_) => return Ok(None),
        };

        Ok(self.snapshot_for_path(&path).await.and_then(|snapshot| snapshot.document_symbols(&path)))
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let position = params.text_document_position_params.position;
        let uri = params.text_document_position_params.text_document.uri;
        let path = match uri.to_file_path() {
            Ok(path) => path,
            Err(_) => return Ok(None),
        };

        Ok(self
            .snapshot_for_path(&path)
            .await
            .and_then(|snapshot| snapshot.definition_at(&path, position))
            .map(GotoDefinitionResponse::Scalar))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let position = params.text_document_position_params.position;
        let uri = params.text_document_position_params.text_document.uri;
        let path = match uri.to_file_path() {
            Ok(path) => path,
            Err(_) => return Ok(None),
        };

        Ok(self
            .snapshot_for_path(&path)
            .await
            .and_then(|snapshot| snapshot.hover_at(&path, position)))
    }

    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<Vec<tower_lsp::lsp_types::SymbolInformation>>> {
        let state = self.state.read().await;
        let symbols = ProjectSnapshot::workspace_symbols(
            &state.workspace_folders,
            &state.overlays(),
            &state.registry,
            state.git_cache_root.as_deref(),
            &params.query,
        );
        Ok(Some(symbols))
    }
}

fn root_workspace_folder(root_uri: Option<Url>) -> Vec<WorkspaceFolder> {
    root_uri
        .map(|uri| {
            vec![WorkspaceFolder {
                name: uri
                    .to_file_path()
                    .ok()
                    .and_then(|path| path.file_name().map(|name| name.to_string_lossy().into_owned()))
                    .unwrap_or_else(|| "workspace".to_string()),
                uri,
            }]
        })
        .unwrap_or_default()
}
