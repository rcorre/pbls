use std::collections::hash_map;

use crate::file::{self};

use anyhow::{anyhow, Context, Result};
use lsp_types::{SymbolInformation, Url};
use regex::RegexBuilder;
use tree_sitter::QueryCursor;

const OPTIONS: &[&str] = &[
    "cc_enable_arenas",
    "cc_generic_services",
    "csharp_namespace",
    "deprecated",
    "features",
    "go_package",
    "java_generate_equals_and_hash",
    "java_generic_services",
    "java_multiple_files",
    "java_outer_classname",
    "java_package",
    "java_string_check_utf8",
    "objc_class_prefix",
    "optimize_for",
    "php_class_prefix",
    "php_metadata_namespace",
    "php_namespace",
    "py_generic_services",
    "ruby_package",
    "swift_prefix",
];

pub struct Workspace {
    proto_paths: Vec<std::path::PathBuf>,
    files: std::collections::HashMap<Url, file::File>,
}

// Return the possible package qualifiers to_pkg could use for a type imported from from_pkg
fn possible_qualifiers<'a>(from_pkg: &'a str, to_pkg: &'a str) -> Vec<&'a str> {
    log::trace!("possible_qualifiers({from_pkg}, {to_pkg})");
    if to_pkg == "" {
        return vec![from_pkg];
    }

    let mut res = vec![];
    if let Some(pkg) = from_pkg.strip_prefix(to_pkg) {
        let pkg = pkg.strip_prefix(".").unwrap_or(pkg);
        res.push(pkg);
    }

    if let Some((to_pkg, _)) = to_pkg.rsplit_once(".") {
        res.append(&mut possible_qualifiers(from_pkg, to_pkg));
    } else {
        res.push(from_pkg);
    }
    return res;
}

#[test]
fn test_possible_qualifiers() {
    let _ = env_logger::builder().is_test(true).try_init();
    assert_eq!(possible_qualifiers("", ""), vec![""]);
    assert_eq!(possible_qualifiers("foo", ""), vec!["foo"]);
    assert_eq!(possible_qualifiers("foo.bar", ""), vec!["foo.bar"]);
    assert_eq!(possible_qualifiers("foo", "bar"), vec!["foo"]);
    assert_eq!(possible_qualifiers("foo.bar", "bar"), vec!["foo.bar"]);
    assert_eq!(possible_qualifiers("foo", "foo"), vec!["", "foo"]);
    assert_eq!(
        possible_qualifiers("foo.bar", "foo"),
        vec!["bar", "foo.bar"]
    );
    assert_eq!(
        possible_qualifiers("foo.bar.baz", "foo.bar"),
        vec!["baz", "bar.baz", "foo.bar.baz",]
    );
    assert_eq!(
        possible_qualifiers("foo.bar.baz", "foo.bar.baz"),
        vec!["", "baz", "bar.baz", "foo.bar.baz",]
    );
    assert_eq!(
        possible_qualifiers("folder.stuff", "folder.what"),
        vec!["stuff", "folder.stuff"]
    );
}

impl Workspace {
    pub fn new(proto_paths: Vec<std::path::PathBuf>) -> Workspace {
        Workspace {
            proto_paths: proto_paths.clone(),
            files: hash_map::HashMap::new(),
        }
    }

    fn get(self: &Self, uri: &Url) -> Result<&file::File> {
        Ok(self
            .files
            .get(uri)
            .with_context(|| format!("File not loaded: {uri}"))?)
    }

    fn find_import(&self, name: &str) -> Option<std::path::PathBuf> {
        self.proto_paths
            .iter()
            .map(|dir| dir.join(name))
            .find(|path| path.exists())
    }

    // Open and parse an imported file if we haven't already
    fn open_import(&mut self, name: &str) -> Result<()> {
        let Some(path) = self.find_import(name) else {
            // TODO: Could generate not-found import diagnostic here, if we stop using protoc
            return Ok(());
        };

        let uri = Url::from_file_path(path.clone()).or(Err(anyhow!("Invalid path {path:?}")))?;
        if self.files.contains_key(&uri) {
            return Ok(()); // already parsed
        };

        let text = std::fs::read_to_string(path)?;
        let file = file::File::new(text)?;
        let mut qc = tree_sitter::QueryCursor::new();
        let imports = Vec::from_iter(file.imports(&mut qc).map(str::to_string));
        self.files.insert(uri, file);
        for import in imports {
            self.open_import(import.as_str())?;
        }
        Ok(())
    }

