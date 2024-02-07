use core::panic;
use lsp_server::{Connection, Message};
use lsp_types::notification::{
    DidChangeTextDocument, DidOpenTextDocument, DidSaveTextDocument, PublishDiagnostics,
};
use lsp_types::request::{
    Completion, DocumentSymbolRequest, GotoDefinition, Shutdown, WorkspaceSymbolRequest,
};
use lsp_types::{notification::Initialized, request::Initialize, InitializedParams};
use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionParams, Diagnostic, DiagnosticSeverity,
    DidChangeTextDocumentParams, DidOpenTextDocumentParams, DidSaveTextDocumentParams,
    DocumentSymbolParams, DocumentSymbolResponse, GotoDefinitionParams, GotoDefinitionResponse,
    InitializeParams, Location, PartialResultParams, Position, PublishDiagnosticsParams, Range,
    SymbolInformation, SymbolKind, TextDocumentContentChangeEvent, TextDocumentIdentifier,
    TextDocumentItem, TextDocumentPositionParams, Url, WorkDoneProgressParams,
    WorkspaceSymbolParams, WorkspaceSymbolResponse,
};
use pbls::Result;
use pretty_assertions::assert_eq;
use std::error::Error;

fn base_uri() -> Url {
    Url::from_file_path(std::fs::canonicalize("./testdata/simple.proto").unwrap()).unwrap()
}

fn other_uri() -> Url {
    Url::from_file_path(std::fs::canonicalize("./testdata/other.proto").unwrap()).unwrap()
}

fn dep_uri() -> Url {
    Url::from_file_path(std::fs::canonicalize("./testdata/dep.proto").unwrap()).unwrap()
}

fn error_uri() -> Url {
    Url::from_file_path(std::fs::canonicalize("./testdata/error.proto").unwrap()).unwrap()
}

fn diag(uri: Url, target: &str, message: &str) -> Diagnostic {
    Diagnostic {
        range: locate_sym(uri, target).range,
        message: message.into(),
        severity: Some(DiagnosticSeverity::ERROR),
        source: Some("pbls".into()),
        ..Default::default()
    }
}

fn sym(uri: Url, name: &str, text: &str) -> SymbolInformation {
    let kind = text
        .split_once(" ")
        .unwrap_or_else(|| panic!("Invalid symbol {text}"))
        .0;
    // deprecated field is deprecated, but cannot be omitted
    #[allow(deprecated)]
    SymbolInformation {
        name: name.into(),
        kind: match kind {
            "enum" => SymbolKind::ENUM,
            "message" => SymbolKind::STRUCT,
            _ => panic!("Invalid symbol {text}"),
        },
        tags: None,
        deprecated: None,
        location: locate_sym(uri, text),
        container_name: None,
    }
}

// Generate TextDocumentPositionParams for the given string and offset.
fn position(uri: Url, text: &str, column: u32) -> TextDocumentPositionParams {
    let filetext = std::fs::read_to_string(uri.to_file_path().unwrap()).unwrap();
    let (lineno, line) = filetext
        .lines()
        .enumerate()
        .skip_while(|(_, l)| !l.contains(text))
        .next()
        .unwrap_or_else(|| panic!("{text} not found in {uri}"));

    let character = line.find(text).unwrap_or(0);
    TextDocumentPositionParams {
        text_document: TextDocumentIdentifier { uri: base_uri() },
        position: Position {
            line: lineno.try_into().unwrap(),
            character: column + u32::try_from(character).unwrap(),
        },
    }
}

// Generate a GotoDefinition request for a line containing `text`,
// with the cursor offset from the start of the search string by `offset`
fn goto(uri: Url, text: &str, column: u32) -> GotoDefinitionParams {
    GotoDefinitionParams {
        work_done_progress_params: lsp_types::WorkDoneProgressParams {
            work_done_token: None,
        },
        partial_result_params: lsp_types::PartialResultParams {
            partial_result_token: None,
        },
        text_document_position_params: position(uri, text, column),
    }
}

