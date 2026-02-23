use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Instant, SystemTime};

use serde_json::Value;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

struct VersionCache {
    versions: HashMap<String, String>,
    lock_path: PathBuf,
    lock_mtime: SystemTime,
    fetched_at: Instant,
}

struct ComposerLsp {
    workspace_root: Mutex<Option<PathBuf>>,
    documents: Mutex<HashMap<Url, String>>,
    cache: Mutex<Option<VersionCache>>,
}

impl ComposerLsp {
    fn new(_client: Client) -> Self {
        Self {
            workspace_root: Mutex::new(None),
            documents: Mutex::new(HashMap::new()),
            cache: Mutex::new(None),
        }
    }

    fn resolve_lock_path(&self, composer_json_uri: &Url) -> Option<PathBuf> {
        if let Ok(json_path) = composer_json_uri.to_file_path() {
            let lock_path = json_path.with_file_name("composer.lock");
            if lock_path.exists() {
                return Some(lock_path);
            }
        }

        let root = self.workspace_root.lock().ok()?.clone()?;
        let lock_path = root.join("composer.lock");
        lock_path.exists().then_some(lock_path)
    }

    fn get_versions(&self, composer_json_uri: &Url) -> HashMap<String, String> {
        let lock_path = match self.resolve_lock_path(composer_json_uri) {
            Some(p) => p,
            None => return HashMap::new(),
        };

        let mtime = lock_path
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);

        // Return cached versions if the lock file hasn't changed and cache is < 5s old
        if let Ok(cache) = self.cache.lock() {
            if let Some(ref c) = *cache {
                if c.lock_path == lock_path
                    && c.lock_mtime == mtime
                    && c.fetched_at.elapsed().as_secs() < 5
                {
                    return c.versions.clone();
                }
            }
        }

        let versions = self.parse_lock_file(&lock_path);

        if let Ok(mut cache) = self.cache.lock() {
            *cache = Some(VersionCache {
                versions: versions.clone(),
                lock_path,
                lock_mtime: mtime,
                fetched_at: Instant::now(),
            });
        }

        versions
    }

    fn parse_lock_file(&self, lock_path: &PathBuf) -> HashMap<String, String> {
        let content = match std::fs::read_to_string(lock_path) {
            Ok(c) => c,
            Err(_) => return HashMap::new(),
        };
        let lock: Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(_) => return HashMap::new(),
        };

        let mut map = HashMap::new();
        for key in &["packages", "packages-dev"] {
            if let Some(packages) = lock.get(key).and_then(|v| v.as_array()) {
                for pkg in packages {
                    if let (Some(name), Some(version)) = (
                        pkg.get("name").and_then(|v| v.as_str()),
                        pkg.get("version").and_then(|v| v.as_str()),
                    ) {
                        let version = version.strip_prefix('v').unwrap_or(version);
                        map.insert(name.to_string(), version.to_string());
                    }
                }
            }
        }
        map
    }

    fn compute_hints(&self, uri: &Url, text: &str) -> Vec<InlayHint> {
        let versions = self.get_versions(uri);
        if versions.is_empty() {
            return vec![];
        }

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

            brace_depth += count_braces(trimmed);

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

                    hints.push(InlayHint {
                        position: Position {
                            line: line_idx as u32,
                            character: line.len() as u32,
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

        if !uri.path().ends_with("composer.json") {
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

        Ok(Some(self.compute_hints(uri, &text)))
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
