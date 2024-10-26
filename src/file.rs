use std::sync::OnceLock;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn language() -> tree_sitter::Language {
    static LANGUAGE: OnceLock<tree_sitter::Language> = OnceLock::new();
    *LANGUAGE.get_or_init(|| tree_sitter_protobuf::language())
}

#[derive(Debug, PartialEq)]
pub enum SymbolKind {
    Message,
    Enum,
}

#[derive(Debug, PartialEq)]
pub struct Symbol {
    pub kind: SymbolKind,
    pub name: String,
    pub range: tree_sitter::Range,
}

#[derive(Debug, PartialEq)]
pub enum CompletionContext<'a> {
    Message(&'a str),
    Enum(&'a str),
    Import,
    Keyword,
    Syntax,
    Option,
}

#[derive(Debug, PartialEq)]
pub struct GotoTypeContext<'a> {
    pub name: &'a str,
    pub parent: Option<String>,
}

#[derive(Debug, PartialEq)]
pub enum GotoContext<'a> {
    Type(GotoTypeContext<'a>),
    Import(&'a str),
}

pub struct File {
    tree: tree_sitter::Tree,
    text: String,
}

impl File {
    pub fn new(text: String) -> Result<File> {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(language())
            .expect("Error loading proto language");

        let tree = parser.parse(&text, None).ok_or("Parse failed")?;
        log::trace!("Parsed: {}", tree.root_node().to_sexp());
        Ok(File { tree, text })
    }

    pub fn edit(&mut self, changes: Vec<lsp_types::TextDocumentContentChangeEvent>) -> Result<()> {
        for change in changes {
            let range = change
                .range
                .ok_or("No range in change notification {change:?}")?;
            let mut lines = self.text.split_inclusive("\n").peekable();
            // First count bytes in all lines preceding the edit.
            let start_byte = lines
                .by_ref()
                .take(range.start.line.try_into()?)
                .map(str::len)
                .sum::<usize>();
            // Now add bytes up to the character within the start line.
            let start_offset = lines
                .peek()
                .map(|line| char_to_byte(&line, range.start.character))
                .unwrap_or(0);
            let start_byte = start_byte + start_offset;
            // Now count bytes in all lines following the edit.
            let end_byte = start_byte
                + lines
                    .by_ref()
                    .take((range.end.line - range.start.line).try_into()?)
                    .map(str::len)
                    .sum::<usize>();
            // Now add bytes up to the character within the end line.
            let end_offset = lines
                .peek()
                .map(|line| char_to_byte(&line, range.end.character))
                .unwrap_or(0);
            let end_byte = end_byte + end_offset - start_offset;

            log::trace!(
                "Computing change {start_byte}..{end_byte} with text {}",
                change.text
            );

            self.text.replace_range(start_byte..end_byte, &change.text);
        }
        log::trace!("Edited text to: {}", self.text);

        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(language())
            .expect("Error loading proto language");
        self.tree = parser.parse(&self.text, None).ok_or("Parse failed")?;
        log::trace!("Edited tree to: {}", self.tree.root_node().to_sexp());

        Ok(())
    }

    fn get_text(&self, node: tree_sitter::Node) -> &str {
        node.utf8_text(self.text.as_bytes()).unwrap()
    }

    pub fn text(&self) -> &str {
        self.text.as_str()
    }

    pub fn package(&self) -> Option<&str> {
        static QUERY: OnceLock<tree_sitter::Query> = OnceLock::new();
        let query = QUERY.get_or_init(|| {
            tree_sitter::Query::new(language(), "(package (fullIdent (ident)) @id)").unwrap()
        });

        let mut qc = tree_sitter::QueryCursor::new();
        let res = qc
            .matches(&query, self.tree.root_node(), self.text.as_bytes())
            .next()
            .map(|m| m.captures[0].node)
            .map(|n| self.get_text(n))
            .map(|s| s.trim_matches('"'));
        res
    }

    pub fn imports<'this: 'cursor, 'cursor>(
        &'this self,
        qc: &'cursor mut tree_sitter::QueryCursor,
    ) -> impl Iterator<Item = &'this str> + 'cursor {
        static QUERY: OnceLock<tree_sitter::Query> = OnceLock::new();
        let query = QUERY.get_or_init(|| {
            tree_sitter::Query::new(language(), "(import (strLit) @path)").unwrap()
        });

        qc.matches(&query, self.tree.root_node(), self.text.as_bytes())
            .map(|m| m.captures[0].node)
            .map(|n| self.get_text(n))
            .map(|s| s.trim_matches('"'))
    }

    pub fn symbols<'this: 'cursor, 'cursor>(
        &'this self,
        qc: &'cursor mut tree_sitter::QueryCursor,
    ) -> impl Iterator<Item = Symbol> + 'cursor {
        static QUERY: OnceLock<tree_sitter::Query> = OnceLock::new();
        let query = QUERY.get_or_init(|| {
            tree_sitter::Query::new(
                language(),
                "[
                     (message (messageName (ident) @id))
                     (enum (enumName (ident) @id))
                 ] @def",
            )
            .unwrap()
        });

        qc.matches(&query, self.tree.root_node(), self.text.as_bytes())
            .map(|m| (m.captures[0].node, m.captures[1].node))
            .map(|(def, id)| {
                let name = self.get_text(id);
                let name = if let Some(p) = self.parent_name(def) {
                    p + "." + name
                } else {
                    name.to_string()
                };
                Symbol {
                    kind: match def.kind() {
                        "message" => SymbolKind::Message,
                        _ => SymbolKind::Enum,
                    },
                    name,
                    range: def.range(),
                }
            })
    }

    // Return all symbols adjusted relative to a message.
    // For example, given base_name=Foo.Bar:
    // symbols()          -> [Foo, Foo.Bar, Foo.Bar.Baz, Foo.Bar.Baz.Biz]
    // relative_symbols() -> [Foo, Bar    , Baz        , Baz.Biz]
    pub fn relative_symbols<'this: 'cursor, 'cursor>(
        &'this self,
        base_name: &'this str,
        qc: &'cursor mut tree_sitter::QueryCursor,
    ) -> impl Iterator<Item = Symbol> + 'cursor {
        self.symbols(qc).map(|s| Symbol {
            name: relative_name(base_name, &s.name),
            ..s
        })
    }

    // Given an "ident" or "enumMessageType", node representing a type, find the name of the type.
    fn field_type(&self, node: Option<tree_sitter::Node>) -> Option<&str> {
        log::trace!("Finding type of {node:?}");
        match node {
            None => None,
            Some(n) if n.kind() == "type" => Some(self.get_text(n)),
            Some(n) => self.field_type(n.parent()),
        }
    }

    fn parent_context(&self, node: Option<tree_sitter::Node>) -> Option<CompletionContext> {
        log::trace!(
            "Checking parent context: {node:?} - {}",
            node.map_or("".into(), |n| n.to_sexp())
        );
        match node {
            // Hit the document root
            None => None,
            // Don't complete if we're typing a field name or number
            Some(n) if n.kind() == "fieldName" => None,
            Some(n) if n.kind() == "enumBody" => n
                .parent() // enum
                .and_then(|p| self.type_name(p))
                .and_then(|n| Some(CompletionContext::Enum(n))),
            Some(n) if n.kind() == "messageBody" => n
                .parent() // message
                .and_then(|p| self.type_name(p))
                .and_then(|n| Some(CompletionContext::Message(n))),
            Some(n) => self.parent_context(n.parent()),
        }
    }

    pub fn completion_context(
        self: &Self,
        row: usize,
        col: usize,
    ) -> Result<Option<CompletionContext>> {
        if self.tree.root_node().kind() != "source_file" {
            // If the whole document is invalid, we need to define a syntax.
            return Ok(Some(CompletionContext::Syntax));
        }

        let pos = tree_sitter::Point {
            row: row.try_into().unwrap(),
            // Generally, the node before the cursor is more interesting for context.
            column: (col.checked_sub(1).unwrap_or(0)).try_into()?,
        };
        let node = self
            .tree
            .root_node()
            .named_descendant_for_point_range(pos, pos)
            .ok_or(format!("No descendant for range {pos:?}"))?;

        log::debug!(
            "Getting completion context for node={node:?} parent={:?}",
            node.parent(),
        );

        log::trace!(
            "Getting completion context for node text: {}",
            self.get_text(node),
        );

        Ok(if node.kind() == "option" {
            // option | -> (option)
            Some(CompletionContext::Option)
        } else if is_sexp(node, &["optionName", "fullIdent", "ident"]) {
            // option c| -> (option (optionName (fullIdent (ident))))
            Some(CompletionContext::Option)
        } else if node.is_error() && self.get_text(node).starts_with("option ") {
            // option | -> (ERROR)
            Some(CompletionContext::Option)
        } else if node
            .parent()
            .is_some_and(|p| p.is_error() && self.get_text(p).starts_with("option "))
        {
            // option | -> (ERROR)
            Some(CompletionContext::Option)
        } else if node.is_error() && self.get_text(node).starts_with("import ") {
            // import "| -> (ERROR)
            Some(CompletionContext::Import)
        } else if is_sexp(node, &["import", "strLit"]) {
            // import "foo|.proto" -> (import (strLit))
            Some(CompletionContext::Import)
        } else if (node.kind() == "ident" || node.kind() == "type")
            && !node.parent().is_some_and(|p| p.kind() == "oneofName")
        {
            // message Foo { Bar| -> (ident)
            // message Foo { string| -> (type (string))
            self.parent_context(Some(node))
        } else if is_top_level_error(node) {
            // typically means we're typing the first word of a line
            // mes| -> (source_file (ERROR (ERROR)))
            Some(CompletionContext::Keyword)
        } else if node.kind() == "source_file" {
            // NOTE: Not very efficient, but we're in a difficult spot here.
            let line: String = self
                .text
                .lines()
                .skip(row)
                .next()
                .ok_or(format!("Line {row} out of range"))?
                .chars()
                .take(col)
                .collect();
            let line = line.trim_start();

            log::trace!("Checking keyword completion for line {line}");

            if line.starts_with("option ") {
                Some(CompletionContext::Option)
            } else if line.split(char::is_whitespace).count() <= 1 {
                // first word of the line
                Some(CompletionContext::Keyword)
            } else {
                None
            }
        } else {
            None
        })
    }

    pub fn references(self: &Self, item: &GotoContext) -> Vec<tree_sitter::Range> {
        match item {
            GotoContext::Type(typ) => {
                static QUERY: OnceLock<tree_sitter::Query> = OnceLock::new();
                let typ = typ.name;
                let pkg = self.package();
                let query = QUERY.get_or_init(|| {
                    tree_sitter::Query::new(language(), "(field (type) @name)").unwrap()
                });
                log::trace!("Searching for references to {typ} in package {pkg:?}");

                let mut qc = tree_sitter::QueryCursor::new();
                qc.matches(&query, self.tree.root_node(), self.text.as_bytes())
                    .map(|m| m.captures[0].node)
                    .inspect(|x| log::trace!("Check {x:?}: {} == {typ}", self.get_text(*x)))
                    .filter(|node| {
                        let text = self.get_text(*node);
                        // first check the fully qualified name
                        text == typ
                            || match pkg {
                                // If we have a package, try stripping that, in case we're in the same package
                                Some(pkg) => {
                                    log::trace!(
                                        "Trying to match '{typ}' to '{text}' without package {pkg}"
                                    );
                                    typ.strip_prefix(pkg)
                                        .and_then(|typ| typ.strip_prefix("."))
                                        .map(|target| target == text)
                                        .unwrap_or(false)
                                }
                                None => false,
                            }
                    })
                    .map(|node| node.range())
                    .collect()
            }
            GotoContext::Import(name) => {
                static QUERY: OnceLock<tree_sitter::Query> = OnceLock::new();
                let query = QUERY.get_or_init(|| {
                    tree_sitter::Query::new(language(), "(import (strLit) @name)").unwrap()
                });

                let mut qc = tree_sitter::QueryCursor::new();
                qc.matches(&query, self.tree.root_node(), self.text.as_bytes())
                    .map(|m| m.captures[0].node)
                    .filter(|n| self.get_text(*n).trim_matches('"') == *name)
                    .map(|n| n.range())
                    .collect()
            }
        }
    }

    pub fn type_at(self: &Self, row: usize, col: usize) -> Option<GotoContext> {
        log::trace!("Getting type at row: {row} col: {col}");

        let pos = tree_sitter::Point {
            row: row.try_into().unwrap(),
            column: col.try_into().unwrap(),
        };
        let node = self
            .tree
            .root_node()
            .named_descendant_for_point_range(pos, pos)?;

        log::debug!("Getting type at node: {node:?} parent: {:?}", node.parent());

        if node.kind() == "strLit" && node.parent().is_some_and(|p| p.kind() == "import") {
            return Some(GotoContext::Import(self.get_text(node).trim_matches('"')));
        }

        // Cursor is over a message or enum name.
        if is_sexp(node, &["enum", "enumName", "ident"])
            || is_sexp(node, &["message", "messageName", "ident"])
        {
            return Some(GotoContext::Type(GotoTypeContext {
                name: self.get_text(node.parent().unwrap()),
                parent: None,
            }));
        }

        // Cursor is over a field type.
        if node.kind() == "ident" || node.kind() == "enumMessageType" {
            if let Some(name) = self.field_type(Some(node)) {
                return Some(GotoContext::Type(GotoTypeContext {
                    name,
                    parent: self.parent_name(node),
                }));
            }
        }

        None
    }

    fn parent_name(&self, node: tree_sitter::Node) -> Option<String> {
        log::trace!("Finding parent name for {node:?}");
        let mut node = node;
        let mut res = Vec::<&str>::new();
        loop {
            if let Some(parent) = node.parent() {
                if parent.kind() == "message" {
                    let name = self.type_name(parent);
                    log::trace!("Appending parent name {name:?}");
                    name.map(|n| res.push(n));
                }
                node = parent;
            } else {
                break;
            }
        }
        if res.is_empty() {
            None
        } else {
            res.reverse();
            Some(res.join("."))
        }
    }

    // Get the name of a Enum or Message node.
    fn type_name(&self, node: tree_sitter::Node) -> Option<&str> {
        debug_assert!(
            node.kind() == "enum" || node.kind() == "message",
            "{node:?}"
        );
        let mut cursor = node.walk();
        let child = node
            .named_children(&mut cursor)
            .find(|c| c.kind() == "messageName" || c.kind() == "enumName");
        child.and_then(|c| c.utf8_text(self.text.as_bytes()).ok())
    }
}