// Given "some |search| string", locate "some search string" in the document
// and return the Location of "search".
fn locate(uri: Url, text: &str) -> Location {
    let start_off = text.find("|").unwrap();
    let end_off = text.rfind("|").unwrap() - 1;
    let text = text.replace("|", "");
    let filetext = std::fs::read_to_string(uri.to_file_path().unwrap()).unwrap();
    let (start_line, start_col) = filetext
        .lines()
        .enumerate()
        .find_map(|(i, l)| match l.find(text.as_str()) {
            Some(col) => Some((i, col)),
            None => None,
        })
        .unwrap_or_else(|| panic!("{text} not found in {uri}"));

    Location {
        uri,
        range: Range {
            start: Position {
                line: start_line.try_into().unwrap(),
                character: (start_col + start_off).try_into().unwrap(),
            },
            end: Position {
                line: start_line.try_into().unwrap(),
                character: (start_col + end_off).try_into().unwrap(),
            },
        },
    }
}

// Return the Location for the definition of the type `name`.
// `name` should include the "message" or "enum" prefix.
// Assumes that the leading { is on the same line as the enum/message.
fn locate_sym(uri: Url, name: &str) -> Location {
    let filetext = std::fs::read_to_string(uri.to_file_path().unwrap()).unwrap();
    let (start_line, start_col) = filetext
        .lines()
        .enumerate()
        .find_map(|(i, l)| match l.find(name) {
            Some(col) => Some((i, col)),
            None => None,
        })
        .unwrap_or_else(|| panic!("{name} not found in {uri}"));

    let mut nesting = 0;
    let mut end_line = 0;
    let mut end_col = 0;
    for (lineno, line) in filetext.lines().skip(start_line).enumerate() {
        nesting += line.chars().filter(|c| c == &'{').count();
        nesting -= line.chars().filter(|c| c == &'}').count();
        if nesting == 0 {
            end_line = lineno + start_line;
            end_col = line.len();
            break;
        }
    }

    Location {
        uri,
        range: Range {
            start: Position {
                line: start_line.try_into().unwrap(),
                character: start_col.try_into().unwrap(),
            },
            end: Position {
                line: end_line.try_into().unwrap(),
                character: end_col.try_into().unwrap(),
            },
        },
    }
}

fn completion_params(uri: Url, position: Position) -> CompletionParams {
    CompletionParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri },
            position,
        },
        work_done_progress_params: WorkDoneProgressParams {
            work_done_token: None,
        },
        partial_result_params: PartialResultParams {
            partial_result_token: None,
        },
        context: None,
    }
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

struct TestClient {
    conn: Connection,
    thread: Option<std::thread::JoinHandle<()>>,
    id: i32,
}

impl TestClient {
    fn new() -> Result<TestClient> {
        Self::new_with_root("testdata")
    }

