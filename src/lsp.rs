//! A small language server for `yatr.toml`.
//!
//! `yatr lsp` speaks LSP over stdio so editors get live **diagnostics** (parse
//! and validation errors as you type) and a **document outline** of tasks.
//!
//! The logic lives in pure, testable functions ([`compute_diagnostics`],
//! [`document_symbols`]); the protocol layer is a thin wrapper around them.

// `Uri` keys are flagged as "mutable" by clippy but are effectively immutable
// here; the generic error mapper takes its value by value by design.
#![allow(clippy::mutable_key_type, clippy::needless_pass_by_value)]

use std::collections::HashMap;

use lsp_server::{Connection, Message, Notification, Response};
use lsp_types::{
    request::DocumentSymbolRequest, Diagnostic, DiagnosticSeverity, DocumentSymbol,
    DocumentSymbolParams, DocumentSymbolResponse, OneOf, Position, PublishDiagnosticsParams, Range,
    ServerCapabilities, SymbolKind, TextDocumentSyncCapability, TextDocumentSyncKind, Uri,
};

use crate::config::Config;
use crate::error::{Result, YatrError};
use crate::graph::TaskGraph;

fn other<E: std::fmt::Debug>(e: E) -> YatrError {
    YatrError::Io(std::io::Error::other(format!("{e:?}")))
}

/// Run the language server over stdio until the client shuts it down.
pub fn run() -> Result<()> {
    let (connection, io_threads) = Connection::stdio();

    let capabilities = ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        document_symbol_provider: Some(OneOf::Left(true)),
        ..Default::default()
    };
    let init = serde_json::to_value(capabilities).map_err(other)?;
    connection.initialize(init).map_err(other)?;

    // `main_loop` takes the connection by value so it is dropped (closing the
    // I/O channels) before we join the stdio threads — otherwise join() hangs.
    main_loop(connection)?;
    io_threads.join().map_err(other)?;
    Ok(())
}

fn main_loop(connection: Connection) -> Result<()> {
    let mut docs: HashMap<Uri, String> = HashMap::new();

    for msg in &connection.receiver {
        match msg {
            Message::Request(req) => {
                if connection.handle_shutdown(&req).map_err(other)? {
                    return Ok(());
                }
                if req.method == <DocumentSymbolRequest as lsp_types::request::Request>::METHOD {
                    let (id, params) = req
                        .extract::<DocumentSymbolParams>(
                            <DocumentSymbolRequest as lsp_types::request::Request>::METHOD,
                        )
                        .map_err(other)?;
                    let symbols = docs
                        .get(&params.text_document.uri)
                        .map(|t| document_symbols(t))
                        .unwrap_or_default();
                    let resp = Response::new_ok(id, DocumentSymbolResponse::Nested(symbols));
                    connection
                        .sender
                        .send(Message::Response(resp))
                        .map_err(other)?;
                }
            }
            Message::Notification(not) => match not.method.as_str() {
                "textDocument/didOpen" => {
                    let p: lsp_types::DidOpenTextDocumentParams =
                        serde_json::from_value(not.params).map_err(other)?;
                    let text = p.text_document.text;
                    publish(&connection, &p.text_document.uri, &text)?;
                    docs.insert(p.text_document.uri, text);
                }
                "textDocument/didChange" => {
                    let p: lsp_types::DidChangeTextDocumentParams =
                        serde_json::from_value(not.params).map_err(other)?;
                    if let Some(change) = p.content_changes.into_iter().next_back() {
                        publish(&connection, &p.text_document.uri, &change.text)?;
                        docs.insert(p.text_document.uri, change.text);
                    }
                }
                _ => {}
            },
            Message::Response(_) => {}
        }
    }
    Ok(())
}

fn publish(connection: &Connection, uri: &Uri, text: &str) -> Result<()> {
    let params = PublishDiagnosticsParams {
        uri: uri.clone(),
        diagnostics: compute_diagnostics(text),
        version: None,
    };
    let not = Notification {
        method: "textDocument/publishDiagnostics".to_string(),
        params: serde_json::to_value(params).map_err(other)?,
    };
    connection
        .sender
        .send(Message::Notification(not))
        .map_err(other)?;
    Ok(())
}