fn is_top_level_error(node: tree_sitter::Node) -> bool {
    if node.is_error() || node.is_missing() {
        match node.parent() {
            None => true,
            Some(n) if n.kind() == "source_file" => true,
            Some(n) if n.is_error() || n.is_missing() => is_top_level_error(n),
            _ => false,
        }
    } else {
        false
    }
}

// Find the shortest form of a type name relative to a message
// relative_name("Foo", "Foo.Bar.Baz") -> "Bar.Baz"
// relative_name("Foo.Bar", "Foo.Bar.Baz") -> "Baz"
// relative_name("Foo.Bar", "Foo.Bar") -> "Bar"
// relative_name("Foo.Bar", "Foo") -> "Foo"
fn relative_name<'a>(message: &str, name: &'a str) -> String {
    let prefix = name
        .split(".")
        .zip(message.split("."))
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a)
        .collect::<Vec<_>>()
        .join(".");

    if prefix.len() == name.len() {
        let Some((_, name)) = name.rsplit_once(".") else {
            return name.to_string();
        };
        name.to_string()
    } else {
        name.strip_prefix(prefix.as_str())
            .unwrap_or(name)
            .strip_prefix(".")
            .unwrap_or(name)
            .to_string()
    }
}

fn is_sexp(node: tree_sitter::Node, sexp: &[&str]) -> bool {
    let Some((kind, rest)) = sexp.split_last() else {
        return true; // got to end, whole sexp matched
    };

    if &node.kind() != kind {
        return false;
    }

    if let Some(parent) = node.parent() {
        is_sexp(parent, rest)
    } else {
        false
    }
}

