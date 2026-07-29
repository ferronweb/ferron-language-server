use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;

use ferronconf::{Config, Statement};
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

use crate::directives;

#[derive(Debug)]
struct Backend {
    client: Client,
    path_to_ferron: PathBuf,
    documents: tokio::sync::RwLock<HashMap<Url, String>>,
    directives: tokio::sync::RwLock<Option<directives::DirectiveRegistry>>,
}

/// The chain of blocks from the file root down to the cursor position.
#[derive(Debug, Clone)]
#[allow(dead_code)]
enum BlockContext {
    TopLevel,
    GlobalBlock,
    HostBlock {
        protocol: Option<String>,
    },
    SnippetBlock {
        name: String,
    },
    DirectiveBlock {
        name: String,
        subblock_link: Option<String>,
        parent: Box<BlockContext>,
    },
}

impl Backend {
    async fn fetch_directives(&self) {
        let registry = directives::DirectiveRegistry::fetch(&self.path_to_ferron).await;
        *self.directives.write().await = Some(registry);
    }

    fn publish_diagnostics_for_uri(&self, uri: &Url, text: &str) -> Vec<Diagnostic> {
        match Config::from_str(text) {
            Ok(config) => self.lsp_diagnostics_from_config(uri, &config),
            Err(parse_err) => vec![Diagnostic {
                range: Range {
                    start: Position {
                        line: (parse_err.span.line as u32).saturating_sub(1),
                        character: (parse_err.span.column as u32).saturating_sub(1),
                    },
                    end: Position {
                        line: (parse_err.span.line as u32).saturating_sub(1),
                        character: parse_err.span.column as u32,
                    },
                },
                severity: Some(DiagnosticSeverity::ERROR),
                message: parse_err.message.clone(),
                source: Some("ferronconf".to_string()),
                ..Default::default()
            }],
        }
    }

    fn lsp_diagnostics_from_config(&self, _uri: &Url, _config: &Config) -> Vec<Diagnostic> {
        Vec::new()
    }

    /// Extract the word under the cursor and its column span.
    fn word_at_position(text: &str, position: Position) -> Option<(String, u32, u32)> {
        let lines: Vec<&str> = text.lines().collect();
        let line_idx = position.line as usize;
        if line_idx >= lines.len() {
            return None;
        }
        let line = lines[line_idx];
        let chars: Vec<char> = line.chars().collect();
        let col = position.character as usize;
        if col > chars.len() {
            return None;
        }

        let mut start = col;
        while start > 0 && chars[start - 1] != ' ' && chars[start - 1] != '\t' {
            start -= 1;
        }
        let mut end = col;
        while end < chars.len() && chars[end] != ' ' && chars[end] != '\t' {
            end += 1;
        }
        if start == end {
            return None;
        }
        let word: String = chars[start..end].iter().collect();
        if !word.is_empty()
            && word
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
        {
            Some((word, start as u32, end as u32))
        } else {
            None
        }
    }

    /// Returns true when the cursor sits on the first word of the line (the directive name position).
    fn is_first_word_on_line(text: &str, position: Position) -> bool {
        let (_, start_col, _) = match Self::word_at_position(text, position) {
            Some(v) => v,
            None => return false,
        };
        let lines: Vec<&str> = text.lines().collect();
        let line_idx = position.line as usize;
        if let Some(line) = lines.get(line_idx) {
            if let Some(first_col) = line.chars().position(|c| !c.is_whitespace()) {
                return start_col as usize == first_col;
            }
        }
        false
    }

    /// Look up the subblock_link for a directive by searching the section that
    /// corresponds to the current block context (instead of the flat `by_name` map).
    fn resolve_subblock_link(
        directive_name: &str,
        context: &BlockContext,
        registry: &directives::DirectiveRegistry,
    ) -> Option<String> {
        match context {
            BlockContext::GlobalBlock | BlockContext::HostBlock { .. } => registry
                .sections
                .get("default")
                .and_then(|entries| entries.iter().find(|e| e.name == directive_name))
                .and_then(|e| e.subblock_link.clone()),
            BlockContext::DirectiveBlock {
                subblock_link: Some(section),
                ..
            } => registry
                .sections
                .get(section.as_str())
                .and_then(|entries| entries.iter().find(|e| e.name == directive_name))
                .and_then(|e| e.subblock_link.clone()),
            BlockContext::SnippetBlock { .. } => registry
                .sections
                .values()
                .flatten()
                .find(|e| e.name == directive_name)
                .and_then(|e| e.subblock_link.clone()),
            _ => None,
        }
    }

