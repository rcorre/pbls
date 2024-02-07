use lsp_types::{Diagnostic, DiagnosticSeverity, Range, Url};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub fn diags(
    uri: &Url,
    text: &str,
    proto_paths: &Vec<std::path::PathBuf>,
) -> Result<Vec<Diagnostic>> {
    if uri.scheme() != "file" {
        Err(format!("Unsupported URI scheme {uri}"))?;
    }

    let path = uri
        .to_file_path()
        .or(Err(format!("Failed to normalize URI path: {uri}")))?;

    let mut cmd = std::process::Command::new("protoc");
    cmd
        // Protoc requires some output
        // Tell it to generate a descriptor, but discard it
        .args(["-o", if cfg!(windows) { "NUL" } else { "/dev/null" }])
        // Add include paths.
        .args(
            proto_paths
                .iter()
                .filter_map(|p| {
                    p.to_str().or_else(|| {
                        log::warn!("Non-unicode path: {p:?}");
                        None
                    })
                })
                .map(|p| "-I".to_string() + p),
        )
        // Add the file we're compiling
        .arg(path.to_str().ok_or(format!("Non-unicode path: {path:?}"))?);

    log::debug!("Running protoc: {cmd:?}");
    let output = cmd.output()?;

    log::debug!("Protoc exited: {output:?}");
    let stderr = std::str::from_utf8(output.stderr.as_slice())?;

    Ok(stderr
        .lines()
        .filter_map(|l| parse_diag(l, &text))
        .collect())
}

// Parse a single error line from the protoc parser into a diagnostic.
// Usually each error has a line containing a location, like:
// foo.proto:4:13: "int" is not defined
// Other lines do not contain location info.
// We'll return None to skip these, as usually another line contains the location.
fn parse_diag(diag: &str, file_contents: &str) -> Option<lsp_types::Diagnostic> {
    log::debug!("Parsing diagnostic {diag}");
    let (_, rest) = diag.split_once(".proto:")?;
    let (linestr, rest) = rest.split_once(':')?;
    let (_, msg) = rest.split_once(':')?;
    let msg = msg.trim().trim_end_matches(".");

    log::debug!("Parsing msg {msg}");
    let (msg, severity) = match msg.strip_prefix("warning: ") {
        Some(msg) => (msg, DiagnosticSeverity::WARNING),
        None => (msg, DiagnosticSeverity::ERROR),
    };

    // Lines from protoc stderr are 1-indexed.
    let lineno = linestr.parse::<u32>().unwrap() - 1;
    let line = file_contents.lines().skip(lineno.try_into().ok()?).next()?;
    let start = line.find(|c: char| !c.is_whitespace()).unwrap_or(0);
    let end = line
        .rfind(|c: char| !c.is_whitespace())
        .map(|c| c + 1) // include the final character
        .unwrap_or(line.len());

    Some(lsp_types::Diagnostic {
        range: Range {
            start: lsp_types::Position {
                line: lineno,
                character: start.try_into().ok()?,
            },
            end: lsp_types::Position {
                line: lineno,
                character: end.try_into().ok()?,
            },
        },
        severity: Some(severity),
        source: Some(String::from("pbls")),
        message: msg.trim().into(),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn proto(tmp: &tempfile::TempDir, path: &str, lines: &[&str]) -> (Url, String) {
        let path = tmp.path().join(path);
        let text = lines.join("\n") + "\n";
        std::fs::write(&path, &text).unwrap();
        (Url::from_file_path(path).unwrap(), text)
    }

    #[test]
    fn test_errors() {
        let _ = env_logger::builder().is_test(true).try_init();
        let tmp = tempfile::tempdir().unwrap();

        let (uri, text) = proto(
            &tmp,
            "foo.proto",
            &[
                "syntax = \"proto3\";",
                "message Foo {",
                "int i = 1;",
                "uint32 u = 1;",
                "}",
            ],
        );

        let diags = diags(&uri, &text, &vec![tmp.path().to_path_buf()]).unwrap();

        assert_eq!(
            diags,
            vec![
                Diagnostic {
                    range: Range {
                        start: lsp_types::Position {
                            line: 2,
                            character: 0,
                        },
                        end: lsp_types::Position {
                            line: 2,
                            character: 10,
                        },
                    },
                    severity: Some(DiagnosticSeverity::ERROR),
                    source: Some("pbls".into()),
                    message: "\"int\" is not defined".into(),
                    ..Default::default()
                },
                Diagnostic {
                    range: Range {
                        start: lsp_types::Position {
                            line: 3,
                            character: 0,
                        },
                        end: lsp_types::Position {
                            line: 3,
                            character: 13,
                        },
                    },
                    severity: Some(DiagnosticSeverity::ERROR),
                    source: Some("pbls".into()),
                    message: "Field number 1 has already been used in \"Foo\" by field \"i\""
                        .into(),
                    ..Default::default()
                },
            ]
        );
    }

    #[test]
    fn test_warnings() {
        let _ = env_logger::builder().is_test(true).try_init();
        let tmp = tempfile::tempdir().unwrap();

        proto(&tmp, "bar.proto", &["syntax = \"proto3\";"]);

        let (uri, text) = proto(
            &tmp,
            "foo.proto",
            &["syntax = \"proto3\";", "import \"bar.proto\";"],
        );

        let diags = diags(&uri, &text, &vec![tmp.path().to_path_buf()]).unwrap();

        assert_eq!(
            diags,
            vec![Diagnostic {
                range: Range {
                    start: lsp_types::Position {
                        line: 1,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 1,
                        character: 19,
                    },
                },
                severity: Some(DiagnosticSeverity::WARNING),
                source: Some("pbls".into()),
                message: "Import bar.proto is unused".into(),
                ..Default::default()
            },]
        );
    }
}
