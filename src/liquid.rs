use std::{ffi::OsString, path::PathBuf, str::FromStr};

use bitcoin::{Amount, Denomination};

use crate::{IssuanceTxIn, Liquid, MintResponse, NigiriClient, NigiriError, rpc::RpcInvocation};

// Phase 3 removes this dormant CLI builder with the rest of the CLI transport.
#[allow(dead_code)]
fn build_faucet_asset_invocation(
    executable: PathBuf,
    address: &str,
    amount: Amount,
    asset: &elements::AssetId,
) -> RpcInvocation {
    RpcInvocation {
        executable,
        args: vec![
            OsString::from("faucet"),
            OsString::from("--liquid"),
            OsString::from(address),
            OsString::from(exact_btc_amount(amount)),
            OsString::from(asset.to_string()),
        ],
        // `nigiri faucet --liquid <address> <amount> <asset>`: the address is
        // the first caller value, not part of a method prefix.
        caller_args_from: 2,
    }
}

// Phase 3 removes this dormant CLI builder with the rest of the CLI transport.
#[allow(dead_code)]
fn build_mint_invocation(
    executable: PathBuf,
    address: &str,
    quantity: u64,
    name: &str,
    ticker: &str,
) -> RpcInvocation {
    RpcInvocation {
        executable,
        args: vec![
            OsString::from("mint"),
            OsString::from(address),
            OsString::from(quantity.to_string()),
            OsString::from(name),
            OsString::from(ticker),
        ],
        // `nigiri mint <address> <quantity> <name> <ticker>`: every argument
        // after the subcommand came from the caller.
        caller_args_from: 1,
    }
}

fn exact_btc_amount(amount: Amount) -> String {
    amount.to_string_in(Denomination::Bitcoin)
}

impl NigiriClient<Liquid> {
    /// Mints a Liquid asset and sends it to `address` over the Elements node RPC.
    ///
    /// The asset ID is derived from the JSON contract submitted to `issueasset`,
    /// so identical inputs intentionally produce a different asset ID than
    /// Nigiri's `mint` command.
    ///
    /// This operation is not atomic: if `issueasset` succeeds but `sendtoaddress`
    /// fails, the asset remains issued. Inspect the node state before retrying.
    pub async fn mint(
        &self,
        address: &str,
        quantity: u64,
        name: &str,
        ticker: &str,
    ) -> Result<MintResponse, NigiriError> {
        let contract_hash = elements::ContractHash::from_json_contract(&asset_contract(
            name, ticker,
        ))
        .map_err(|_| NigiriError::InvalidRequest {
            detail: "asset contract could not be constructed".into(),
        })?;
        let issued: IssueAssetResponse = crate::node_rpc::call(
            self,
            "issueasset",
            (quantity, 0_u64, true, contract_hash.to_string()),
        )
        .await?;
        let txid = crate::node_rpc::call(
            self,
            "sendtoaddress",
            (
                address,
                quantity,
                "",
                "",
                false,
                false,
                1_u64,
                "unset",
                false,
                issued.asset.to_string(),
            ),
        )
        .await?;
        Ok(MintResponse {
            asset: issued.asset,
            txid,
            issuance_txin: IssuanceTxIn {
                txid: issued.txid,
                vin: issued.vin,
            },
        })
    }

    /// Sends a Liquid asset to an address over the Elements node RPC.
    pub async fn faucet_asset(
        &self,
        address: &str,
        amount: Amount,
        asset: &elements::AssetId,
    ) -> Result<elements::Txid, NigiriError> {
        let amount = serde_json::Number::from_str(&amount.to_string_in(Denomination::Bitcoin))
            .map_err(|_| NigiriError::InvalidRequest {
                detail: "asset amount could not be represented as JSON".into(),
            })?;
        crate::node_rpc::call(
            self,
            "sendtoaddress",
            (
                address,
                amount,
                "",
                "",
                false,
                false,
                1_u64,
                "unset",
                false,
                asset.to_string(),
            ),
        )
        .await
    }
}

#[derive(serde::Deserialize)]
struct IssueAssetResponse {
    txid: elements::Txid,
    vin: u32,
    asset: elements::AssetId,
}