    pub fn open(&mut self, uri: Url, text: String) -> Result<Vec<lsp_types::Diagnostic>> {
        let file = file::File::new(text)?;
        let diags = file.diags(&uri, &self.proto_paths);

        let mut qc = tree_sitter::QueryCursor::new();
        let imports = Vec::from_iter(file.imports(&mut qc).map(str::to_string));

        self.files.insert(uri.clone(), file);

        for import in imports {
            self.open_import(import.as_str())?;
        }

        diags
    }

    pub fn save(&mut self, uri: Url) -> Result<Vec<lsp_types::Diagnostic>> {
        let file = self.get(&uri)?;
        file.diags(&uri, &self.proto_paths)
    }

    pub fn edit(
        &mut self,
        uri: &Url,
        changes: Vec<lsp_types::TextDocumentContentChangeEvent>,
    ) -> Result<()> {
        log::trace!("Editing {uri:?}");
        let file = self
            .files
            .get_mut(uri)
            .with_context(|| format!("File not loaded: {uri}"))?;
        file.edit(changes)?;

        let mut qc = tree_sitter::QueryCursor::new();
        let imports = Vec::from_iter(file.imports(&mut qc).map(str::to_string));

        for import in imports {
            log::trace!("Loading new import {import:?}");
            self.open_import(import.as_str())?;
        }

        Ok(())
    }

    pub fn symbols(&self, uri: &Url) -> Result<Vec<SymbolInformation>> {
        let mut qc = tree_sitter::QueryCursor::new();
        Ok(self
            .get(uri)?
            .symbols(&mut qc)
            .map(|s| to_lsp_symbol(uri.clone(), s))
            .collect())
    }

    fn load_all(&mut self) -> Result<()> {
        log::debug!("Loading all files");
        let paths = self
            .proto_paths
            .iter()
            .filter_map(|p| std::fs::read_dir(p).ok())
            .flatten()
            .filter_map(|p| p.ok())
            .map(|f| f.path())
            .filter(|p| p.is_file() && p.extension().map_or(false, |e| e == "proto"))
            .filter_map(|p| match std::fs::canonicalize(&p) {
                Ok(p) => Some(p),
                Err(err) => {
                    log::error!("Failed to open '{p:?}': {err:?}");
                    None
                }
            });

        for path in paths {
            log::debug!("Loading {path:?}");
            let uri = Url::from_file_path(&path).or(Err(anyhow!("Invalid path: {path:?}")))?;
            if let Some(file) = self.files.get(&uri) {
                file
            } else {
                let text = std::fs::read_to_string(uri.path())?;
                let file = file::File::new(text)?;
                self.files.insert(uri.clone(), file);
                self.files.get(&uri).unwrap()
            };
        }

        Ok(())
    }

    pub fn all_symbols(&mut self, query: &str) -> Result<Vec<SymbolInformation>> {
        self.load_all()?;

        let regexes: std::result::Result<Vec<_>, _> = query
            .split_whitespace()
            .map(|s| {
                RegexBuilder::new(
                    &s.chars()
                        .map(|c| c.to_string())
                        .collect::<Vec<_>>()
                        .join(".*"),
                )
                .case_insensitive(query.chars().all(|c| !c.is_uppercase()))
                .build()
            })
            .collect();
        let regexes = regexes?;
        log::debug!("Searching workspace symbols with patterns: {regexes:?}");

        let mut res = vec![];
        let mut qc = tree_sitter::QueryCursor::new();
        for (uri, file) in &self.files {
            let symbols = file.symbols(&mut qc);
            let syms = symbols
                .filter(|s| regexes.iter().all(|r| r.is_match(&s.name)))
                .map(|s| to_lsp_symbol(uri.clone(), s));
            res.extend(syms);
        }
        Ok(res)
    }

