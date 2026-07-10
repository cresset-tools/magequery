//! The Zed extension for magequery: locate (or fetch) the `magequery` binary and hand
//! Zed the `magequery lsp` command. All language smarts live in the server.

use zed_extension_api::{self as zed, LanguageServerId, Result};

struct MagequeryExtension {
    cached_binary_path: Option<String>,
}

impl MagequeryExtension {
    fn server_path(&mut self, id: &LanguageServerId, worktree: &zed::Worktree) -> Result<String> {
        // A user-installed binary wins: same resolution order as the VS Code client.
        if let Some(path) = worktree.which("magequery") {
            return Ok(path);
        }
        if let Some(path) = &self.cached_binary_path {
            if std::fs::metadata(path).is_ok_and(|meta| meta.is_file()) {
                return Ok(path.clone());
            }
        }

        zed::set_language_server_installation_status(
            id,
            &zed::LanguageServerInstallationStatus::CheckingForUpdate,
        );
        let release = zed::latest_github_release(
            "cresset-tools/magequery",
            zed::GithubReleaseOptions { require_assets: true, pre_release: false },
        )?;

        let (platform, arch) = zed::current_platform();
        let triple = match (platform, arch) {
            (zed::Os::Mac, zed::Architecture::Aarch64) => "aarch64-apple-darwin",
            (zed::Os::Mac, zed::Architecture::X8664) => "x86_64-apple-darwin",
            (zed::Os::Linux, zed::Architecture::Aarch64) => "aarch64-unknown-linux-gnu",
            (zed::Os::Linux, zed::Architecture::X8664) => "x86_64-unknown-linux-gnu",
            (zed::Os::Windows, zed::Architecture::X8664) => "x86_64-pc-windows-msvc",
            _ => return Err(format!("no prebuilt magequery binary for {platform:?}/{arch:?}")),
        };
        let (extension, file_type) = match platform {
            zed::Os::Windows => ("zip", zed::DownloadedFileType::Zip),
            _ => ("tar.gz", zed::DownloadedFileType::GzipTar),
        };
        let asset_name = format!("magequery-{triple}.{extension}");
        let asset = release
            .assets
            .iter()
            .find(|asset| asset.name == asset_name)
            .ok_or_else(|| format!("release {} has no asset {asset_name}", release.version))?;

        // release.version is the tag (`magequery-v0.4.0`); one directory per version so
        // an upgrade is a fresh download and stale versions can be swept.
        let version_dir = release.version.clone();
        let binary_name = match platform {
            zed::Os::Windows => "magequery.exe",
            _ => "magequery",
        };
        let binary_path = format!("{version_dir}/{binary_name}");

        if std::fs::metadata(&binary_path).is_err() {
            zed::set_language_server_installation_status(
                id,
                &zed::LanguageServerInstallationStatus::Downloading,
            );
            zed::download_file(&asset.download_url, &version_dir, file_type)?;
            zed::make_file_executable(&binary_path)?;

            // Sweep older version directories.
            if let Ok(entries) = std::fs::read_dir(".") {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name != version_dir && name.starts_with("magequery-v") {
                        let _ = std::fs::remove_dir_all(entry.path());
                    }
                }
            }
        }

        self.cached_binary_path = Some(binary_path.clone());
        Ok(binary_path)
    }
}

impl zed::Extension for MagequeryExtension {
    fn new() -> Self {
        Self { cached_binary_path: None }
    }

    fn language_server_command(
        &mut self,
        id: &LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<zed::Command> {
        Ok(zed::Command {
            command: self.server_path(id, worktree)?,
            args: vec!["lsp".to_string()],
            env: Default::default(),
        })
    }
}

zed::register_extension!(MagequeryExtension);
