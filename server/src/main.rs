use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::Value;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

struct ComposerLsp {
    #[allow(dead_code)]
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

    fn read_lock_file(&self, composer_json_uri: &Url) -> Option<Value> {
        if let Ok(json_path) = composer_json_uri.to_file_path() {
            let lock_path = json_path.with_file_name("composer.lock");
            if let Ok(content) = std::fs::read_to_string(&lock_path) {
                if let Ok(parsed) = serde_json::from_str(&content) {
                    return Some(parsed);
                }
            }
        }

        let root = self.workspace_root.lock().ok()?.clone()?;
        let lock_path = root.join("composer.lock");
        let content = std::fs::read_to_string(lock_path).ok()?;
        serde_json::from_str(&content).ok()
    }

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

            if !in_require_section {
                if (trimmed.starts_with("\"require\"") || trimmed.starts_with("\"require-dev\""))
                    && trimmed.contains('{')
                {
                    in_require_section = true;
                    section_start_depth = brace_depth;
                    brace_depth += count_braces(trimmed);
                    continue;
                }
            }

            let brace_delta = count_braces(trimmed);
            brace_depth += brace_delta;

            if in_require_section {
                if brace_depth <= section_start_depth {
                    in_require_section = false;
                    continue;
                }

                if let Some(package_name) = extract_package_name(trimmed) {
                    let version_text = match versions.get(&package_name) {
                        Some(v) => v.clone(),
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
            }
        }

        hints
    }
}

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

fn extract_package_name(trimmed: &str) -> Option<String> {
    if !trimmed.starts_with('"') {
        return None;
    }
    let end_quote = trimmed[1..].find('"')? + 1;
    let name = &trimmed[1..end_quote];

    if !name.contains('/') {
        return None;
    }

    Some(name.to_string())
}

#[tower_lsp::async_trait]
impl LanguageServer for ComposerLsp {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        if let Some(root_uri) = params.root_uri.as_ref() {
            if let Ok(path) = root_uri.to_file_path() {
                *self.workspace_root.lock().unwrap() = Some(path);
            }
        } else if let Some(folders) = params.workspace_folders.as_ref() {
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

    async fn initialized(&self, _: InitializedParams) {}

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

        let path = uri.path();
        if !path.ends_with("composer.json") {
            return Ok(None);
        }

        let text = {
            let docs = self.documents.lock().unwrap();
            docs.get(uri).cloned()
        };
        let text = match text {
            Some(t) => t,
            None => return Ok(None),
        };

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
