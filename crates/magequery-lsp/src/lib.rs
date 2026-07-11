//! The LSP server over `magequery-core` — Magento wiring answers inside the editor.
//!
//! This crate's relationship to core mirrors the CLI's: core computes owned, structured
//! data; this is a renderer, speaking LSP instead of ANSI. Three properties are locked:
//!
//! - **Open buffers overlay the checkout.** Every content read in core goes through a
//!   [`magequery_core::Magento::open_with_overlay`] overlay of the open buffers
//!   (full-text sync), so diagnostics and answers reflect what the editor shows —
//!   as-you-type, debounced. Files nobody has open read from disk as before.
//! - **No async runtime, no incrementality.** A full [`magequery_core::Magento::open`]
//!   is the rebuild — tens of ms warm on a real install, cheaper than any invalidation
//!   scheme is worth. The server is a single-threaded event loop over `lsp-server`'s
//!   channels (the stdio transport owns its own IO threads); a per-workspace dirty flag
//!   plus a `recv_timeout` quiet period is the whole debounce.
//! - **Not a PHP language server.** Intelephense/Phpactor own PHP symbols; this server
//!   answers the layer they can't see — the XML config graph (di.xml, events.xml, …)
//!   and Magento-semantic overlays on PHP (DI resolution, plugins, reverse DI).

mod diag;
mod entity;
mod features;
mod layout;
mod server;
mod symbols;
mod textpos;

use lsp_server::Connection;

/// Run the server on stdio until the client disconnects or asks us to exit. This is what
/// the CLI's hidden `magequery lsp` subcommand calls, and what editor plugins spawn.
pub fn run_stdio() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (connection, io_threads) = Connection::stdio();
    run(connection)?;
    io_threads.join()?;
    Ok(())
}

/// Serve one client over any `lsp-server` transport — the integration tests drive this
/// through `Connection::memory()`, no process or wire framing involved.
pub fn run(connection: Connection) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (init_id, init_value) = connection.initialize_start()?;
    let init_params: lsp_types::InitializeParams = serde_json::from_value(init_value)?;
    connection.initialize_finish(
        init_id,
        serde_json::json!({
            "capabilities": server::capabilities(),
            "serverInfo": { "name": "magequery", "version": env!("CARGO_PKG_VERSION") },
        }),
    )?;

    server::Server::new(&connection, init_params).run()
    // The connection drops here — with the stdio transport the writer io-thread runs
    // until every channel sender is gone, so it must die before io_threads.join().
}
