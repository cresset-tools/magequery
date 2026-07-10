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
    <type name="Acme\Widget\Model\Thing">
        <plugin name="acme_registered" type="Acme\Widget\Plugin\Registered"/>
    </type>
</config>
"#,
        );
        write(
            "app/code/Acme/Widget/Model/Thing.php",
            "<?php\nnamespace Acme\\Widget\\Model;\n\nclass Thing\n{\n    public function save($input)\n    {\n        return $input;\n    }\n}\n",
        );
        // A *registered* plugin: its aroundSave must jump to Thing::save.
        write(
            "app/code/Acme/Widget/Plugin/Registered.php",
            "<?php\nnamespace Acme\\Widget\\Plugin;\n\nclass Registered\n{\n    public function aroundSave($subject, callable $proceed)\n    {\n        return $proceed();\n    }\n}\n",
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
                inlay_hint: Some(lsp_types::InlayHintWorkspaceClientCapabilities {
                    refresh_support: Some(true),
                }),
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

    // --- completion: classes after a partial `type="Acme` in di.xml, events after a
    // partial `name="acme` in events.xml. Position = right after the typed prefix.
    let position_after = |rel: &str, needle: &str| -> lsp_types::Position {
        let content = std::fs::read_to_string(fixture.path(rel)).unwrap();
        let offset = content.find(needle).unwrap() + needle.len();
        let line = content[..offset].matches('\n').count() as u32;
        let line_start = content[..offset].rfind('\n').map_or(0, |i| i + 1);
        lsp_types::Position::new(line, (offset - line_start) as u32)
    };
    let complete = |client: &mut Client, rel: &str, needle: &str| -> Vec<String> {
        let response = client.request::<lsp_types::request::Completion>(
            lsp_types::CompletionParams {
                text_document_position: lsp_types::TextDocumentPositionParams {
                    text_document: lsp_types::TextDocumentIdentifier {
                        uri: fixture.uri(rel),
                    },
                    position: position_after(rel, needle),
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
                context: None,
            },
        );
        match response {
            Some(lsp_types::CompletionResponse::List(list)) => {
                list.items.into_iter().map(|item| item.label).collect()
            }
            other => panic!("expected a completion list, got {other:?}"),
        }
    };

    let labels = complete(&mut client, "app/code/Acme/Widget/etc/di.xml", "type=\"Acme");
    assert!(
        labels.contains(&"Acme\\Widget\\Model\\Thing".to_string()),
        "class completion missing Thing: {labels:?}"
    );
    assert!(labels.contains(&"Acme\\Widget\\Plugin\\Registered".to_string()));

    let labels = complete(&mut client, "app/code/Acme/Widget/etc/events.xml", "name=\"acme");
    assert_eq!(labels, vec!["acme_thing_saved".to_string()]);

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

    // --- definition on the plugin's `aroundSave` declaration → the intercepted
    // Thing::save implementation.
    let jump = client.request::<lsp_types::request::GotoDefinition>(
        lsp_types::GotoDefinitionParams {
            text_document_position_params: lsp_types::TextDocumentPositionParams {
                text_document: lsp_types::TextDocumentIdentifier {
                    uri: fixture.uri("app/code/Acme/Widget/Plugin/Registered.php"),
                },
                // line 6 (0-based 5): inside `function aroundSave(`.
                position: lsp_types::Position::new(5, 25),
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        },
    );
    let location = match jump {
        Some(lsp_types::GotoDefinitionResponse::Scalar(location)) => location,
        other => panic!("expected the intercepted method, got {other:?}"),
    };
    assert_eq!(location.uri, fixture.uri("app/code/Acme/Widget/Model/Thing.php"));
    assert_eq!(location.range.start.line, 5); // `public function save` is on line 6

    // --- and the reverse: definition on Thing::save itself → the aroundSave plugin
    // method intercepting it.
    let reverse = client.request::<lsp_types::request::GotoDefinition>(
        lsp_types::GotoDefinitionParams {
            text_document_position_params: lsp_types::TextDocumentPositionParams {
                text_document: lsp_types::TextDocumentIdentifier {
                    uri: fixture.uri("app/code/Acme/Widget/Model/Thing.php"),
                },
                // line 6 (0-based 5): inside `function save(`.
                position: lsp_types::Position::new(5, 22),
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        },
    );
    let location = match reverse {
        Some(lsp_types::GotoDefinitionResponse::Scalar(location)) => location,
        other => panic!("expected the intercepting plugin method, got {other:?}"),
    };
    assert_eq!(location.uri, fixture.uri("app/code/Acme/Widget/Plugin/Registered.php"));
    assert_eq!(location.range.start.line, 5); // `function aroundSave` is on line 6

    // --- code lens on the intercepted class: a method-level "intercepted by" lens on
    // save(), pointing at the plugin method.
    let lenses = client
        .request::<lsp_types::request::CodeLensRequest>(lsp_types::CodeLensParams {
            text_document: lsp_types::TextDocumentIdentifier {
                uri: fixture.uri("app/code/Acme/Widget/Model/Thing.php"),
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
        .expect("lenses on Thing.php");
    let save_lens = lenses
        .iter()
        .find(|lens| lens.range.start.line == 5)
        .expect("lens on the save() line");
    let command = save_lens.command.as_ref().expect("lens command");
    assert_eq!(command.title, "intercepted by 1 plugin method(s)");

    // And on the plugin class: the forward lens naming the intercepted method.
    let lenses = client
        .request::<lsp_types::request::CodeLensRequest>(lsp_types::CodeLensParams {
            text_document: lsp_types::TextDocumentIdentifier {
                uri: fixture.uri("app/code/Acme/Widget/Plugin/Registered.php"),
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
        .expect("lenses on Registered.php");
    let around_lens = lenses
        .iter()
        .find(|lens| lens.range.start.line == 5)
        .expect("lens on the aroundSave() line");
    assert_eq!(
        around_lens.command.as_ref().expect("lens command").title,
        "intercepts Thing::save()"
    );

    // --- inlay hints: the Zed-visible plugin indicators on both sides.
    let full_range = lsp_types::Range::new(
        lsp_types::Position::new(0, 0),
        lsp_types::Position::new(999, 0),
    );
    let hints = client
        .request::<lsp_types::request::InlayHintRequest>(lsp_types::InlayHintParams {
            text_document: lsp_types::TextDocumentIdentifier {
                uri: fixture.uri("app/code/Acme/Widget/Model/Thing.php"),
            },
            range: full_range,
            work_done_progress_params: Default::default(),
        })
        .expect("hints on Thing.php");
    assert_eq!(hints.len(), 1);
    assert_eq!(hints[0].position.line, 5); // end of the save() signature line
    let lsp_types::InlayHintLabel::LabelParts(parts) = &hints[0].label else {
        panic!("label parts expected");
    };
    assert_eq!(parts[0].value, "« 1 plugin(s) »");

    let hints = client
        .request::<lsp_types::request::InlayHintRequest>(lsp_types::InlayHintParams {
            text_document: lsp_types::TextDocumentIdentifier {
                uri: fixture.uri("app/code/Acme/Widget/Plugin/Registered.php"),
            },
            range: full_range,
            work_done_progress_params: Default::default(),
        })
        .expect("hints on Registered.php");
    let lsp_types::InlayHintLabel::LabelParts(parts) = &hints[0].label else {
        panic!("label parts expected");
    };
    assert_eq!(parts[0].value, "→ Thing::save()");

    // --- as-you-type: an open buffer overlays disk. The buffer fixes the preference
    // (disk still broken) → the diagnostic clears without saving; an edit that breaks
    // it again brings the diagnostic back; closing the buffer reverts to disk state.
    let di_uri_typed = fixture.uri("app/code/Acme/Widget/etc/di.xml");
    let await_di_publish = |client: &Client| -> Vec<lsp_types::Diagnostic> {
        loop {
            match client.recv() {
                Message::Notification(notification)
                    if notification.method
                        == lsp_types::notification::PublishDiagnostics::METHOD =>
                {
                    let params: lsp_types::PublishDiagnosticsParams =
                        serde_json::from_value(notification.params).unwrap();
                    if params.uri == di_uri_typed {
                        return params.diagnostics;
                    }
                }
                Message::Request(request) => client.ack(request),
                _ => {}
            }
        }
    };
    let fixed = r#"<?xml version="1.0"?>
<config>
    <preference for="Acme\Widget\Api\ThingInterface" type="Acme\Widget\Observer\Recalc"/>
    <type name="Acme\Widget\Model\Thing">
        <plugin name="acme_registered" type="Acme\Widget\Plugin\Registered"/>
    </type>
</config>
"#;
    let broken = std::fs::read_to_string(fixture.path("app/code/Acme/Widget/etc/di.xml")).unwrap();

    client.notify::<lsp_types::notification::DidOpenTextDocument>(
        lsp_types::DidOpenTextDocumentParams {
            text_document: lsp_types::TextDocumentItem {
                uri: di_uri_typed.clone(),
                language_id: "xml".to_string(),
                version: 1,
                text: fixed.to_string(),
            },
        },
    );
    let cleared = await_di_publish(&client);
    assert!(cleared.is_empty(), "buffer fix should clear unsaved: {cleared:?}");

    client.notify::<lsp_types::notification::DidChangeTextDocument>(
        lsp_types::DidChangeTextDocumentParams {
            text_document: lsp_types::VersionedTextDocumentIdentifier {
                uri: di_uri_typed.clone(),
                version: 2,
            },
            content_changes: vec![lsp_types::TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: broken.clone(),
            }],
        },
    );
    let returned = await_di_publish(&client);
    assert!(!returned.is_empty(), "breaking the buffer should re-flag it");

    // Close with the buffer differing from the last-indexed state → revert to disk
    // (which is still broken, so the diagnostic must still be there after rebuild).
    client.notify::<lsp_types::notification::DidChangeTextDocument>(
        lsp_types::DidChangeTextDocumentParams {
            text_document: lsp_types::VersionedTextDocumentIdentifier {
                uri: di_uri_typed.clone(),
                version: 3,
            },
            content_changes: vec![lsp_types::TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: fixed.to_string(),
            }],
        },
    );
    assert!(await_di_publish(&client).is_empty());
    client.notify::<lsp_types::notification::DidCloseTextDocument>(
        lsp_types::DidCloseTextDocumentParams {
            text_document: lsp_types::TextDocumentIdentifier { uri: di_uri_typed.clone() },
        },
    );
    let after_close = await_di_publish(&client);
    assert!(!after_close.is_empty(), "closing reverts to the broken disk state");

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
    // The rebuild must also ask us to re-query inlay hints (they're disk-computed).
    let mut hint_refresh_requested = false;
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
            Message::Request(request) => {
                if request.method == "workspace/inlayHint/refresh" {
                    hint_refresh_requested = true;
                }
                client.ack(request);
            }
            _ => {}
        }
    };
    assert!(cleared.is_empty(), "diagnostic should clear after the fix: {cleared:?}");
    assert!(hint_refresh_requested, "rebuild should trigger an inlay-hint refresh");

    shutdown(client, handle);
}
