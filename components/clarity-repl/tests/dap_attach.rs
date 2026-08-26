//! Wire-level tests for `DAPDebugger`'s attach handshake.
//!
//! Each test drives a real `DAPDebugger` over a TCP socket using the same
//! `Content-Length`-framed JSON an editor sends, and runs `init_attach` on its
//! own thread — the arrangement `clarinet dap --dap-port` uses. Joining that
//! thread is how a test observes a panic inside the debugger, which is what
//! several of the findings are about.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use clarity_repl::repl::debug::dap::codec::ParseError;
use clarity_repl::repl::debug::dap::DAPDebugger;

/// A `DAPDebugger` running `init_attach` on a thread, plus the socket the test
/// uses as the editor.
struct Editor {
    socket: TcpStream,
    handle: Option<JoinHandle<Result<(), ParseError>>>,
}

impl Editor {
    fn attach() -> Self {
        Self::attach_with(|_| {})
    }

    /// `configure` runs on the debugger before the handshake starts, standing in
    /// for the contract-map registration `run_dap_server` does.
    fn attach_with(configure: impl FnOnce(&mut DAPDebugger) + Send + 'static) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind");
        let port = listener.local_addr().expect("no local addr").port();
        let socket = TcpStream::connect(("127.0.0.1", port)).expect("failed to connect");
        socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("failed to set a read timeout");
        let (server_side, _) = listener.accept().expect("failed to accept");

        let handle = std::thread::spawn(move || {
            let mut debugger = DAPDebugger::from_std_tcp_stream(server_side);
            configure(&mut debugger);
            debugger.init_attach()
        });

        Editor {
            socket,
            handle: Some(handle),
        }
    }

    fn send(&mut self, message: serde_json::Value) {
        let body = serde_json::to_string(&message).expect("failed to serialize");
        write!(self.socket, "Content-Length: {}\r\n\r\n{body}", body.len())
            .expect("failed to write");
        self.socket.flush().expect("failed to flush");
    }

    /// Read one `Content-Length`-framed message.
    fn recv(&mut self) -> serde_json::Value {
        let mut header = Vec::new();
        loop {
            let mut byte = [0u8; 1];
            let read = self
                .socket
                .read(&mut byte)
                .expect("failed to read a header");
            assert_ne!(read, 0, "the debugger closed the connection");
            header.push(byte[0]);
            if header.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let header = String::from_utf8(header).expect("the header was not UTF-8");
        let length: usize = header
            .lines()
            .find_map(|line| line.strip_prefix("Content-Length: "))
            .expect("no Content-Length header")
            .trim()
            .parse()
            .expect("Content-Length was not a number");

        let mut body = vec![0u8; length];
        self.socket
            .read_exact(&mut body)
            .expect("failed to read the body");
        serde_json::from_slice(&body).expect("the debugger sent invalid JSON")
    }

    /// Complete `initialize` + `attach`, draining the responses and the
    /// `initialized` event, so a test can start from a configured session.
    fn handshake(&mut self) {
        self.send(serde_json::json!({
            "seq": 1,
            "type": "request",
            "command": "initialize",
            "arguments": {"adapterID": "clarinet"},
        }));
        self.recv(); // initialize response

        self.send(serde_json::json!({
            "seq": 2,
            "type": "request",
            "command": "attach",
            "arguments": {},
        }));
        self.recv(); // attach response
        self.recv(); // initialized event
    }

    /// Wait up to `timeout` for `init_attach` to return. `Some(Err(_))` from the
    /// join means the debugger panicked.
    fn wait_for_init_attach(
        &mut self,
        timeout: Duration,
    ) -> Option<Result<Result<(), ParseError>, String>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(handle) = self.handle.take_if(|handle| handle.is_finished()) {
                return Some(
                    handle
                        .join()
                        .map_err(|_| "the debugger panicked".to_string()),
                );
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

/// Smoke test: the harness completes a real attach handshake.
#[test]
fn the_harness_completes_an_attach_handshake() {
    let mut editor = Editor::attach();
    editor.handshake();

    editor.send(serde_json::json!({
        "seq": 3,
        "type": "request",
        "command": "configurationDone",
    }));
    editor.recv();

    let outcome = editor
        .wait_for_init_attach(Duration::from_secs(5))
        .expect("init_attach did not return after configurationDone");
    assert!(
        matches!(outcome, Ok(Ok(()))),
        "init_attach should have returned Ok, got {outcome:?}"
    );
}

/// Finding 29 of the PR #2483 review: DAP defines the body of an
/// `InitializeResponse` as the `Capabilities` object itself. `debug_types`
/// declares `InitializeResponse { capabilities: Capabilities }` without
/// `#[serde(flatten)]`, so the adapter emits the capabilities one level too
/// deep and clients see none of the features it advertises.
///
/// That matters most for `supportsConfigurationDoneRequest`: `init_attach`
/// waits for `configurationDone`, but the client never learns the adapter
/// supports it. The new relay in `debug/debug.ts` reproduces the same nesting
/// by hand, so both paths need fixing together.
#[test]
fn the_initialize_response_advertises_capabilities_where_dap_expects_them() {
    let mut editor = Editor::attach();

    editor.send(serde_json::json!({
        "seq": 1,
        "type": "request",
        "command": "initialize",
        "arguments": {"adapterID": "clarinet"},
    }));
    let response = editor.recv();

    assert_eq!(
        response["body"]["supportsConfigurationDoneRequest"],
        serde_json::json!(true),
        "capabilities must sit directly in `body`, not nested under \
         `body.capabilities`; the adapter sent {response}"
    );
}
