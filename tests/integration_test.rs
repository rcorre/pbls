use core::panic;
use lsp_server::{Connection, Message};
use lsp_types::notification::{DidOpenTextDocument, PublishDiagnostics};
use lsp_types::request::{DocumentSymbolRequest, GotoDefinition, Shutdown, WorkspaceSymbolRequest};
use lsp_types::{notification::Initialized, request::Initialize, InitializedParams};
use lsp_types::{
    Diagnostic, DiagnosticSeverity, DidOpenTextDocumentParams, DocumentSymbolParams,
    DocumentSymbolResponse, GotoDefinitionParams, GotoDefinitionResponse, InitializeParams,
    Location, Position, PublishDiagnosticsParams, Range, SymbolInformation, SymbolKind,
    TextDocumentIdentifier, TextDocumentItem, TextDocumentPositionParams, Url,
    WorkspaceSymbolParams, WorkspaceSymbolResponse,
};
use pbls::Result;
use pretty_assertions::assert_eq;
use std::error::Error;

struct TestClient {
    conn: Connection,
    thread: Option<std::thread::JoinHandle<()>>,
    id: i32,
}

fn assert_elements_equal<T, K, F>(mut a: Vec<T>, mut b: Vec<T>, key: F)
where
    T: Clone + std::fmt::Debug + std::cmp::PartialEq,
    K: Ord,
    F: Clone + FnMut(&T) -> K,
{
    a.sort_by_key(key.clone());
    b.sort_by_key(key);

    assert_eq!(a, b);
}

impl TestClient {
    fn new() -> Result<TestClient> {
        let (client, server) = Connection::memory();
        let thread = std::thread::spawn(|| {
            pbls::run(server).unwrap();
        });
        let mut client = TestClient {
            conn: client,
            thread: Some(thread),
            id: 0,
        };

        client.request::<Initialize>(InitializeParams {
            root_uri: Some(
                Url::from_file_path(std::path::Path::new("testdata").canonicalize()?).unwrap(),
            ),
            ..Default::default()
        })?;
        client.notify::<Initialized>(InitializedParams {})?;

        Ok(client)
    }

    fn recv<T>(&self) -> std::result::Result<T::Params, Box<dyn Error>>
    where
        T: lsp_types::notification::Notification,
    {
        match self.conn.receiver.recv()? {
            Message::Request(r) => Err(format!("Expected notification, got: {r:?}"))?,
            Message::Response(r) => Err(format!("Expected notification, got: {r:?}"))?,
            Message::Notification(resp) => {
                assert_eq!(resp.method, T::METHOD);
                Ok(serde_json::from_value(resp.params)?)
            }
        }
    }

    fn request<T>(&mut self, params: T::Params) -> pbls::Result<T::Result>
    where
        T: lsp_types::request::Request,
        T::Params: serde::de::DeserializeOwned,
    {
        let req = Message::Request(lsp_server::Request {
            id: self.id.into(),
            method: T::METHOD.to_string(),
            params: serde_json::to_value(params)?,
        });
        eprintln!("Sending {:?}", req);
        self.id += 1;
        self.conn.sender.send(req)?;
        eprintln!("Waiting");
        match self.conn.receiver.recv()? {
            Message::Request(r) => Err(format!("Expected response, got: {r:?}"))?,
            Message::Notification(r) => Err(format!("Expected response, got: {r:?}"))?,
            Message::Response(resp) => Ok(serde_json::from_value(
                resp.result.ok_or("Missing result from response")?,
            )?),
        }
    }

    fn notify<T>(&self, params: T::Params) -> pbls::Result<()>
    where
        T: lsp_types::notification::Notification,
        T::Params: serde::de::DeserializeOwned,
    {
        self.conn
            .sender
            .send(Message::Notification(lsp_server::Notification {
                method: T::METHOD.to_string(),
                params: serde_json::to_value(params)?,
            }))?;
        Ok(())
    }
}

impl Drop for TestClient {
    fn drop(&mut self) {
        self.request::<Shutdown>(()).unwrap();
        self.notify::<lsp_types::notification::Exit>(()).unwrap();
        self.thread.take().unwrap().join().unwrap();
    }
}

#[test]
fn test_start_stop() -> pbls::Result<()> {
    TestClient::new()?;
    Ok(())
}

#[test]
fn test_open_ok() -> pbls::Result<()> {
    let client = TestClient::new()?;

    let uri =
        Url::from_file_path(std::path::Path::new("testdata/simple.proto").canonicalize()?).unwrap();

    client.notify::<DidOpenTextDocument>(DidOpenTextDocumentParams {
        text_document: TextDocumentItem {
            uri: uri.clone(),
            language_id: "".into(),
            version: 0,
            text: "".into(),
        },
    })?;
    let diags = client.recv::<PublishDiagnostics>()?;
    assert_eq!(
        diags,
        PublishDiagnosticsParams {
            uri: uri,
            diagnostics: vec![],
            version: None,
        }
    );
    Ok(())
}

