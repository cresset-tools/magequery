//! The event loop: workspace roots, the dirty/rebuild cycle, and request dispatch.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::RecvTimeoutError;
use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use lsp_types::notification::Notification as _;
use lsp_types::request::Request as _;
use lsp_types::Url;
use magequery_core::Magento;

use crate::{diag, features};

/// Quiet period after the last invalidating event before rebuilding. Save bursts (format
/// on save, `git checkout`) collapse into one rebuild; a request arriving mid-burst
/// forces the rebuild early so answers never come from a stale index.
const DEBOUNCE: Duration = Duration::from_millis(300);

pub(crate) fn capabilities() -> lsp_types::ServerCapabilities {
    use lsp_types::*;
    ServerCapabilities {
        // Save-based model: we want didOpen/didSave notifications, never didChange
        // content (core reads from disk).
        text_document_sync: Some(TextDocumentSyncCapability::Options(
            TextDocumentSyncOptions {
                open_close: Some(true),
                change: Some(TextDocumentSyncKind::NONE),
                save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                    include_text: Some(false),
                })),
                ..Default::default()
            },
        )),
        definition_provider: Some(OneOf::Left(true)),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        references_provider: Some(OneOf::Left(true)),
        code_lens_provider: Some(CodeLensOptions { resolve_provider: Some(false) }),
        ..Default::default()
    }
}

/// One Magento installation the server answers for. A workspace folder maps to at most
/// one root (via [`Magento::find_root`]); several folders inside the same install share
/// one entry.
pub(crate) struct Workspace {
    pub root: PathBuf,
    /// `None` until the first successful build, or after a rebuild that failed (a
    /// half-saved config.php) — requests then answer null rather than lying from a
    /// stale index.
    pub handle: Option<Arc<Magento>>,
    /// Files whose diagnostics we last published, so a rebuild clears the stale ones.
    published: HashSet<Url>,
    dirty: bool,
}

pub(crate) struct Server<'a> {
    connection: &'a Connection,
    workspaces: Vec<Workspace>,
    /// Client accepts dynamic `workspace/didChangeWatchedFiles` registration. When it
    /// doesn't (or events never arrive), `didSave` of an interesting file is the
    /// fallback invalidation — editing inside the editor still refreshes.
    can_watch: bool,
    /// Client understands `RelativePattern` watchers (LSP 3.17).
    relative_patterns: bool,
    next_request_id: i32,
}

impl<'a> Server<'a> {
    pub(crate) fn new(connection: &'a Connection, init: lsp_types::InitializeParams) -> Self {
        let watched = init
            .capabilities
            .workspace
            .as_ref()
            .and_then(|w| w.did_change_watched_files.as_ref());

        let mut server = Server {
            connection,
            workspaces: Vec::new(),
            can_watch: watched.and_then(|w| w.dynamic_registration).unwrap_or(false),
            relative_patterns: watched.and_then(|w| w.relative_pattern_support).unwrap_or(false),
            next_request_id: 0,
        };

        // Workspace folders (multi-root capable), with the deprecated rootUri as the
        // fallback for older clients.
        #[allow(deprecated)]
        let folders: Vec<Url> = match init.workspace_folders {
            Some(folders) if !folders.is_empty() => folders.into_iter().map(|f| f.uri).collect(),
            _ => init.root_uri.into_iter().collect(),
        };
        for uri in folders {
            let Ok(folder) = uri.to_file_path() else { continue };
            match Magento::find_root(&folder) {
                Some(root) => {
                    if !server.workspaces.iter().any(|ws| ws.root == root) {
                        server.workspaces.push(Workspace {
                            root,
                            handle: None,
                            published: HashSet::new(),
                            dirty: true,
                        });
                    }
                }
                None => server.log(
                    lsp_types::MessageType::INFO,
                    format!(
                        "no Magento installation found for workspace folder {} — ignoring it",
                        folder.display()
                    ),
                ),
            }
        }
        server
    }

