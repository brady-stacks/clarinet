//! Regression tests for finding 3 of the PR #2483 review: client input must not
//! panic the DAP server.
//!
//! `run_dap_server` runs `init_attach` on a background thread and `join()`s it
//! (`clarinet-cli/src/frontend/dap.rs:191-194`). A panic on that thread makes
//! `join()` return `Err`, so the whole process exits 1 with
//! `"DAP handshake thread panicked"` — tearing down a server that may already
//! have a connected SDK client. Every request below is legal on the wire, so a
//! misbehaving or merely early editor can trigger it.
//!
//! Each test drives a real `TcpStream` with real `Content-Length` framing and
//! asserts that the server *answers* the request. They fail today because it
//! aborts instead.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use clarity::vm::types::QualifiedContractIdentifier;
use clarity_repl::repl::debug::dap::DAPDebugger;

const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// What became of the handshake thread.
enum Outcome {
    /// `init_attach` returned on its own.
    Returned,
    /// `init_attach` unwound. This is the bug.
    Panicked(String),
}

/// A connected editor-side socket plus the server-side stream to hand to the
/// debugger.
fn dap_pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let editor = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (server, _) = listener.accept().unwrap();
    editor.set_read_timeout(Some(REPLY_TIMEOUT)).unwrap();
    (editor, server)
}

/// Write one `Content-Length`-framed DAP message.
fn send(editor: &mut TcpStream, body: &str) {
    write!(editor, "Content-Length: {}\r\n\r\n{}", body.len(), body).unwrap();
    editor.flush().unwrap();
}

/// Read whatever the server writes back, or an empty string if it wrote nothing
/// before dying.
fn read_reply(editor: &mut TcpStream) -> String {
    let mut buf = [0u8; 4096];
    match editor.read(&mut buf) {
        Ok(n) => String::from_utf8_lossy(&buf[..n]).into_owned(),
        Err(_) => String::new(),
    }
}

/// Run the attach handshake against `server` on its own thread, exactly as
/// `run_dap_server` does, and report whether it unwound.
fn run_handshake(
    server: TcpStream,
    setup: impl FnOnce(&mut DAPDebugger) + Send + 'static,
) -> mpsc::Receiver<Outcome> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut dap = DAPDebugger::from_std_tcp_stream(server);
        setup(&mut dap);
        let outcome = match panic::catch_unwind(AssertUnwindSafe(|| dap.init_attach())) {
            Ok(_) => Outcome::Returned,
            Err(payload) => {
                let msg = payload
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "<non-string panic payload>".to_string());
                Outcome::Panicked(msg)
            }
        };
        let _ = tx.send(outcome);
    });
    rx
}

/// Assert the server answered rather than aborting. `reply` is whatever arrived
/// on the editor socket; `rx` carries the handshake thread's fate.
fn assert_answered(reply: &str, rx: &mpsc::Receiver<Outcome>, context: &str) {
    if let Ok(Outcome::Panicked(msg)) = rx.recv_timeout(REPLY_TIMEOUT) {
        panic!(
            "server panicked on {context}: {msg}\n\
             It should have written a DAP error response and stayed up. \
             In `run_dap_server` this panic propagates through `join()` and exits \
             the process, killing any already-connected SDK client.\n\
             bytes received by the editor before the abort: {reply:?}"
        );
    }
    assert!(
        !reply.is_empty(),
        "server wrote nothing back on {context}; expected a DAP response"
    );
}

/// `quit()` calls `self.get_state().quit()` (`dap/mod.rs:878`) *before*
/// `send_response`, and `state` is `None` until `attach()` runs (`:456`). A
/// `disconnect` arriving first therefore unwraps a `None`.
#[test]
fn disconnect_before_attach_is_answered_not_fatal() {
    let (mut editor, server) = dap_pair();

    send(
        &mut editor,
        r#"{"seq":1,"type":"request","command":"disconnect","arguments":{}}"#,
    );

    let rx = run_handshake(server, |_| {});
    let reply = read_reply(&mut editor);

    assert_answered(&reply, &rx, "a `disconnect` received before `attach`");
    assert!(
        reply.contains("disconnect"),
        "expected a Disconnect response, got: {reply:?}"
    );
}

/// `set_breakpoints` unwraps `arguments.source.path` (`dap/mod.rs:493` and
/// `:500`). The DAP spec allows a `source` that carries only a
/// `sourceReference`, so this is reachable without a malformed client.
#[test]
fn set_breakpoints_without_a_source_path_is_answered_not_fatal() {
    let (mut editor, server) = dap_pair();

    send(
        &mut editor,
        r#"{"seq":1,"type":"request","command":"setBreakpoints",
            "arguments":{"source":{"sourceReference":1},"breakpoints":[{"line":3}]}}"#,
    );

    let rx = run_handshake(server, |_| {});
    let reply = read_reply(&mut editor);

    assert_answered(&reply, &rx, "a `setBreakpoints` whose source has no `path`");
}

/// The same `get_state()` unwrap, reached by the ordering the review describes:
/// a `setBreakpoints` for a *known* contract path that races ahead of `attach`.
/// Registering the path gets past the early return at `:493` so execution
/// reaches `self.get_state().add_breakpoint(...)` at `:526`.
#[test]
fn set_breakpoints_before_attach_is_answered_not_fatal() {
    let (mut editor, server) = dap_pair();
    let path = PathBuf::from("/tmp/clarinet-dap-test/contracts/counter.clar");
    let contract_id = QualifiedContractIdentifier::transient();

    send(
        &mut editor,
        &format!(
            r#"{{"seq":1,"type":"request","command":"setBreakpoints",
                "arguments":{{"source":{{"path":{path:?}}},"breakpoints":[{{"line":3}}]}}}}"#
        ),
    );

    let registered = path.clone();
    let rx = run_handshake(server, move |dap| {
        dap.path_to_contract_id
            .insert(registered.clone(), contract_id.clone());
        dap.contract_id_to_path.insert(contract_id, registered);
    });
    let reply = read_reply(&mut editor);

    assert_answered(
        &reply,
        &rx,
        "a `setBreakpoints` that arrives before `attach`",
    );
}
