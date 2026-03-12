use std::sync::Mutex;

use clarity_lsp::backend::{
    process_mutating_request, process_notification, process_request, EditorStateInput,
    LspNotification, LspNotificationResponse, LspRequest, LspRequestResponse,
};
use clarity_lsp::state::EditorState;
use crossbeam_channel::{Receiver as MultiplexableReceiver, Select, Sender as MultiplexableSender};
use tokio::sync::oneshot;
use tower_lsp_server::jsonrpc::{Error, ErrorCode, Result};
use tower_lsp_server::ls_types::{
    CompletionParams, CompletionResponse, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, DidSaveTextDocumentParams, DocumentFormattingParams,
    DocumentRangeFormattingParams, DocumentSymbolParams, DocumentSymbolResponse,
    ExecuteCommandParams, GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverParams,
    InitializeParams, InitializeResult, InitializedParams, MessageType, SignatureHelp,
    SignatureHelpParams, TextEdit,
};
use tower_lsp_server::{Client, LanguageServer};

use super::utils;
use crate::lsp::clarity_diagnostics_to_tower_lsp_type;

pub type NotificationMsg = (LspNotification, oneshot::Sender<LspNotificationResponse>);
pub type RequestMsg = (LspRequest, oneshot::Sender<LspRequestResponse>);

pub async fn start_language_server(
    notification_rx: MultiplexableReceiver<NotificationMsg>,
    request_rx: MultiplexableReceiver<RequestMsg>,
) {
    let mut editor_state = EditorStateInput::Owned(EditorState::new());

    let mut sel = Select::new();
    let notifications_oper = sel.recv(&notification_rx);
    let requests_oper = sel.recv(&request_rx);

    loop {
        let oper = sel.select();
        match oper.index() {
            i if i == notifications_oper => match oper.recv(&notification_rx) {
                Ok((notification, reply_tx)) => {
                    let result = process_notification(notification, &mut editor_state, None).await;
                    if let Ok(response) = result {
                        let _ = reply_tx.send(response);
                    }
                }
                Err(_e) => {
                    continue;
                }
            },
            i if i == requests_oper => {
                let msg: std::result::Result<RequestMsg, _> = oper.recv(&request_rx);
                match msg {
                    Ok((request, reply_tx)) => {
                        let request_result = match request {
                            LspRequest::Initialize(_) => {
                                process_mutating_request(request, &mut editor_state)
                            }
                            _ => process_request(request, &editor_state),
                        };
                        if let Ok(response) = request_result {
                            let _ = reply_tx.send(response);
                        }
                    }
                    Err(_e) => {
                        continue;
                    }
                }
            }
            _ => unreachable!(),
        }
    }
}

#[derive(Debug)]
pub struct LspNativeBridge {
    client: Client,
    notification_tx: Mutex<MultiplexableSender<NotificationMsg>>,
    request_tx: Mutex<MultiplexableSender<RequestMsg>>,
}

impl LspNativeBridge {
    pub fn new(
        client: Client,
        notification_tx: MultiplexableSender<NotificationMsg>,
        request_tx: MultiplexableSender<RequestMsg>,
    ) -> Self {
        Self {
            client,
            notification_tx: Mutex::new(notification_tx),
            request_tx: Mutex::new(request_tx),
        }
    }

    async fn send_notification(&self, notification: LspNotification) {
        let (reply_tx, reply_rx) = oneshot::channel();
        {
            let Ok(tx) = self.notification_tx.lock() else {
                return;
            };
            if tx.send((notification, reply_tx)).is_err() {
                return;
            }
        }

        let Ok(response) = reply_rx.await else {
            return;
        };

        for (location, diags) in &response.aggregated_diagnostics {
            if let Ok(url) = clarinet_files::paths::path_to_url_string(location) {
                self.client
                    .publish_diagnostics(
                        url.parse().expect("Failed to parse URL"),
                        clarity_diagnostics_to_tower_lsp_type(diags),
                        None,
                    )
                    .await;
            }
        }

        if let Some((level, message)) = response.notification {
            self.client.show_message(level, message).await;
        }
    }