#[test]
fn test_diagnostics() -> pbls::Result<()> {
    let client = TestClient::new()?;

    let uri =
        Url::from_file_path(std::path::Path::new("testdata/error.proto").canonicalize()?).unwrap();

    client.notify::<DidOpenTextDocument>(DidOpenTextDocumentParams {
        text_document: TextDocumentItem {
            uri: uri.clone(),
            language_id: "".into(),
            version: 0,
            text: "".into(),
        },
    })?;
    let diags = client.recv::<PublishDiagnostics>()?;
    assert_eq!(diags.uri, uri);
    let base_diag = Diagnostic {
        range: Range {
            start: Position {
                line: 16,
                character: 0,
            },
            end: Position {
                line: 16,
                character: 318,
            },
        },
        severity: Some(DiagnosticSeverity::ERROR),
        source: Some("pbls".into()),
        ..Default::default()
    };
    assert_elements_equal(
        diags.diagnostics,
        vec![
            Diagnostic {
                range: Range {
                    start: Position {
                        line: 16,
                        character: 0,
                    },
                    end: Position {
                        line: 16,
                        character: 318,
                    },
                },
                message: "\"f\" is already defined in \"main.Bar\".".into(),
                ..base_diag.clone()
            },
            Diagnostic {
                range: Range {
                    start: Position {
                        line: 11,
                        character: 0,
                    },
                    end: Position {
                        line: 11,
                        character: 44,
                    },
                },
                message: "\"Thingy\" is not defined.".into(),
                ..base_diag.clone()
            },
            Diagnostic {
                range: Range {
                    start: Position {
                        line: 16,
                        character: 0,
                    },
                    end: Position {
                        line: 16,
                        character: 88,
                    },
                },
                message: "Field number 1 has already been used in \"main.Bar\" by field \"f\""
                    .into(),
                ..base_diag.clone()
            },
        ],
        |s| s.message.clone(),
    );
    Ok(())
}

#[test]
fn test_no_diagnostics() -> pbls::Result<()> {
    let client = TestClient::new()?;

    let uri =
        Url::from_file_path(std::path::Path::new("testdata/simple.proto").canonicalize()?).unwrap();

    client.notify::<DidOpenTextDocument>(DidOpenTextDocumentParams {
        text_document: TextDocumentItem {
            uri: uri.clone(),
            language_id: "".into(),
            version: 0,
            text: "".into(),
        },
    })?;
    let diags = client.recv::<PublishDiagnostics>()?;

    assert_eq!(
        diags,
        PublishDiagnosticsParams {
            uri,
            diagnostics: vec![],
            version: None
        }
    );
    Ok(())
}

#[test]
fn test_document_symbols() -> pbls::Result<()> {
    let mut client = TestClient::new()?;

    let uri =
        Url::from_file_path(std::path::Path::new("testdata/simple.proto").canonicalize()?).unwrap();

    client.notify::<DidOpenTextDocument>(DidOpenTextDocumentParams {
        text_document: TextDocumentItem {
            uri: uri.clone(),
            language_id: "".into(),
            version: 0,
            text: "".into(),
        },
    })?;
    client.recv::<PublishDiagnostics>()?;

    let Some(DocumentSymbolResponse::Flat(actual)) =
        client.request::<DocumentSymbolRequest>(DocumentSymbolParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            work_done_progress_params: lsp_types::WorkDoneProgressParams {
                work_done_token: None,
            },
            partial_result_params: lsp_types::PartialResultParams {
                partial_result_token: None,
            },
        })?
    else {
        panic!("Expected DocumentSymbolResponse::Flat")
    };
    assert_elements_equal(
        actual,
        vec![
            // deprecated field is deprecated, but cannot be omitted
            #[allow(deprecated)]
            SymbolInformation {
                name: "Thing".into(),
                kind: SymbolKind::ENUM,
                tags: None,
                deprecated: None,
                location: Location {
                    uri: uri.clone(),
                    range: Range {
                        start: Position {
                            line: 7,
                            character: 0,
                        },
                        end: Position {
                            line: 11,
                            character: 1,
                        },
                    },
                },
                container_name: Some("main".into()),
            },
            // deprecated field is deprecated, but cannot be omitted
            #[allow(deprecated)]
            SymbolInformation {
                name: "Foo".into(),
                kind: SymbolKind::STRUCT,
                tags: None,
                deprecated: None,
                location: Location {
                    uri: uri.clone(),
                    range: Range {
                        start: Position {
                            line: 13,
                            character: 0,
                        },
                        end: Position {
                            line: 17,
                            character: 1,
                        },
                    },
                },
                container_name: Some("main".into()),
            },
            // deprecated field is deprecated, but cannot be omitted
            #[allow(deprecated)]
            SymbolInformation {
                name: "Bar".into(),
                kind: SymbolKind::STRUCT,
                tags: None,
                deprecated: None,
                location: Location {
                    uri: uri.clone(),
                    range: Range {
                        start: Position {
                            line: 19,
                            character: 0,
                        },
                        end: Position {
                            line: 22,
                            character: 1,
                        },
                    },
                },
                container_name: Some("main".into()),
            },
            #[allow(deprecated)]
            SymbolInformation {
                name: "Empty".into(),
                kind: SymbolKind::STRUCT,
                tags: None,
                deprecated: None,
                location: Location {
                    uri: uri.clone(),
                    range: Range {
                        start: Position {
                            line: 24,
                            character: 0,
                        },
                        end: Position {
                            line: 24,
                            character: 16,
                        },
                    },
                },
                container_name: Some("main".into()),
            },
        ],
        |s| s.name.clone(),
    );
    Ok(())
}

