mod file;
mod protoc;
mod workspace;

use lsp_types::notification::DidChangeTextDocument;
use lsp_types::request::Completion;
use lsp_types::CompletionParams;
use lsp_types::CompletionResponse;
use lsp_types::DidChangeTextDocumentParams;
use lsp_types::ReferenceParams;
use lsp_types::SaveOptions;
use lsp_types::TextDocumentSyncKind;

use lsp_server::{Connection, Message};
use lsp_types::request::References;
use lsp_types::request::{DocumentSymbolRequest, GotoDefinition, Request, WorkspaceSymbolRequest};
use lsp_types::{
    notification::{DidOpenTextDocument, DidSaveTextDocument, Notification, PublishDiagnostics},
    DiagnosticServerCapabilities, InitializeParams, ServerCapabilities, TextDocumentSyncCapability,
    TextDocumentSyncOptions, TextDocumentSyncSaveOptions,
};
use lsp_types::{
    DidOpenTextDocumentParams, DidSaveTextDocumentParams, DocumentSymbolParams,
    DocumentSymbolResponse, GotoDefinitionParams, GotoDefinitionResponse, OneOf,
    WorkspaceSymbolParams,
};
use std::fs;

pub use anyhow::Result;

#[derive(Debug, serde::Deserialize)]
struct Config {
    proto_paths: Vec<std::path::PathBuf>,
}

// Handle a request, returning the response to send.
fn handle<Req>(
    workspace: &mut workspace::Workspace,
    req: lsp_server::Request,
    handler: impl Fn(&mut workspace::Workspace, Req::Params) -> Result<Req::Result>,
) -> Result<lsp_server::Message>
where
    Req: lsp_types::request::Request,
{
    let (id, params) = req.extract::<Req::Params>(Req::METHOD)?;
    Ok(Message::Response(match handler(workspace, params) {
        Ok(resp) => lsp_server::Response {
            id,
            result: Some(serde_json::to_value(resp)?),
            error: None,
        },
        Err(err) => lsp_server::Response {
            id,
            result: None,
            error: Some(lsp_server::ResponseError {
                code: lsp_server::ErrorCode::InternalError as i32,
                message: err.to_string(),
                data: None,
            }),
        },
    }))
}

// Handle a notification, optionally returning a notification to send in response.
fn notify<N>(
    workspace: &mut workspace::Workspace,
    not: lsp_server::Notification,
    handler: impl Fn(&mut workspace::Workspace, N::Params) -> Result<Option<lsp_server::Notification>>,
) -> Result<Option<lsp_server::Message>>
where
    N: lsp_types::notification::Notification,
{
    let params = not.extract::<N::Params>(N::METHOD)?;
    Ok(match handler(workspace, params) {
        Ok(Some(resp)) => Some(Message::Notification(resp)),
        Ok(None) => None,
        // If we get an error, we can't respond directly as with a Request.
        // Instead, send a ShowMessage notification with the error.
        Err(err) => Some(Message::Notification(lsp_server::Notification {
            method: lsp_types::notification::ShowMessage::METHOD.into(),
            params: serde_json::to_value(lsp_types::ShowMessageParams {
                typ: lsp_types::MessageType::ERROR,
                message: err.to_string(),
            })?,
        })),
    })
}

fn handle_document_symbols(
    workspace: &mut workspace::Workspace,
    params: DocumentSymbolParams,
) -> Result<Option<DocumentSymbolResponse>> {
    Ok(Some(DocumentSymbolResponse::Flat(
        workspace.symbols(&params.text_document.uri)?,
    )))
}

fn handle_workspace_symbols(
    workspace: &mut workspace::Workspace,
    params: WorkspaceSymbolParams,
) -> Result<Option<lsp_types::WorkspaceSymbolResponse>> {
    Ok(Some(lsp_types::WorkspaceSymbolResponse::Flat(
        workspace.all_symbols(&params.query)?,
    )))
}