fn char_to_byte(line: &str, char: u32) -> usize {
    line.chars()
        .take(char.try_into().unwrap())
        .map(|c| c.len_utf8())
        .sum()
}

#[cfg(test)]
mod tests {
    use tree_sitter::Point;

    use super::*;
    use pretty_assertions::assert_eq;

    // Takes a string with '|' characters representing cursors.
    // Builds a file from the string with '|' removed, and returns the positions of the cursors.
    fn cursors(text: &str) -> (File, Vec<Point>) {
        let cursors = text
            .lines()
            .enumerate()
            .flat_map(|(row, line)| {
                line.match_indices('|')
                    .enumerate()
                    .map(move |(i, (column, _))| Point {
                        row,
                        // subtract 1 for each '|' before in this row,
                        // as those offset the position of following '|'
                        column: column - i,
                    })
            })
            .collect();
        (File::new(text.replace('|', "")).unwrap(), cursors)
    }

    // Like cursors, but expect exactly one |
    fn cursor(text: &str) -> (File, Point) {
        let cursor = text
            .lines()
            .enumerate()
            .flat_map(|(row, line)| {
                line.match_indices('|')
                    .enumerate()
                    .map(move |(i, (column, _))| Point {
                        row,
                        // subtract 1 for each '|' before in this row,
                        // as those offset the position of following '|'
                        column: column - i,
                    })
            })
            .next()
            .unwrap();
        (File::new(text.replace('|', "")).unwrap(), cursor)
    }