    pub fn complete(
        &self,
        uri: &Url,
        line: usize,
        character: usize,
    ) -> Result<Option<lsp_types::CompletionResponse>> {
        let file = self
            .files
            .get(uri)
            .with_context(|| format!("Completion requested on file with no tree for {uri}"))?;
        match file.completion_context(line, character)? {
            Some(file::CompletionContext::Message(msg)) => self.complete_types(&msg, file),
            Some(file::CompletionContext::Enum(_)) => Ok(None), // TODO
            Some(file::CompletionContext::Keyword) => Ok(complete_keywords()),
            Some(file::CompletionContext::Import) => self.complete_imports(uri),
            Some(file::CompletionContext::Option) => {
                Ok(Some(lsp_types::CompletionResponse::Array(
                    OPTIONS
                        .iter()
                        .map(|name| lsp_types::CompletionItem {
                            label: name.to_string(),
                            kind: Some(lsp_types::CompletionItemKind::TEXT),
                            ..Default::default()
                        })
                        .collect(),
                )))
            }
            Some(file::CompletionContext::Syntax) => {
                Ok(Some(lsp_types::CompletionResponse::Array(
                    [3, 2]
                        .iter()
                        .map(|n| lsp_types::CompletionItem {
                            label: format!("syntax = \"proto{n}\";"),
                            kind: Some(lsp_types::CompletionItemKind::TEXT),
                            ..Default::default()
                        })
                        .collect(),
                )))
            }
            None => Ok(None),
        }
    }

    // Return the relative paths of proto files under the given dir.
    pub fn goto(&self, uri: Url, pos: lsp_types::Position) -> Result<Option<lsp_types::Location>> {
        let file = self.get(&uri)?;
        let ctx = file.type_at(pos.line.try_into()?, pos.character.try_into()?);
        log::debug!("Finding definition for {ctx:?}");
        match ctx {
            None => Ok(None),
            Some(file::GotoContext::Type(typ)) => self.find_symbol(uri, file, &typ),
            Some(file::GotoContext::Import(name)) => {
                log::debug!("Looking up import {name:?}");
                Ok(self.find_import(name).map(|path| lsp_types::Location {
                    uri: Url::from_file_path(path).unwrap(),
                    range: lsp_types::Range::default(),
                }))
            }
        }
    }

    pub fn references(
        &mut self,
        params: lsp_types::ReferenceParams,
    ) -> Result<Option<Vec<lsp_types::Location>>> {
        self.load_all()?;

        let doc = params.text_document_position;
        let uri = &doc.text_document.uri;
        let file = self.get(&uri)?;

        let Some(item) = file.type_at(
            doc.position.line.try_into()?,
            doc.position.character.try_into()?,
        ) else {
            return Ok(None);
        };

        let mut res = Vec::new();
        match &item {
            file::GotoContext::Type(t) => {
                let src = self
                    .find_symbol(uri.clone(), file, &t)?
                    .with_context(|| format!("Symbol not found: {t:?}"))?;
                let src = self.get(&src.uri)?;
                let pkg = src.package();
                for (uri, file) in self.files.iter() {
                    res.extend(file.type_references(pkg, t).iter().map(|range| {
                        lsp_types::Location {
                            uri: uri.clone(),
                            range: to_lsp_range(*range),
                        }
                    }));
                }
            }
            file::GotoContext::Import(import) => {
                for (uri, file) in self.files.iter() {
                    res.extend(file.import_references(import).iter().map(|range| {
                        lsp_types::Location {
                            uri: uri.clone(),
                            range: to_lsp_range(*range),
                        }
                    }));
                }
            }
        };

        Ok(Some(res))
    }