fn handle_references(
    workspace: &mut workspace::Workspace,
    params: ReferenceParams,
) -> Result<Option<Vec<lsp_types::Location>>> {
    workspace.references(params)
}

fn handle_goto_definition(
    workspace: &mut workspace::Workspace,
    params: GotoDefinitionParams,
) -> Result<Option<GotoDefinitionResponse>> {
    let uri = params.text_document_position_params.text_document.uri;
    let pos = params.text_document_position_params.position;
    let loc = (*workspace).goto(uri, pos)?;
    Ok(loc.map(GotoDefinitionResponse::Scalar))
}

fn handle_completion(
    workspace: &mut workspace::Workspace,
    params: CompletionParams,
) -> Result<Option<CompletionResponse>> {
    let pos = params.text_document_position.position;
    let uri = params.text_document_position.text_document.uri;
    workspace.complete(&uri, pos.line.try_into()?, pos.character.try_into()?)
}

fn notify_did_open(
    workspace: &mut workspace::Workspace,
    params: DidOpenTextDocumentParams,
) -> Result<Option<lsp_server::Notification>> {
    let uri = params.text_document.uri;
    let diags = workspace.open(uri.clone(), params.text_document.text)?;

    let params = lsp_types::PublishDiagnosticsParams {
        uri,
        diagnostics: diags,
        version: None,
    };

    Ok(Some(lsp_server::Notification {
        method: PublishDiagnostics::METHOD.into(),
        params: serde_json::to_value(&params)?,
    }))
}

fn notify_did_save(
    workspace: &mut workspace::Workspace,
    params: DidSaveTextDocumentParams,
) -> Result<Option<lsp_server::Notification>> {
    let uri = params.text_document.uri;
    let diags = workspace.save(uri.clone())?;

    let params = lsp_types::PublishDiagnosticsParams {
        uri,
        diagnostics: diags,
        version: None,
    };

    Ok(Some(lsp_server::Notification {
        method: PublishDiagnostics::METHOD.into(),
        params: serde_json::to_value(&params)?,
    }))
}

fn notify_did_change(
    workspace: &mut workspace::Workspace,
    params: DidChangeTextDocumentParams,
) -> Result<Option<lsp_server::Notification>> {
    let uri = params.text_document.uri;
    workspace.edit(&uri, params.content_changes)?;
    Ok(None)
}

fn has_proto_files(path: impl AsRef<std::path::Path>) -> Result<bool> {
    Ok(std::fs::read_dir(path)?
        .find(|x| match x {
            Ok(entry) => entry
                .path()
                .extension()
                .map_or(false, |e| e.to_str() == Some("proto")),
            Err(_) => false,
        })
        .is_some())
}

fn find_dirs(root: std::path::PathBuf) -> Result<Vec<std::path::PathBuf>> {
    let mut res = vec![];
    for entry in std::fs::read_dir(&root)? {
        if let Ok(entry) = entry {
            if entry.metadata().is_ok_and(|m| m.is_dir()) {
                let mut dirs = find_dirs(entry.path())?;
                res.append(&mut dirs);
            }
        }
    }
    res.push(root);
    Ok(res)
}

fn find_import_paths(root: std::path::PathBuf) -> Result<Vec<std::path::PathBuf>> {
    let dirs = find_dirs(root)?;
    Ok(dirs
        .iter()
        .filter(|&x| has_proto_files(x).unwrap_or(false))
        .map(|x| x.to_owned())
        .collect())
}