    #[test]
    fn test_package() {
        let _ = env_logger::builder().is_test(true).try_init();
        let text = r#"syntax="proto3"; package main;"#;
        let file = File::new(text.to_string()).unwrap();
        assert_eq!(file.package(), Some("main".into()));

        let text = r#"syntax="proto3"; package main; package other"#;
        let file = File::new(text.to_string()).unwrap();
        assert_eq!(file.package(), Some("main".into()));

        let text = r#"syntax="proto3"; package foo.bar.baz;"#;
        let file = File::new(text.to_string()).unwrap();
        assert_eq!(file.package(), Some("foo.bar.baz".into()));

        let text = r#"syntax="proto3";"#;
        let file = File::new(text.to_string()).unwrap();
        assert_eq!(file.package(), None);
    }

    #[test]
    fn test_imports() {
        let _ = env_logger::builder().is_test(true).try_init();
        let text = r#"
            syntax="proto3";
            package main;
            import "foo.proto";
            import "bar.proto";
            import "ba
        "#;
        let file = File::new(text.to_string()).unwrap();
        let mut qc = tree_sitter::QueryCursor::new();
        assert_eq!(
            file.imports(&mut qc).collect::<Vec<_>>(),
            vec!["foo.proto", "bar.proto"]
        );
    }