    pub(crate) fn run(mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.register_watchers();
        // Initial build + project-wide diagnostics for every workspace.
        self.rebuild_dirty();

        loop {
            let message = if self.workspaces.iter().any(|ws| ws.dirty) {
                match self.connection.receiver.recv_timeout(DEBOUNCE) {
                    Ok(message) => Some(message),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return Ok(()),
                }
            } else {
                match self.connection.receiver.recv() {
                    Ok(message) => Some(message),
                    Err(_) => return Ok(()),
                }
            };
            match message {
                Some(Message::Request(request)) => {
                    if self.connection.handle_shutdown(&request)? {
                        return Ok(());
                    }
                    // Answer from fresh state: a pending invalidation rebuilds now
                    // rather than after the debounce window.
                    self.rebuild_dirty();
                    self.handle_request(request);
                }
                Some(Message::Notification(notification)) => {
                    self.handle_notification(notification);
                }
                // Only acks of our own client requests (watcher registration) come back.
                Some(Message::Response(_)) => {}
                // Debounce window passed with no further events.
                None => self.rebuild_dirty(),
            }
        }
    }

    // ---- invalidation ----------------------------------------------------------------

    fn handle_notification(&mut self, notification: Notification) {
        match notification.method.as_str() {
            lsp_types::notification::DidChangeWatchedFiles::METHOD => {
                if let Ok(params) = serde_json::from_value::<
                    lsp_types::DidChangeWatchedFilesParams,
                >(notification.params)
                {
                    for event in params.changes {
                        if let Ok(path) = event.uri.to_file_path() {
                            self.mark_dirty(&path);
                        }
                    }
                }
            }
            lsp_types::notification::DidSaveTextDocument::METHOD => {
                if let Ok(params) = serde_json::from_value::<
                    lsp_types::DidSaveTextDocumentParams,
                >(notification.params)
                {
                    if let Ok(path) = params.text_document.uri.to_file_path() {
                        if is_interesting(&path) {
                            self.mark_dirty(&path);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn mark_dirty(&mut self, path: &Path) {
        if let Some(index) = self.workspace_of(path) {
            self.workspaces[index].dirty = true;
        }
    }

    fn rebuild_dirty(&mut self) {
        for index in 0..self.workspaces.len() {
            if !self.workspaces[index].dirty {
                continue;
            }
            let root = self.workspaces[index].root.clone();
            let handle = match Magento::open(&root) {
                Ok(magento) => Some(Arc::new(magento)),
                Err(error) => {
                    self.log(
                        lsp_types::MessageType::ERROR,
                        format!("failed to index {}: {error}", root.display()),
                    );
                    None
                }
            };
            self.workspaces[index].handle = handle;
            self.workspaces[index].dirty = false;
            self.publish_diagnostics(index);
        }
    }

    fn publish_diagnostics(&mut self, index: usize) {
        let mut published = HashSet::new();
        if let Some(handle) = self.workspaces[index].handle.clone() {
            for (file, diagnostics) in diag::diagnostics_by_file(&handle) {
                let Ok(uri) = Url::from_file_path(&file) else { continue };
                published.insert(uri.clone());
                self.notify::<lsp_types::notification::PublishDiagnostics>(
                    lsp_types::PublishDiagnosticsParams { uri, diagnostics, version: None },
                );
            }
        }
        // Files that had findings last build and are clean (or gone) now.
        let stale: Vec<Url> = self.workspaces[index]
            .published
            .difference(&published)
            .cloned()
            .collect();
        for uri in stale {
            self.notify::<lsp_types::notification::PublishDiagnostics>(
                lsp_types::PublishDiagnosticsParams { uri, diagnostics: Vec::new(), version: None },
            );
        }
        self.workspaces[index].published = published;
    }

    // ---- requests --------------------------------------------------------------------

    fn handle_request(&mut self, request: Request) {
        use lsp_types::request::Request as _;
        let id = request.id.clone();
        let result = match request.method.as_str() {
            lsp_types::request::GotoDefinition::METHOD => {
                self.with_doc_position(request, |magento, path, position| {
                    serde_json::to_value(features::definition(magento, path, position)).ok()
                })
            }
            lsp_types::request::HoverRequest::METHOD => {
                self.with_doc_position(request, |magento, path, position| {
                    serde_json::to_value(features::hover(magento, path, position)).ok()
                })
            }
            lsp_types::request::References::METHOD => {
                self.with_doc_position(request, |magento, path, position| {
                    serde_json::to_value(features::references(magento, path, position)).ok()
                })
            }
            lsp_types::request::CodeLensRequest::METHOD => {
                let params: Option<lsp_types::CodeLensParams> =
                    serde_json::from_value(request.params).ok();
                params.and_then(|p| {
                    let path = p.text_document.uri.to_file_path().ok()?;
                    let magento = self.handle_for(&path)?;
                    serde_json::to_value(features::code_lens(&magento, &path)).ok()
                })
            }
            _ => None,
        };
        let response = Response::new_ok(id, result.unwrap_or(serde_json::Value::Null));
        let _ = self.connection.sender.send(Message::Response(response));
    }

    /// Boilerplate shared by the position-based requests: params → (handle, file,
    /// position), null when the file is outside every workspace or the index is broken.
    fn with_doc_position(
        &mut self,
        request: Request,
        f: impl FnOnce(&Magento, &Path, lsp_types::Position) -> Option<serde_json::Value>,
    ) -> Option<serde_json::Value> {
        // All three share TextDocumentPositionParams as a prefix; deserialize just that.
        #[derive(serde::Deserialize)]
        struct Positional {
            #[serde(flatten)]
            inner: lsp_types::TextDocumentPositionParams,
        }
        let params: Positional = serde_json::from_value(request.params).ok()?;
        let path = params.inner.text_document.uri.to_file_path().ok()?;
        let magento = self.handle_for(&path)?;
        f(&magento, &path, params.inner.position)
    }

    fn handle_for(&self, path: &Path) -> Option<Arc<Magento>> {
        self.workspace_of(path)
            .and_then(|index| self.workspaces[index].handle.clone())
    }

    /// The workspace whose root contains `path` — longest root wins so nested installs
    /// resolve to the inner one.
    fn workspace_of(&self, path: &Path) -> Option<usize> {
        self.workspaces
            .iter()
            .enumerate()
            .filter(|(_, ws)| path.starts_with(&ws.root))
            .max_by_key(|(_, ws)| ws.root.as_os_str().len())
            .map(|(index, _)| index)
    }

    // ---- client plumbing ---------------------------------------------------------------

    fn register_watchers(&mut self) {
        if !self.can_watch || self.workspaces.is_empty() {
            return;
        }
        let watchers: Vec<lsp_types::FileSystemWatcher> = self
            .workspaces
            .iter()
            .flat_map(|ws| {
                let relative = self.relative_patterns;
                Magento::watch_globs().iter().map(move |glob| lsp_types::FileSystemWatcher {
                    glob_pattern: if relative {
                        lsp_types::GlobPattern::Relative(lsp_types::RelativePattern {
                            base_uri: lsp_types::OneOf::Right(
                                Url::from_file_path(&ws.root).expect("roots are absolute"),
                            ),
                            pattern: (*glob).to_string(),
                        })
                    } else {
                        lsp_types::GlobPattern::String(format!(
                            "{}/{glob}",
                            ws.root.display()
                        ))
                    },
                    kind: None, // create | change | delete
                })
            })
            .collect();
        self.next_request_id += 1;
        let request = Request::new(
            RequestId::from(self.next_request_id),
            lsp_types::request::RegisterCapability::METHOD.to_string(),
            lsp_types::RegistrationParams {
                registrations: vec![lsp_types::Registration {
                    id: "magequery-watch".to_string(),
                    method: lsp_types::notification::DidChangeWatchedFiles::METHOD.to_string(),
                    register_options: serde_json::to_value(
                        lsp_types::DidChangeWatchedFilesRegistrationOptions { watchers },
                    )
                    .ok(),
                }],
            },
        );
        let _ = self.connection.sender.send(Message::Request(request));
    }

    fn notify<N: lsp_types::notification::Notification>(&self, params: N::Params) {
        let notification = Notification::new(N::METHOD.to_string(), params);
        let _ = self.connection.sender.send(Message::Notification(notification));
    }

    fn log(&self, typ: lsp_types::MessageType, message: String) {
        self.notify::<lsp_types::notification::LogMessage>(lsp_types::LogMessageParams {
            typ,
            message,
        });
    }
}

/// Files whose save can change an answer — the didSave fallback filter (watched-file
/// events are already narrowed by [`Magento::watch_globs`]).
fn is_interesting(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some("php" | "xml" | "graphqls" | "csv") => true,
        _ => path.file_name().is_some_and(|n| n == "installed.json"),
    }
}
