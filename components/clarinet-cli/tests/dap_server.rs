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
fn fixture_manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("dap")
        .join("Clarinet.toml")
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
    handle: Option<JoinHandle<Result<(), String>>>,
}

impl DapServer {
    /// Start the server in SDK-only mode (no DAP client).
    fn start() -> Self {
        let sdk_port = free_port();
        let manifest = fixture_manifest();
        let handle = std::thread::spawn(move || run_dap_server(None, sdk_port, manifest));
        DapServer {
            sdk_port,
            handle: Some(handle),
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
        self.request(serde_json::json!({
            "method": "callPublicFn",
            "contract": contract,
            "function": function,
            "args": [],
        }))
    }
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
