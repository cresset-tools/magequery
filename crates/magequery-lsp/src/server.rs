//! The event loop: workspace roots, the dirty/rebuild cycle, and request dispatch.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::RecvTimeoutError;
use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use lsp_types::notification::Notification as _;
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
        // Full-content sync: open buffers overlay the checkout, so analysis (and the
        // as-you-type diagnostics) reflect what the editor shows, not what's saved.
        text_document_sync: Some(TextDocumentSyncCapability::Options(
            TextDocumentSyncOptions {
                open_close: Some(true),
                change: Some(TextDocumentSyncKind::FULL),
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
        completion_provider: Some(CompletionOptions {
            // `\` and `/` aren't word characters, so clients only re-query on them
            // when told to; quotes open attribute values, `>` opens text nodes.
            trigger_characters: Some(
                ["\"", "'", "\\", "/", "_", ">"].map(str::to_string).to_vec(),
            ),
            ..Default::default()
        }),
        // The lens data as inline annotations — the plugin indicator editors without
        // code-lens rendering (Zed) can show.
        inlay_hint_provider: Some(OneOf::Left(true)),
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
    /// Rebuilt but diagnostics not yet recomputed — published after the pending request
    /// is answered, so doctor never sits on a definition/hover's critical path.
    needs_publish: bool,
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
    /// Client re-queries inlay hints / code lenses when asked (LSP 3.17 refresh
    /// requests) — sent after every rebuild, since hints are computed from disk and
    /// the client can't otherwise know a save changed them.
    refresh_inlay_hints: bool,
    refresh_code_lenses: bool,
    /// Open buffers' current contents (full-text sync), keyed by file path. These form
    /// the overlay each rebuild hands to [`Magento::open_with_overlay`].
    buffers: std::collections::HashMap<PathBuf, String>,
    /// Class-name enumerations per workspace root, for completion. Built on first use
    /// (~100ms walk) and kept across rebuilds — the set only changes when PHP files are
    /// created or deleted (watched-file events evict it), never on content edits.
    class_catalogs: std::collections::HashMap<PathBuf, Arc<Vec<magequery_core::ClassName>>>,
    next_request_id: i32,
}

impl<'a> Server<'a> {
    pub(crate) fn new(connection: &'a Connection, init: lsp_types::InitializeParams) -> Self {
        let watched = init
            .capabilities
            .workspace
            .as_ref()
            .and_then(|w| w.did_change_watched_files.as_ref());

        let workspace_caps = init.capabilities.workspace.as_ref();
        let mut server = Server {
            connection,
            workspaces: Vec::new(),
            can_watch: watched.and_then(|w| w.dynamic_registration).unwrap_or(false),
            relative_patterns: watched.and_then(|w| w.relative_pattern_support).unwrap_or(false),
            refresh_inlay_hints: workspace_caps
                .and_then(|w| w.inlay_hint.as_ref())
                .and_then(|c| c.refresh_support)
                .unwrap_or(false),
            refresh_code_lenses: workspace_caps
                .and_then(|w| w.code_lens.as_ref())
                .and_then(|c| c.refresh_support)
                .unwrap_or(false),
            buffers: std::collections::HashMap::new(),
            class_catalogs: std::collections::HashMap::new(),
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
                            needs_publish: false,
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
        self.publish_pending();

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
                    // rather than after the debounce window — but diagnostics
                    // (doctor, the expensive part) wait until the answer is out.
                    self.rebuild_dirty();
                    self.handle_request(request);
                    self.publish_pending();
                }
                Some(Message::Notification(notification)) => {
                    self.handle_notification(notification);
                }
                // Only acks of our own client requests (watcher registration) come back.
                Some(Message::Response(_)) => {}
                // Debounce window passed with no further events.
                None => {
                    self.rebuild_dirty();
                    self.publish_pending();
                }
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
                            // A PHP file appearing/disappearing changes the class set.
                            if event.typ != lsp_types::FileChangeType::CHANGED
                                && path.extension().is_some_and(|e| e == "php")
                            {
                                if let Some(index) = self.workspace_of(&path) {
                                    let root = self.workspaces[index].root.clone();
                                    self.class_catalogs.remove(&root);
                                }
                            }
                            self.mark_dirty(&path);
                        }
                    }
                }
            }
            lsp_types::notification::DidOpenTextDocument::METHOD => {
                if let Ok(params) = serde_json::from_value::<
                    lsp_types::DidOpenTextDocumentParams,
                >(notification.params)
                {
                    if let Ok(path) = params.text_document.uri.to_file_path() {
                        self.buffer_updated(path, Some(params.text_document.text));
                    }
                }
            }
            lsp_types::notification::DidChangeTextDocument::METHOD => {
                if let Ok(params) = serde_json::from_value::<
                    lsp_types::DidChangeTextDocumentParams,
                >(notification.params)
                {
                    // Full-content sync: the last change event carries the whole text.
                    if let (Ok(path), Some(change)) = (
                        params.text_document.uri.to_file_path(),
                        params.content_changes.into_iter().last(),
                    ) {
                        self.buffer_updated(path, Some(change.text));
                    }
                }
            }
            lsp_types::notification::DidCloseTextDocument::METHOD => {
                if let Ok(params) = serde_json::from_value::<
                    lsp_types::DidCloseTextDocumentParams,
                >(notification.params)
                {
                    if let Ok(path) = params.text_document.uri.to_file_path() {
                        self.buffer_updated(path, None);
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

    /// The buffers under `root`, each inserted under its own path *and* the
    /// canonicalized form, so core's root-joined paths match whichever spelling the
    /// editor's URIs use (macOS `/private` symlinks and friends).
    fn overlay_for(&self, root: &Path) -> std::collections::HashMap<PathBuf, String> {
        let mut overlay = std::collections::HashMap::new();
        for (path, text) in &self.buffers {
            if !path.starts_with(root) {
                continue;
            }
            if let Ok(canonical) = path.canonicalize() {
                if canonical != *path {
                    overlay.insert(canonical, text.clone());
                }
            }
            overlay.insert(path.clone(), text.clone());
        }
        overlay
    }

    /// The class enumeration for a workspace, built once and reused across rebuilds.
    fn class_catalog(
        &mut self,
        workspace: usize,
        magento: &Magento,
    ) -> Arc<Vec<magequery_core::ClassName>> {
        let root = self.workspaces[workspace].root.clone();
        if let Some(catalog) = self.class_catalogs.get(&root) {
            return Arc::clone(catalog);
        }
        let started = std::time::Instant::now();
        let catalog = Arc::new(magento.class_names());
        self.log(
            lsp_types::MessageType::INFO,
            format!(
                "class catalog for {}: {} names in {}ms",
                root.display(),
                catalog.len(),
                started.elapsed().as_millis(),
            ),
        );
        self.class_catalogs.insert(root, Arc::clone(&catalog));
        catalog
    }

    /// A buffer opened, changed, or closed: update the overlay and invalidate when the
    /// buffer diverges from (or reverts toward) what the index last saw.
    fn buffer_updated(&mut self, path: PathBuf, text: Option<String>) {
        if !is_interesting(&path) || self.workspace_of(&path).is_none() {
            return;
        }
        match text {
            Some(text) => {
                let previous = self.buffers.insert(path.clone(), text.clone());
                // didOpen with pristine content changes nothing; every other
                // transition (edit, revert, restored unsaved buffer) invalidates.
                let baseline = match &previous {
                    Some(previous) => previous.clone(),
                    None => std::fs::read_to_string(&path).unwrap_or_default(),
                };
                if baseline != text {
                    self.mark_dirty(&path);
                }
            }
            None => {
                if let Some(last) = self.buffers.remove(&path) {
                    // Closed with unsaved changes: the index must revert to disk.
                    if std::fs::read_to_string(&path).ok().as_deref() != Some(last.as_str()) {
                        self.mark_dirty(&path);
                    }
                }
            }
        }
    }

    fn rebuild_dirty(&mut self) {
        let mut rebuilt = false;
        for index in 0..self.workspaces.len() {
            if !self.workspaces[index].dirty {
                continue;
            }
            rebuilt = true;
            let root = self.workspaces[index].root.clone();
            let overlay = self.overlay_for(&root);
            let handle = match Magento::open_with_overlay(&root, overlay) {
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
            self.workspaces[index].needs_publish = true;
        }
        if rebuilt {
            // Hints/lenses are computed from disk; tell the client the save it just
            // made (or the external change) invalidated what it renders.
            if self.refresh_inlay_hints {
                self.send_request::<lsp_types::request::InlayHintRefreshRequest>(());
            }
            if self.refresh_code_lenses {
                self.send_request::<lsp_types::request::CodeLensRefresh>(());
            }
        }
    }

    fn send_request<R: lsp_types::request::Request>(&mut self, params: R::Params) {
        self.next_request_id += 1;
        let request = Request::new(
            RequestId::from(self.next_request_id),
            R::METHOD.to_string(),
            params,
        );
        let _ = self.connection.sender.send(Message::Request(request));
    }

    fn publish_pending(&mut self) {
        for index in 0..self.workspaces.len() {
            if self.workspaces[index].needs_publish {
                self.workspaces[index].needs_publish = false;
                self.publish_diagnostics(index);
            }
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
            lsp_types::request::Completion::METHOD => {
                let params: Option<lsp_types::CompletionParams> =
                    serde_json::from_value(request.params).ok();
                params.and_then(|p| {
                    let doc = p.text_document_position;
                    let path = doc.text_document.uri.to_file_path().ok()?;
                    let workspace = self.workspace_of(&path)?;
                    let magento = self.workspaces[workspace].handle.clone()?;
                    let catalog = self.class_catalog(workspace, &magento);
                    serde_json::to_value(
                        features::completions(&magento, &catalog, &path, doc.position)
                            .map(lsp_types::CompletionResponse::List),
                    )
                    .ok()
                })
            }
            lsp_types::request::InlayHintRequest::METHOD => {
                let params: Option<lsp_types::InlayHintParams> =
                    serde_json::from_value(request.params).ok();
                params.and_then(|p| {
                    let path = p.text_document.uri.to_file_path().ok()?;
                    let magento = self.handle_for(&path)?;
                    serde_json::to_value(features::inlay_hints(&magento, &path, p.range)).ok()
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
        self.send_request::<lsp_types::request::RegisterCapability>(
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