    fn find_symbol(
        &self,
        uri: Url,
        file: &file::File,
        typ: &file::GotoTypeContext,
    ) -> Result<Option<lsp_types::Location>> {
        let mut qc = tree_sitter::QueryCursor::new();

        // First look within the file, qualifying the name if it is nested.
        if let Some(sym) = typ.parent.as_ref().and_then(|p| {
            let qualified = format!("{p}.{}", typ.name);
            log::trace!("Searching for {qualified} in {uri}");
            file.symbols(&mut qc).find(|sym| sym.name == qualified)
        }) {
            return Ok(Some(lsp_types::Location {
                uri,
                range: to_lsp_range(sym.range),
            }));
        }

        log::trace!("Searching for {} in {uri}", typ.name);
        // Next look within the file for the unqualified name.
        if let Some(sym) = file.symbols(&mut qc).find(|s| s.name == typ.name) {
            return Ok(Some(lsp_types::Location {
                uri,
                range: to_lsp_range(sym.range),
            }));
        };

        // If the type is nested, try the fully qualified name
        log::trace!("Searching for {} in {uri}", typ.name);
        let mut qc = tree_sitter::QueryCursor::new();
        if let Some(sym) = file.symbols(&mut qc).find(|s| s.name == typ.name) {
            return Ok(Some(lsp_types::Location {
                uri,
                range: to_lsp_range(sym.range),
            }));
        };

        // Next look within the file imports.
        let mut qc = tree_sitter::QueryCursor::new();
        let imports = file
            .imports(&mut qc)
            .filter_map(|name| self.find_import(name))
            .map(|path| Url::from_file_path(path).unwrap())
            .map(|uri| (uri.clone(), self.get(&uri).unwrap()));

        let mut qc = tree_sitter::QueryCursor::new();
        let local_package = file.package();
        for (uri, file) in imports {
            let package = file.package();
            if let Some(sym) = if package == local_package {
                log::trace!("Searching for {} in {uri} (same package)", typ.name);
                // same package, match the name without the package prefix
                file.symbols(&mut qc).find(|sym| sym.name == typ.name)
            } else if let Some(package) = package {
                log::trace!("Searching for {} in {uri} (different package)", typ.name);
                // different package, fully qualify the name
                let local_package = local_package.unwrap_or("");
                file.symbols(&mut qc).find(|sym| {
                    let quals = possible_qualifiers(package, local_package);
                    log::trace!("Qualifiers: {quals:?}");
                    quals
                        .iter()
                        .inspect(|q| log::trace!("Qual == {q}"))
                        .filter_map(|qual| typ.name.strip_prefix(qual))
                        .inspect(|q| log::trace!("stripped == {q}"))
                        .filter_map(|name| name.strip_prefix("."))
                        .inspect(|q| log::trace!("name == {q} == {}", sym.name))
                        .any(|name| name == sym.name)
                })
            } else {
                // target file has no package
                log::trace!("Searching for {} in {uri}", typ.name);
                file.symbols(&mut qc).find(|sym| sym.name == typ.name)
            } {
                return Ok(Some(lsp_types::Location {
                    uri,
                    range: to_lsp_range(sym.range),
                }));
            }
        }
        Ok(None)
    }

    fn complete_types(
        &self,
        base_name: &str,
        file: &file::File,
    ) -> Result<Option<lsp_types::CompletionResponse>> {
        let current_package = file.package();
        let mut qc = QueryCursor::new();
        let mut items: Vec<_> = file
            .relative_symbols(base_name, &mut qc)
            .map(to_lsp_completion)
            .collect();

        let imports = file
            .imports(&mut qc)
            .filter_map(|name| self.find_import(name))
            .map(|path| Url::from_file_path(path).unwrap())
            .map(|uri| self.get(&uri).unwrap());

        for file in imports {
            let package = file.package();
            if package.is_none() || package == current_package {
                let mut qc = tree_sitter::QueryCursor::new();
                items.extend(file.symbols(&mut qc).map(to_lsp_completion));
            } else if let Some(package) = package {
                let mut qc = tree_sitter::QueryCursor::new();
                items.extend(
                    file.symbols(&mut qc)
                        .map(|s| file::Symbol {
                            name: package.to_owned() + "." + &s.name,
                            ..s
                        })
                        .map(to_lsp_completion),
                );
            }
        }

        let builtins = [
            "bool", "bytes", "double", "fixed32", "fixed64", "float", "int32", "int64", "sfixed32",
            "sfixed64", "sint32", "sint64", "string", "uint32", "uint64",
        ]
        .map(|s| lsp_types::CompletionItem {
            label: s.to_string(),
            kind: Some(lsp_types::CompletionItemKind::STRUCT),
            ..Default::default()
        });
        items.extend(builtins);

        let keywords = [
            "enum", "extend", "import", "message", "oneof", "option", "optional", "package",
            "repeated", "reserved", "returns", "rpc", "service", "stream", "map",
        ]
        .map(|s| lsp_types::CompletionItem {
            label: s.to_string(),
            kind: Some(lsp_types::CompletionItemKind::KEYWORD),
            ..Default::default()
        });
        items.extend(keywords);

        Ok(Some(lsp_types::CompletionResponse::Array(items)))
    }

