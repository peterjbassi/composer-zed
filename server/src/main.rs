use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::Value;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

struct ComposerLsp {
    client: Client,
    workspace_root: Mutex<Option<PathBuf>>,
    documents: Mutex<HashMap<Url, String>>,
}

impl ComposerLsp {
    fn new(client: Client) -> Self {
        Self {
            client,
            workspace_root: Mutex::new(None),
            documents: Mutex::new(HashMap::new()),
        }
    }

    /// Read composer.lock from the same directory as the given composer.json URI,
    /// falling back to the workspace root.
    fn read_lock_file(&self, composer_json_uri: &Url) -> Option<Value> {
        // Try sibling composer.lock first
        if let Ok(json_path) = composer_json_uri.to_file_path() {
            let lock_path = json_path.with_file_name("composer.lock");
            if let Ok(content) = std::fs::read_to_string(&lock_path) {
                if let Ok(parsed) = serde_json::from_str(&content) {
                    return Some(parsed);
                }
            }
        }

        // Fall back to workspace root
        let root = self.workspace_root.lock().ok()?.clone()?;
        let lock_path = root.join("composer.lock");
        let content = std::fs::read_to_string(lock_path).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// Build a map of package name → installed version from composer.lock.
    fn build_version_map(&self, lock: &Value) -> HashMap<String, String> {
        let mut map = HashMap::new();
        for key in &["packages", "packages-dev"] {
            if let Some(packages) = lock.get(key).and_then(|v| v.as_array()) {
                for pkg in packages {
                    if let (Some(name), Some(version)) = (
                        pkg.get("name").and_then(|v| v.as_str()),
                        pkg.get("version").and_then(|v| v.as_str()),
                    ) {
                        map.insert(name.to_string(), version.to_string());
                    }
                }
            }
        }
        map
    }

    /// Parse document text and return inlay hints for dependency lines
    /// inside "require" and "require-dev" sections.
    fn compute_hints(&self, uri: &Url, text: &str) -> Vec<InlayHint> {
        let lock = match self.read_lock_file(uri) {
            Some(l) => l,
            None => return vec![],
        };
        let versions = self.build_version_map(&lock);

        let mut hints = Vec::new();
        let mut in_require_section = false;
        let mut brace_depth: i32 = 0;
        let mut section_start_depth: i32 = 0;

        for (line_idx, line) in text.lines().enumerate() {
            let trimmed = line.trim();

            // Detect entering a require or require-dev section.
            // Matches lines like: "require": {  or  "require-dev": {
            if !in_require_section {
                if (trimmed.starts_with("\"require\"") || trimmed.starts_with("\"require-dev\""))
                    && trimmed.contains('{')
                {
                    in_require_section = true;
                    section_start_depth = brace_depth;
                    // Count braces on this line
                    brace_depth += count_braces(trimmed);
                    continue;
                }
            }

            // Count braces to track depth
            let brace_delta = count_braces(trimmed);
            brace_depth += brace_delta;

            if in_require_section {
                // Check if we've exited the section
                if brace_depth <= section_start_depth {
                    in_require_section = false;
                    continue;
                }

                // Try to extract a package name from this line.
                // Lines look like: "vendor/package": "^1.0"
                if let Some(package_name) = extract_package_name(trimmed) {
                    let version_text = match versions.get(&package_name) {
                        Some(v) => format!("installed: {v}"),
                        None => "not installed".to_string(),
                    };

                    let line_len = line.len() as u32;
                    hints.push(InlayHint {
                        position: Position {
                            line: line_idx as u32,
                            character: line_len,
                        },
                        label: InlayHintLabel::String(version_text),
                        kind: None,
                        text_edits: None,
                        tooltip: None,
                        padding_left: Some(true),
                        padding_right: None,
                        data: None,
                    });
                }
            } else {
                // Not in a section yet — just track braces
            }
        }

        hints
    }
}

/// Count net brace changes in a line: +1 for '{', -1 for '}'.
fn count_braces(line: &str) -> i32 {
    let mut count = 0i32;
    let mut in_string = false;
    let mut escape = false;
    for ch in line.chars() {
        if escape {
            escape = false;
            continue;
        }
        match ch {
            '\\' if in_string => escape = true,
            '"' => in_string = !in_string,
            '{' if !in_string => count += 1,
            '}' if !in_string => count -= 1,
            _ => {}
        }
    }
    count
}

/// Extract the package name from a JSON line like `"vendor/package": "^1.0"`.
/// Returns None if the line doesn't look like a dependency entry.
fn extract_package_name(trimmed: &str) -> Option<String> {
    // Must start with a quote and contain a colon
    if !trimmed.starts_with('"') {
        return None;
    }
    let end_quote = trimmed[1..].find('"')? + 1;
    let name = &trimmed[1..end_quote];

    // Package names contain a slash (vendor/package).
    // This filters out section keys like "require", "require-dev", etc.
    if !name.contains('/') {
        return None;
    }

    Some(name.to_string())
}

#[tower_lsp::async_trait]
impl LanguageServer for ComposerLsp {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        // Store workspace root
        if let Some(root_uri) = params.root_uri {
            if let Ok(path) = root_uri.to_file_path() {
                *self.workspace_root.lock().unwrap() = Some(path);
            }
        } else if let Some(folders) = params.workspace_folders {
            if let Some(folder) = folders.first() {
                if let Ok(path) = folder.uri.to_file_path() {
                    *self.workspace_root.lock().unwrap() = Some(path);
                }
            }
        }

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                inlay_hint_provider: Some(OneOf::Left(true)),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "Composer LSP initialized")
            .await;
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        self.documents
            .lock()
            .unwrap()
            .insert(params.text_document.uri, params.text_document.text);
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        if let Some(change) = params.content_changes.into_iter().last() {
            self.documents
                .lock()
                .unwrap()
                .insert(params.text_document.uri, change.text);
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.documents
            .lock()
            .unwrap()
            .remove(&params.text_document.uri);
    }

    async fn inlay_hint(&self, params: InlayHintParams) -> Result<Option<Vec<InlayHint>>> {
        let uri = &params.text_document.uri;

        // Only provide hints for composer.json files
        let path = uri.path();
        if !path.ends_with("composer.json") {
            return Ok(None);
        }

        let docs = self.documents.lock().unwrap();
        let text = match docs.get(uri) {
            Some(t) => t.clone(),
            None => return Ok(None),
        };
        drop(docs);

        let hints = self.compute_hints(uri, &text);
        Ok(Some(hints))
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}

#[tokio::main]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(ComposerLsp::new);
    Server::new(stdin, stdout, socket).serve(service).await;
}
