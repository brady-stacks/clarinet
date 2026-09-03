//! Integration tests for `clarinet dap`'s SDK-facing JSON-line server
//! (`clarinet_lib::frontend::dap::run_dap_server`).
//!
//! Each test starts the server in-process on a thread and talks to it over a
//! real TCP socket, exactly as the SDK's sync worker does. Because the server
//! is run in-process, a test can also join the thread and assert on the
//! `Result<(), String>` it returned — several findings are precisely "the
//! server exits with an error when it should keep serving".

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use clarinet_lib::frontend::dap::run_dap_server;

/// The fixture project every test runs against: one `counter` contract with a
/// public function that mutates state and emits a `print` event, a read-only
/// getter, and a private function.
fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("dap")
}

fn fixture_manifest() -> PathBuf {
    fixture_dir().join("Clarinet.toml")
}

/// Ask the OS for an unused port, then release it so the server can bind it.
fn free_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind an ephemeral port");
    listener
        .local_addr()
        .expect("failed to read ephemeral port")
        .port()
}

/// A `run_dap_server` instance running on its own thread.
struct DapServer {
    sdk_port: u16,
    dap_port: Option<u16>,
    handle: Option<JoinHandle<Result<(), String>>>,
}

impl DapServer {
    /// Start the server in SDK-only mode (no DAP client).
    fn start() -> Self {
        Self::start_with_dap_port(None)
    }

    /// Start the server in attach mode, listening for a DAP client too.
    fn start_attach() -> Self {
        Self::start_with_dap_port(Some(free_port()))
    }

    fn start_with_dap_port(dap_port: Option<u16>) -> Self {
        let sdk_port = free_port();
        let manifest = fixture_manifest();
        let handle = std::thread::spawn(move || run_dap_server(dap_port, sdk_port, manifest));
        DapServer {
            sdk_port,
            dap_port,
            handle: Some(handle),
        }
    }