fn asset_contract(name: &str, ticker: &str) -> String {
    serde_json::json!({
        "entity": { "domain": "nigiri-rs.invalid" },
        "issuer_pubkey": "00".repeat(33),
        "name": name,
        "precision": 0,
        "ticker": ticker,
        "version": 0,
    })
    .to_string()
}

// Phase 3 removes this dormant CLI parser with the rest of the CLI transport.
#[allow(dead_code)]
fn parse_mint_response(stdout: &str) -> Result<MintResponse, NigiriError> {
    let asset = parse_label(stdout, "asset:", "mint asset identifier")?;
    let txid = parse_label(stdout, "txId:", "mint transaction identifier")?;
    let issuance_txid = optional_label(stdout, "  txid:")?;
    let issuance_vin = optional_label(stdout, "  vin:")?;
    let issuance_txin = match (issuance_txid, issuance_vin) {
        (Some(txid), Some(vin)) => IssuanceTxIn { txid, vin },
        (None, None) => return Err(invalid("mint", "complete issuance input")),
        _ => return Err(invalid("mint", "complete issuance input")),
    };
    Ok(MintResponse {
        asset,
        txid,
        issuance_txin,
    })
}

fn parse_label<T>(
    stdout: &str,
    label: &'static str,
    expected: &'static str,
) -> Result<T, NigiriError>
where
    T: FromStr,
{
    let value = stdout
        .lines()
        .find_map(|line| line.strip_prefix(label))
        .map(str::trim)
        .ok_or_else(|| invalid(label.trim_end_matches(':'), expected))?;
    value
        .parse()
        .map_err(|_| invalid(label.trim_end_matches(':'), expected))
}

