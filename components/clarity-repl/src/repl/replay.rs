use std::path::PathBuf;

use clarity::vm::{EvaluationResult, ExecutionResult};

use super::remote_data::{HttpClient, TransactionDetails};
use super::session::Session;
use super::settings::{ApiUrl, RemoteDataSettings, Settings};
use super::{ClarityCodeSource, ClarityContract, ContractDeployer, Epoch, SessionSettings};

/// The result of replaying a transaction via MXS.
#[derive(Debug)]
pub struct ReplayResult {
    pub txid: String,
    pub tx_type: String,
    /// The Stacks block height at which the session was initialized (block_height - 1).
    pub session_height: u32,
    pub sender: String,
    /// The Clarity snippet that was executed.
    pub snippet: String,
    pub execution: ExecutionResult,
}

/// Fetch a transaction and replay it in an MXS session initialized at the block
/// height just before the transaction was mined.
///
/// `block_height_override` forces a specific session height instead of deriving
/// it from the transaction's `block_height`.
pub fn replay_transaction(
    api_url: ApiUrl,
    txid: &str,
    block_height_override: Option<u32>,
    cache_location: Option<PathBuf>,
) -> Result<ReplayResult, String> {
    let client = HttpClient::new(api_url.clone());
    let tx = client.fetch_transaction(txid)?;

    let session_height = match block_height_override {
        Some(h) => h,
        None => tx
            .block_height
            .map(|h| h.saturating_sub(1))
            .ok_or_else(|| {
                format!(
                    "transaction {} has no block_height — is it still pending?",
                    tx.tx_id
                )
            })?,
    };

    let use_mainnet_wallets = is_mainnet_url(&api_url);

    let settings = SessionSettings {
        repl_settings: Settings {
            remote_data: RemoteDataSettings {
                enabled: true,
                api_url,
                initial_height: Some(session_height),
                use_mainnet_wallets,
            },
            ..Default::default()
        },
        cache_location,
        ..Default::default()
    };

    let mut session = Session::new(settings);
    let (snippet, execution) = execute_tx(&mut session, &tx)?;

    Ok(ReplayResult {
        txid: tx.tx_id,
        tx_type: tx.tx_type,
        session_height,
        sender: tx.sender_address,
        snippet,
        execution,
    })
}

fn is_mainnet_url(api_url: &ApiUrl) -> bool {
    let url = api_url.0.to_lowercase();
    !url.contains("testnet") && !url.contains("krypton")
}

fn execute_tx(
    session: &mut Session,
    tx: &TransactionDetails,
) -> Result<(String, ExecutionResult), String> {
    match tx.tx_type.as_str() {
        "contract_call" => {
            let call = tx
                .contract_call
                .as_ref()
                .ok_or("missing contract_call field on transaction")?;

            let args_str = call
                .function_args
                .iter()
                .map(|a| a.repr.as_str())
                .collect::<Vec<_>>()
                .join(" ");

            let snippet = if args_str.is_empty() {
                format!(
                    "(contract-call? '{} {})",
                    call.contract_id, call.function_name
                )
            } else {
                format!(
                    "(contract-call? '{} {} {})",
                    call.contract_id, call.function_name, args_str
                )
            };

            let original_sender = session.get_tx_sender();
            session.set_tx_sender(&tx.sender_address);
            let result = session
                .eval(snippet.clone(), true)
                .map_err(|diags| {
                    diags
                        .iter()
                        .map(|d| d.message.clone())
                        .collect::<Vec<_>>()
                        .join("; ")
                })?;
            session.set_tx_sender(&original_sender);

            Ok((snippet, result.execution_result))
        }

        "smart_contract" => {
            let sc = tx
                .smart_contract
                .as_ref()
                .ok_or("missing smart_contract field on transaction")?;

            let (deployer_addr, contract_name) = sc
                .contract_id
                .rsplit_once('.')
                .ok_or_else(|| format!("invalid contract_id: {}", sc.contract_id))?;

            let current_epoch = session.interpreter.datastore.get_current_epoch();
            let clarity_version =
                clarity::vm::ClarityVersion::default_for_epoch(current_epoch);

            let contract = ClarityContract {
                code_source: ClarityCodeSource::ContractInMemory(sc.source_code.clone()),
                name: contract_name.to_string(),
                deployer: ContractDeployer::Address(deployer_addr.to_string()),
                clarity_version,
                epoch: Epoch::Specific(current_epoch),
                skip_analysis: false,
            };

            let snippet = format!("(deploy '{}.{})", deployer_addr, contract_name);

            let result = session
                .deploy_contract(&contract, true, None)
                .map_err(|diags| {
                    diags
                        .iter()
                        .map(|d| d.message.clone())
                        .collect::<Vec<_>>()
                        .join("; ")
                })?;

            Ok((snippet, result.execution_result))
        }

        "token_transfer" => {
            let transfer = tx
                .token_transfer
                .as_ref()
                .ok_or("missing token_transfer field on transaction")?;

            let amount: u64 = transfer.amount.parse().map_err(|_| {
                format!("invalid token_transfer amount: {}", transfer.amount)
            })?;

            let snippet = format!(
                "(stx-transfer? u{} tx-sender '{})",
                amount, transfer.recipient_address
            );

            let original_sender = session.get_tx_sender();
            session.set_tx_sender(&tx.sender_address);
            let result = session
                .eval(snippet.clone(), true)
                .map_err(|diags| {
                    diags
                        .iter()
                        .map(|d| d.message.clone())
                        .collect::<Vec<_>>()
                        .join("; ")
                })?;
            session.set_tx_sender(&original_sender);

            Ok((snippet, result.execution_result))
        }

        other => Err(format!(
            "unsupported transaction type '{other}' — only contract_call, smart_contract, and token_transfer are supported"
        )),
    }
}

/// Format an `ExecutionResult` into a human-readable string for CLI output.
pub fn format_execution_result(result: &ExecutionResult) -> String {
    let mut out = String::new();

    match &result.result {
        EvaluationResult::Snippet(res) => {
            out.push_str(&format!("Result: {}\n", res.result));
        }
        EvaluationResult::Contract(res) => {
            out.push_str(&format!(
                "Result: contract '{}' deployed\n",
                res.contract.contract_identifier
            ));
        }
    }

    if !result.events.is_empty() {
        out.push_str("\nEvents:\n");
        for event in &result.events {
            out.push_str(&format!("  - {}\n", crate::utils::serialize_event(event)));
        }
    }

    if let Some(cost) = &result.cost {
        out.push_str("\nExecution costs:\n");
        out.push_str(&format!("  runtime:      {}\n", cost.total.runtime));
        out.push_str(&format!("  read_count:   {}\n", cost.total.read_count));
        out.push_str(&format!("  read_length:  {}\n", cost.total.read_length));
        out.push_str(&format!("  write_count:  {}\n", cost.total.write_count));
        out.push_str(&format!("  write_length: {}\n", cost.total.write_length));
    }

    out
}
