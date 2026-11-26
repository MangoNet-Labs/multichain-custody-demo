use crate::Signer;
use anyhow::{ensure, Context, Result};
use custody_store::mango::Quote;
use std::{env, path::Path, time::Duration};
use uuid::Uuid;

impl Signer {
    /// The local helper uses the pinned Mango SDK, never supplies key material
    /// and has no signing endpoint. Its only input is a verified frozen order.
    pub async fn prepare_mango(&self, helper: &Path, budget: u64) -> Result<bool> {
        ensure!(budget > 0, "invalid Mango gas budget");
        let mut tx = self.db.begin().await?;
        // Serialize signers without blocking checkpoint inserts that reference this wallet.
        let hot:Option<(String,Vec<u8>)>=sqlx::query_as("SELECT key_ref,address FROM wallets WHERE network='mango-testnet' AND role='hot' FOR NO KEY UPDATE SKIP LOCKED").fetch_optional(&mut *tx).await?;
        let Some((key, sender)) = hot else {
            return Ok(false);
        };
        let blocked:bool=sqlx::query_scalar("SELECT paused OR EXISTS(SELECT 1 FROM mango_payouts WHERE sponsor IS NULL AND confirmed_height IS NULL)
            OR EXISTS(SELECT 1 FROM withdrawals WHERE network='mango-testnet' AND status='review') FROM payment_control WHERE singleton")
            .fetch_one(&mut *tx).await?;
        if blocked {
            return Ok(false);
        }
        let row:Option<(String,Vec<u8>,String,String,bool)>=sqlx::query_as("SELECT w.id::text,w.recipient,w.amount::text,convert_from(w.asset,'UTF8'),
            (j.network=w.network AND j.asset=w.asset AND j.amount=w.amount AND j.debit_account='user:'||w.user_id||':available' AND j.credit_account='user:'||w.user_id||':frozen'
            AND NOT EXISTS(SELECT 1 FROM wallets x WHERE x.network=w.network AND x.address=w.recipient)) FROM withdrawals w JOIN journal j ON j.id=w.freeze_id
            WHERE w.network='mango-testnet' AND w.next_prepare<=now() AND
            (w.status='consolidating' OR (w.status='queued' AND NOT EXISTS(SELECT 1 FROM withdrawals WHERE network=w.network AND status='consolidating')))
            ORDER BY w.created_at,w.id LIMIT 1 FOR UPDATE OF w SKIP LOCKED")
            .fetch_optional(&mut *tx).await?;
        let Some((id, recipient, amount, asset, valid)) = row else {
            return Ok(false);
        };
        ensure!(
            valid && recipient.iter().any(|b| *b != 0) && amount.parse::<u64>()? > 0,
            "invalid frozen Mango order"
        );
        let q = async {
            let mut cmd = tokio::process::Command::new(helper);
            cmd.kill_on_drop(true)
                .env_remove("DATABASE_URL")
                .env_remove("CUSTODY_KEYSTORE")
                .env_remove("CUSTODY_UNLOCK_FILE")
                .args([
                    "--quote-withdrawal",
                    &format!("0x{}", hex::encode(&sender)),
                    &format!("0x{}", hex::encode(&recipient)),
                    &amount,
                    &budget.to_string(),
                ]);
            if !asset.is_empty() {
                cmd.arg(&asset);
            }
            let out = tokio::time::timeout(Duration::from_secs(90), cmd.output()).await??;
            ensure!(
                out.status.success(),
                "Mango quote helper exited with {}",
                out.status
            );
            Ok::<Quote, anyhow::Error>(serde_json::from_slice(&out.stdout)?)
        }
        .await;
        let (merge, bytes, digest, intent, inputs, gas_count) = match q {
            Ok(Quote::Ready {
                merge,
                amount: quoted_amount,
                bytes,
                digest,
                intent,
                inputs,
                gas_count,
            }) if quoted_amount == amount.parse::<u64>()? => {
                (merge, bytes, digest, intent, inputs, gas_count)
            }
            Ok(Quote::Ready { .. }) => anyhow::bail!("Mango withdrawal quote amount mismatch"),
            other => {
                let reason = match &other {
                    Ok(Quote::Waiting(s)) => s.as_str(),
                    _ => "Mango preflight unavailable; nothing was signed",
                };
                sqlx::query("UPDATE withdrawals SET detail=$2,next_prepare=now()+interval '30 seconds' WHERE id=$1::text::uuid")
                    .bind(&id).bind(reason).execute(&mut *tx).await?;
                tx.commit().await?;
                return other.map(|_| false);
            }
        };
        let gas_only = gas_count == inputs.len();
        let valid_inputs = match (merge, gas_only) {
            (true, true) => gas_count >= 2,
            (true, false) => !asset.is_empty() && inputs.len().saturating_sub(gas_count) >= 2,
            (false, _) => asset.is_empty() == gas_only,
        };
        ensure!(
            !bytes.is_empty()
                && gas_count > 0
                && gas_count <= inputs.len()
                && inputs.len() <= 128
                && valid_inputs,
            "invalid Mango transaction quote"
        );
        let ids: Vec<_> = inputs.iter().map(|i| i.id.as_slice()).collect();
        let versions: Vec<i64> = inputs
            .iter()
            .map(|i| i64::try_from(i.version))
            .collect::<std::result::Result<_, _>>()?;
        let stale:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM mango_payout_inputs i JOIN unnest($1::bytea[],$2::bigint[]) x(id,v) ON i.object_id=x.id AND i.version=x.v WHERE i.reserved)")
            .bind(ids).bind(versions).fetch_one(&mut *tx).await?;
        if stale {
            sqlx::query("UPDATE withdrawals SET detail='waiting for fresh Mango coin versions',next_prepare=now()+interval '30 seconds' WHERE id=$1::text::uuid")
            .bind(&id).execute(&mut *tx).await?;
            tx.commit().await?;
            return Ok(false);
        }
        let paused: bool =
            sqlx::query_scalar("SELECT paused FROM payment_control WHERE singleton FOR SHARE")
                .fetch_one(&mut *tx)
                .await?;
        if paused {
            return Ok(false);
        }
        let vault = self.vault.clone();
        let signer = sender.clone();
        let signature = tokio::task::spawn_blocking(move || {
            vault.sign_hot_mango(Uuid::parse_str(&key)?, &signer, &intent)
        })
        .await??;
        let parent = if merge { "merge_for" } else { "withdrawal_id" };
        let payout_id:String=sqlx::query_scalar(&format!("INSERT INTO mango_payouts({parent},sender,digest,tx_bytes,signature,budget) VALUES($1::text::uuid,$2,$3,$4,$5,$6::text::positive_raw) RETURNING payout_id::text"))
            .bind(&id).bind(sender).bind(digest.as_slice()).bind(bytes).bind(signature).bind(budget.to_string()).fetch_one(&mut *tx).await?;
        let first_gas = inputs.len() - gas_count;
        for (k, i) in inputs.into_iter().enumerate() {
            sqlx::query("INSERT INTO mango_payout_inputs(object_id,version,object_digest,payout_id,input_index,is_gas) VALUES($1,$2,$3,$4::text::uuid,$5,$6)")
                .bind(i.id.as_slice()).bind(i64::try_from(i.version)?).bind(i.digest.as_slice()).bind(&payout_id).bind(i16::try_from(k)?).bind(k>=first_gas).execute(&mut *tx).await?;
        }
        sqlx::query("UPDATE withdrawals SET status=$2,detail=$3 WHERE id=$1::text::uuid")
            .bind(id)
            .bind(if merge { "consolidating" } else { "signed" })
            .bind(merge.then_some("waiting for finalized hot-wallet coin consolidation"))
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }
    pub async fn prepare_configured_mango(&self) -> Result<bool> {
        let (helper, budget) = mango_settings()?;
        self.prepare_mango(Path::new(&helper), budget).await
    }
}
pub(crate) fn mango_settings() -> Result<(std::ffi::OsString, u64)> {
    let helper = env::var_os("MANGO_WORKER_BIN").unwrap_or_else(|| "mango-worker".into());
    let budget = env::var("MANGO_PAYOUT_GAS_BUDGET")
        .unwrap_or_else(|_| "10000000".into())
        .parse()
        .context("invalid MANGO_PAYOUT_GAS_BUDGET")?;
    Ok((helper, budget))
}
