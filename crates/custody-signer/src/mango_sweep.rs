use crate::{mango::mango_settings, Signer};
use anyhow::{ensure, Context, Result};
use custody_store::{
    mango::{Input, Quote, SponsoredSweep},
    Cursor, PgPool,
};
use sqlx::{Postgres, Transaction};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

#[derive(sqlx::FromRow)]
struct Wallet {
    address: Vec<u8>,
    key_ref: String,
    slot: String,
}
struct Plan {
    req: SponsoredSweep,
    deposit: Wallet,
    gas: Wallet,
    checkpoint: Cursor,
    minimum: u64,
    total: String,
}
enum Check {
    Dormant,
    Wait(&'static str),
    Eligible(Box<Plan>),
}

async fn wallet(
    tx: &mut Transaction<'_, Postgres>,
    role: &str,
    addr: Option<&[u8]>,
) -> Result<Wallet> {
    Ok(sqlx::query_as("SELECT address,key_ref,CASE WHEN owner_id IS NULL THEN role ELSE 'deposit:'||owner_id END AS slot
        FROM wallets WHERE network='mango-testnet' AND role=$1 AND ($2::bytea IS NULL OR address=$2)")
        .bind(role).bind(addr).fetch_one(&mut **tx).await?)
}
async fn sponsor_inputs(
    tx: &mut Transaction<'_, Postgres>,
    owner: &[u8],
    budget: u64,
) -> Result<Option<Vec<Input>>> {
    let rows:Vec<(Vec<u8>,i64,Vec<u8>,String)>=sqlx::query_as("SELECT c.object_id::bytea,c.version,c.digest::bytea,c.balance::text FROM mango_wallet_coins c
        WHERE c.owner=$1 AND c.asset='' AND c.balance>0 AND NOT EXISTS(SELECT 1 FROM mango_payout_inputs i WHERE i.object_id=c.object_id AND i.version=c.version AND i.reserved)
        ORDER BY c.balance DESC,c.object_id LIMIT 127").bind(owner).fetch_all(&mut **tx).await?;
    let mut gas = Vec::new();
    let mut total = 0u64;
    for (id, v, d, b) in rows {
        let Some(sum) = total.checked_add(b.parse()?) else {
            continue;
        };
        total = sum;
        gas.push(Input {
            id: id.as_slice().try_into()?,
            version: v.try_into()?,
            digest: d.as_slice().try_into()?,
        });
        if total >= budget {
            return Ok(Some(gas));
        }
    }
    Ok(None)
}
async fn plan(db: &PgPool, addr: &[u8], budget: u64) -> Result<Check> {
    let mut tx = db.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    let eligible: Option<(Vec<u8>, String, String)> = sqlx::query_as(
        "SELECT r.asset,r.minimum::text,b.total::text FROM sweep_rules r
         LEFT JOIN opening_balances o ON (o.network,o.asset,o.address)=(r.network,r.asset,$1)
         CROSS JOIN LATERAL (SELECT coalesce(o.amount,0)+coalesce(sum(m.delta),0) AS total
             FROM custody_wallet_movements m WHERE m.network=r.network AND m.address=$1 AND m.asset=r.asset) b
         WHERE r.network='mango-testnet' AND b.total>=r.minimum ORDER BY r.asset LIMIT 1",
    )
    .bind(addr)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((asset, minimum, total)) = eligible else {
        return Ok(Check::Dormant);
    };
    let minimum = minimum.parse()?;
    let deposit = wallet(&mut tx, "deposit", Some(addr)).await?;
    let hot = wallet(&mut tx, "hot", None).await?;
    let gas = wallet(&mut tx, "gas", None).await?;
    let Some(gas_coins) = sponsor_inputs(&mut tx, &gas.address, budget).await? else {
        return Ok(Check::Wait(
            "waiting for confirmed, unreserved platform gas coins",
        ));
    };
    let (height, d): (i64, Vec<u8>) = sqlx::query_as(
        "SELECT height,digest::bytea FROM scan_cursors WHERE network='mango-testnet'",
    )
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Check::Eligible(Box::new(Plan {
        req: SponsoredSweep {
            asset: String::from_utf8(asset)?,
            sender: addr.try_into()?,
            recipient: hot.address.as_slice().try_into()?,
            sponsor: gas.address.as_slice().try_into()?,
            max_amount: total.parse::<u128>()?.min(u128::from(u64::MAX)) as u64,
            budget,
            gas: gas_coins,
        },
        deposit,
        gas,
        minimum,
        total,
        checkpoint: Cursor {
            height,
            digest: d.as_slice().try_into()?,
        },
    })))
}
async fn quote(helper: &Path, req: &SponsoredSweep) -> Result<Quote> {
    let mut child = tokio::process::Command::new(helper)
        .arg("--quote-sweep")
        .kill_on_drop(true)
        .env_remove("DATABASE_URL")
        .env_remove("CUSTODY_KEYSTORE")
        .env_remove("CUSTODY_UNLOCK_FILE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    tokio::time::timeout(Duration::from_secs(90), async {
        let mut stdin = child.stdin.take().context("missing Mango helper input")?;
        stdin.write_all(&serde_json::to_vec(req)?).await?;
        drop(stdin);
        let out = child.wait_with_output().await?;
        ensure!(
            out.status.success(),
            "Mango sweep helper failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(serde_json::from_slice(&out.stdout)?)
    })
    .await?
}
impl Signer {
    pub async fn prepare_configured_mango_sweep(&self) -> Result<bool> {
        let (helper, budget) = mango_settings()?;
        self.prepare_mango_sweep(Path::new(&helper), budget).await
    }
    pub async fn prepare_mango_sweep(&self, helper: &Path, budget: u64) -> Result<bool> {
        ensure!(budget > 0, "invalid Mango sponsor budget");
        let lease = Uuid::new_v4().to_string();
        let due:Option<(Vec<u8>,i64)>=sqlx::query_as("WITH due AS (SELECT address FROM sweep_checks c WHERE network='mango-testnet'
            AND next_check<=now() AND (lease_until IS NULL OR lease_until<now()) AND NOT (SELECT paused FROM payment_control WHERE singleton)
            AND NOT EXISTS(SELECT 1 FROM sweep_orders s WHERE s.network=c.network AND s.deposit_address=c.address AND s.status IN ('signed','submitted','review'))
            ORDER BY next_check,c.address LIMIT 1 FOR UPDATE OF c SKIP LOCKED)
            UPDATE sweep_checks c SET lease=$1::text::uuid,lease_until=now()+interval '2 minutes' FROM due WHERE c.network='mango-testnet' AND c.address=due.address RETURNING c.address,c.revision")
            .bind(&lease).fetch_optional(&self.db).await?;
        let Some((addr, revision)) = due else {
            return Ok(false);
        };
        let result=async{
            let (mut delay,mut reason)=(true,"waiting for confirmed coin versions".to_string());
            let prepared=match plan(&self.db,&addr,budget).await?{
                Check::Dormant=>{delay=false;reason="below configured threshold or asset disabled".into();None}
                Check::Wait(s)=>{reason=s.into();None}
                Check::Eligible(p)=>match quote(helper,&p.req).await?{
                    Quote::Waiting(s)=>{reason=s;None}
                    q=>Some((p,q))
                }
            };
            let mut tx=self.db.begin().await?;
            // Same lock ordering as checkpoint application. No RPC while held.
            if prepared.is_some(){sqlx::query("SELECT height FROM scan_cursors WHERE network='mango-testnet' FOR UPDATE").fetch_one(&mut *tx).await?;}
            let current:Option<i64>=sqlx::query_scalar("SELECT revision FROM sweep_checks WHERE network='mango-testnet' AND address=$1 AND lease=$2::text::uuid FOR UPDATE")
                .bind(&addr).bind(&lease).fetch_optional(&mut *tx).await?;
            let Some(current)=current else{return Ok(false)};
            let paused:bool=sqlx::query_scalar("SELECT paused FROM payment_control WHERE singleton FOR SHARE").fetch_one(&mut *tx).await?;
            let mut signed=false;
            if current!=revision || paused{delay=true;reason="state changed; checking again".into();}
            else if let Some((p,Quote::Ready{merge:false,amount,bytes,digest,intent,inputs,gas_count}))=prepared{
                let sent_count=inputs.len().checked_sub(gas_count).context("invalid gas count")?;
                ensure!(!bytes.is_empty() && amount<=p.req.max_amount && (1..=50).contains(&sent_count)
                    && inputs.len()<=128 && inputs[sent_count..]==p.req.gas && gas_count==p.req.gas.len(),"invalid Mango sweep quote");
                let ids:Vec<_>=inputs.iter().map(|i|i.id.as_slice()).collect();
                let versions:Vec<_>=inputs.iter().map(|i|i64::try_from(i.version)).collect::<std::result::Result<_,_>>()?;
                let digests:Vec<_>=inputs.iter().map(|i|i.digest.as_slice()).collect();
                // Deposit refs are latest RPC candidates, not scanner inventory. Sponsor refs must still be confirmed.
                let valid:bool=sqlx::query_scalar("SELECT count(DISTINCT x.id)=count(*) AND bool_and(i.object_id IS NULL AND
                    (x.n<=$5 OR (c.object_id IS NOT NULL AND c.owner=$4 AND c.asset='' AND c.version=x.v AND c.digest=x.d)))
                    IS TRUE FROM unnest($1::bytea[],$2::bigint[],$3::bytea[]) WITH ORDINALITY x(id,v,d,n)
                    LEFT JOIN mango_payout_inputs i ON i.object_id=x.id AND i.version=x.v AND i.reserved
                    LEFT JOIN mango_wallet_coins c ON c.object_id=x.id")
                    .bind(ids).bind(versions).bind(digests).bind(p.req.sponsor.as_slice()).bind(i64::try_from(sent_count)?).fetch_one(&mut *tx).await?;
                if valid {
                    let id:String=sqlx::query_scalar("INSERT INTO sweep_orders(network,deposit_address,sender,recipient,kind,trigger_asset,asset,amount,minimum,observed_balance,observed_height,observed_hash)
                        VALUES('mango-testnet',$1,$1,$2,'sweep',$8,$8,$3::text::nonnegative_raw,$4::text::positive_raw,$5::text::positive_raw,$6,$7) RETURNING id::text")
                        .bind(&addr).bind(p.req.recipient.as_slice()).bind(amount.to_string()).bind(p.minimum.to_string()).bind(&p.total).bind(p.checkpoint.height).bind(p.checkpoint.digest.as_slice()).bind(p.req.asset.as_bytes()).fetch_one(&mut *tx).await?;
                    let vault=self.vault.clone();
                    let (dep,gas)=(p.deposit,p.gas);
                    let (sig,sponsor_sig)=tokio::task::spawn_blocking(move ||->Result<_>{
                        Ok((vault.sign_mango(Uuid::parse_str(&dep.key_ref)?,&dep.slot,&dep.address,&intent)?,
                            vault.sign_mango(Uuid::parse_str(&gas.key_ref)?,"gas",&gas.address,&intent)?))
                    }).await??;
                    let payout_id:String=sqlx::query_scalar("INSERT INTO mango_payouts(sweep_id,sender,sponsor,digest,tx_bytes,signature,sponsor_signature,budget) VALUES($1::text::uuid,$2,$3,$4,$5,$6,$7,$8::text::positive_raw) RETURNING payout_id::text")
                        .bind(&id).bind(&addr).bind(p.req.sponsor.as_slice()).bind(digest.as_slice()).bind(bytes).bind(sig).bind(sponsor_sig).bind(budget.to_string()).fetch_one(&mut *tx).await?;
                    for (k,i) in inputs.iter().enumerate(){
                        sqlx::query("INSERT INTO mango_payout_inputs(payout_id,object_id,version,object_digest,is_gas,input_index) VALUES($1::text::uuid,$2,$3,$4,$5,$6)")
                            .bind(&payout_id).bind(i.id.as_slice()).bind(i64::try_from(i.version)?).bind(i.digest.as_slice()).bind(k>=sent_count).bind(i16::try_from(k)?).execute(&mut *tx).await?;
                    }
                    sqlx::query("INSERT INTO mango_internal_outputs(tx,sender,recipient,amount,order_ref,asset) VALUES($1,$2,$3,$4::text::nonnegative_raw,$5,$6)")
                        .bind(digest.as_slice()).bind(&addr).bind(p.req.recipient.as_slice()).bind(amount.to_string()).bind(&id).bind(p.req.asset.as_bytes()).execute(&mut *tx).await?;
                    signed=true;delay=false;reason="waiting for final Mango checkpoint".into();
                }
            }
            sqlx::query("UPDATE sweep_checks SET lease=NULL,lease_until=NULL,detail=$2,next_check=CASE WHEN $3 THEN now()+interval '30 seconds' ELSE NULL END WHERE network='mango-testnet' AND address=$1")
                .bind(&addr).bind(reason).bind(delay).execute(&mut *tx).await?;
            tx.commit().await?;
            Ok::<bool,anyhow::Error>(signed)
        }.await;
        if result.is_err() {
            sqlx::query("UPDATE sweep_checks SET lease=NULL,lease_until=NULL,detail='sweep preparation failed; retrying',next_check=now()+interval '30 seconds' WHERE network='mango-testnet' AND address=$1 AND lease=$2::text::uuid")
                .bind(&addr).bind(lease).execute(&self.db).await?;
        }
        result
    }
}