    async fn send_request(&self, request: LspRequest) -> Option<LspRequestResponse> {
        let (reply_tx, reply_rx) = oneshot::channel();
        {
            let Ok(tx) = self.request_tx.lock() else {
                return None;
            };
            if tx.send((request, reply_tx)).is_err() {
                return None;
            }
        }
        reply_rx.await.ok()
    }
}

impl LanguageServer for LspNativeBridge {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        let response = self
            .send_request(LspRequest::Initialize(Box::new(params)))
            .await;
        match response {
            Some(LspRequestResponse::Initialize(result)) => Ok(*result),
            _ => Err(Error::new(ErrorCode::InternalError)),
        }
    }

    async fn initialized(&self, _params: InitializedParams) {}

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn execute_command(&self, _: ExecuteCommandParams) -> Result<Option<serde_json::Value>> {
        Ok(None)
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        match self.send_request(LspRequest::Completion(params)).await {
            Some(LspRequestResponse::CompletionItems(items)) => {
                Ok(Some(CompletionResponse::from(items)))
            }
            _ => Ok(None),
        }
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        match self.send_request(LspRequest::Definition(params)).await {
            Some(LspRequestResponse::Definition(Some(data))) => {
                Ok(Some(GotoDefinitionResponse::Scalar(data)))
            }
            _ => Ok(None),
        }
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        match self.send_request(LspRequest::DocumentSymbol(params)).await {
            Some(LspRequestResponse::DocumentSymbol(symbols)) => {
                Ok(Some(DocumentSymbolResponse::Nested(symbols)))
            }
            _ => Ok(None),
        }
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        match self.send_request(LspRequest::Hover(params)).await {
            Some(LspRequestResponse::Hover(data)) => Ok(data),
            _ => Ok(None),
        }
    }

    async fn signature_help(&self, params: SignatureHelpParams) -> Result<Option<SignatureHelp>> {
        match self.send_request(LspRequest::SignatureHelp(params)).await {
            Some(LspRequestResponse::SignatureHelp(data)) => Ok(data),
            _ => Ok(None),
        }
    }

    async fn formatting(&self, params: DocumentFormattingParams) -> Result<Option<Vec<TextEdit>>> {
        match self
            .send_request(LspRequest::DocumentFormatting(params))
            .await
        {
            Some(LspRequestResponse::DocumentFormatting(data)) => Ok(data),
            _ => Ok(None),
        }
    }

    async fn range_formatting(
        &self,
        params: DocumentRangeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        match self
            .send_request(LspRequest::DocumentRangeFormatting(params))
            .await
        {
            Some(LspRequestResponse::DocumentRangeFormatting(data)) => Ok(data),
            _ => Ok(None),
        }
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        if let Some(contract_location) = utils::get_contract_location(&params.text_document.uri) {
            self.send_notification(LspNotification::ContractOpened(contract_location))
                .await;
        } else if let Some(manifest_location) =
            utils::get_manifest_location(&params.text_document.uri)
        {
            self.send_notification(LspNotification::ManifestOpened(manifest_location))
                .await;
        } else {
            self.client
                .log_message(MessageType::WARNING, "Unsupported file opened")
                .await;
            return;
        };

        let _ = self.client.code_lens_refresh().await;
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        if let Some(contract_location) = utils::get_contract_location(&params.text_document.uri) {
            self.send_notification(LspNotification::ContractSaved(contract_location))
                .await;
        } else if let Some(manifest_location) =
            utils::get_manifest_location(&params.text_document.uri)
        {
            self.send_notification(LspNotification::ManifestSaved(manifest_location))
                .await;
        } else {
            return;
        };

        let _ = self.client.code_lens_refresh().await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        if let Some(contract_location) = utils::get_contract_location(&params.text_document.uri) {
            self.send_notification(LspNotification::ContractChanged(
                contract_location,
                params.content_changes[0].text.to_string(),
            ))
            .await;
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        if let Some(contract_location) = utils::get_contract_location(&params.text_document.uri) {
            self.send_notification(LspNotification::ContractClosed(contract_location))
                .await;
        }
    }
}
