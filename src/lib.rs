use zed_extension_api::{self as zed, Result};

struct ComposerExtension;

impl zed::Extension for ComposerExtension {
    fn new() -> Self {
        ComposerExtension
    }

    fn language_server_command(
        &mut self,
        _language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<zed::Command> {
        let path = worktree
            .which("composer-lsp")
            .ok_or_else(|| "composer-lsp not found on PATH. Build it with: cargo build -p composer-lsp --release".to_string())?;

        Ok(zed::Command {
            command: path,
            args: vec![],
            env: worktree.shell_env(),
        })
    }
}

zed::register_extension!(ComposerExtension);