    #[test]
    fn test_symbols() {
        let _ = env_logger::builder().is_test(true).try_init();
        let text = r#"
            syntax="proto3"; 
            package main;
            message Foo{}
            enum Bar{}
            message Baz{
                message Biz{
                    message Buz{}
                }
            }
        "#;
        let file = File::new(text.to_string()).unwrap();
        let mut qc = tree_sitter::QueryCursor::new();
        assert_eq!(
            file.symbols(&mut qc).collect::<Vec<_>>(),
            vec![
                Symbol {
                    kind: SymbolKind::Message,
                    name: "Foo".into(),
                    range: tree_sitter::Range {
                        start_byte: 69,
                        end_byte: 82,
                        start_point: Point { row: 3, column: 12 },
                        end_point: Point { row: 3, column: 25 },
                    },
                },
                Symbol {
                    kind: SymbolKind::Enum,
                    name: "Bar".into(),
                    range: tree_sitter::Range {
                        start_byte: 95,
                        end_byte: 105,
                        start_point: Point { row: 4, column: 12 },
                        end_point: Point { row: 4, column: 22 },
                    },
                },
                Symbol {
                    kind: SymbolKind::Message,
                    name: "Baz".into(),
                    range: tree_sitter::Range {
                        start_byte: 118,
                        end_byte: 225,
                        start_point: Point { row: 5, column: 12 },
                        end_point: Point { row: 9, column: 13 },
                    },
                },
                Symbol {
                    kind: SymbolKind::Message,
                    name: "Baz.Biz".into(),
                    range: tree_sitter::Range {
                        start_byte: 147,
                        end_byte: 211,
                        start_point: Point { row: 6, column: 16 },
                        end_point: Point { row: 8, column: 17 },
                    },
                },
                Symbol {
                    kind: SymbolKind::Message,
                    name: "Baz.Biz.Buz".into(),
                    range: tree_sitter::Range {
                        start_byte: 180,
                        end_byte: 193,
                        start_point: Point { row: 7, column: 20 },
                        end_point: Point { row: 7, column: 33 },
                    },
                },
            ]
        );
    }

    #[test]
    fn test_relative_symbols() {
        let _ = env_logger::builder().is_test(true).try_init();
        let text = r#"
            syntax="proto3"; 
            package main;
            message Foo{}
            enum Bar{}
            message Baz{
                message Biz{}
            }
        "#;
        let file = File::new(text.to_string()).unwrap();
        let mut qc = tree_sitter::QueryCursor::new();
        assert_eq!(
            file.relative_symbols("Foo", &mut qc).collect::<Vec<_>>(),
            vec![
                Symbol {
                    kind: SymbolKind::Message,
                    name: "Foo".into(),
                    range: tree_sitter::Range {
                        start_byte: 69,
                        end_byte: 82,
                        start_point: Point { row: 3, column: 12 },
                        end_point: Point { row: 3, column: 25 },
                    },
                },
                Symbol {
                    kind: SymbolKind::Enum,
                    name: "Bar".into(),
                    range: tree_sitter::Range {
                        start_byte: 95,
                        end_byte: 105,
                        start_point: Point { row: 4, column: 12 },
                        end_point: Point { row: 4, column: 22 },
                    },
                },
                Symbol {
                    kind: SymbolKind::Message,
                    name: "Baz".into(),
                    range: tree_sitter::Range {
                        start_byte: 118,
                        end_byte: 174,
                        start_point: Point { row: 5, column: 12 },
                        end_point: Point { row: 7, column: 13 },
                    },
                },
                Symbol {
                    kind: SymbolKind::Message,
                    name: "Baz.Biz".into(),
                    range: tree_sitter::Range {
                        start_byte: 147,
                        end_byte: 160,
                        start_point: Point { row: 6, column: 16 },
                        end_point: Point { row: 6, column: 29 },
                    },
                }
            ]
        );
    }

    #[test]
    fn test_completion_context() {
        let _ = env_logger::builder().is_test(true).try_init();
        let (file, points) = cursors(
            r#"|
            synt|ax = "proto3";

            import "other|.proto";

            message Foo {
                uint32 i = 1;
                st|ring s = 2;
            }

            message Bar {
                message Buz {
                    b|
                }

                Buz b = 1;
                Ba|
            }

            |

            enum Enum {
                ENUM_ONE_TWO = |
                ENUM_TWO_|
                |
            }
            "#,
        );

        assert_eq!(
            points
                .iter()
                .map(|p| file.completion_context(p.row, p.column).unwrap())
                .collect::<Vec<Option<CompletionContext>>>(),
            vec![
                Some(CompletionContext::Keyword),
                None,
                Some(CompletionContext::Import),
                Some(CompletionContext::Message("Foo")),
                Some(CompletionContext::Message("Buz")),
                Some(CompletionContext::Message("Bar")),
                Some(CompletionContext::Keyword),
                None,
                Some(CompletionContext::Enum("Enum")),
                None,
            ]
        );
    }