    /// Connect to the DAP port, retrying until the server has bound it.
    fn connect_dap(&mut self) -> TcpStream {
        let port = self
            .dap_port
            .expect("server was started without a DAP port");
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(stream) = TcpStream::connect(("127.0.0.1", port)) {
                return stream;
            }
            if let Some(result) = self.exit_result() {
                panic!("the server exited before accepting a DAP client: {result:?}");
            }
            assert!(
                Instant::now() < deadline,
                "could not connect to the DAP port {port}"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// Connect an SDK client, retrying until the server has bound its listener.
    fn connect(&mut self) -> SdkClient {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(stream) = TcpStream::connect(("127.0.0.1", self.sdk_port)) {
                return SdkClient::new(stream);
            }
            if let Some(result) = self.exit_result() {
                panic!("the server exited before accepting a client: {result:?}");
            }
            assert!(
                Instant::now() < deadline,
                "could not connect to the SDK port {}",
                self.sdk_port
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// Wait up to `timeout` for the server thread to return. `None` means it is
    /// still running, which for most tests is the passing outcome.
    fn wait_for_exit(&mut self, timeout: Duration) -> Option<Result<(), String>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(result) = self.exit_result() {
                return Some(result);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// The server's return value if the thread has already finished, else `None`.
    /// Never blocks.
    fn exit_result(&mut self) -> Option<Result<(), String>> {
        let handle = self.handle.take_if(|handle| handle.is_finished())?;
        Some(
            handle
                .join()
                .unwrap_or_else(|_| panic!("the server thread panicked")),
        )
    }
}

/// A newline-delimited JSON client, matching what the SDK's worker speaks.
struct SdkClient {
    writer: TcpStream,
    reader: BufReader<TcpStream>,
    next_id: u64,
}

impl SdkClient {
    fn new(stream: TcpStream) -> Self {
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .expect("failed to set a read timeout");
        let reader = BufReader::new(stream.try_clone().expect("failed to clone the stream"));
        SdkClient {
            writer: stream,
            reader,
            next_id: 1,
        }
    }

    /// Send a request with an auto-assigned `id` and read the single-line reply.
    fn request(&mut self, mut request: serde_json::Value) -> serde_json::Value {
        let id = self.next_id;
        self.next_id += 1;
        request["id"] = serde_json::json!(id);
        self.send_raw(&serde_json::to_string(&request).unwrap());
        self.read_response()
    }

    fn send_raw(&mut self, line: &str) {
        writeln!(self.writer, "{line}").expect("failed to write a request");
        self.writer.flush().expect("failed to flush a request");
    }

    fn read_response(&mut self) -> serde_json::Value {
        let mut line = String::new();
        let read = self
            .reader
            .read_line(&mut line)
            .expect("failed to read a response");
        assert_ne!(read, 0, "the server closed the connection without replying");
        serde_json::from_str(&line)
            .unwrap_or_else(|e| panic!("the server sent invalid JSON ({e}): {line}"))
    }

    fn call_public(&mut self, contract: &str, function: &str) -> serde_json::Value {
        self.call(contract, function, "callPublicFn")
    }

    fn call_read_only(&mut self, contract: &str, function: &str) -> serde_json::Value {
        self.call(contract, function, "callReadOnlyFn")
    }

    fn call(&mut self, contract: &str, function: &str, method: &str) -> serde_json::Value {
        self.request(serde_json::json!({
            "method": method,
            "contract": contract,
            "function": function,
            "args": [],
        }))
    }

    fn mine_block(&mut self, txs: serde_json::Value) -> serde_json::Value {
        self.request(serde_json::json!({"method": "mineBlock", "txs": txs}))
    }

    /// `(get-count)` as a hex-encoded Clarity `uint`.
    fn count(&mut self) -> String {
        let response = self.call_read_only("counter", "get-count");
        response["result"]["result"]
            .as_str()
            .unwrap_or_else(|| panic!("no result in {response}"))
            .to_string()
    }
}

/// A Clarity `uint` as the server hex-encodes it: type byte `0x01` followed by
/// a 16-byte big-endian value.
fn clarity_uint(value: u128) -> String {
    format!("0x01{:032x}", value)
}

/// Smoke test: the harness can start the server and complete a round trip.
#[test]
fn harness_starts_the_server_and_completes_a_round_trip() {
    let mut server = DapServer::start();
    let mut client = server.connect();

    let accounts = client.request(serde_json::json!({"method": "getAccounts"}));
    assert!(
        accounts["result"]["accounts"]["deployer"].is_string(),
        "expected a deployer account, got {accounts}"
    );

    let result = client.call_public("counter", "increment");
    // 0x07 = `ok`, 0x03 = `true`.
    assert_eq!(
        result["result"]["result"], "0x0703",
        "increment should return (ok true), got {result}"
    );
}

/// Finding 20: `eval_snippet_as_tx` hardcodes `"events": "[]"`
/// (`src/frontend/dap.rs`), discarding the events `ExecutionResult` carries.
/// Under `CLARINET_DEBUG_PORT` every event assertion in an existing test
/// therefore fails even though the transaction emitted the event.
#[test]
fn a_contract_call_reports_the_events_it_emitted() {
    let mut server = DapServer::start();
    let mut client = server.connect();

    // `increment` runs `(print { event: "increment" })`, so exactly one
    // print event is emitted.
    let response = client.call_public("counter", "increment");
    let events = response["result"]["events"]
        .as_str()
        .unwrap_or_else(|| panic!("no events field in {response}"));
    let events: serde_json::Value = serde_json::from_str(events)
        .unwrap_or_else(|e| panic!("events was not a JSON array ({e}): {events}"));

    assert_eq!(
        events.as_array().map(Vec::len),
        Some(1),
        "expected the print event emitted by `increment`, got {events}"
    );
}

/// Finding 9: `dap.rs` routes `"callReadOnlyFn"` to the same `call_contract` as
/// `"callPublicFn"`, so the call emits an ordinary `contract-call?` and commits
/// whatever it did. Nothing checks the function's declared type, so a
/// state-mutating public function invoked through `simnet.callReadOnlyFn` — the
/// form existing tests use to assert *without* mutating — changes the chain.
#[test]
fn call_read_only_fn_does_not_commit_state() {
    let mut server = DapServer::start();
    let mut client = server.connect();

    assert_eq!(client.count(), clarity_uint(0), "the fixture starts at u0");

    // `increment` is `define-public` and mutates `count`. Reaching it through
    // callReadOnlyFn should be refused, or at minimum leave no trace.
    let response = client.call_read_only("counter", "increment");

    assert_eq!(
        client.count(),
        clarity_uint(0),
        "callReadOnlyFn committed the state change from `increment`; \
         the server answered {response}"
    );
}

/// Finding 20: `mineBlock` matches `"callPrivateFn"` alongside
/// `"callPublicFn"` (`dap.rs`) and sends both through `call_contract`, which
/// builds a `contract-call?`. A `define-private` function is not reachable that
/// way, so the transaction type the SDK exposes cannot do what it claims —
/// unlike the real `simnet.callPrivateFn`. The top-level proxy method has the
/// same problem from the other direction: it sends `method: "callPublicFn"`
/// (`syncDebugSimnet.ts`).
#[test]
fn mine_block_can_call_a_private_function() {
    let mut server = DapServer::start();
    let mut client = server.connect();

    let response = client.mine_block(serde_json::json!([{
        "type": "callPrivateFn",
        "contract": "counter",
        "function": "double",
        "args": ["u21"],
    }]));

    let tx = &response["result"]["results"][0];
    assert_eq!(
        tx["result"].as_str(),
        Some(clarity_uint(42).as_str()),
        "`double` should have returned u42; the server answered {response}"
    );
}

/// Finding 21: `initSession` canonicalizes the requested `manifestPath` in the
/// *server's* process and ignores the `cwd` the client sends alongside it
/// (`syncDebugSimnet.ts` sends both). `initSimnet()`'s default manifest is the
/// relative `"./Clarinet.toml"`, so it resolves against wherever `clarinet dap`
/// was started rather than where the test runs, and the guard rejects a request
/// that names the very manifest the server is already using.
///
/// The CodeLens flow only works because it happens to spawn the server with
/// `cwd: projectRoot`; a server started anywhere else refuses every session.
#[test]
fn init_session_resolves_a_relative_manifest_against_the_client_cwd() {
    let mut server = DapServer::start();
    let mut client = server.connect();

    // `<fixture>/./Clarinet.toml` is exactly the manifest the server loaded.
    let response = client.request(serde_json::json!({
        "method": "initSession",
        "cwd": fixture_dir(),
        "manifestPath": "./Clarinet.toml",
    }));

    assert!(
        response["error"].is_null(),
        "initSession rejected the manifest it is already using, because it \
         resolved the relative path against its own cwd instead of the `cwd` \
         the client sent: {response}"
    );
}

/// Finding 26: the read arm of the request loop was deliberately softened to
/// log and `break 'request`, but every response still goes out through
/// `write_response(&mut writer, &resp)?`, whose error propagates out of
/// `run_dap_server` and takes the process down. A client that disappears
/// between its request and the response — the common case when a Vitest worker
/// times out or is killed — kills the server for every other client.
///
/// The `disconnect` arm already ignores write errors with
/// `let _ = writeln!(...)`, so the file is inconsistent with itself.
///
/// The client below pipelines several requests in one write and then drops the
/// socket without reading. The server answers the first request successfully,
/// the peer's kernel resets the connection because data arrived for a closed
/// socket, and the second `write_response` fails — while the remaining requests
/// are already sitting in the server's `BufReader`, so it still has work to do.
#[test]
fn a_client_that_disconnects_before_reading_does_not_kill_the_server() {
    let mut server = DapServer::start();

    let pipelined = (1..=20)
        .map(|id| format!(r#"{{"id":{id},"method":"getAccounts"}}"#))
        .collect::<Vec<_>>()
        .join("\n");
    {
        let mut abandoning = server.connect();
        abandoning.send_raw(&pipelined);
    } // dropped without ever reading a response

    if let Some(result) = server.wait_for_exit(Duration::from_secs(5)) {
        panic!("the server exited when a client stopped reading: {result:?}");
    }

    // The server should be back in its accept loop, serving the next client.
    let mut next = server.connect();
    let response = next.request(serde_json::json!({"method": "getAccounts"}));
    assert!(
        response["result"]["accounts"]["deployer"].is_string(),
        "the server survived but stopped answering: {response}"
    );
}

/// Finding 27: the EOF fix turned `init_attach`'s busy-spin into a hard exit.
/// `init_attach` returns `Err(ParseError::Eof)`, the handshake thread maps it to
/// a `String`, and `??` propagates it out of `run_dap_server`. Because the join
/// now happens *before* the SDK accept loop, an editor that connects and then
/// closes — a cancelled debug session, an extension reload — kills the server
/// before it ever accepts the test runner.
///
/// In the CodeLens flow the terminal command has already been queued with
/// `CLARINET_DEBUG_PORT` pointing at a port that is about to close, so the
/// developer sees `connectSyncSocket` fail while the real reason sits in the
/// extension's output channel. Falling back to `DAPDebugger::no_op()` would
/// degrade to SDK-only mode and keep the test run working.
#[test]
fn a_dap_client_disconnecting_mid_handshake_does_not_kill_the_server() {
    let mut server = DapServer::start_attach();

    // An editor attaches and then goes away without completing the handshake.
    drop(server.connect_dap());

    if let Some(result) = server.wait_for_exit(Duration::from_secs(5)) {
        panic!("the server exited when the DAP client disconnected: {result:?}");
    }

    let mut client = server.connect();
    let response = client.request(serde_json::json!({"method": "getAccounts"}));
    assert!(
        response["result"]["accounts"]["deployer"].is_string(),
        "the server should have degraded to SDK-only mode: {response}"
    );
}
