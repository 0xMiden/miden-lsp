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
        CodeLens, CodeLensOptions, CodeLensParams, CompletionOptions, CompletionParams,
        CompletionResponse, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
        DidOpenTextDocumentParams, DidSaveTextDocumentParams, DocumentSymbolParams,
        ExecuteCommandOptions, ExecuteCommandParams, GotoDefinitionParams, GotoDefinitionResponse,
        Hover, HoverParams, InitializeParams, InitializeResult, InitializedParams, InlayHint,
        InlayHintOptions, InlayHintParams, InlayHintServerCapabilities, MessageType, OneOf,
        PrepareRenameResponse, ReferenceParams, RenameOptions, RenameParams,
        SemanticTokensFullOptions, SemanticTokensOptions, SemanticTokensParams,
        SemanticTokensResult, ServerCapabilities, TextDocumentPositionParams,
        TextDocumentSyncCapability, TextDocumentSyncKind, Url, WorkspaceEdit, WorkspaceFolder,
        WorkspaceSymbolParams,
    },
};

use crate::{
    analysis::{
        InitializationOptions, ProjectSnapshot, RegistryState, SHOW_SYMBOL_INFO_COMMAND,
        normalize_path, project_diagnostic, semantic_token_legend,
    },
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
        Self {
            client,
            state: Arc::new(RwLock::new(ServerState::default())),
        }
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

        self.client.publish_diagnostics(uri, diagnostics.1, Some(diagnostics.0)).await;
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
                code_lens_provider: Some(CodeLensOptions {
                    resolve_provider: Some(false),
                }),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![".".to_string(), ":".to_string()]),
                    ..CompletionOptions::default()
                }),
                definition_provider: Some(OneOf::Left(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                execute_command_provider: Some(ExecuteCommandOptions {
                    commands: vec![SHOW_SYMBOL_INFO_COMMAND.to_string()],
                    work_done_progress_options: Default::default(),
                }),
                hover_provider: Some(tower_lsp::lsp_types::HoverProviderCapability::Simple(true)),
                inlay_hint_provider: Some(OneOf::Right(InlayHintServerCapabilities::Options(
                    InlayHintOptions {
                        resolve_provider: Some(false),
                        work_done_progress_options: Default::default(),
                    },
                ))),
                references_provider: Some(OneOf::Left(true)),
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: Default::default(),
                })),
                semantic_tokens_provider: Some(
                    SemanticTokensOptions {
                        work_done_progress_options: Default::default(),
                        legend: semantic_token_legend(),
                        range: Some(false),
                        full: Some(SemanticTokensFullOptions::Bool(true)),
                    }
                    .into(),
                ),
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
        self.client.log_message(MessageType::INFO, "miden-lsp initialized").await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let document = params.text_document;
        match TextDocument::new(document.uri.clone(), document.version, document.text) {
            Ok(text_document) => {
                self.state.write().await.documents.insert(document.uri.clone(), text_document);
                self.publish_diagnostics(document.uri, true).await;
            }
            Err(error) => {
                self.client.log_message(MessageType::ERROR, error).await;
            }
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
            }
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

        Ok(self
            .snapshot_for_path(&path)
            .await
            .and_then(|snapshot| snapshot.document_symbols(&path)))
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

    async fn code_lens(&self, params: CodeLensParams) -> Result<Option<Vec<CodeLens>>> {
        let path = match params.text_document.uri.to_file_path() {
            Ok(path) => path,
            Err(_) => return Ok(None),
        };

        Ok(self
            .snapshot_for_path(&path)
            .await
            .and_then(|snapshot| snapshot.code_lenses(&path)))
    }

    async fn references(
        &self,
        params: ReferenceParams,
    ) -> Result<Option<Vec<tower_lsp::lsp_types::Location>>> {
        let position = params.text_document_position.position;
        let uri = params.text_document_position.text_document.uri;
        let path = match uri.to_file_path() {
            Ok(path) => path,
            Err(_) => return Ok(None),
        };

        Ok(self.snapshot_for_path(&path).await.and_then(|snapshot| {
            snapshot.references_at(&path, position, params.context.include_declaration)
        }))
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        let position = params.position;
        let path = match params.text_document.uri.to_file_path() {
            Ok(path) => path,
            Err(_) => return Ok(None),
        };

        Ok(self
            .snapshot_for_path(&path)
            .await
            .and_then(|snapshot| snapshot.prepare_rename_at(&path, position).ok()))
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let position = params.text_document_position.position;
        let path = match params.text_document_position.text_document.uri.to_file_path() {
            Ok(path) => path,
            Err(_) => return Ok(None),
        };

        Ok(self
            .snapshot_for_path(&path)
            .await
            .and_then(|snapshot| snapshot.rename_edits(&path, position, &params.new_name).ok()))
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let path = match params.text_document.uri.to_file_path() {
            Ok(path) => path,
            Err(_) => return Ok(None),
        };

        Ok(self
            .snapshot_for_path(&path)
            .await
            .and_then(|snapshot| snapshot.semantic_tokens(&path)))
    }

    async fn inlay_hint(&self, params: InlayHintParams) -> Result<Option<Vec<InlayHint>>> {
        let path = match params.text_document.uri.to_file_path() {
            Ok(path) => path,
            Err(_) => return Ok(None),
        };

        Ok(self
            .snapshot_for_path(&path)
            .await
            .and_then(|snapshot| snapshot.inlay_hints(&path, params.range)))
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let position = params.text_document_position.position;
        let uri = params.text_document_position.text_document.uri;
        let path = match uri.to_file_path() {
            Ok(path) => path,
            Err(_) => return Ok(None),
        };

        let text = {
            let state = self.state.read().await;
            state.documents.get(&uri).map(|document| document.text().to_string())
        }
        .or_else(|| std::fs::read_to_string(&path).ok());

        let Some(text) = text else {
            return Ok(None);
        };

        Ok(self.snapshot_for_path(&path).await.map(|snapshot| {
            CompletionResponse::Array(snapshot.completion_items(&path, &text, position))
        }))
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

    async fn execute_command(
        &self,
        params: ExecuteCommandParams,
    ) -> Result<Option<serde_json::Value>> {
        match params.command.as_str() {
            SHOW_SYMBOL_INFO_COMMAND => {
                if let Some(message) = params.arguments.first().and_then(|value| value.as_str()) {
                    self.client.show_message(MessageType::INFO, message).await;
                }
                Ok(None)
            }
            _ => Ok(None),
        }
    }
}

