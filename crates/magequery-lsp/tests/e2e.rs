//! End-to-end over `Connection::memory()`: a synthetic Magento root with one app/code
//! module, driven through the full protocol — initialize, the initial diagnostics wave,
//! definition/hover, watched-file invalidation, shutdown. This is the CI twin of the
//! manual validation against a real checkout.

use std::path::{Path, PathBuf};
use std::time::Duration;

use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use lsp_types::notification::Notification as _;
use lsp_types::request::Request as _;
use lsp_types::Url;

const RECV_TIMEOUT: Duration = Duration::from_secs(30);

/// A throwaway Magento root: `config.php` + one module with a broken preference (the
/// doctor-diagnostic case), an observer registration (the definition case), and the
/// observer class on disk.
struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("magequery-lsp-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let write = |rel: &str, content: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        };
        write(
            "app/etc/config.php",
            "<?php\nreturn ['modules' => ['Acme_Widget' => 1]];\n",
        );
        write(
            "app/code/Acme/Widget/etc/module.xml",
            r#"<?xml version="1.0"?>
<config><module name="Acme_Widget"/></config>
"#,
        );
        write(
            "app/code/Acme/Widget/etc/di.xml",
            r#"<?xml version="1.0"?>
<config>
    <preference for="Acme\Widget\Api\ThingInterface" type="Acme\Widget\Model\Missing"/>
</config>
"#,
        );
        write(
            "app/code/Acme/Widget/etc/events.xml",
            r#"<?xml version="1.0"?>
<config>
    <event name="acme_thing_saved">
        <observer name="acme_recalc" instance="Acme\Widget\Observer\Recalc"/>
    </event>
</config>
"#,
        );
        write(
            "app/code/Acme/Widget/Observer/Recalc.php",
            "<?php\nnamespace Acme\\Widget\\Observer;\n\nclass Recalc\n{\n}\n",
        );
        // An interception-shaped class no di.xml declares: the plugin-unregistered
        // warning, which must land on the class declaration line, not line 1.
        write(
            "app/code/Acme/Widget/Plugin/Tweak.php",
            "<?php\nnamespace Acme\\Widget\\Plugin;\n\nclass Tweak\n{\n    public function beforeSave($subject)\n    {\n        return null;\n    }\n}\n",
        );
        Self(root)
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }

    fn uri(&self, rel: &str) -> Url {
        Url::from_file_path(self.path(rel)).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The client half: send/receive helpers over the in-memory connection.
struct Client {
    connection: Connection,
    next_id: i32,
}

impl Client {
    fn request<R: lsp_types::request::Request>(&mut self, params: R::Params) -> R::Result {
        self.next_id += 1;
        let id = RequestId::from(self.next_id);
        self.connection
            .sender
            .send(Message::Request(Request::new(id.clone(), R::METHOD.to_string(), params)))
            .unwrap();
        loop {
            match self.recv() {
                Message::Response(Response { id: got, result, error, .. }) if got == id => {
                    assert!(error.is_none(), "{}: {error:?}", R::METHOD);
                    return serde_json::from_value(result.unwrap_or(serde_json::Value::Null))
                        .expect("result shape");
                }
                // Server→client requests (watcher registration) need an ack.
                Message::Request(request) => self.ack(request),
                _ => {}
            }
        }
    }

    fn notify<N: lsp_types::notification::Notification>(&self, params: N::Params) {
        self.connection
            .sender
            .send(Message::Notification(Notification::new(N::METHOD.to_string(), params)))
            .unwrap();
    }

    fn recv(&self) -> Message {
        self.connection
            .receiver
            .recv_timeout(RECV_TIMEOUT)
            .expect("server went quiet")
    }

    fn ack(&self, request: Request) {
        self.connection
            .sender
            .send(Message::Response(Response::new_ok(request.id, serde_json::Value::Null)))
            .unwrap();
    }

    /// Consume messages until every expected file has published diagnostics, answering
    /// server→client requests along the way. Returns uri → diagnostics.
    fn collect_diagnostics(
        &self,
        expected: usize,
    ) -> std::collections::HashMap<Url, Vec<lsp_types::Diagnostic>> {
        let mut published = std::collections::HashMap::new();
        while published.len() < expected {
            match self.recv() {
                Message::Notification(notification)
                    if notification.method
                        == lsp_types::notification::PublishDiagnostics::METHOD =>
                {
                    let params: lsp_types::PublishDiagnosticsParams =
                        serde_json::from_value(notification.params).unwrap();
                    if !params.diagnostics.is_empty() {
                        published.insert(params.uri, params.diagnostics);
                    }
                }
                Message::Request(request) => self.ack(request),
                _ => {}
            }
        }
        published
    }
}

fn start(fixture: &Fixture) -> (Client, std::thread::JoinHandle<()>) {
    let (server_side, client_side) = Connection::memory();
    let handle = std::thread::spawn(move || {
        magequery_lsp::run(server_side).expect("server run");
    });
    let mut client = Client { connection: client_side, next_id: 0 };

    #[allow(deprecated)]
    let init = lsp_types::InitializeParams {
        capabilities: lsp_types::ClientCapabilities {
            workspace: Some(lsp_types::WorkspaceClientCapabilities {
                did_change_watched_files: Some(
                    lsp_types::DidChangeWatchedFilesClientCapabilities {
                        dynamic_registration: Some(true),
                        relative_pattern_support: Some(true),
                    },
                ),
                ..Default::default()
            }),
            ..Default::default()
        },
        workspace_folders: Some(vec![lsp_types::WorkspaceFolder {
            uri: Url::from_file_path(&fixture.0).unwrap(),
            name: "shop".to_string(),
        }]),
        ..Default::default()
    };
    let _init_result: lsp_types::InitializeResult =
        client.request::<lsp_types::request::Initialize>(init);
    client.notify::<lsp_types::notification::Initialized>(lsp_types::InitializedParams {});
    (client, handle)
}

fn shutdown(mut client: Client, handle: std::thread::JoinHandle<()>) {
    client.request::<lsp_types::request::Shutdown>(());
    client.notify::<lsp_types::notification::Exit>(());
    handle.join().expect("server thread");
}

#[test]
fn diagnostics_definition_hover_and_invalidation() {
    let fixture = Fixture::new();
    let (mut client, handle) = start(&fixture);

    // --- initial diagnostics: the broken preference on di.xml line 3, and the
    // unregistered plugin on its *class declaration* line (never line 1).
    let published = client.collect_diagnostics(2);
    let plugin_uri = fixture.uri("app/code/Acme/Widget/Plugin/Tweak.php");
    let unregistered = published.get(&plugin_uri).expect("diagnostics on Tweak.php");
    assert_eq!(
        unregistered[0].code,
        Some(lsp_types::NumberOrString::String("plugin-unregistered".to_string()))
    );
    assert_eq!(unregistered[0].range.start.line, 3); // `class Tweak` is on line 4

    let di_uri = fixture.uri("app/code/Acme/Widget/etc/di.xml");
    let diagnostics = published.get(&di_uri).expect("diagnostics on di.xml");
    let broken = diagnostics
        .iter()
        .find(|d| {
            d.code
                == Some(lsp_types::NumberOrString::String(
                    "preference-target-missing".to_string(),
                ))
        })
        .expect("preference-target-missing finding");
    assert_eq!(broken.severity, Some(lsp_types::DiagnosticSeverity::ERROR));
    assert_eq!(broken.range.start.line, 2); // 1-based line 3 in the fixture file
    assert!(broken.message.contains("Acme\\Widget\\Model\\Missing"));

    // --- definition on the observer instance in events.xml → Recalc.php's class line.
    let events_uri = fixture.uri("app/code/Acme/Widget/etc/events.xml");
    let definition = client.request::<lsp_types::request::GotoDefinition>(
        lsp_types::GotoDefinitionParams {
            text_document_position_params: lsp_types::TextDocumentPositionParams {
                text_document: lsp_types::TextDocumentIdentifier { uri: events_uri.clone() },
                // line 4 (0-based 3): inside `instance="Acme\Widget\Observer\Recalc"`.
                position: lsp_types::Position::new(3, 55),
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        },
    );
    let location = match definition {
        Some(lsp_types::GotoDefinitionResponse::Scalar(location)) => location,
        other => panic!("expected one location, got {other:?}"),
    };
    assert_eq!(location.uri, fixture.uri("app/code/Acme/Widget/Observer/Recalc.php"));
    assert_eq!(location.range.start.line, 3); // `class Recalc` is on line 4

    // --- hover on the event name lists the observer.
    let hover = client.request::<lsp_types::request::HoverRequest>(lsp_types::HoverParams {
        text_document_position_params: lsp_types::TextDocumentPositionParams {
            text_document: lsp_types::TextDocumentIdentifier { uri: events_uri },
            position: lsp_types::Position::new(2, 20), // inside `acme_thing_saved`
        },
        work_done_progress_params: Default::default(),
    });
    let hover = hover.expect("hover on event");
    let lsp_types::HoverContents::Markup(markup) = hover.contents else {
        panic!("markdown hover expected");
    };
    assert!(markup.value.contains("acme_thing_saved"));
    assert!(markup.value.contains("Acme\\Widget\\Observer\\Recalc"));

    // --- fixing the broken preference + a watched-file event clears the diagnostic.
    std::fs::write(
        fixture.path("app/code/Acme/Widget/etc/di.xml"),
        r#"<?xml version="1.0"?>
<config>
    <preference for="Acme\Widget\Api\ThingInterface" type="Acme\Widget\Observer\Recalc"/>
</config>
"#,
    )
    .unwrap();
    client.notify::<lsp_types::notification::DidChangeWatchedFiles>(
        lsp_types::DidChangeWatchedFilesParams {
            changes: vec![lsp_types::FileEvent {
                uri: di_uri.clone(),
                typ: lsp_types::FileChangeType::CHANGED,
            }],
        },
    );
    // The rebuild republishes; di.xml's entry must now be the empty clearing publish.
    let cleared = loop {
        match client.recv() {
            Message::Notification(notification)
                if notification.method == lsp_types::notification::PublishDiagnostics::METHOD =>
            {
                let params: lsp_types::PublishDiagnosticsParams =
                    serde_json::from_value(notification.params).unwrap();
                if params.uri == di_uri {
                    break params.diagnostics;
                }
            }
            Message::Request(request) => client.ack(request),
            _ => {}
        }
    };
    assert!(cleared.is_empty(), "diagnostic should clear after the fix: {cleared:?}");

    shutdown(client, handle);
}