fn optional_label<T>(stdout: &str, label: &'static str) -> Result<Option<T>, NigiriError>
where
    T: FromStr,
{
    stdout
        .lines()
        .find_map(|line| line.strip_prefix(label))
        .map(str::trim)
        .map(|value| value.parse().map_err(|_| invalid("mint", "issuance input")))
        .transpose()
}

fn invalid(operation: &'static str, expected: &'static str) -> NigiriError {
    NigiriError::InvalidResponse {
        operation: operation.into(),
        detail: format!("expected {expected}"),
    }
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, path::PathBuf, time::Duration};

    use bitcoin::Amount;
    use serde_json::{Value, json};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    use url::Url;

    use super::{
        asset_contract, build_faucet_asset_invocation, build_mint_invocation, parse_mint_response,
    };
    use crate::{IssuanceTxIn, Liquid, NigiriClient, NigiriConfig, NigiriError};

    const ASSET: &str = "5ac9f65c0efcc4775e0baec4ec03abdde22473cd3cf33c0419ca290e0751b225";
    const ISSUE_TXID: &str = "7777777777777777777777777777777777777777777777777777777777777777";
    const SEND_TXID: &str = "8888888888888888888888888888888888888888888888888888888888888888";

    async fn sequential_server(
        responses: Vec<(&'static str, String)>,
    ) -> (Url, tokio::task::JoinHandle<Vec<Value>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut requests = Vec::with_capacity(responses.len());
            for (status, body) in responses {
                let request_number = requests.len() + 1;
                let (mut stream, _) =
                    tokio::time::timeout(Duration::from_secs(3), listener.accept())
                        .await
                        .unwrap_or_else(|_| {
                            panic!("timed out waiting for JSON-RPC request {request_number}")
                        })
                        .unwrap();
                let mut request = Vec::new();
                let mut buffer = [0_u8; 4096];
                loop {
                    let count = stream.read(&mut buffer).await.unwrap();
                    if count == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..count]);
                    let header_end = request
                        .windows(4)
                        .position(|window| window == b"\r\n\r\n")
                        .map(|index| index + 4);
                    if let Some(header_end) = header_end {
                        let headers = String::from_utf8_lossy(&request[..header_end]);
                        let content_length = headers
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .and_then(|value| value.parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if request.len() >= header_end + content_length {
                            let body = &request[header_end..header_end + content_length];
                            requests.push(serde_json::from_slice(body).unwrap());
                            break;
                        }
                    }
                }
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
            requests
        });
        (Url::parse(&format!("http://{address}/")).unwrap(), task)
    }

    fn rpc_response(result: Value) -> String {
        json!({"result": result, "error": null, "id": "nigiri-rs"}).to_string()
    }

    fn rpc_client(url: Url) -> NigiriClient<Liquid> {
        NigiriClient::with_config(NigiriConfig {
            node_rpc_url: url,
            timeout: Duration::from_secs(2),
            ..Default::default()
        })
        .unwrap()
    }

    #[test]
    fn asset_contract_matches_the_published_elements_vector_and_crate_contract() {
        let tether = r#"{"entity":{"domain":"tether.to"},"issuer_pubkey":"0337cceec0beea0232ebe14cba0197a9fbd45fcf2ec946749de920e71434c2b904","name":"Tether USD","precision":8,"ticker":"USDt","version":0}"#;
        assert_eq!(
            elements::ContractHash::from_json_contract(tether)
                .unwrap()
                .to_string(),
            "3c7f0a53c2ff5b99590620d7f6604a7a3a7bfbaaa6aa61f7bfc7833ca03cde82"
        );

        let contract: Value = serde_json::from_str(&asset_contract("NigiriRsTest", "NRT")).unwrap();
        assert_eq!(contract["entity"]["domain"], "nigiri-rs.invalid");
        assert_eq!(contract["issuer_pubkey"], "00".repeat(33));
        assert_eq!(contract["name"], "NigiriRsTest");
        assert_eq!(contract["precision"], 0);
        assert_eq!(contract["ticker"], "NRT");
        assert_eq!(contract["version"], 0);
    }

    #[tokio::test]
    async fn faucet_asset_sends_the_elements_sendtoaddress_vector() {
        let (url, requests) = sequential_server(vec![(
            "200 OK",
            rpc_response(Value::String(SEND_TXID.to_owned())),
        )])
        .await;
        let client = rpc_client(url);
        let asset = ASSET.parse().unwrap();

        assert_eq!(
            client
                .faucet_asset("ert1qdestination", Amount::from_sat(1), &asset)
                .await
                .unwrap()
                .to_string(),
            SEND_TXID
        );

        assert_eq!(
            requests.await.unwrap(),
            vec![json!({
                "jsonrpc": "1.0",
                "id": "nigiri-rs",
                "method": "sendtoaddress",
                "params": serde_json::from_str::<Value>(
                    r#"["ert1qdestination",0.00000001,"","",false,false,1,"unset",false,"5ac9f65c0efcc4775e0baec4ec03abdde22473cd3cf33c0419ca290e0751b225"]"#
                ).unwrap(),
            })]
        );
    }

    #[tokio::test]
    async fn mint_issues_then_sends_the_asset_and_returns_issuance_input() {
        let (url, requests) = sequential_server(vec![
            (
                "200 OK",
                rpc_response(json!({
                    "txid": ISSUE_TXID,
                    "vin": 2,
                    "asset": ASSET,
                    "entropy": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "token": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                })),
            ),
            ("200 OK", rpc_response(Value::String(SEND_TXID.to_owned()))),
        ])
        .await;
        let client = rpc_client(url);

        let minted = client
            .mint("ert1qdestination", 1_000, "NigiriRsTest", "NRT")
            .await
            .unwrap();

        assert_eq!(minted.asset.to_string(), ASSET);
        assert_eq!(minted.txid.to_string(), SEND_TXID);
        assert_eq!(
            minted.issuance_txin,
            IssuanceTxIn {
                txid: ISSUE_TXID.parse().unwrap(),
                vin: 2,
            }
        );

        let requests = requests.await.unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["method"], "issueasset");
        let expected_contract_hash =
            elements::ContractHash::from_json_contract(&asset_contract("NigiriRsTest", "NRT"))
                .unwrap()
                .to_string();
        assert_eq!(
            requests[0]["params"],
            json!([1_000, 0, true, expected_contract_hash])
        );
        assert_eq!(
            requests[1],
            json!({
                "jsonrpc": "1.0",
                "id": "nigiri-rs",
                "method": "sendtoaddress",
                "params": ["ert1qdestination", 1000, "", "", false, false, 1, "unset", false, ASSET],
            })
        );
    }

    #[tokio::test]
    async fn mint_returns_the_send_error_after_the_asset_has_been_issued() {
        let (url, requests) = sequential_server(vec![
            (
                "200 OK",
                rpc_response(json!({
                    "txid": ISSUE_TXID,
                    "vin": 2,
                    "asset": ASSET,
                    "entropy": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "token": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                })),
            ),
            (
                "500 Internal Server Error",
                json!({
                    "result": null,
                    "error": {"code": -6, "message": "Insufficient funds"},
                    "id": "nigiri-rs",
                })
                .to_string(),
            ),
        ])
        .await;
        let client = rpc_client(url);

        let error = client
            .mint("ert1qdestination", 1_000, "NigiriRsTest", "NRT")
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            NigiriError::RpcFailed {
                code: -6,
                ref message,
                ..
            } if message == "Insufficient funds"
        ));
        let requests = requests.await.unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["method"], "issueasset");
        assert_eq!(requests[1]["method"], "sendtoaddress");
    }

    #[tokio::test]
    async fn mint_returns_the_issue_error_without_attempting_a_send() {
        let (url, requests) = sequential_server(vec![(
            "500 Internal Server Error",
            json!({
                "result": null,
                "error": {"code": -8, "message": "Invalid asset amount"},
                "id": "nigiri-rs",
            })
            .to_string(),
        )])
        .await;
        let client = rpc_client(url);

        let error = client
            .mint("ert1qdestination", 1_000, "NigiriRsTest", "NRT")
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            NigiriError::RpcFailed {
                code: -8,
                ref message,
                ..
            } if message == "Invalid asset amount"
        ));
        let requests = requests.await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["method"], "issueasset");
    }

    #[test]
    fn cli_subcommand_invocations_mark_every_caller_value_as_redactable() {
        let asset: elements::AssetId =
            "5ac9f65c0efcc4775e0baec4ec03abdde22473cd3cf33c0419ca290e0751b225"
                .parse()
                .unwrap();

        let mint = build_mint_invocation(
            PathBuf::from("nigiri"),
            "ert1qdestination",
            7,
            "AssetName",
            "TIK",
        );
        assert_eq!(
            mint.args[mint.caller_args_from..],
            [
                OsString::from("ert1qdestination"),
                OsString::from("7"),
                OsString::from("AssetName"),
                OsString::from("TIK"),
            ]
        );

        let faucet = build_faucet_asset_invocation(
            PathBuf::from("nigiri"),
            "ert1qdestination",
            Amount::from_sat(1),
            &asset,
        );
        assert_eq!(
            faucet.args[faucet.caller_args_from..],
            [
                OsString::from("ert1qdestination"),
                OsString::from("0.00000001"),
                OsString::from(asset.to_string()),
            ]
        );
    }

    #[test]
    fn asset_faucet_uses_liquid_flag_and_exact_decimal_amount() {
        let asset: elements::AssetId =
            "5ac9f65c0efcc4775e0baec4ec03abdde22473cd3cf33c0419ca290e0751b225"
                .parse()
                .unwrap();

        let invocation = build_faucet_asset_invocation(
            PathBuf::from("nigiri"),
            "ert1qdestination",
            Amount::from_sat(1),
            &asset,
        );

        assert_eq!(
            invocation.args,
            [
                OsString::from("faucet"),
                OsString::from("--liquid"),
                OsString::from("ert1qdestination"),
                OsString::from("0.00000001"),
                OsString::from(asset.to_string()),
            ]
        );
    }

    #[test]
    fn mint_parser_returns_native_asset_and_transaction_identifiers() {
        let output = r#"asset: 5ac9f65c0efcc4775e0baec4ec03abdde22473cd3cf33c0419ca290e0751b225
txId: 7777777777777777777777777777777777777777777777777777777777777777
  txid: 8888888888888888888888888888888888888888888888888888888888888888
  vin: 2
"#;

        let parsed = parse_mint_response(output).unwrap();

        assert_eq!(
            parsed.asset.to_string(),
            "5ac9f65c0efcc4775e0baec4ec03abdde22473cd3cf33c0419ca290e0751b225"
        );
        assert_eq!(
            parsed.txid.to_string(),
            "7777777777777777777777777777777777777777777777777777777777777777"
        );
        assert_eq!(parsed.issuance_txin.vin, 2);
    }
}