#[test]
fn test_workspace_symbols() -> pbls::Result<()> {
    let mut client = TestClient::new()?;

    let base_uri = Url::from_file_path(std::fs::canonicalize("testdata/simple.proto")?).unwrap();
    let dep_uri = Url::from_file_path(std::fs::canonicalize("testdata/dep.proto")?).unwrap();
    let other_uri = Url::from_file_path(std::fs::canonicalize("testdata/other.proto")?).unwrap();

    client.notify::<DidOpenTextDocument>(DidOpenTextDocumentParams {
        text_document: TextDocumentItem {
            uri: base_uri.clone(),
            language_id: "".into(),
            version: 0,
            text: "".into(),
        },
    })?;
    client.recv::<PublishDiagnostics>()?;

    let Some(WorkspaceSymbolResponse::Flat(actual)) =
        client.request::<WorkspaceSymbolRequest>(WorkspaceSymbolParams {
            query: "".into(),
            work_done_progress_params: lsp_types::WorkDoneProgressParams {
                work_done_token: None,
            },
            partial_result_params: lsp_types::PartialResultParams {
                partial_result_token: None,
            },
        })?
    else {
        panic!("Symbols response is not Flat")
    };
    let expected = vec![
        // deprecated field is deprecated, but cannot be omitted
        #[allow(deprecated)]
        SymbolInformation {
            name: "Other".into(),
            kind: SymbolKind::STRUCT,
            tags: None,
            deprecated: None,
            location: Location {
                uri: other_uri,
                range: Range {
                    start: Position {
                        line: 4,
                        character: 0,
                    },
                    end: Position {
                        line: 6,
                        character: 1,
                    },
                },
            },
            container_name: Some("other".into()),
        },
        // deprecated field is deprecated, but cannot be omitted
        #[allow(deprecated)]
        SymbolInformation {
            name: "Dep".into(),
            kind: SymbolKind::STRUCT,
            tags: None,
            deprecated: None,
            location: Location {
                uri: dep_uri.clone(),
                range: Range {
                    start: Position {
                        line: 4,
                        character: 0,
                    },
                    end: Position {
                        line: 6,
                        character: 1,
                    },
                },
            },
            container_name: Some("main".into()),
        },
        // deprecated field is deprecated, but cannot be omitted
        #[allow(deprecated)]
        SymbolInformation {
            name: "Dep2".into(),
            kind: SymbolKind::ENUM,
            tags: None,
            deprecated: None,
            location: Location {
                uri: dep_uri.clone(),
                range: Range {
                    start: Position {
                        line: 8,
                        character: 0,
                    },
                    end: Position {
                        line: 11,
                        character: 1,
                    },
                },
            },
            container_name: Some("main".into()),
        },
        // deprecated field is deprecated, but cannot be omitted
        #[allow(deprecated)]
        SymbolInformation {
            name: "Thing".into(),
            kind: SymbolKind::ENUM,
            tags: None,
            deprecated: None,
            location: Location {
                uri: base_uri.clone(),
                range: Range {
                    start: Position {
                        line: 7,
                        character: 0,
                    },
                    end: Position {
                        line: 11,
                        character: 1,
                    },
                },
            },
            container_name: Some("main".into()),
        },
        // deprecated field is deprecated, but cannot be omitted
        #[allow(deprecated)]
        SymbolInformation {
            name: "Foo".into(),
            kind: SymbolKind::STRUCT,
            tags: None,
            deprecated: None,
            location: Location {
                uri: base_uri.clone(),
                range: Range {
                    start: Position {
                        line: 13,
                        character: 0,
                    },
                    end: Position {
                        line: 17,
                        character: 1,
                    },
                },
            },
            container_name: Some("main".into()),
        },
        // deprecated field is deprecated, but cannot be omitted
        #[allow(deprecated)]
        SymbolInformation {
            name: "Bar".into(),
            kind: SymbolKind::STRUCT,
            tags: None,
            deprecated: None,
            location: Location {
                uri: base_uri.clone(),
                range: Range {
                    start: Position {
                        line: 19,
                        character: 0,
                    },
                    end: Position {
                        line: 22,
                        character: 1,
                    },
                },
            },
            container_name: Some("main".into()),
        },
        #[allow(deprecated)]
        SymbolInformation {
            name: "Empty".into(),
            kind: SymbolKind::STRUCT,
            tags: None,
            deprecated: None,
            location: Location {
                uri: base_uri.clone(),
                range: Range {
                    start: Position {
                        line: 24,
                        character: 0,
                    },
                    end: Position {
                        line: 24,
                        character: 16,
                    },
                },
            },
            container_name: Some("main".into()),
        },
    ];
    assert_elements_equal(actual, expected, |s| s.name.clone());
    Ok(())
}