pub fn run(connection: Connection) -> Result<()> {
    let server_capabilities = serde_json::to_value(&ServerCapabilities {
        // BUG: technically we are supposed to support UTF-16.
        // From what I've seen editors seem to be happy with UTF-8.
        position_encoding: Some(lsp_types::PositionEncodingKind::UTF8),
        document_symbol_provider: Some(OneOf::Left(true)),
        workspace_symbol_provider: Some(OneOf::Left(true)),
        references_provider: Some(OneOf::Left(true)),
        text_document_sync: Some(TextDocumentSyncCapability::Options(
            TextDocumentSyncOptions {
                open_close: Some(true),
                save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                    include_text: Some(false),
                })),
                change: Some(TextDocumentSyncKind::INCREMENTAL),
                ..Default::default()
            },
        )),
        definition_provider: Some(OneOf::Left(true)),
        completion_provider: Some(lsp_types::CompletionOptions {
            trigger_characters: Some(vec!["\"".into()]),
            ..Default::default()
        }),
        diagnostic_provider: Some(DiagnosticServerCapabilities::Options(
            lsp_types::DiagnosticOptions {
                identifier: Some(String::from("pbls")),
                workspace_diagnostics: true,
                ..Default::default()
            },
        )),
        ..Default::default()
    })
    .unwrap();

    log::info!("Initializing");
    let init_params = connection.initialize(server_capabilities)?;
    let params: InitializeParams = serde_json::from_value(init_params).unwrap();
    let root = params
        .root_uri
        .map(|u| u.to_file_path().unwrap())
        .unwrap_or(std::env::current_dir().unwrap());

    // TODO: merge config from init params

    let path = root.join(".pbls.toml");
    let conf = if path.is_file() {
        log::info!("Reading config from {path:?}");
        toml::from_str(fs::read_to_string(path)?.as_str())?
    } else {
        log::info!("Using default config");
        Config {
            proto_paths: find_import_paths(root.clone())?,
        }
    };
    log::info!("Using config {:?}", conf);

    let proto_paths = conf
        .proto_paths
        .iter()
        .map(|path| {
            if path.is_relative() {
                root.join(path)
            } else {
                path.clone()
            }
        })
        .filter_map(|p| match p.canonicalize() {
            Ok(path) => Some(path),
            Err(err) => {
                log::warn!("Failed to canonicalize {p:?}: {err}");
                None
            }
        })
        .collect();
    log::debug!("Using proto_paths {:?}", proto_paths);

    let mut workspace = workspace::Workspace::new(proto_paths);

    for msg in &connection.receiver {
        log::info!("Handling message {msg:?}");
        match msg {
            Message::Request(req) => {
                if connection.handle_shutdown(&req)? {
                    log::info!("Shutting down");
                    return Ok(());
                }
                let resp = match req.method.as_str() {
                    DocumentSymbolRequest::METHOD => Some(handle::<DocumentSymbolRequest>(
                        &mut workspace,
                        req,
                        handle_document_symbols,
                    )),
                    WorkspaceSymbolRequest::METHOD => Some(handle::<WorkspaceSymbolRequest>(
                        &mut workspace,
                        req,
                        handle_workspace_symbols,
                    )),
                    References::METHOD => {
                        Some(handle::<References>(&mut workspace, req, handle_references))
                    }
                    GotoDefinition::METHOD => Some(handle::<GotoDefinition>(
                        &mut workspace,
                        req,
                        handle_goto_definition,
                    )),
                    Completion::METHOD => {
                        Some(handle::<Completion>(&mut workspace, req, handle_completion))
                    }
                    _ => None,
                };
                if let Some(resp) = resp {
                    connection.sender.send(resp?)?;
                }
            }
            Message::Response(_) => {}
            Message::Notification(not) => {
                let resp = match not.method.as_str() {
                    DidOpenTextDocument::METHOD => {
                        notify::<DidOpenTextDocument>(&mut workspace, not, notify_did_open)?
                    }
                    DidSaveTextDocument::METHOD => {
                        notify::<DidSaveTextDocument>(&mut workspace, not, notify_did_save)?
                    }
                    DidChangeTextDocument::METHOD => {
                        notify::<DidChangeTextDocument>(&mut workspace, not, notify_did_change)?
                    }
                    _ => None,
                };
                if let Some(resp) = resp {
                    connection.sender.send(resp)?;
                }
            }
        }
    }
    Ok(())
}