    fn new_with_root(path: impl AsRef<std::path::Path>) -> Result<TestClient> {
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
            root_uri: Some(Url::from_file_path(std::fs::canonicalize(path).unwrap()).unwrap()),
            ..Default::default()
        })?;
        client.notify::<Initialized>(InitializedParams {})?;

        Ok(client)
    }

    fn recv<T>(&self) -> std::result::Result<T::Params, Box<dyn Error>>
    where
        T: lsp_types::notification::Notification,
    {
        match self
            .conn
            .receiver
            .recv_timeout(std::time::Duration::from_secs(5))?
        {
            Message::Request(r) => Err(format!("Expected notification, got: {r:?}"))?,
            Message::Response(r) => Err(format!("Expected notification, got: {r:?}"))?,
            Message::Notification(resp) => {
                assert_eq!(resp.method, T::METHOD, "Unexpected response {resp:?}");
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
        match self
            .conn
            .receiver
            .recv_timeout(std::time::Duration::from_secs(5))?
        {
            Message::Request(r) => Err(format!("Expected response, got: {r:?}"))?,
            Message::Notification(r) => Err(format!("Expected response, got: {r:?}"))?,
            Message::Response(resp) if resp.error.is_some() => {
                Err(format!("Got error response {:?}", resp))?
            }
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

    fn open(&self, uri: Url) -> pbls::Result<PublishDiagnosticsParams> {
        let text = std::fs::read_to_string(uri.path())?;
        self.notify::<DidOpenTextDocument>(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri,
                language_id: "".into(),
                version: 0,
                text,
            },
        })?;
        self.recv::<PublishDiagnostics>()
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
fn test_open() -> pbls::Result<()> {
    let client = TestClient::new()?;

    assert_eq!(
        client.open(base_uri())?,
        PublishDiagnosticsParams {
            uri: base_uri(),
            diagnostics: vec![],
            version: None,
        }
    );
    Ok(())
}

#[test]
fn test_diagnostics_on_open() -> pbls::Result<()> {
    let client = TestClient::new()?;

    let diags = client.open(error_uri())?;
    assert_eq!(diags.uri, error_uri());
    assert_elements_equal(
        diags.diagnostics,
        vec![
            diag(error_uri(), "Unknown t =", "\"Unknown\" is not defined"),
            diag(
                error_uri(),
                "int32 bar =",
                "Field number 1 has already been used in \"main.Noo\" by field \"foo\"",
            ),
        ],
        |s| s.message.clone(),
    );
    Ok(())
}

#[test]
fn test_diagnostics_on_save() -> pbls::Result<()> {
    let tmp = tempfile::tempdir()?;
    let path = tmp.path().join("example.proto");
    let uri = Url::from_file_path(&path).unwrap();
    let client = TestClient::new_with_root(&tmp)?;

    let text = r#"
syntax = "proto3";
package main;
message Foo{}
"#;
    std::fs::write(&path, text)?;

    let diags = client.open(uri.clone())?;
    assert_eq!(
        diags,
        PublishDiagnosticsParams {
            uri: uri.clone(),
            diagnostics: vec![],
            version: None,
        }
    );

    // modify the file, check that we pick up the change
    let text = r#"
syntax = "proto3";
package main;
message Foo{Flob flob = 1;}
"#;
    std::fs::write(&path, text)?;

    let start = lsp_types::Position {
        line: 3,
        character: "message Foo{ ".len() as u32,
    };
    client.notify::<DidChangeTextDocument>(DidChangeTextDocumentParams {
        text_document: lsp_types::VersionedTextDocumentIdentifier {
            uri: uri.clone(),
            version: 0,
        },
        content_changes: vec![TextDocumentContentChangeEvent {
            text: "Flob flob = 1;".into(),
            range: Some(lsp_types::Range { start, end: start }),
            range_length: None,
        }],
    })?;

    client.notify::<DidSaveTextDocument>(DidSaveTextDocumentParams {
        text_document: TextDocumentIdentifier { uri: uri.clone() },
        text: None,
    })?;
    let diags = client.recv::<PublishDiagnostics>()?;
    assert_eq!(
        diags,
        PublishDiagnosticsParams {
            uri: uri.clone(),
            diagnostics: vec![diag(
                uri,
                "message Foo{Flob flob = 1;}",
                "\"Flob\" is not defined",
            )],
            version: None,
        }
    );

    Ok(())
}

#[test]
fn test_no_diagnostics_on_open() -> pbls::Result<()> {
    let client = TestClient::new()?;
    assert_eq!(
        client.open(base_uri())?,
        PublishDiagnosticsParams {
            uri: base_uri(),
            diagnostics: vec![],
            version: None
        }
    );
    Ok(())
}

#[test]
fn test_document_symbols() -> pbls::Result<()> {
    let mut client = TestClient::new()?;
    client.open(base_uri())?;

    let Some(DocumentSymbolResponse::Flat(actual)) =
        client.request::<DocumentSymbolRequest>(DocumentSymbolParams {
            text_document: TextDocumentIdentifier {
                uri: base_uri().clone(),
            },
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
            sym(base_uri(), "Thing", "enum Thing"),
            sym(base_uri(), "Foo", "message Foo"),
            sym(base_uri(), "Foo.Buz", "message Buz"),
            sym(base_uri(), "Bar", "message Bar"),
            sym(base_uri(), "Empty", "message Empty"),
        ],
        |s| s.name.clone(),
    );
    Ok(())
}

#[test]
fn test_workspace_symbols() -> pbls::Result<()> {
    let mut client = TestClient::new()?;
    client.open(base_uri())?;

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
        sym(other_uri(), "Other", "message Other"),
        sym(dep_uri(), "Dep", "message Dep"),
        sym(dep_uri(), "Dep2", "enum Dep2"),
        sym(base_uri(), "Thing", "enum Thing"),
        sym(base_uri(), "Foo", "message Foo"),
        sym(base_uri(), "Foo.Buz", "message Buz"),
        sym(base_uri(), "Bar", "message Bar"),
        sym(base_uri(), "Empty", "message Empty"),
        sym(other_uri(), "Other.Nested", "message Nested"),
        sym(error_uri(), "Nope", "enum Nope"),
        sym(error_uri(), "Nah", "message Nah"),
        sym(error_uri(), "Noo", "message Noo"),
    ];
    assert_elements_equal(actual, expected, |s| s.name.clone());
    Ok(())
}

#[test]
fn test_goto_import() -> pbls::Result<()> {
    let mut client = TestClient::new()?;
    client.open(base_uri())?;

    assert_eq!(
        client.request::<GotoDefinition>(goto(base_uri(), "\"dep.proto\"", 3))?,
        Some(GotoDefinitionResponse::Scalar(lsp_types::Location {
            uri: dep_uri(),
            range: lsp_types::Range::default(),
        }))
    );

    assert_eq!(
        client.request::<GotoDefinition>(goto(base_uri(), "\"other.proto\"", 6))?,
        Some(GotoDefinitionResponse::Scalar(lsp_types::Location {
            uri: other_uri(),
            range: lsp_types::Range::default(),
        }))
    );

    Ok(())
}

#[test]
fn test_goto_definition_same_file() -> pbls::Result<()> {
    let mut client = TestClient::new()?;
    client.open(base_uri())?;

    assert_eq!(
        client.request::<GotoDefinition>(goto(base_uri(), "Thing t =", 3))?,
        Some(GotoDefinitionResponse::Scalar(locate_sym(
            base_uri(),
            "enum Thing"
        )))
    );

    assert_eq!(
        client.request::<GotoDefinition>(goto(base_uri(), "Foo f =", 2))?,
        Some(GotoDefinitionResponse::Scalar(locate_sym(
            base_uri(),
            "message Foo",
        )))
    );

    Ok(())
}

#[test]
fn test_goto_definition_same_file_nested() -> pbls::Result<()> {
    let mut client = TestClient::new()?;
    client.open(base_uri())?;

    assert_eq!(
        client.request::<GotoDefinition>(goto(base_uri(), "Foo.Buz buz =", 6))?,
        Some(GotoDefinitionResponse::Scalar(locate_sym(
            base_uri(),
            "message Buz"
        )))
    );

    Ok(())
}

#[test]
fn test_goto_definition_different_file() -> pbls::Result<()> {
    let mut client = TestClient::new()?;
    client.open(base_uri())?;

    let resp = client.request::<GotoDefinition>(goto(base_uri(), "Dep d =", 0))?;
    assert_eq!(
        resp,
        Some(GotoDefinitionResponse::Scalar(locate_sym(
            dep_uri(),
            "message Dep",
        )))
    );

    Ok(())
}

#[test]
fn test_goto_definition_different_file_nested() -> pbls::Result<()> {
    let mut client = TestClient::new()?;
    client.open(base_uri())?;

    let resp = client.request::<GotoDefinition>(goto(
        base_uri(),
        "other.Other.Nested other_nested =",
        0,
    ))?;
    assert_eq!(
        resp,
        Some(GotoDefinitionResponse::Scalar(locate_sym(
            other_uri(),
            "message Nested",
        )))
    );

    Ok(())
}

#[test]
fn test_message_references() -> pbls::Result<()> {
    let mut client = TestClient::new()?;
    client.open(base_uri())?;

    assert_eq!(
        client.request::<lsp_types::request::References>(lsp_types::ReferenceParams {
            text_document_position: position(base_uri(), "message Foo", 9),
            work_done_progress_params: lsp_types::WorkDoneProgressParams {
                work_done_token: None,
            },
            partial_result_params: lsp_types::PartialResultParams {
                partial_result_token: None
            },
            context: lsp_types::ReferenceContext {
                include_declaration: false,
            },
        })?,
        Some(vec![locate(base_uri(), "|Foo| f = 1")])
    );

    Ok(())
}

#[test]
fn test_complete_import() -> pbls::Result<()> {
    let mut client = TestClient::new()?;
    client.open(base_uri())?;

    let loc = locate_sym(base_uri(), "import \"other.proto\"");
    let pos = lsp_types::Position {
        line: loc.range.start.line + 1,
        character: 0,
    };
    let change = TextDocumentContentChangeEvent {
        text: "import \"".into(),
        range: Some(lsp_types::Range {
            start: pos,
            end: pos,
        }),
        range_length: None,
    };
    client.notify::<DidChangeTextDocument>(DidChangeTextDocumentParams {
        text_document: lsp_types::VersionedTextDocumentIdentifier {
            uri: base_uri().clone(),
            version: 0,
        },
        content_changes: vec![change],
    })?;

    let pos = lsp_types::Position {
        line: loc.range.start.line + 1,
        character: "import \"".len().try_into().unwrap(),
    };
    let resp = client.request::<Completion>(completion_params(base_uri(), pos))?;

    let Some(lsp_types::CompletionResponse::Array(actual)) = resp else {
        panic!("Unexpected completion response {resp:?}");
    };

    // excludes simple.proto (the current file)
    // excludes other.proto (already imported)
    // excludes dep.proto (already imported)
    assert_elements_equal(
        actual,
        vec![CompletionItem {
            label: "error.proto".into(),
            kind: Some(CompletionItemKind::FILE),
            insert_text: Some("error.proto\";".into()),
            ..Default::default()
        }],
        |s| s.label.clone(),
    );

    Ok(())
}

#[test]
fn test_complete_keyword() -> pbls::Result<()> {
    let mut client = TestClient::new()?;
    client.open(base_uri())?;

    // get completion on the line after "package"
    let loc = locate_sym(base_uri(), "package main;");

    let resp = client.request::<Completion>(completion_params(
        base_uri(),
        Position {
            line: loc.range.end.line + 1,
            character: 0,
        },
    ))?;

    let Some(lsp_types::CompletionResponse::Array(actual)) = resp else {
        panic!("Unexpected completion response {resp:?}");
    };

    assert_elements_equal(
        actual,
        vec![
            CompletionItem {
                label: "message".into(),
                kind: Some(CompletionItemKind::KEYWORD),
                ..Default::default()
            },
            CompletionItem {
                label: "enum".into(),
                kind: Some(CompletionItemKind::KEYWORD),
                ..Default::default()
            },
            CompletionItem {
                label: "import".into(),
                kind: Some(CompletionItemKind::KEYWORD),
                ..Default::default()
            },
            CompletionItem {
                label: "option".into(),
                kind: Some(CompletionItemKind::KEYWORD),
                ..Default::default()
            },
        ],
        |s| s.label.clone(),
    );

    Ok(())
}

#[test]
fn test_complete_type() -> pbls::Result<()> {
    let mut client = TestClient::new()?;
    client.open(base_uri())?;

    // get completion in the body of message Foo.
    let pos = locate_sym(base_uri(), "Thing t =").range.start;

    let resp = client.request::<Completion>(completion_params(
        base_uri(),
        Position {
            line: pos.line,
            character: 4,
        },
    ))?;

    let Some(lsp_types::CompletionResponse::Array(actual)) = resp else {
        panic!("Unexpected completion response {resp:?}");
    };

    let keyword = |name: &str| CompletionItem {
        label: name.into(),
        kind: Some(CompletionItemKind::KEYWORD),
        ..Default::default()
    };

    let _struct = |name: &str| CompletionItem {
        label: name.into(),
        kind: Some(CompletionItemKind::STRUCT),
        ..Default::default()
    };

    let _enum = |name: &str| CompletionItem {
        label: name.into(),
        kind: Some(CompletionItemKind::ENUM),
        ..Default::default()
    };

    assert_elements_equal(
        actual,
        vec![
            keyword("enum"),
            keyword("extend"),
            keyword("import"),
            keyword("map"),
            keyword("message"),
            keyword("oneof"),
            keyword("option"),
            keyword("optional"),
            keyword("package"),
            keyword("repeated"),
            keyword("reserved"),
            keyword("returns"),
            keyword("rpc"),
            keyword("service"),
            keyword("stream"),
            _struct("bool"),
            _struct("bytes"),
            _struct("double"),
            _struct("fixed32"),
            _struct("fixed64"),
            _struct("float"),
            _struct("int32"),
            _struct("int64"),
            _struct("sfixed32"),
            _struct("sfixed64"),
            _struct("sint32"),
            _struct("sint64"),
            _struct("string"),
            _struct("uint32"),
            _struct("uint64"),
            _struct("Bar"),
            _struct("Buz"),
            _struct("Empty"),
            _struct("Foo"),
            _enum("Thing"),
            _struct("Dep"),
            _enum("Dep2"),
            _struct("other.Other"),
            _struct("other.Other.Nested"),
        ],
        |s| s.label.clone(),
    );

    Ok(())
}

#[test]
fn test_import_discovery() -> pbls::Result<()> {
    // Test the following structure
    // tmp
    // ├── root.proto
    // ├── sibling.proto
    // ├── a
    // │   ├── a.txt
    // │   ├── a.proto
    // │   └── f
    // │       └── af.proto
    // ├── b
    // │   ├── c
    // │   │   ├── bc.proto
    // │   │   └── bc.txt
    // │   └── d
    // │       └── bd.txt
    // ├── e
    // │
    // └── loop
    //     └── loop -> ../loop

    let tmp = tempfile::tempdir()?;

    std::fs::create_dir_all(tmp.path().join("a/f"))?;
    std::fs::create_dir_all(tmp.path().join("b/c"))?;
    std::fs::create_dir_all(tmp.path().join("b/d"))?;
    std::fs::create_dir_all(tmp.path().join("e"))?;
    std::fs::create_dir_all(tmp.path().join("loop"))?;

    #[cfg(unix)]
    std::os::unix::fs::symlink(tmp.path().join("loop"), tmp.path().join("loop/loop"))?;

    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(tmp.path().join("loop"), tmp.path().join("loop/loop"))?;

    std::fs::write(
        &tmp.path().join("root.proto"),
        r#"
        syntax = "proto3"; 

        import "sibling.proto"; 
        import "a.proto"; 
        import "af.proto"; 
        import "bc.proto"; 

        message Root{
            Sibling s = 1;
            A a = 2;
            AF af = 3;
            BC bc = 4;
        }"#,
    )?;

    std::fs::write(
        &tmp.path().join("sibling.proto"),
        "syntax = \"proto3\"; message Sibling{}",
    )?;

    std::fs::write(&tmp.path().join("a/a.txt"), "not a proto")?;
    std::fs::write(
        &tmp.path().join("a/a.proto"),
        "syntax = \"proto3\"; message A{}",
    )?;
    std::fs::write(
        &tmp.path().join("a/f/af.proto"),
        "syntax = \"proto3\"; message AF{}",
    )?;

    std::fs::write(&tmp.path().join("b/c/bc.txt"), "not a proto")?;
    std::fs::write(
        &tmp.path().join("b/c/bc.proto"),
        "syntax = \"proto3\"; message BC{}",
    )?;

    std::fs::write(&tmp.path().join("b/d/bd.txt"), "not a proto")?;

    let root_uri = Url::from_file_path(&tmp.path().join("root.proto")).unwrap();
    let client = TestClient::new_with_root(&tmp)?;

    assert_eq!(
        client.open(root_uri.clone())?,
        PublishDiagnosticsParams {
            uri: root_uri,
            diagnostics: vec![],
            version: None,
        }
    );
    Ok(())
}