    #[test]
    fn test_completion_context_syntax() {
        let _ = env_logger::builder().is_test(true).try_init();
        let (file, pos) = cursor("|");

        assert_eq!(
            file.completion_context(pos.row, pos.column).unwrap(),
            Some(CompletionContext::Syntax),
        );
    }

    #[test]
    fn test_completion_context_import() {
        let _ = env_logger::builder().is_test(true).try_init();

        let (file, pos) = cursor(
            r#"
            syntax = "proto3";
            import "|
            "#,
        );
        assert_eq!(
            file.completion_context(pos.row, pos.column).unwrap(),
            Some(CompletionContext::Import),
        );

        let (file, pos) = cursor(
            r#"
            syntax = "proto3";
            import "fo|o";
            "#,
        );
        assert_eq!(
            file.completion_context(pos.row, pos.column).unwrap(),
            Some(CompletionContext::Import),
        );

        let (file, pos) = cursor(
            r#"
            syntax = "proto3";
            import "foo.proto";
            import "|
            "#,
        );
        assert_eq!(
            file.completion_context(pos.row, pos.column).unwrap(),
            Some(CompletionContext::Import),
        );
    }

    #[test]
    fn test_completion_context_keyword() {
        let _ = env_logger::builder().is_test(true).try_init();

        fn test(lines: &[&str], expected: Option<CompletionContext>) {
            let text = lines.join("\n");
            let (file, point) = cursor(text.as_str());
            assert_eq!(
                file.completion_context(point.row, point.column).unwrap(),
                expected,
                "text:\n{}",
                text
            );
        }

        // first word of the line
        test(
            &[r#"syntax = "proto3";"#, "|", ""],
            Some(CompletionContext::Keyword),
        );
        test(
            &[r#"syntax = "proto3";"#, "mes|", ""],
            Some(CompletionContext::Keyword),
        );

        // following words of the line
        test(&[r#"syntax = "proto3";"#, "message |", ""], None);
        test(&[r#"syntax = "proto3";"#, "message Fo|", ""], None);
        test(&[r#"syntax = "proto3";"#, "message Foo |", ""], None);

        // new line
        test(
            &[r#"syntax = "proto3";"#, "message Foo{}", "mes|"],
            Some(CompletionContext::Keyword),
        );
    }

    #[test]
    fn test_completion_context_message() {
        let _ = env_logger::builder().is_test(true).try_init();

        fn test(lines: &[&str], expected: Option<CompletionContext>) {
            let text = format!("syntax = \"proto3\";\n{}\n", lines.join("\n"));
            let (file, point) = cursor(text.as_str());
            assert_eq!(
                file.completion_context(point.row, point.column).unwrap(),
                expected,
                "text:\n{}",
                text
            );
        }

        test(&["message Foo{ | }", ""], None);
        test(
            &["message Foo{ B| }", ""],
            Some(CompletionContext::Message("Foo")),
        );
        test(
            &["message Foo{ B|ar }", ""],
            Some(CompletionContext::Message("Foo")),
        );
        test(
            &["message Foo{ s|tring }", ""],
            Some(CompletionContext::Message("Foo")),
        );
        test(&["message Foo{ Bar | }"], None);
        test(&["message Foo{ Bar b| }"], None);
        test(&["message Foo{ Bar b|ar }"], None);
        test(&["message Foo{ Bar bar |}"], None);
        test(&["message Foo{ Bar bar | }"], None);
        test(&["message Foo{ Bar bar |= }"], None);
        test(&["message Foo{ Bar bar =| }"], None);
        test(&["message Foo{ Bar bar = | }"], None);
        test(&["message Foo{ Bar bar = |1 }"], None);
        test(&["message Foo{ Bar bar = 1| }"], None);
        test(&["message Foo{ Bar bar = 1|; }"], None);
        test(&["message Foo{ Bar bar = 1;| }"], None);
        test(&["message Foo{ oneof th| }"], None);
    }

    #[test]
    fn test_completion_context_option() {
        let _ = env_logger::builder().is_test(true).try_init();

        fn test(lines: &[&str]) {
            let text = format!("syntax = \"proto3\";\n{}\n", lines.join("\n"));
            let (file, point) = cursor(text.as_str());
            assert_eq!(
                file.completion_context(point.row, point.column).unwrap(),
                Some(CompletionContext::Option),
                "text:\n{}",
                text
            );
        }

        test(&["option |"]);
        test(&["option java|"]);
        test(&["option |java"]);
        test(&["import \"blah.proto\";", "option |java"]);
        test(&["option |java", "import \"blah.proto\";"]);
        test(&["message Foo{}", "option |java"]);
        test(&["option |java", "message Foo{}"]);
    }

    #[test]
    fn test_type_at() {
        let _ = env_logger::builder().is_test(true).try_init();
        let (file, points) = cursors(
            r#"
            synt|ax = "proto3";

            import "other|.proto";

            message F|oo {
                st|ring s = 1;
                int|32 i = 2;
                B|ar |b = 3;
                Baz.|Buz b = |4;
                foo.bar|.|Buz.Boz g = 5|;
            }
            "#,
        );

        assert_eq!(
            points
                .iter()
                .map(|p| file.type_at(p.row, p.column))
                .collect::<Vec<Option<GotoContext>>>(),
            vec![
                None,
                Some(GotoContext::Import("other.proto")),
                Some(GotoContext::Type(GotoTypeContext {
                    name: "Foo",
                    parent: None
                })),
                None,
                None,
                Some(GotoContext::Type(GotoTypeContext {
                    name: "Bar",
                    parent: Some("Foo".into()),
                })),
                None,
                Some(GotoContext::Type(GotoTypeContext {
                    name: "Baz.Buz",
                    parent: Some("Foo".into()),
                })),
                None,
                Some(GotoContext::Type(GotoTypeContext {
                    name: "foo.bar.Buz.Boz",
                    parent: Some("Foo".into()),
                })),
                Some(GotoContext::Type(GotoTypeContext {
                    name: "foo.bar.Buz.Boz",
                    parent: Some("Foo".into()),
                })),
                None,
            ]
        );
    }

    #[test]
    fn test_references() {
        let _ = env_logger::builder().is_test(true).try_init();
        let file = File::new(
            [
                "syntax = \"proto3\";",
                "package thing;",
                "import \"foo.proto\";",
                "import \"bar.proto\";",
                "message Foo {",
                "    Bar b = 1;",
                "    Biz.Buz bb = 2;",
                "    buf.Buf buf = 3;",
                "}",
                "",
            ]
            .join("\n"),
        )
        .unwrap();

        assert_eq!(
            file.references(&GotoContext::Import("foo.proto")),
            vec![tree_sitter::Range {
                start_byte: 41,
                end_byte: 52,
                start_point: Point { row: 2, column: 7 },
                end_point: Point { row: 2, column: 18 }
            }]
        );

        assert_eq!(
            file.references(&GotoContext::Import("bar.proto")),
            vec![tree_sitter::Range {
                start_byte: 61,
                end_byte: 72,
                start_point: Point { row: 3, column: 7 },
                end_point: Point { row: 3, column: 18 }
            }]
        );

        assert_eq!(file.references(&GotoContext::Import("baz.proto")), vec![]);

        assert_eq!(
            file.references(&GotoContext::Type(GotoTypeContext {
                name: "thing.Bar",
                parent: None
            })),
            vec![tree_sitter::Range {
                start_byte: 92,
                end_byte: 95,
                start_point: Point { row: 5, column: 4 },
                end_point: Point { row: 5, column: 7 }
            }]
        );

        assert_eq!(
            file.references(&GotoContext::Type(GotoTypeContext {
                name: "thing.Biz.Buz",
                parent: None
            })),
            vec![tree_sitter::Range {
                start_byte: 107,
                end_byte: 114,
                start_point: Point { row: 6, column: 4 },
                end_point: Point { row: 6, column: 11 }
            }]
        );

        assert_eq!(
            file.references(&GotoContext::Type(GotoTypeContext {
                name: "buf.Buf",
                parent: None
            })),
            vec![tree_sitter::Range {
                start_byte: 127,
                end_byte: 134,
                start_point: Point { row: 7, column: 4 },
                end_point: Point { row: 7, column: 11 }
            }]
        );

        assert_eq!(
            file.references(&GotoContext::Type(GotoTypeContext {
                name: "Buf",
                parent: None
            })),
            vec![]
        );

        assert_eq!(
            file.references(&GotoContext::Type(GotoTypeContext {
                name: "buf",
                parent: None
            })),
            vec![]
        );
    }

    #[test]
    fn test_relative_name() {
        assert_eq!(relative_name("Foo", "Biz.Bar.Baz"), "Biz.Bar.Baz");
        assert_eq!(relative_name("Foo.Bar", "Biz.Bar.Baz"), "Biz.Bar.Baz");
        assert_eq!(relative_name("Foo.Bar.Baz", "Biz.Bar.Baz"), "Biz.Bar.Baz");

        assert_eq!(relative_name("Foo.Bar.Baz", "Foo.Bar.Baz"), "Baz");
        assert_eq!(relative_name("Foo.Bar.Baz", "Foo.Bar"), "Bar");
        assert_eq!(relative_name("Foo.Bar.Baz", "Foo"), "Foo");

        assert_eq!(relative_name("Foo.Bar", "Foo.Bar.Baz"), "Baz");
        assert_eq!(relative_name("Foo.Bar", "Foo.Bar"), "Bar");
        assert_eq!(relative_name("Foo.Bar", "Foo"), "Foo");

        assert_eq!(relative_name("Foo", "Foo.Bar.Baz"), "Bar.Baz");
        assert_eq!(relative_name("Foo", "Foo.Bar"), "Bar");
        assert_eq!(relative_name("Foo", "Foo"), "Foo");
    }

    #[test]
    fn test_edit() {
        let text = "yn";
        let mut file = File::new(text.into()).unwrap();
        assert_eq!(file.text, text);

        let change = |(start_line, start_char), (end_line, end_char), text: &str| {
            lsp_types::TextDocumentContentChangeEvent {
                range: Some(lsp_types::Range {
                    start: lsp_types::Position {
                        line: start_line,
                        character: start_char,
                    },
                    end: lsp_types::Position {
                        line: end_line,
                        character: end_char,
                    },
                }),
                range_length: None,
                text: text.into(),
            }
        };

        file.edit(vec![]).unwrap();
        assert_eq!(file.text, text);

        file.edit(vec![change((0, 0), (0, 0), "s")]).unwrap();
        assert_eq!(file.text, "syn");

        file.edit(vec![change((0, 3), (0, 3), "tax = \"proto2\";\n")])
            .unwrap();
        assert_eq!(file.text, "syntax = \"proto2\";\n");

        file.edit(vec![change((0, 10), (0, 16), "proto3")]).unwrap();
        assert_eq!(file.text, "syntax = \"proto3\";\n");

        file.edit(vec![change((1, 0), (1, 0), "message Foo {}\n")])
            .unwrap();
        assert_eq!(file.text, "syntax = \"proto3\";\nmessage Foo {}\n");

        file.edit(vec![change((1, 13), (1, 14), "\n\n}")]).unwrap();
        assert_eq!(file.text, "syntax = \"proto3\";\nmessage Foo {\n\n}\n");

        file.edit(vec![change(
            (2, 0),
            (2, 0),
            "uint32 i = 1;\nstring s = 2;\nbytes b = 3;",
        )])
        .unwrap();
        assert_eq!(
            file.text,
            [
                "syntax = \"proto3\";",
                "message Foo {",
                "uint32 i = 1;",
                "string s = 2;",
                "bytes b = 3;",
                "}",
                ""
            ]
            .join("\n")
        );

        file.edit(vec![change(
            (2, 0),
            (5, 0),
            "uint32 i = 2;\nstring s = 3;\nbytes b = 4;\n",
        )])
        .unwrap();
        assert_eq!(
            file.text,
            [
                "syntax = \"proto3\";",
                "message Foo {",
                "uint32 i = 2;",
                "string s = 3;",
                "bytes b = 4;",
                "}",
                ""
            ]
            .join("\n")
        );

        file.edit(vec![change((2, 4), (3, 8), "64 u = 2;\nstring str")])
            .unwrap();
        assert_eq!(
            file.text,
            [
                "syntax = \"proto3\";",
                "message Foo {",
                "uint64 u = 2;",
                "string str = 3;",
                "bytes b = 4;",
                "}",
                ""
            ]
            .join("\n")
        );

        file.edit(vec![change((3, 13), (4, 0), "5;\n")]).unwrap();
        assert_eq!(
            file.text,
            [
                "syntax = \"proto3\";",
                "message Foo {",
                "uint64 u = 2;",
                "string str = 5;",
                "bytes b = 4;",
                "}",
                ""
            ]
            .join("\n")
        );
    }

    #[test]
    fn test_edit_unicode() {
        let text = [
            "syntax = \"proto3\";",
            "import \"examp𐐀e.proto\";",
            "import \"other.proto\";",
            "",
        ]
        .join("\n");
        let mut file = File::new(text.clone()).unwrap();
        assert_eq!(file.text, text);

        let change = |(start_line, start_char), (end_line, end_char), text: &str| {
            lsp_types::TextDocumentContentChangeEvent {
                range: Some(lsp_types::Range {
                    start: lsp_types::Position {
                        line: start_line,
                        character: start_char,
                    },
                    end: lsp_types::Position {
                        line: end_line,
                        character: end_char,
                    },
                }),
                range_length: None,
                text: text.into(),
            }
        };

        file.edit(vec![change((1, 8), (1, 15), "thing")]).unwrap();
        assert_eq!(
            file.text,
            [
                "syntax = \"proto3\";",
                "import \"thing.proto\";",
                "import \"other.proto\";",
                "",
            ]
            .join("\n")
        );
    }
}