    /// Look up a directive entry in the section appropriate for the given context.
    fn lookup_entry_in_context(
        directive_name: &str,
        context: &BlockContext,
        registry: &directives::DirectiveRegistry,
    ) -> Option<directives::DirectiveEntry> {
        match context {
            BlockContext::TopLevel => {
                if directive_name == "include" {
                    // Hard-coded definintion for "include" top-level directive
                    Some(directives::DirectiveEntry {
                        name: "include".to_string(),
                        usage: "include <path>".to_string(),
                        description: "Include another configuration file. Supports glob patterns."
                            .to_string(),
                        applicable_protocols: None,
                        global_only: false,
                        subblock_link: None,
                    })
                } else {
                    None
                }
            }
            BlockContext::GlobalBlock | BlockContext::HostBlock { .. } => registry
                .sections
                .get("default")
                .and_then(|entries| entries.iter().find(|e| e.name == directive_name))
                .cloned(),
            BlockContext::DirectiveBlock {
                subblock_link: Some(section),
                ..
            } => registry
                .sections
                .get(section.as_str())
                .and_then(|entries| entries.iter().find(|e| e.name == directive_name))
                .cloned(),
            BlockContext::SnippetBlock { .. } => registry
                .sections
                .values()
                .flatten()
                .find(|e| e.name == directive_name)
                .cloned(),
            _ => None,
        }
    }

