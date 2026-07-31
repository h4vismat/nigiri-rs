use std::{ffi::OsString, path::PathBuf, str::FromStr};

use bitcoin::{Amount, Denomination};

use crate::{IssuanceTxIn, Liquid, MintResponse, NigiriClient, NigiriError, rpc::RpcInvocation};

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
    }
}

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
    }
}

fn exact_btc_amount(amount: Amount) -> String {
    amount.to_string_in(Denomination::Bitcoin)
}

impl NigiriClient<Liquid> {
    /// Mints a Liquid asset through Nigiri's structured CLI boundary.
    pub async fn mint(
        &self,
        address: &str,
        quantity: u64,
        name: &str,
        ticker: &str,
    ) -> Result<MintResponse, NigiriError> {
        let invocation = build_mint_invocation(
            self.config.executable.clone(),
            address,
            quantity,
            name,
            ticker,
        );
        let stdout = self.execute_invocation("mint", invocation).await?;
        parse_mint_response(&stdout)
    }

    /// Sends a native Liquid asset to an address through Nigiri's faucet CLI.
    pub async fn faucet_asset(
        &self,
        address: &str,
        amount: Amount,
        asset: &elements::AssetId,
    ) -> Result<elements::Txid, NigiriError> {
        let invocation =
            build_faucet_asset_invocation(self.config.executable.clone(), address, amount, asset);
        let stdout = self.execute_invocation("faucet", invocation).await?;
        parse_label(&stdout, "txId:", "asset faucet transaction id")
    }
}

fn parse_mint_response(stdout: &str) -> Result<MintResponse, NigiriError> {
    let asset = parse_label(stdout, "asset:", "mint asset identifier")?;
    let txid = parse_label(stdout, "txId:", "mint transaction identifier")?;
    let issuance_txid = optional_label(stdout, "  txid:")?;
    let issuance_vin = optional_label(stdout, "  vin:")?;
    let issuance_txin = match (issuance_txid, issuance_vin) {
        (Some(txid), Some(vin)) => Some(IssuanceTxIn { txid, vin }),
        (None, None) => None,
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
        operation,
        detail: format!("expected {expected}"),
    }
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, path::PathBuf};

    use bitcoin::Amount;

    use super::{build_faucet_asset_invocation, parse_mint_response};

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
        assert_eq!(parsed.issuance_txin.unwrap().vin, 2);
    }
}
