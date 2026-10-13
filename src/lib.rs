//! cce-sync: the vault's sync engine, shared by the server
//! (`cce-sync-server`, a Cloudflare Durable Object), the laptop daemon (this
//! crate's `cce-sync` binary) and the phone (`cce-vault-web`).
//!
//! The library has no I/O and builds for wasm32:
//! - [`proto`]: the wire protocol, path rules and hashing.
//! - [`merge`]: the three-way line merge of notes.
//! - [`core`]: the server's write rules, change feed and history over a
//!   [`core::Store`].
//! - [`mem`]: an in-memory store, for tests and other hosts.
//! - [`client`]: a device's side: what to download, remove and push.
//! - [`mcp`] (feature `mcp`): the MCP server Claude's connector calls.
//! - [`agenda`], [`gtask`]: the Google bridge's rules (the day's events,
//!   `#gtask` tasks kept in step with Google Tasks).
//!
//! The design is the doc "cce-sync: design".

pub mod agenda;
pub mod client;
pub mod core;
pub mod gtask;
pub mod mem;
#[cfg(feature = "mcp")]
pub mod mcp;
pub mod merge;
pub mod proto;

/// Drive a future whose every await is already ready (an in-memory store's)
/// to completion on this thread. For tests and for synchronous hosts; a
/// future that really waits panics.
pub fn block_on_ready<F: std::future::Future>(fut: F) -> F::Output {
    let mut fut = std::pin::pin!(fut);
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    match fut.as_mut().poll(&mut cx) {
        std::task::Poll::Ready(v) => v,
        std::task::Poll::Pending => panic!("block_on_ready: the future is not ready"),
    }
}

#[cfg(test)]
mod tests;