/// Compute diagnostics for `yatr.toml` source text.
///
/// Reports TOML parse errors, then task-config validation, then dependency-graph
/// problems (cycles / missing deps — skipped when the file uses `include`, to
/// avoid cross-file false positives).
#[must_use]
pub fn compute_diagnostics(text: &str) -> Vec<Diagnostic> {
    let config = match toml::from_str::<Config>(text) {
        Ok(config) => config,
        Err(e) => {
            let range = e.span().map_or_else(zero_range, |s| Range {
                start: offset_to_position(text, s.start),
                end: offset_to_position(text, s.end),
            });
            return vec![diagnostic(range, e.message().to_string())];
        }
    };

    if let Err(e) = config.validate() {
        return vec![diagnostic(locate(text, &e), message_for(&e))];
    }
    if config.include.is_empty() {
        if let Err(e) = TaskGraph::from_config(&config) {
            return vec![diagnostic(locate(text, &e), message_for(&e))];
        }
    }
    Vec::new()
}

/// A document outline: one symbol per top-level `[tasks.<name>]`.
#[must_use]
#[allow(deprecated)] // DocumentSymbol::deprecated is a deprecated protocol field
pub fn document_symbols(text: &str) -> Vec<DocumentSymbol> {
    let mut symbols = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if let Some(name) = trimmed
            .strip_prefix("[tasks.")
            .and_then(|r| r.strip_suffix(']'))
        {
            if name.is_empty() || name.contains('.') {
                continue; // skip subtables like [tasks.x.env]
            }
            let range = line_range(i, line);
            symbols.push(DocumentSymbol {
                name: name.to_string(),
                detail: None,
                kind: SymbolKind::FUNCTION,
                tags: None,
                deprecated: None,
                range,
                selection_range: range,
                children: None,
            });
        }
    }
    symbols
}

fn message_for(e: &YatrError) -> String {
    match e {
        YatrError::InvalidTask { task, reason } => format!("task '{task}': {reason}"),
        YatrError::TaskNotFound { name, .. } => {
            format!("task '{name}' is referenced but not defined")
        }
        YatrError::CyclicDependency { cycle } => format!("circular dependency: {cycle}"),
        YatrError::InvalidConfig { message } => message.clone(),
        other => other.to_string(),
    }
}

/// Find a sensible range for an error — the offending task's header line if we
/// can identify it, otherwise the start of the file.
fn locate(text: &str, e: &YatrError) -> Range {
    let task = match e {
        YatrError::InvalidTask { task, .. } => Some(task.as_str()),
        YatrError::TaskNotFound { name, .. } => Some(name.as_str()),
        _ => None,
    };
    if let Some(name) = task {
        let header = format!("[tasks.{name}]");
        for (i, line) in text.lines().enumerate() {
            if line.trim() == header {
                return line_range(i, line);
            }
        }
    }
    zero_range()
}

fn diagnostic(range: Range, message: String) -> Diagnostic {
    Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::ERROR),
        source: Some("yatr".to_string()),
        message,
        ..Default::default()
    }
}

fn line_range(line: usize, text: &str) -> Range {
    let line = u32::try_from(line).unwrap_or(0);
    Range {
        start: Position { line, character: 0 },
        end: Position {
            line,
            character: u32::try_from(text.len()).unwrap_or(0),
        },
    }
}

const fn zero_range() -> Range {
    Range {
        start: Position {
            line: 0,
            character: 0,
        },
        end: Position {
            line: 0,
            character: 0,
        },
    }
}

/// Map a byte offset in `text` to an LSP line/character position.
fn offset_to_position(text: &str, offset: usize) -> Position {
    let mut line = 0u32;
    let mut line_start = 0usize;
    for (i, ch) in text.char_indices() {
        if i >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            line_start = i + 1;
        }
    }
    Position {
        line,
        character: u32::try_from(offset.saturating_sub(line_start)).unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_config_has_no_diagnostics() {
        let text = "[tasks.build]\nrun = [\"cargo build\"]\n";
        assert!(compute_diagnostics(text).is_empty());
    }

    #[test]
    fn parse_error_is_diagnosed() {
        let text = "[tasks.build\nrun = [\"x\"]\n"; // missing ]
        let diags = compute_diagnostics(text);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].severity, Some(DiagnosticSeverity::ERROR));
    }

    #[test]
    fn missing_dependency_is_diagnosed_at_the_task() {
        let text = "[tasks.build]\ndepends = [\"nope\"]\nrun = [\"x\"]\n";
        let diags = compute_diagnostics(text);
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("nope"));
    }

    #[test]
    fn symbols_list_top_level_tasks() {
        let text =
            "[tasks.build]\nrun=[\"x\"]\n[tasks.test]\nrun=[\"y\"]\n[tasks.test.env]\nA=\"1\"\n";
        let names: Vec<_> = document_symbols(text).into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["build", "test"]); // subtable [tasks.test.env] excluded
    }
}