#[test]
fn test_goto_definition_same_file() -> pbls::Result<()> {
    let mut client = TestClient::new()?;

    let uri = Url::from_file_path(std::fs::canonicalize("testdata/simple.proto")?).unwrap();

    client.notify::<DidOpenTextDocument>(DidOpenTextDocumentParams {
        text_document: TextDocumentItem {
            uri: uri.clone(),
            language_id: "".into(),
            version: 0,
            text: "".into(),
        },
    })?;
    client.recv::<PublishDiagnostics>()?;

    // goto Thing enum
    {
        let resp = client.request::<GotoDefinition>(GotoDefinitionParams {
            work_done_progress_params: lsp_types::WorkDoneProgressParams {
                work_done_token: None,
            },
            partial_result_params: lsp_types::PartialResultParams {
                partial_result_token: None,
            },
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position: Position {
                    line: 15,
                    character: 5,
                },
            },
        })?;
        assert_eq!(
            resp,
            Some(GotoDefinitionResponse::Scalar(Location {
                uri: uri.clone(),
                range: Range {
                    start: Position {
                        line: 7,
                        character: 0
                    },
                    end: Position {
                        line: 11,
                        character: 1
                    }
                }
            }))
        );
    }

    // goto Foo message
    {
        let resp = client.request::<GotoDefinition>(GotoDefinitionParams {
            work_done_progress_params: lsp_types::WorkDoneProgressParams {
                work_done_token: None,
            },
            partial_result_params: lsp_types::PartialResultParams {
                partial_result_token: None,
            },
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position: Position {
                    line: 20,
                    character: 2,
                },
            },
        })?;
        assert_eq!(
            resp,
            Some(GotoDefinitionResponse::Scalar(Location {
                uri: uri.clone(),
                range: Range {
                    start: Position {
                        line: 13,
                        character: 0
                    },
                    end: Position {
                        line: 17,
                        character: 1
                    }
                }
            }))
        );
    }
    Ok(())
}

#[test]
fn test_goto_definition_different_file() -> pbls::Result<()> {
    let mut client = TestClient::new()?;

    let src_uri = Url::from_file_path(std::fs::canonicalize("testdata/simple.proto")?).unwrap();
    let dst_uri = Url::from_file_path(std::fs::canonicalize("testdata/dep.proto")?).unwrap();

    client.notify::<DidOpenTextDocument>(DidOpenTextDocumentParams {
        text_document: TextDocumentItem {
            uri: src_uri.clone(),
            language_id: "".into(),
            version: 0,
            text: "".into(),
        },
    })?;
    client.recv::<PublishDiagnostics>()?;

    // goto Dep message
    let resp = client.request::<GotoDefinition>(GotoDefinitionParams {
        work_done_progress_params: lsp_types::WorkDoneProgressParams {
            work_done_token: None,
        },
        partial_result_params: lsp_types::PartialResultParams {
            partial_result_token: None,
        },
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: src_uri.clone(),
            },
            position: Position {
                line: 16,
                character: 5,
            },
        },
    })?;
    assert_eq!(
        resp,
        Some(GotoDefinitionResponse::Scalar(Location {
            uri: dst_uri.clone(),
            range: Range {
                start: Position {
                    line: 4,
                    character: 0
                },
                end: Position {
                    line: 6,
                    character: 1
                }
            }
        }))
    );

    Ok(())
}
