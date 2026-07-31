use std::{
    error::Error,
    fs::{File, OpenOptions},
    process::Stdio,
};

use fs2::FileExt;
use serde_json::Value;
use tokio::process::Command;

pub type BoxError = Box<dyn Error + Send + Sync>;

pub struct HostChainLock {
    file: File,
}

impl HostChainLock {
    pub fn acquire() -> Result<Self, BoxError> {
        let path = std::env::temp_dir().join("nigiri-rs-host-chain.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        file.lock_exclusive()?;
        Ok(Self { file })
    }
}

impl Drop for HostChainLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

pub async fn host_rpc(
    liquid: bool,
    method: &'static str,
    args: &[&str],
) -> Result<String, BoxError> {
    let mut command = Command::new("nigiri");
    command.arg("rpc");
    if liquid {
        command.arg("--liquid");
    }
    let output = command
        .arg(method)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await?;
    let stdout = strip_ansi(std::str::from_utf8(&output.stdout)?)
        .trim()
        .to_owned();
    // Anchored to a line start, matching the crate: response content that merely
    // contains the phrase is not an error.
    let reported_error = stdout
        .lines()
        .any(|line| line.trim_start().starts_with("error code:"));
    if !output.status.success() || stdout.is_empty() || reported_error {
        return Err(format!("host Nigiri RPC {method} failed").into());
    }
    Ok(stdout)
}

pub async fn signed_wallet_transaction(
    liquid: bool,
    destination: &str,
) -> Result<String, BoxError> {
    let outputs = if liquid {
        serde_json::to_string(&serde_json::json!([{ destination: 0.0001 }]))?
    } else {
        serde_json::to_string(&serde_json::json!({ destination: 0.0001 }))?
    };
    let raw = host_rpc(liquid, "createrawtransaction", &["[]", &outputs]).await?;
    let funded = host_rpc(liquid, "fundrawtransaction", &[&raw]).await?;
    let funded: Value = serde_json::from_str(&funded)
        .map_err(|_| BoxError::from("fundrawtransaction returned invalid JSON"))?;
    let funded_hex = funded["hex"]
        .as_str()
        .ok_or("fundrawtransaction omitted hex")?;
    let blinded;
    let signable_hex = if liquid {
        blinded = host_rpc(true, "blindrawtransaction", &[funded_hex]).await?;
        blinded.as_str()
    } else {
        funded_hex
    };
    let signed = host_rpc(liquid, "signrawtransactionwithwallet", &[signable_hex]).await?;
    let signed: Value = serde_json::from_str(&signed)
        .map_err(|_| BoxError::from("signrawtransactionwithwallet returned invalid JSON"))?;
    if signed["complete"] != Value::Bool(true) {
        return Err("wallet did not completely sign fixture transaction".into());
    }
    Ok(signed["hex"]
        .as_str()
        .ok_or("signrawtransactionwithwallet omitted hex")?
        .to_owned())
}

// Mirrors the crate's implementation. Iterates chars, not bytes: a byte-wise
// version rebuilt with char::from would turn every multibyte scalar into
// Latin-1 mojibake, corrupting non-ASCII node output.
fn strip_ansi(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut characters = input.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\u{1b}' && characters.peek() == Some(&'[') {
            let _ = characters.next();
            for control in characters.by_ref() {
                if ('@'..='~').contains(&control) {
                    break;
                }
            }
        } else {
            output.push(character);
        }
    }
    output
}