fn root_workspace_folder(root_uri: Option<Url>) -> Vec<WorkspaceFolder> {
    root_uri
        .map(|uri| {
            vec![WorkspaceFolder {
                name: uri
                    .to_file_path()
                    .ok()
                    .and_then(|path| {
                        path.file_name().map(|name| name.to_string_lossy().into_owned())
                    })
                    .unwrap_or_else(|| "workspace".to_string()),
                uri,
            }]
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    use futures::StreamExt;
    use serde_json::{Value, json};
    use tokio::sync::mpsc;
    use tower::{Service, ServiceExt};
    use tower_lsp::{
        LspService,
        jsonrpc::Request,
        lsp_types::{Hover, InitializeResult, Location, WorkspaceEdit},
    };

    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let suffix = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("miden-lsp-backend-{name}-{suffix}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    async fn call(
        service: &mut LspService<Backend>,
        method: &'static str,
        params: Value,
        id: i64,
    ) -> Value {
        let response = service
            .ready()
            .await
            .unwrap()
            .call(Request::build(method).params(params).id(id).finish())
            .await
            .unwrap()
            .unwrap();
        assert!(response.is_ok(), "request '{}' failed: {:?}", method, response.error());
        response.result().cloned().unwrap_or(Value::Null)
    }

    async fn notify(service: &mut LspService<Backend>, method: &'static str, params: Value) {
        let response = service
            .ready()
            .await
            .unwrap()
            .call(Request::build(method).params(params).finish())
            .await
            .unwrap();
        assert!(response.is_none(), "notification '{}' returned a response", method);
    }

    async fn next_notification(
        socket: &mut mpsc::UnboundedReceiver<Request>,
        method: &str,
    ) -> Value {
        loop {
            let request = socket.recv().await.expect("expected a server notification");
            if request.method() == method {
                return request.params().cloned().unwrap_or(Value::Null);
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn serves_initialize_diagnostics_hover_definition_and_rename_end_to_end() {
        let root = temp_dir("e2e");
        let manifest_path = root.join("miden-project.toml");
        let source_path = root.join("mod.masm");
        write_file(
            &manifest_path,
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\n[lib]\npath = 1\nnamespace = \
             \"app\"\n",
        );
        let valid_text = "pub proc helper\n    push.1\nend\npub proc main\n    call.helper\nend\n";
        write_file(&source_path, valid_text);

        let source_uri = Url::from_file_path(&source_path).unwrap();
        let root_uri = Url::from_file_path(&root).unwrap();
        let (mut service, mut socket) = LspService::new(Backend::new);
        let (tx, mut rx) = mpsc::unbounded_channel::<Request>();
        tokio::spawn(async move {
            while let Some(request) = socket.next().await {
                let _ = tx.send(request);
            }
        });

        let initialize = call(
            &mut service,
            "initialize",
            json!({
                "rootUri": root_uri,
                "capabilities": {}
            }),
            1,
        )
        .await;
        let initialize: InitializeResult = serde_json::from_value(initialize).unwrap();
        assert!(initialize.capabilities.definition_provider.is_some());
        assert!(initialize.capabilities.rename_provider.is_some());
        assert!(initialize.capabilities.semantic_tokens_provider.is_some());
        assert!(initialize.capabilities.inlay_hint_provider.is_some());
        assert!(initialize.capabilities.code_lens_provider.is_some());

        notify(&mut service, "initialized", json!({})).await;

        notify(
            &mut service,
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": source_uri,
                    "languageId": "masm",
                    "version": 1,
                    "text": valid_text
                }
            }),
        )
        .await;
        let diagnostics = next_notification(&mut rx, "textDocument/publishDiagnostics").await;
        assert_eq!(diagnostics["uri"], json!(source_uri));
        assert!(!diagnostics["diagnostics"].as_array().unwrap().is_empty());

        write_file(
            &manifest_path,
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\n[lib]\npath = \
             \"mod.masm\"\nnamespace = \"app\"\n",
        );

        notify(
            &mut service,
            "textDocument/didChange",
            json!({
                "textDocument": {
                    "uri": source_uri,
                    "version": 2
                },
                "contentChanges": [
                    {
                        "text": valid_text
                    }
                ]
            }),
        )
        .await;
        let diagnostics = next_notification(&mut rx, "textDocument/publishDiagnostics").await;
        assert_eq!(diagnostics["uri"], json!(source_uri));
        assert!(diagnostics["diagnostics"].as_array().unwrap().is_empty());

        let hover = call(
            &mut service,
            "textDocument/hover",
            json!({
                "textDocument": { "uri": source_uri },
                "position": { "line": 4, "character": 9 }
            }),
            2,
        )
        .await;
        let hover: Hover = serde_json::from_value(hover).unwrap();
        match hover.contents {
            tower_lsp::lsp_types::HoverContents::Markup(content) => {
                assert!(content.value.contains("proc ::app::helper"))
            }
            _ => panic!("expected markdown hover"),
        }

        let definition = call(
            &mut service,
            "textDocument/definition",
            json!({
                "textDocument": { "uri": source_uri },
                "position": { "line": 4, "character": 9 }
            }),
            3,
        )
        .await;
        let definition: Location = serde_json::from_value(definition).unwrap();
        assert_eq!(
            normalize_path(&definition.uri.to_file_path().unwrap()),
            normalize_path(&source_uri.to_file_path().unwrap())
        );
        assert_eq!(definition.range.start.line, 0);

        let rename = call(
            &mut service,
            "textDocument/rename",
            json!({
                "textDocument": { "uri": source_uri },
                "position": { "line": 0, "character": 9 },
                "newName": "renamed_helper"
            }),
            4,
        )
        .await;
        let rename: WorkspaceEdit = serde_json::from_value(rename).unwrap();
        let changes = rename.changes.unwrap();
        let edits = changes
            .iter()
            .find_map(|(uri, edits)| {
                (normalize_path(&uri.to_file_path().unwrap())
                    == normalize_path(&source_uri.to_file_path().unwrap()))
                .then_some(edits)
            })
            .unwrap();
        assert_eq!(edits.len(), 2);
        assert!(edits.iter().all(|edit| edit.new_text == "renamed_helper"));
    }
}
