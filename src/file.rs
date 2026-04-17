use anyhow::{Context, Result};
use ropey::Rope;
use std::sync::OnceLock;
use tree_sitter::StreamingIterator;

fn language() -> &'static tree_sitter::Language {
    static LANGUAGE: OnceLock<tree_sitter::Language> = OnceLock::new();
    LANGUAGE.get_or_init(|| tree_sitter_proto::LANGUAGE.into())
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
pub struct FieldNumberContext {
    pub used: Vec<u32>,
    pub reserved: Vec<std::ops::RangeInclusive<u32>>,
}

impl FieldNumberContext {
    pub fn next_free(&self) -> u32 {
        let mut candidate = 1u32;
        loop {
            if !self.used.contains(&candidate)
                && !self.reserved.iter().any(|r| r.contains(&candidate))
            {
                return candidate;
            }
            candidate += 1;
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum CompletionContext<'a> {
    Message(&'a str),
    Enum(&'a str),
    FieldNumber(FieldNumberContext),
    Rpc,
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
    rope: Rope,
}

impl File {
    pub fn new(text: String) -> Result<File> {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(language())
            .expect("Error loading proto language");

        let tree = parser.parse(&text, None).context("Parse failed")?;
        log::trace!("Parsed: {}", tree.root_node().to_sexp());
        let rope = Rope::from_str(&text);
        Ok(File { tree, text, rope })
    }

    /// Convert a tree-sitter Point (row + byte column) to an LSP Position (line + UTF-16 character).
    pub fn ts_to_lsp_pos(&self, pos: tree_sitter::Point) -> lsp_types::Position {
        let line = self.rope.line(pos.row);
        let char_offset = line.byte_to_char(pos.column);
        let utf16_offset = line.char_to_utf16_cu(char_offset);
        lsp_types::Position::new(pos.row as u32, utf16_offset as u32)
    }

    /// Convert an LSP Position (line + UTF-16 character) to a tree-sitter Point (row + byte column).
    pub fn lsp_to_ts_pos(&self, pos: lsp_types::Position) -> tree_sitter::Point {
        let line = self.rope.line(pos.line as usize);
        let char_offset = line.utf16_cu_to_char(pos.character as usize);
        let byte_offset = line.char_to_byte(char_offset);
        tree_sitter::Point::new(pos.line as usize, byte_offset)
    }

    /// Convert a tree-sitter Range to an LSP Range.
    pub fn ts_to_lsp_range(&self, range: tree_sitter::Range) -> lsp_types::Range {
        lsp_types::Range {
            start: self.ts_to_lsp_pos(range.start_point),
            end: self.ts_to_lsp_pos(range.end_point),
        }
    }

    /// Convert an LSP Position to an absolute byte offset in the text.
    fn lsp_pos_to_byte(&self, pos: lsp_types::Position) -> usize {
        let point = self.lsp_to_ts_pos(pos);
        self.rope.line_to_byte(point.row) + point.column
    }

    pub fn edit(&mut self, changes: Vec<lsp_types::TextDocumentContentChangeEvent>) -> Result<()> {
        for change in changes {
            let range = change
                .range
                .with_context(|| format!("No range in change notification {change:?}"))?;
            let start_byte = self.lsp_pos_to_byte(range.start);
            let end_byte = self.lsp_pos_to_byte(range.end);

            log::trace!(
                "Computing change {start_byte}..{end_byte} with text {}",
                change.text
            );

            self.text.replace_range(start_byte..end_byte, &change.text);
            self.rope = Rope::from_str(&self.text);
        }
        log::trace!("Edited text to: {}", self.text);

        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(language())
            .expect("Error loading proto language");
        self.tree = parser.parse(&self.text, None).context("Parse failed")?;
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
            tree_sitter::Query::new(language(), "(package (full_ident) @id)").unwrap()
        });

        let mut qc = tree_sitter::QueryCursor::new();
        let mut matches = qc.matches(query, self.tree.root_node(), self.text.as_bytes());
        matches
            .next()
            .map(|m| m.captures[0].node)
            .map(|n| self.get_text(n))
    }

    pub fn imports(&self, qc: &mut tree_sitter::QueryCursor) -> Vec<&str> {
        static QUERY: OnceLock<tree_sitter::Query> = OnceLock::new();
        let query = QUERY.get_or_init(|| {
            tree_sitter::Query::new(language(), "(import (string) @path)").unwrap()
        });

        let mut matches = qc.matches(query, self.tree.root_node(), self.text.as_bytes());
        let mut result = Vec::new();
        while let Some(m) = matches.next() {
            result.push(self.get_text(m.captures[0].node).trim_matches('"'));
        }
        result
    }

    pub fn symbols(&self, qc: &mut tree_sitter::QueryCursor) -> Vec<Symbol> {
        static QUERY: OnceLock<tree_sitter::Query> = OnceLock::new();
        let query = QUERY.get_or_init(|| {
            tree_sitter::Query::new(
                language(),
                "[
                     (message (message_name (identifier) @id))
                     (enum (enum_name (identifier) @id))
                 ] @def",
            )
            .unwrap()
        });

        let mut matches = qc.matches(query, self.tree.root_node(), self.text.as_bytes());
        let mut result = Vec::new();
        while let Some(m) = matches.next() {
            let def = m.captures[0].node;
            let id = m.captures[1].node;
            let name = self.get_text(id).to_string();
            let name = if let Some(p) = self.parent_name(def) {
                p + "." + &name
            } else {
                name
            };
            result.push(Symbol {
                kind: match def.kind() {
                    "message" => SymbolKind::Message,
                    _ => SymbolKind::Enum,
                },
                name,
                range: def.range(),
            });
        }
        result
    }

    // Return all symbols adjusted relative to a message.
    // For example, given base_name=Foo.Bar:
    // symbols()          -> [Foo, Foo.Bar, Foo.Bar.Baz, Foo.Bar.Baz.Biz]
    // relative_symbols() -> [Foo, Bar    , Baz        , Baz.Biz]
    pub fn relative_symbols(
        &self,
        base_name: &str,
        qc: &mut tree_sitter::QueryCursor,
    ) -> Vec<Symbol> {
        self.symbols(qc)
            .into_iter()
            .map(|s| Symbol {
                name: relative_name(base_name, &s.name),
                ..s
            })
            .collect()
    }

    // Given an "ident" or "enumMessageType", node representing a type, find the name of the type.
    fn field_type(&self, node: Option<tree_sitter::Node>) -> Option<&str> {
        log::trace!("Finding type of {node:?}");
        match node {
            None => None,
            Some(n) if n.kind() == "type" || n.kind() == "message_or_enum_type" => {
                Some(self.get_text(n))
            }
            Some(n) => self.field_type(n.parent()),
        }
    }

    fn parent_context(&self, node: Option<tree_sitter::Node<'_>>) -> Option<CompletionContext<'_>> {
        log::trace!(
            "Checking parent context: {node:?} - {}",
            node.map_or("".into(), |n| n.to_sexp())
        );
        match node {
            // Hit the document root
            None => None,
            // Don't complete if we're typing a field name or number
            // Don't complete field names (parent is a named field node)
            Some(n)
                if n.kind() == "identifier"
                    && n.parent().is_some_and(|p| {
                        matches!(p.kind(), "field" | "oneof_field" | "map_field")
                    }) =>
            {
                None
            }
            // Don't complete field names in error-recovered incomplete fields:
            // identifier immediately following a `type` sibling is a field name
            Some(n)
                if n.kind() == "identifier"
                    && n.prev_named_sibling().is_some_and(|s| s.kind() == "type") =>
            {
                None
            }
            Some(n)
                if n.kind() == "message_name"
                    || n.kind() == "enum_name"
                    || n.kind() == "service_name" =>
            {
                None
            }
            Some(n) if n.kind() == "enum_body" => n
                .parent() // enum
                .and_then(|p| self.type_name(p))
                .map(CompletionContext::Enum),
            // Fallback: reach the enum node directly when enum_body is absent (error recovery)
            Some(n) if n.kind() == "enum" => self.type_name(n).map(CompletionContext::Enum),
            Some(n) if n.kind() == "message_body" => n
                .parent() // message
                .and_then(|p| self.type_name(p))
                .map(CompletionContext::Message),
            // Fallback: reach the message node directly when message_body is absent
            Some(n) if n.kind() == "message" => self.type_name(n).map(CompletionContext::Message),
            // In the new grammar there is no service_body wrapper; rpc nodes are direct children
            Some(n) if n.kind() == "service" => Some(CompletionContext::Rpc),
            // ERROR containing "oneof <name>" means we're typing an oneof name, not a type
            Some(n) if n.is_error() && self.get_text(n).starts_with("oneof ") => None,
            Some(n) => self.parent_context(n.parent()),
        }
    }

    fn extract_field_number(&self, node: tree_sitter::Node) -> Option<u32> {
        let mut cursor = node.walk();
        // Assign to a variable so the iterator (and its borrow of `cursor`) is dropped before
        // we create a second cursor below.
        let fn_node = node
            .named_children(&mut cursor)
            .find(|c| c.kind() == "field_number")?;
        let mut c2 = fn_node.walk();
        let int_node = fn_node
            .named_children(&mut c2)
            .find(|c| c.kind() == "int_lit")?;
        self.get_text(int_node).parse::<u32>().ok()
    }

    fn collect_field_numbers(&self, message_body: tree_sitter::Node) -> FieldNumberContext {
        let mut used = Vec::new();
        let mut reserved = Vec::new();
        let mut cursor = message_body.walk();

        for child in message_body.named_children(&mut cursor) {
            match child.kind() {
                "field" | "map_field" => {
                    if let Some(num) = self.extract_field_number(child) {
                        used.push(num);
                    }
                }
                "oneof" => {
                    let mut c2 = child.walk();
                    for oneof_child in child.named_children(&mut c2) {
                        if oneof_child.kind() == "oneof_field" {
                            if let Some(num) = self.extract_field_number(oneof_child) {
                                used.push(num);
                            }
                        }
                    }
                }
                "reserved" => {
                    let mut c2 = child.walk();
                    for ranges_node in child.named_children(&mut c2) {
                        if ranges_node.kind() == "ranges" {
                            let mut c3 = ranges_node.walk();
                            for range_node in ranges_node.named_children(&mut c3) {
                                if range_node.kind() == "range" {
                                    let mut c4 = range_node.walk();
                                    let ints: Vec<u32> = range_node
                                        .named_children(&mut c4)
                                        .filter(|c| c.kind() == "int_lit")
                                        .filter_map(|c| self.get_text(c).parse::<u32>().ok())
                                        .collect();
                                    match ints.as_slice() {
                                        [n] => reserved.push(*n..=*n),
                                        [low, high] => reserved.push(*low..=*high),
                                        _ => {}
                                    }
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        FieldNumberContext { used, reserved }
    }

    fn field_number_completion_context(
        &self,
        node: tree_sitter::Node,
    ) -> Option<CompletionContext<'_>> {
        let mut n = Some(node);
        while let Some(current) = n {
            match current.kind() {
                "message_body" => {
                    return Some(CompletionContext::FieldNumber(
                        self.collect_field_numbers(current),
                    ));
                }
                // Don't cross into enum body — enum field numbers are semantic values, not positions
                "enum_body" => return None,
                _ => {}
            }
            n = current.parent();
        }
        None
    }

    pub fn completion_context(
        &self,
        row: usize,
        col: usize,
    ) -> Result<Option<CompletionContext<'_>>> {
        if self.tree.root_node().kind() != "source_file" || self.text.trim().is_empty() {
            // If the whole document is invalid or empty, we need to define a syntax.
            return Ok(Some(CompletionContext::Syntax));
        }

        let pos = tree_sitter::Point {
            row,
            // Generally, the node before the cursor is more interesting for context.
            column: col.saturating_sub(1),
        };
        let node = self
            .tree
            .root_node()
            .named_descendant_for_point_range(pos, pos)
            .with_context(|| format!("No descendant for range {pos:?}"))?;

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
        } else if is_sexp(node, &["option", "identifier"])
            || is_sexp(node, &["option", "full_ident", "identifier"])
        {
            // option c| -> (option (identifier)) or (option (full_ident (identifier)))
            Some(CompletionContext::Option)
        } else if (node.is_error() && self.get_text(node).starts_with("option "))
            || node
                .parent()
                .is_some_and(|p| p.is_error() && self.get_text(p).starts_with("option "))
        {
            // option | -> (ERROR)
            Some(CompletionContext::Option)
        } else if node.is_error() && self.get_text(node).starts_with("import ") {
            // import "| -> (ERROR)
            Some(CompletionContext::Import)
        } else if is_sexp(node, &["import", "string"]) {
            // import "foo|.proto" -> (import (string))
            Some(CompletionContext::Import)
        } else if node.kind() == "type"
            || (node.kind() == "identifier" && !node.parent().is_some_and(|p| p.kind() == "oneof"))
        {
            // message Foo { Bar| -> (identifier)
            // message Foo { string| -> (type)
            self.parent_context(Some(node))
        } else if is_sexp(node, &["field_number", "int_lit"])
            || is_sexp(node, &["field_number", "int_lit", "decimal_lit"])
        {
            // message Foo { string d = 1| -> (field (field_number (int_lit (decimal_lit))))
            self.field_number_completion_context(node)
        } else if let Some(ctx) = {
            // Cursor is after '=' on a field number position. This covers several parse states:
            //   - ERROR node (incomplete field): node text contains '='
            //   - field/message_body node: space after '=' lands on a non-ERROR node
            // Check the line before the cursor ends with '=' then walk up for message_body.
            let line = self.text.lines().nth(row).unwrap_or("");
            let before = &line[..col.min(line.len())];
            if before.trim_end().ends_with('=') {
                self.field_number_completion_context(node)
            } else {
                None
            }
        } {
            Some(ctx)
        } else if node.is_error() && !is_top_level_error(node) && !self.get_text(node).contains('=')
        {
            // Nested ERROR node (e.g. a new identifier being typed inside enum_body/message_body).
            // If the error text contains '=', we are in a partial assignment (value position) where
            // no type completion is appropriate.
            self.parent_context(node.parent())
        } else if is_top_level_error(node) {
            // typically means we're typing the first word of a line
            // mes| -> (source_file (ERROR (ERROR)))
            Some(CompletionContext::Keyword)
        } else if node.kind() == "source_file" {
            // NOTE: Not very efficient, but we're in a difficult spot here.
            let full_line = self
                .text
                .lines()
                .nth(row)
                .with_context(|| format!("Line {row} out of range"))?;
            let line = &full_line[..col.min(full_line.len())];
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

    pub fn type_references(
        &self,
        pkg: Option<&str>,
        typ: &GotoTypeContext,
    ) -> Vec<tree_sitter::Range> {
        static QUERY: OnceLock<tree_sitter::Query> = OnceLock::new();
        let query = QUERY
            .get_or_init(|| tree_sitter::Query::new(language(), "(field (type) @name)").unwrap());
        let typ = typ.name;
        log::trace!("Searching for references to {typ} in package {pkg:?}");

        let mut qc = tree_sitter::QueryCursor::new();
        let mut matches = qc.matches(query, self.tree.root_node(), self.text.as_bytes());
        let mut result = Vec::new();
        while let Some(m) = matches.next() {
            let node = m.captures[0].node;
            log::trace!("Check {node:?}: {} == {typ}", self.get_text(node));
            let text = self.get_text(node);
            let matches = text == typ
                || match pkg {
                    Some(pkg) => {
                        log::trace!("Trying to match '{typ}' to '{text}' without package {pkg}");
                        text.strip_prefix(pkg)
                            .and_then(|text| text.strip_prefix("."))
                            .map(|text| typ == text)
                            .unwrap_or(false)
                    }
                    None => false,
                };
            if matches {
                result.push(node.range());
            }
        }
        result
    }

    pub fn import_references(&self, file: &str) -> Vec<tree_sitter::Range> {
        static QUERY: OnceLock<tree_sitter::Query> = OnceLock::new();
        let query = QUERY.get_or_init(|| {
            tree_sitter::Query::new(language(), "(import (string) @name)").unwrap()
        });

        let mut qc = tree_sitter::QueryCursor::new();
        let mut matches = qc.matches(query, self.tree.root_node(), self.text.as_bytes());
        let mut result = Vec::new();
        while let Some(m) = matches.next() {
            let n = m.captures[0].node;
            if self.get_text(n).trim_matches('"') == file {
                result.push(n.range());
            }
        }
        result
    }

    pub fn type_at(&self, row: usize, column: usize) -> Option<GotoContext<'_>> {
        log::trace!("Getting type at row: {row} col: {column}");

        let pos = tree_sitter::Point { row, column };
        let node = self
            .tree
            .root_node()
            .named_descendant_for_point_range(pos, pos)?;

        log::debug!("Getting type at node: {node:?} parent: {:?}", node.parent());

        if node.kind() == "string" && node.parent().is_some_and(|p| p.kind() == "import") {
            return Some(GotoContext::Import(self.get_text(node).trim_matches('"')));
        }

        // Cursor is over a message or enum name.
        if is_sexp(node, &["enum", "enum_name", "identifier"])
            || is_sexp(node, &["message", "message_name", "identifier"])
        {
            return Some(GotoContext::Type(GotoTypeContext {
                name: self.get_text(node.parent().unwrap()),
                parent: None,
            }));
        }

        // Cursor is over a field type.
        if node.kind() == "identifier" || node.kind() == "message_or_enum_type" {
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
        while let Some(parent) = node.parent() {
            if parent.kind() == "message" {
                let name = self.type_name(parent);
                log::trace!("Appending parent name {name:?}");
                if let Some(n) = name {
                    res.push(n)
                }
            }
            node = parent;
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
            node.kind() == "enum" || node.kind() == "message" || node.kind() == "service",
            "{node:?}"
        );
        let mut cursor = node.walk();
        let child = node.named_children(&mut cursor).find(|c| {
            c.kind() == "message_name" || c.kind() == "enum_name" || c.kind() == "service_name"
        });
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
fn relative_name(message: &str, name: &str) -> String {
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
        assert_eq!(file.package(), Some("main"));

        let text = r#"syntax="proto3"; package main; package other"#;
        let file = File::new(text.to_string()).unwrap();
        assert_eq!(file.package(), Some("main"));

        let text = r#"syntax="proto3"; package foo.bar.baz;"#;
        let file = File::new(text.to_string()).unwrap();
        assert_eq!(file.package(), Some("foo.bar.baz"));

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
        assert_eq!(file.imports(&mut qc), vec!["foo.proto", "bar.proto"]);
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
            file.symbols(&mut qc),
            vec![
                Symbol {
                    kind: SymbolKind::Message,
                    name: "Foo".into(),
                    range: tree_sitter::Range {
                        start_byte: 68,
                        end_byte: 81,
                        start_point: Point { row: 3, column: 12 },
                        end_point: Point { row: 3, column: 25 },
                    },
                },
                Symbol {
                    kind: SymbolKind::Enum,
                    name: "Bar".into(),
                    range: tree_sitter::Range {
                        start_byte: 94,
                        end_byte: 104,
                        start_point: Point { row: 4, column: 12 },
                        end_point: Point { row: 4, column: 22 },
                    },
                },
                Symbol {
                    kind: SymbolKind::Message,
                    name: "Baz".into(),
                    range: tree_sitter::Range {
                        start_byte: 117,
                        end_byte: 224,
                        start_point: Point { row: 5, column: 12 },
                        end_point: Point { row: 9, column: 13 },
                    },
                },
                Symbol {
                    kind: SymbolKind::Message,
                    name: "Baz.Biz".into(),
                    range: tree_sitter::Range {
                        start_byte: 146,
                        end_byte: 210,
                        start_point: Point { row: 6, column: 16 },
                        end_point: Point { row: 8, column: 17 },
                    },
                },
                Symbol {
                    kind: SymbolKind::Message,
                    name: "Baz.Biz.Buz".into(),
                    range: tree_sitter::Range {
                        start_byte: 179,
                        end_byte: 192,
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
            file.relative_symbols("Foo", &mut qc),
            vec![
                Symbol {
                    kind: SymbolKind::Message,
                    name: "Foo".into(),
                    range: tree_sitter::Range {
                        start_byte: 68,
                        end_byte: 81,
                        start_point: Point { row: 3, column: 12 },
                        end_point: Point { row: 3, column: 25 },
                    },
                },
                Symbol {
                    kind: SymbolKind::Enum,
                    name: "Bar".into(),
                    range: tree_sitter::Range {
                        start_byte: 94,
                        end_byte: 104,
                        start_point: Point { row: 4, column: 12 },
                        end_point: Point { row: 4, column: 22 },
                    },
                },
                Symbol {
                    kind: SymbolKind::Message,
                    name: "Baz".into(),
                    range: tree_sitter::Range {
                        start_byte: 117,
                        end_byte: 173,
                        start_point: Point { row: 5, column: 12 },
                        end_point: Point { row: 7, column: 13 },
                    },
                },
                Symbol {
                    kind: SymbolKind::Message,
                    name: "Baz.Biz".into(),
                    range: tree_sitter::Range {
                        start_byte: 146,
                        end_byte: 159,
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

            service Greeter {
              rpc| SayHello (Hello|Request) returns (Hello|Reply) {}
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
                None,
                Some(CompletionContext::Rpc),
                Some(CompletionContext::Rpc),
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
        test(
            &["message Foo{ Bar bar =| }"],
            Some(CompletionContext::FieldNumber(FieldNumberContext {
                used: vec![],
                reserved: vec![],
            })),
        );
        test(
            &["message Foo{ Bar bar = | }"],
            Some(CompletionContext::FieldNumber(FieldNumberContext {
                used: vec![],
                reserved: vec![],
            })),
        );
        test(
            &["message Foo{ Bar bar = |1 }"],
            Some(CompletionContext::FieldNumber(FieldNumberContext {
                used: vec![1],
                reserved: vec![],
            })),
        );
        test(
            &["message Foo{ Bar bar = 1| }"],
            Some(CompletionContext::FieldNumber(FieldNumberContext {
                used: vec![1],
                reserved: vec![],
            })),
        );
        test(
            &["message Foo{ Bar bar = 1|; }"],
            Some(CompletionContext::FieldNumber(FieldNumberContext {
                used: vec![1],
                reserved: vec![],
            })),
        );
        test(&["message Foo{ Bar bar = 1;| }"], None);
        test(&["message Foo{ oneof th| }"], None);
        // Enum values should NOT trigger field number completion
        test(&["enum Foo{ BAR = | }"], None);
        test(&["enum Foo{ BAR = |0 }"], None);
    }

    #[test]
    fn test_field_number_context() {
        let _ = env_logger::builder().is_test(true).try_init();

        fn next_free(lines: &[&str]) -> u32 {
            let text = format!("syntax = \"proto3\";\n{}\n", lines.join("\n"));
            let (file, point) = cursor(text.as_str());
            match file.completion_context(point.row, point.column).unwrap() {
                Some(CompletionContext::FieldNumber(ctx)) => ctx.next_free(),
                other => panic!("Expected FieldNumber, got {other:?}"),
            }
        }

        // Simple: next after existing fields (} on same line)
        assert_eq!(
            next_free(&["message Foo { int32 a = 1; int32 b = 2; int32 c = | }"]),
            3
        );
        // Cursor at end of line, } on next line
        assert_eq!(
            next_free(&["message Foo {", "  int32 a = 1;", "  int32 c = |", "}"]),
            2
        );
        // Gap: skips reserved single number
        assert_eq!(
            next_free(&["message Foo { reserved 2; int32 a = 1; int32 c = | }"]),
            3
        );
        // Gap: skips reserved range
        assert_eq!(
            next_free(&["message Foo { reserved 2 to 4; int32 a = 1; int32 c = | }"]),
            5
        );
        // Finds first gap between existing fields
        assert_eq!(
            next_free(&["message Foo { int32 a = 1; int32 c = 3; int32 d = | }"]),
            2
        );
        // Skips multiple reserved ranges and used numbers
        assert_eq!(
            next_free(&[
                "message Foo {",
                "  reserved 4, 6 to 8;",
                "  int32 a = 1;",
                "  int32 b = 2;",
                "  int32 c = 3;",
                "  string d = |",
                "}"
            ]),
            5
        );
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

            service G|reeter {
              rpc SayH|ello (Hel|loRequest) retu|rns (Hell|oReply) {}
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
                None,
                None,
                Some(GotoContext::Type(GotoTypeContext {
                    name: "HelloRequest",
                    parent: None,
                })),
                None,
                Some(GotoContext::Type(GotoTypeContext {
                    name: "HelloReply",
                    parent: None,
                })),
            ]
        );
    }

    #[test]
    fn test_import_references() {
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
            file.import_references("foo.proto"),
            vec![tree_sitter::Range {
                start_byte: 41,
                end_byte: 52,
                start_point: Point { row: 2, column: 7 },
                end_point: Point { row: 2, column: 18 }
            }]
        );

        assert_eq!(
            file.import_references("bar.proto"),
            vec![tree_sitter::Range {
                start_byte: 61,
                end_byte: 72,
                start_point: Point { row: 3, column: 7 },
                end_point: Point { row: 3, column: 18 }
            }]
        );

        assert_eq!(file.import_references("baz.proto"), vec![]);
    }

    #[test]
    fn test_type_references() {
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
            file.type_references(
                None,
                &GotoTypeContext {
                    name: "Bar",
                    parent: None
                }
            ),
            vec![tree_sitter::Range {
                start_byte: 92,
                end_byte: 95,
                start_point: Point { row: 5, column: 4 },
                end_point: Point { row: 5, column: 7 }
            }]
        );

        assert_eq!(
            file.type_references(
                Some("thing"),
                &GotoTypeContext {
                    name: "Biz.Buz",
                    parent: None
                }
            ),
            vec![tree_sitter::Range {
                start_byte: 107,
                end_byte: 114,
                start_point: Point { row: 6, column: 4 },
                end_point: Point { row: 6, column: 11 }
            }]
        );

        assert_eq!(
            file.type_references(
                Some("buf"),
                &GotoTypeContext {
                    name: "Buf",
                    parent: None
                }
            ),
            vec![tree_sitter::Range {
                start_byte: 127,
                end_byte: 134,
                start_point: Point { row: 7, column: 4 },
                end_point: Point { row: 7, column: 11 }
            }]
        );

        assert_eq!(
            file.type_references(
                Some("thing"),
                &GotoTypeContext {
                    name: "Buf",
                    parent: None
                }
            ),
            vec![]
        );

        assert_eq!(
            file.type_references(
                Some("thing"),
                &GotoTypeContext {
                    name: "buf",
                    parent: None
                }
            ),
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

        // 𐐀 (U+10400) is 2 UTF-16 code units, so the 'e' after it is at UTF-16 position 15,
        // and the '.' after that is at UTF-16 position 16.
        file.edit(vec![change((1, 8), (1, 16), "thing")]).unwrap();
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