    /// Find the position (0-indexed) of the closing `}` that matches the `{` at `open_brace_line`.
    fn find_block_end_pos(text: &str, open_brace_line: usize) -> (usize, usize) {
        let lines: Vec<&str> = text.lines().collect();
        let mut depth = 0i32;
        #[allow(clippy::needless_range_loop)]
        for i in open_brace_line..lines.len() {
            for (j, ch) in lines[i].chars().enumerate() {
                match ch {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            return (i, j);
                        }
                    }
                    _ => {}
                }
            }
        }
        (
            lines.len().saturating_sub(1),
            lines.last().map(|l| l.len().saturating_sub(1)).unwrap_or(0),
        )
    }

    /// Determine the block context at the given cursor position using the parsed AST.
    fn find_context_at_position(
        config: &Config,
        text: &str,
        position: Position,
        registry: &directives::DirectiveRegistry,
    ) -> BlockContext {
        let cursor_line = position.line as usize;
        let cursor_col = position.character as usize;
        Self::context_for_statements(
            &config.statements,
            text,
            cursor_line,
            cursor_col,
            BlockContext::TopLevel,
            registry,
        )
    }

    fn context_for_statements(
        statements: &[Statement],
        text: &str,
        cursor_line: usize,
        cursor_col: usize,
        current: BlockContext,
        registry: &directives::DirectiveRegistry,
    ) -> BlockContext {
        let mut best = current;

        for stmt in statements.iter().rev() {
            let stmt_line = stmt.span().line.saturating_sub(1);
            if stmt_line > cursor_line {
                continue;
            }

            match stmt {
                Statement::HostBlock(hb) => {
                    let brace_line = hb.block.span.line.saturating_sub(1);
                    let brace_col = hb.block.span.column.saturating_sub(1);
                    let (end_line, end_col) = Self::find_block_end_pos(text, brace_line);
                    if (cursor_line > stmt_line
                        || (cursor_line == stmt_line && cursor_col >= brace_col))
                        && (cursor_line <= end_line
                            || (cursor_line == end_line && cursor_col <= end_col))
                    {
                        let protocol = hb.hosts.first().and_then(|h| h.protocol.clone());
                        let ctx = BlockContext::HostBlock { protocol };
                        best = Self::context_for_statements(
                            &hb.block.statements,
                            text,
                            cursor_line,
                            cursor_col,
                            ctx,
                            registry,
                        );
                        break;
                    }
                }
                Statement::GlobalBlock(block) => {
                    let brace_line = block.span.line.saturating_sub(1);
                    let brace_col = block.span.column.saturating_sub(1);
                    let (end_line, end_col) = Self::find_block_end_pos(text, brace_line);
                    if (cursor_line > stmt_line
                        || (cursor_line == stmt_line && cursor_col >= brace_col))
                        && (cursor_line <= end_line
                            || (cursor_line == end_line && cursor_col <= end_col))
                    {
                        best = Self::context_for_statements(
                            &block.statements,
                            text,
                            cursor_line,
                            cursor_col,
                            BlockContext::GlobalBlock,
                            registry,
                        );
                        break;
                    }
                }
                Statement::SnippetBlock(sb) => {
                    let brace_line = sb.block.span.line.saturating_sub(1);
                    let brace_col = sb.block.span.column.saturating_sub(1);
                    let (end_line, end_col) = Self::find_block_end_pos(text, brace_line);
                    if (cursor_line > stmt_line
                        || (cursor_line == stmt_line && cursor_col >= brace_col))
                        && (cursor_line <= end_line
                            || (cursor_line == end_line && cursor_col <= end_col))
                    {
                        let ctx = BlockContext::SnippetBlock {
                            name: sb.name.clone(),
                        };
                        best = Self::context_for_statements(
                            &sb.block.statements,
                            text,
                            cursor_line,
                            cursor_col,
                            ctx,
                            registry,
                        );
                        break;
                    }
                }
                Statement::Directive(d) => {
                    if let Some(block) = &d.block {
                        let brace_line = block.span.line.saturating_sub(1);
                        let brace_col = block.span.column.saturating_sub(1);
                        let (end_line, end_col) = Self::find_block_end_pos(text, brace_line);
                        if (cursor_line > stmt_line
                            || (cursor_line == stmt_line && cursor_col >= brace_col))
                            && (cursor_line <= end_line
                                || (cursor_line == end_line && cursor_col <= end_col))
                        {
                            let subblock_link =
                                Self::resolve_subblock_link(&d.name, &best, registry);
                            let ctx = BlockContext::DirectiveBlock {
                                name: d.name.clone(),
                                subblock_link,
                                parent: Box::new(best),
                            };
                            best = Self::context_for_statements(
                                &block.statements,
                                text,
                                cursor_line,
                                cursor_col,
                                ctx,
                                registry,
                            );
                            break;
                        }
                    }
                }
                _ => {}
            }
        }

        best
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        self.fetch_directives().await;

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::FULL),
                        will_save: Some(true),
                        save: Some(TextDocumentSyncSaveOptions::Supported(true)),
                        ..Default::default()
                    },
                )),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                completion_provider: Some(CompletionOptions::default()),
                document_formatting_provider: Some(OneOf::Left(true)),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: crate::build::PROJECT_NAME.to_string(),
                version: Some(crate::build::VERSION.to_string()),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "server initialized!")
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        let text = params.text_document.text.clone();
        let version = params.text_document.version;

        self.documents
            .write()
            .await
            .insert(uri.clone(), text.clone());

        let diagnostics = self.publish_diagnostics_for_uri(&uri, &text);
        self.client
            .publish_diagnostics(uri, diagnostics, Some(version))
            .await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        let version = params.text_document.version;

        if let Some(last) = params.content_changes.last() {
            let text = last.text.clone();
            self.documents
                .write()
                .await
                .insert(uri.clone(), text.clone());

            let diagnostics = self.publish_diagnostics_for_uri(&uri, &text);
            self.client
                .publish_diagnostics(uri, diagnostics, Some(version))
                .await;
        }
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        let file_path = match uri.to_file_path() {
            Ok(p) => p,
            Err(_) => return,
        };

        // Reject everything that's not "ferron.conf"
        // This is because Zed triggers didSave for every file, not just ferron.conf
        if file_path.file_name() != Some("ferron.conf".as_ref())
            && file_path
                .file_name()
                .is_none_or(|f| !f.to_string_lossy().ends_with(".ferron"))
            && file_path
                .file_name()
                .is_none_or(|f| !f.to_string_lossy().ends_with(".ferron.conf"))
        {
            return;
        }

        let result =
            directives::run_doctor(&self.path_to_ferron, &file_path.to_string_lossy()).await;

        let mut diagnostics = Vec::new();
        let read_file = std::fs::read_to_string(&file_path).unwrap_or_default();
        for d in result.diagnostics {
            let line = d.line.unwrap_or(1).saturating_sub(1) as u32;
            let col = d.column.unwrap_or(1).saturating_sub(1) as u32;

            let severity = match d.kind.as_str() {
                "Invalid configuration" => Some(DiagnosticSeverity::ERROR),
                "Unknown directive" => Some(DiagnosticSeverity::WARNING),
                "Best practice violation" => Some(DiagnosticSeverity::INFORMATION),
                _ => Some(DiagnosticSeverity::WARNING),
            };

            let end_column = read_file.lines().nth(line as usize).map_or(1, |l| {
                let start_col = col + 1;
                if start_col as usize >= l.len() {
                    return l.len();
                }
                l[start_col as usize..]
                    .find(|c: char| c.is_whitespace())
                    .map_or(l.len(), |i| start_col as usize + i)
            }) as u32;

            diagnostics.push(Diagnostic {
                range: Range {
                    start: Position {
                        line,
                        character: col,
                    },
                    end: Position {
                        line,
                        character: end_column,
                    },
                },
                severity,
                message: d.message,
                source: Some("ferron doctor".to_string()),
                ..Default::default()
            });
        }
        self.client
            .publish_diagnostics(uri, diagnostics, None)
            .await;
    }

    async fn formatting(&self, params: DocumentFormattingParams) -> Result<Option<Vec<TextEdit>>> {
        let uri = params.text_document.uri.clone();
        let text = match self.documents.read().await.get(&uri) {
            Some(t) => t.clone(),
            None => return Ok(None),
        };

        let ferron_fmt = self.path_to_ferron.join("ferron-fmt");

        if !ferron_fmt.exists() {
            return Ok(None);
        }

        let indent_width = params.options.tab_size.to_string();

        let mut child = tokio::process::Command::new(&ferron_fmt)
            .arg("--indent-width")
            .arg(&indent_width)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|_| {
                tower_lsp::jsonrpc::Error::new(tower_lsp::jsonrpc::ErrorCode::InternalError)
            })?;

        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            let _ = stdin.write_all(text.as_bytes()).await;
            drop(stdin);
        }

        let output = child.wait_with_output().await.map_err(|_| {
            tower_lsp::jsonrpc::Error::new(tower_lsp::jsonrpc::ErrorCode::InternalError)
        })?;

        if !output.status.success() {
            return Ok(None);
        }

        let formatted_text = String::from_utf8_lossy(&output.stdout).to_string();

        if formatted_text == text {
            return Ok(None);
        }

        let last_line = text.lines().count().saturating_sub(1) as u32;
        let last_col = text.lines().last().map_or(0, |l| l.len() as u32);

        Ok(Some(vec![TextEdit {
            range: Range {
                start: Position::new(0, 0),
                end: Position::new(last_line, last_col),
            },
            new_text: formatted_text,
        }]))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = params
            .text_document_position_params
            .text_document
            .uri
            .clone();
        let position = params.text_document_position_params.position;

        let text = match self.documents.read().await.get(&uri) {
            Some(t) => t.clone(),
            None => return Ok(None),
        };

        let (word, start_col, end_col) = match Self::word_at_position(&text, position) {
            Some(v) => v,
            None => return Ok(None),
        };

        // Only show hover for directive names (first word on the line), not for values.
        if !Self::is_first_word_on_line(&text, position) {
            return Ok(None);
        }

        let directives = self.directives.read().await;
        let registry = match directives.as_ref() {
            Some(r) => r,
            None => return Ok(None),
        };

        // Parse to get context, then look up the directive in the appropriate section.
        let config = Config::from_str(&text);
        let context = config
            .as_ref()
            .ok()
            .map(|c| Self::find_context_at_position(c, &text, position, registry))
            .unwrap_or(BlockContext::TopLevel);

        let entry = Self::lookup_entry_in_context(&word, &context, registry);

        let entry = match entry {
            Some(e) => e,
            None => return Ok(None),
        };

        let mut md = String::new();
        md.push_str(&format!("**`{}`**\n\n", entry.name));
        md.push_str(&entry.description);
        md.push_str("\n\n---\n\n");
        md.push_str(&format!("**Usage:** `{}`\n\n", entry.usage));
        if let Some(ref protocols) = entry.applicable_protocols {
            md.push_str(&format!("**Protocols:** {}\n", protocols.join(", ")));
        }
        if entry.global_only {
            md.push_str("**Scope:** global only\n");
        }
        if let Some(ref subblock) = entry.subblock_link {
            md.push_str(&format!("**Subblock:** `{}`\n", subblock));
        }

        let range = Range {
            start: Position {
                line: position.line,
                character: start_col,
            },
            end: Position {
                line: position.line,
                character: end_col,
            },
        };

        Ok(Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: md,
            }),
            range: Some(range),
        }))
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = params.text_document_position.text_document.uri.clone();
        let position = params.text_document_position.position;

        let text = match self.documents.read().await.get(&uri) {
            Some(t) => t.clone(),
            None => return Ok(None),
        };

        let current_word = Self::word_at_position(&text, position)
            .map(|(w, _, _)| w.to_lowercase())
            .unwrap_or_default();

        // Only show completions for directive names (first word on the line), not for values.
        if !current_word.is_empty() && !Self::is_first_word_on_line(&text, position) {
            return Ok(None);
        }

        let directives = self.directives.read().await;
        let registry = match directives.as_ref() {
            Some(r) => r,
            None => return Ok(None),
        };

        // Parse the document and determine context.
        let config = Config::from_str(&text);
        let context = config
            .as_ref()
            .ok()
            .map(|c| Self::find_context_at_position(c, &text, position, registry))
            .unwrap_or(BlockContext::TopLevel);

        let items: Vec<CompletionItem> = match &context {
            BlockContext::TopLevel => {
                // Top-level: only "include" and block openers (host, match, snippet, global).
                let mut items: Vec<CompletionItem> = Vec::new();

                let include_item = CompletionItem {
                    label: "include".to_string(),
                    kind: Some(CompletionItemKind::KEYWORD),
                    detail: Some("include <path>".to_string()),
                    documentation: Some(Documentation::String(
                        "Include another configuration file. Supports glob patterns.".to_string(),
                    )),
                    insert_text: Some("include ".to_string()),
                    insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
                    ..Default::default()
                };
                if current_word.is_empty() || "include".starts_with(&current_word) {
                    items.push(include_item);
                }

                let snippet_item = CompletionItem {
                    label: "snippet".to_string(),
                    kind: Some(CompletionItemKind::KEYWORD),
                    detail: Some("snippet <name> { ... }".to_string()),
                    documentation: Some(Documentation::String(
                        "Define a reusable configuration snippet.".to_string(),
                    )),
                    insert_text: Some("snippet ${1:name} {\n\t$0\n}".to_string()),
                    insert_text_format: Some(InsertTextFormat::SNIPPET),
                    ..Default::default()
                };
                if current_word.is_empty() || "snippet".starts_with(&current_word) {
                    items.push(snippet_item);
                }

                let match_item = CompletionItem {
                    label: "match".to_string(),
                    kind: Some(CompletionItemKind::KEYWORD),
                    detail: Some("match <name> { ... }".to_string()),
                    documentation: Some(Documentation::String(
                        "Define a conditional matcher for request attributes.".to_string(),
                    )),
                    insert_text: Some("match ${1:name} {\n\t$0\n}".to_string()),
                    insert_text_format: Some(InsertTextFormat::SNIPPET),
                    ..Default::default()
                };
                if current_word.is_empty() || "match".starts_with(&current_word) {
                    items.push(match_item);
                }

                items
            }

            BlockContext::GlobalBlock => registry
                .sections
                .get("default")
                .map(|entries| {
                    entries
                        .iter()
                        .filter(|e| {
                            current_word.is_empty()
                                || e.name.to_lowercase().starts_with(&current_word)
                        })
                        .map(|e| {
                            let mut item =
                                CompletionItem::new_simple(e.name.clone(), e.usage.clone());
                            item.kind = Some(CompletionItemKind::KEYWORD);
                            item.detail = Some(e.usage.clone());
                            item.documentation = Some(Documentation::String(e.description.clone()));
                            item.insert_text = Some(format!("{} ", e.name));
                            item.insert_text_format = Some(InsertTextFormat::PLAIN_TEXT);
                            item
                        })
                        .collect()
                })
                .unwrap_or_default(),

            BlockContext::HostBlock { protocol } => {
                let proto = protocol.as_deref().unwrap_or("http");
                registry
                    .sections
                    .get("default")
                    .map(|entries| {
                        entries
                            .iter()
                            .filter(|e| {
                                if e.global_only {
                                    return false;
                                }
                                match &e.applicable_protocols {
                                    Some(protocols) => protocols.iter().any(|p| p == proto),
                                    None => true,
                                }
                            })
                            .filter(|e| {
                                current_word.is_empty()
                                    || e.name.to_lowercase().starts_with(&current_word)
                            })
                            .map(|e| {
                                let mut item =
                                    CompletionItem::new_simple(e.name.clone(), e.usage.clone());
                                item.kind = Some(CompletionItemKind::KEYWORD);
                                item.detail = Some(e.usage.clone());
                                item.documentation =
                                    Some(Documentation::String(e.description.clone()));
                                item.insert_text = Some(format!("{} ", e.name));
                                item.insert_text_format = Some(InsertTextFormat::PLAIN_TEXT);
                                item
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            }

            BlockContext::DirectiveBlock {
                subblock_link: Some(section),
                ..
            } => registry
                .sections
                .get(section.as_str())
                .map(|entries| {
                    entries
                        .iter()
                        .filter(|e| {
                            current_word.is_empty()
                                || e.name.to_lowercase().starts_with(&current_word)
                        })
                        .map(|e| {
                            let mut item =
                                CompletionItem::new_simple(e.name.clone(), e.usage.clone());
                            item.kind = Some(CompletionItemKind::KEYWORD);
                            item.detail = Some(e.usage.clone());
                            item.documentation = Some(Documentation::String(e.description.clone()));
                            item.insert_text = Some(format!("{} ", e.name));
                            item.insert_text_format = Some(InsertTextFormat::PLAIN_TEXT);
                            item
                        })
                        .collect()
                })
                .unwrap_or_default(),

            BlockContext::DirectiveBlock {
                subblock_link: None,
                ..
            } => Vec::new(),

            BlockContext::SnippetBlock { .. } => {
                // Inside a snippet block, suggest from all sections (same as host block).
                registry
                    .sections
                    .values()
                    .flatten()
                    .filter(|e| {
                        current_word.is_empty() || e.name.to_lowercase().starts_with(&current_word)
                    })
                    .map(|e| {
                        let mut item = CompletionItem::new_simple(e.name.clone(), e.usage.clone());
                        item.kind = Some(CompletionItemKind::KEYWORD);
                        item.detail = Some(e.usage.clone());
                        item.documentation = Some(Documentation::String(e.description.clone()));
                        item.insert_text = Some(format!("{} ", e.name));
                        item.insert_text_format = Some(InsertTextFormat::PLAIN_TEXT);
                        item
                    })
                    .collect()
            }
        };

        Ok(Some(CompletionResponse::Array(items)))
    }
}

pub async fn lsp_main(path_to_ferron: PathBuf) -> std::io::Result<()> {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(|client| Backend {
        client,
        path_to_ferron,
        documents: tokio::sync::RwLock::new(HashMap::new()),
        directives: tokio::sync::RwLock::new(None),
    });
    Server::new(stdin, stdout, socket).serve(service).await;
    Ok(())
}