    fn complete_imports(
        &self,
        url: &lsp_types::Url,
    ) -> Result<Option<lsp_types::CompletionResponse>> {
        log::debug!("Completing imports for {url:?}");

        let current = std::path::Path::new(url.path())
            .file_name()
            .with_context(|| format!("Invalid path: {url}"))?
            .to_str()
            .with_context(|| format!("Invalid path: {url}"))?;

        let file = self
            .files
            .get(url)
            .with_context(|| format!("File not loaded: {url}"))?;
        let mut qc = tree_sitter::QueryCursor::new();
        let existing = file
            .imports(&mut qc)
            .chain(std::iter::once(current))
            .collect::<Vec<_>>();

        log::trace!("Excluding existing imports: {existing:?}");

        let items = self
            .proto_paths
            .iter()
            .map(|p| find_protos(p.as_path()))
            .flat_map(|p| {
                p.iter()
                    .filter(|s| !existing.contains(&s.as_str()))
                    .map(|s| lsp_types::CompletionItem {
                        insert_text: Some(format!("{}\";", s)),
                        label: s.to_owned(),
                        label_details: None,
                        kind: Some(lsp_types::CompletionItemKind::FILE),
                        ..Default::default()
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        Ok(Some(lsp_types::CompletionResponse::Array(items)))
    }
}

fn find_protos(dir: &std::path::Path) -> Vec<String> {
    let mut res = vec![];
    let entries = match std::fs::read_dir(dir) {
        Ok(ok) => ok,
        Err(err) => {
            log::warn!("Failed to read dir {dir:?}: {err:?}");
            return res;
        }
    };
    log::trace!("Finding imports under {dir:?}");
    for path in entries {
        let path = match path {
            Ok(ok) => ok,
            Err(err) => {
                log::warn!("Failed to read dir {dir:?}: {err:?}");
                continue;
            }
        };

        let meta = match path.metadata() {
            Ok(ok) => ok,
            Err(err) => {
                log::warn!("Failed to read dir {dir:?}: {err:?}");
                continue;
            }
        };

        if meta.is_dir() {
            let dir = dir.join(path.path());
            let protos = find_protos(dir.as_path());
            let root = &path.file_name();
            let root = std::path::PathBuf::from(root);
            res.extend(
                protos
                    .iter()
                    .filter_map(|p| root.join(p).to_str().map(str::to_string)),
            );
            continue;
        }

        if !meta.is_file() {
            continue;
        }

        let name = &path.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };

        if !name.ends_with(".proto") {
            continue;
        }

        log::trace!("Found import {name:?}");
        res.push(name.to_string())
    }
    res
}

fn complete_keywords() -> Option<lsp_types::CompletionResponse> {
    let items = ["message", "enum", "import", "option"]
        .iter()
        .map(|s| lsp_types::CompletionItem {
            label: s.to_string(),
            kind: Some(lsp_types::CompletionItemKind::KEYWORD),
            ..Default::default()
        });
    Some(lsp_types::CompletionResponse::Array(items.collect()))
}

fn to_lsp_pos(p: tree_sitter::Point) -> lsp_types::Position {
    lsp_types::Position {
        line: p.row.try_into().unwrap(),
        character: p.column.try_into().unwrap(),
    }
}

fn to_lsp_range(r: tree_sitter::Range) -> lsp_types::Range {
    lsp_types::Range {
        start: to_lsp_pos(r.start_point),
        end: to_lsp_pos(r.end_point),
    }
}

fn to_lsp_symbol(uri: Url, sym: file::Symbol) -> lsp_types::SymbolInformation {
    // deprecated field is deprecated, but cannot be omitted
    #[allow(deprecated)]
    lsp_types::SymbolInformation {
        name: sym.name,
        kind: match sym.kind {
            file::SymbolKind::Enum => lsp_types::SymbolKind::ENUM,
            file::SymbolKind::Message => lsp_types::SymbolKind::STRUCT,
        },
        tags: None,
        deprecated: None,
        location: lsp_types::Location {
            uri,
            range: lsp_types::Range {
                start: to_lsp_pos(sym.range.start_point),
                end: to_lsp_pos(sym.range.end_point),
            },
        },
        container_name: None,
    }
}

fn to_lsp_completion(sym: file::Symbol) -> lsp_types::CompletionItem {
    lsp_types::CompletionItem {
        label: sym.name,
        kind: Some(match sym.kind {
            file::SymbolKind::Enum => lsp_types::CompletionItemKind::ENUM,
            file::SymbolKind::Message => lsp_types::CompletionItemKind::STRUCT,
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn setup() -> (Workspace, tempfile::TempDir) {
        let _ = env_logger::builder().is_test(true).try_init();
        let tmp = tempfile::tempdir().unwrap();
        (Workspace::new(vec![tmp.path().into()]), tmp)
    }

    fn proto(dir: impl AsRef<std::path::Path>, path: &str, lines: &[&str]) -> (Url, String) {
        let path = dir.as_ref().join(path);
        let text = lines.join("\n") + "\n";
        std::fs::write(&path, &text).unwrap();
        (Url::from_file_path(path).unwrap(), text)
    }

    #[test]
    fn test_open_loop() {
        let (mut ws, tmp) = setup();
        let (uri, text) = proto(
            &tmp,
            "foo.proto",
            &["syntax = \"proto3\";", "import \"bar.proto\";"],
        );
        proto(
            &tmp,
            "bar.proto",
            &["syntax = \"proto3\";", "import \"bar.proto\";"],
        );

        ws.open(uri.clone(), text).unwrap();
    }

    #[test]
    fn test_complete_syntax() {
        let _ = env_logger::builder().is_test(true).try_init();
        let mut ws = Workspace::new(vec![]);
        let uri = Url::from_file_path(std::env::temp_dir().join("foo.proto")).unwrap();
        ws.open(uri.clone(), "".into()).unwrap();
        assert_eq!(
            ws.complete(&uri, 0, 0).unwrap().unwrap(),
            lsp_types::CompletionResponse::Array(vec![
                lsp_types::CompletionItem {
                    label: "syntax = \"proto3\";".into(),
                    kind: Some(lsp_types::CompletionItemKind::TEXT),
                    ..Default::default()
                },
                lsp_types::CompletionItem {
                    label: "syntax = \"proto2\";".into(),
                    kind: Some(lsp_types::CompletionItemKind::TEXT),
                    ..Default::default()
                }
            ])
        );
    }

    #[test]
    fn test_complete_import() {
        let (mut ws, tmp) = setup();
        let (uri, text) = proto(
            &tmp,
            "foo.proto",
            &["syntax = \"proto3\";", "import \"bar.proto\";", "import \""],
        );
        proto(&tmp, "bar.proto", &["syntax = \"proto3\";"]);
        proto(&tmp, "baz.proto", &["syntax = \"proto3\";"]);

        ws.open(uri.clone(), text).unwrap();
        assert_eq!(
            ws.complete(&uri, 2, "import \"".len()).unwrap().unwrap(),
            lsp_types::CompletionResponse::Array(vec![lsp_types::CompletionItem {
                label: "baz.proto".into(),
                kind: Some(lsp_types::CompletionItemKind::FILE),
                insert_text: Some("baz.proto\";".into()),
                ..Default::default()
            },])
        );
    }

    #[test]
    fn test_complete_nested_import() {
        let (mut ws, tmp) = setup();
        let (uri, text) = proto(&tmp, "foo.proto", &["syntax = \"proto3\";", "import \""]);
        proto(&tmp, "bar.proto", &["syntax = \"proto3\";"]);

        let subdir = tmp.path().join("subdir");
        let subdir = subdir.as_path();
        std::fs::create_dir(subdir).unwrap();
        proto(subdir, "baz.proto", &["syntax = \"proto3\";"]);

        ws.open(uri.clone(), text).unwrap();
        assert_eq!(
            ws.complete(&uri, 1, "import \"".len()).unwrap().unwrap(),
            lsp_types::CompletionResponse::Array(vec![
                lsp_types::CompletionItem {
                    label: "bar.proto".into(),
                    kind: Some(lsp_types::CompletionItemKind::FILE),
                    insert_text: Some("bar.proto\";".into()),
                    ..Default::default()
                },
                lsp_types::CompletionItem {
                    label: "subdir/baz.proto".into(),
                    kind: Some(lsp_types::CompletionItemKind::FILE),
                    insert_text: Some("subdir/baz.proto\";".into()),
                    ..Default::default()
                },
            ])
        );
    }

    #[test]
    fn test_complete_options() {
        let (mut ws, tmp) = setup();
        let (uri, text) = proto(
            &tmp,
            "foo.proto",
            &["syntax = \"proto3\";", "import \"bar.proto\";", "option j"],
        );
        proto(&tmp, "bar.proto", &["syntax = \"proto3\";"]);
        proto(&tmp, "baz.proto", &["syntax = \"proto3\";"]);

        ws.open(uri.clone(), text).unwrap();
        assert_eq!(
            ws.complete(&uri, 2, "option j".len()).unwrap().unwrap(),
            lsp_types::CompletionResponse::Array(
                OPTIONS
                    .iter()
                    .map(|name| {
                        lsp_types::CompletionItem {
                            label: name.to_string(),
                            kind: Some(lsp_types::CompletionItemKind::TEXT),
                            ..Default::default()
                        }
                    })
                    .collect()
            )
        );
    }

    #[test]
    fn test_goto_import() {
        let (mut ws, tmp) = setup();
        let (foo_uri, text) = proto(
            &tmp,
            "foo.proto",
            &[
                "syntax = \"proto3\";",
                "import \"bar.proto\";",
                "import \"baz.proto\";",
                "import \"biz.proto\";",
            ],
        );
        let (bar_uri, _) = proto(&tmp, "bar.proto", &["syntax = \"proto3\";"]);
        let (baz_uri, _) = proto(&tmp, "baz.proto", &["syntax = \"proto3\";"]);

        ws.open(foo_uri.clone(), text).unwrap();

        assert_eq!(
            ws.goto(
                foo_uri.clone(),
                lsp_types::Position {
                    line: 1,
                    character: "import \"bar.".len().try_into().unwrap(),
                }
            )
            .unwrap(),
            Some(lsp_types::Location {
                uri: bar_uri,
                range: lsp_types::Range::default(),
            })
        );

        assert_eq!(
            ws.goto(
                foo_uri.clone(),
                lsp_types::Position {
                    line: 2,
                    character: "import \"baz".len().try_into().unwrap(),
                }
            )
            .unwrap(),
            Some(lsp_types::Location {
                uri: baz_uri,
                range: lsp_types::Range::default(),
            })
        );

        assert_eq!(
            ws.goto(
                foo_uri,
                lsp_types::Position {
                    line: 3,
                    character: "import \"biz".len().try_into().unwrap(),
                }
            )
            .unwrap(),
            None,
        );
    }

    #[test]
    fn test_goto_type() {
        let (mut ws, tmp) = setup();
        let (foo_uri, text) = proto(
            &tmp,
            "foo.proto",
            &[
                "syntax = \"proto3\";",        // 0
                "package main;",               // 1
                "import \"bar.proto\";",       // 2
                "import \"baz.proto\";",       // 3
                "message One {",               // 4
                "message Two {",               // 5
                "enum Three {}",               // 6
                "}",                           // 7
                "Two.Three tt = 1;",           // 8
                "}",                           // 9
                "message Stuff {",             // 10
                "One one = 1;",                // 11
                "One.Two two = 2;",            // 12
                "One.Two.Three three = 3;",    // 13
                "Two nope = 4;",               // 14
                "bar.One bar_one = 5;",        // 15
                "bar.One.Two b1 = 6;",         // 16
                "bar.One.Two.Three b123 = 7;", // 17
                "baz.buz.Baz bazbuz = 8;",     // 18
                "}",                           // 19
            ],
        );
        let (bar_uri, _) = proto(
            &tmp,
            "bar.proto",
            &[
                "syntax = \"proto3\";", // 0
                "package bar;",         // 1
                "message One",          // 2
                "message One {",        // 3
                "message Two {",        // 4
                "enum Three {",         // 5
                "}",                    // 6
                "}",                    // 7
                "}",                    // 8
            ],
        );
        let (baz_uri, _) = proto(
            &tmp,
            "baz.proto",
            &[
                "syntax = \"proto3\";", // 0
                "package baz.buz;",     // 1
                "message Baz{}",        // 2
            ],
        );

        ws.open(foo_uri.clone(), text).unwrap();

        assert_eq!(
            ws.goto(
                foo_uri.clone(),
                lsp_types::Position {
                    line: 8,
                    character: "Two.Th".len().try_into().unwrap(),
                }
            )
            .unwrap(),
            Some(lsp_types::Location {
                uri: foo_uri.clone(),
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 6,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 6,
                        character: 13,
                    },
                },
            })
        );

        assert_eq!(
            ws.goto(
                foo_uri.clone(),
                lsp_types::Position {
                    line: 11,
                    character: 0,
                }
            )
            .unwrap(),
            Some(lsp_types::Location {
                uri: foo_uri.clone(),
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 4,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 9,
                        character: 1,
                    },
                },
            })
        );

        assert_eq!(
            ws.goto(
                foo_uri.clone(),
                lsp_types::Position {
                    line: 12,
                    character: "One.".len().try_into().unwrap(),
                }
            )
            .unwrap(),
            Some(lsp_types::Location {
                uri: foo_uri.clone(),
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 5,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 7,
                        character: 1,
                    },
                },
            })
        );

        assert_eq!(
            ws.goto(
                foo_uri.clone(),
                lsp_types::Position {
                    line: 13,
                    character: "One.Two.T".len().try_into().unwrap(),
                }
            )
            .unwrap(),
            Some(lsp_types::Location {
                uri: foo_uri.clone(),
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 6,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 6,
                        character: 13,
                    },
                },
            })
        );

        assert_eq!(
            ws.goto(
                foo_uri.clone(),
                lsp_types::Position {
                    line: 15,
                    character: 0,
                }
            )
            .unwrap(),
            Some(lsp_types::Location {
                uri: bar_uri.clone(),
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 3,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 8,
                        character: 1,
                    },
                },
            })
        );

        assert_eq!(
            ws.goto(
                foo_uri.clone(),
                lsp_types::Position {
                    line: 16,
                    character: 0,
                }
            )
            .unwrap(),
            Some(lsp_types::Location {
                uri: bar_uri.clone(),
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 4,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 7,
                        character: 1,
                    },
                },
            })
        );

        assert_eq!(
            ws.goto(
                foo_uri.clone(),
                lsp_types::Position {
                    line: 17,
                    character: 0,
                }
            )
            .unwrap(),
            Some(lsp_types::Location {
                uri: bar_uri.clone(),
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 5,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 6,
                        character: 1,
                    },
                },
            })
        );

        assert_eq!(
            ws.goto(
                foo_uri.clone(),
                lsp_types::Position {
                    line: 18,
                    character: 2,
                }
            )
            .unwrap(),
            Some(lsp_types::Location {
                uri: baz_uri.clone(),
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 2,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 2,
                        character: 13,
                    },
                },
            })
        );

        assert_eq!(
            ws.goto(
                foo_uri.clone(),
                lsp_types::Position {
                    line: 14,
                    character: 0,
                }
            )
            .unwrap(),
            None,
        );
    }
}
