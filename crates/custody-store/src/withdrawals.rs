use super::{network_key, PgPool};
use anyhow::{ensure, Context, Result};
use custody_core::{AssetId, Network};

const MAX_MANGO: &str = "18446744073709551615";
const MAX_EVM: &str =
    "115792089237316195423570985008687907853269984665640564039457584007913129639935";

/// Decimal display input to exact, nonzero base units. Reject excess precision
/// instead of delegating rounding to PostgreSQL or a floating-point parser.
pub fn raw_amount(input: &str, decimals: u8, net: Network) -> Result<String> {
    ensure!(input.len() <= 512, "amount input too long");
    let (whole, frac) = match input.split_once('.') {
        Some((w, f)) => {
            ensure!(!f.is_empty(), "missing fractional digits");
            (w, f)
        }
        None => (input, ""),
    };
    ensure!(
        !whole.is_empty()
            && whole
                .bytes()
                .chain(frac.bytes())
                .all(|c| c.is_ascii_digit()),
        "invalid decimal amount"
    );
    ensure!(
        frac.len() <= usize::from(decimals),
        "amount exceeds asset precision"
    );
    let mut all = String::from(whole);
    all.push_str(frac);
    all.extend(std::iter::repeat_n('0', usize::from(decimals) - frac.len()));
    let raw = all.trim_start_matches('0');
    ensure!(!raw.is_empty(), "amount must be positive");
    let limit = if net == Network::MangoTestnet {
        MAX_MANGO
    } else {
        MAX_EVM
    };
    ensure!(
        raw.len() < limit.len() || raw.len() == limit.len() && raw <= limit,
        "amount exceeds on-chain range"
    );
    Ok(raw.into())
}

pub struct Request<'a> {
    pub user: &'a str,
    pub request_id: &'a str,
    pub asset: AssetId,
    pub recipient: &'a [u8],
    pub amount: &'a str,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Order {
    pub id: String,
    pub status: String,
}

pub async fn submit(db: &PgPool, req: Request<'_>) -> Result<Order> {
    let net = req.asset.network();
    let network = network_key(net);
    let asset = match req.asset {
        AssetId::BscTestnetErc20(a) => a.to_vec(),
        AssetId::MangoTestnetCoin(t) => {
            ensure!(!t.is_empty(), "missing Mango token type");
            t.into_bytes()
        }
        AssetId::BscTestnetBnb | AssetId::MangoTestnetMgo => Vec::new(),
    };
    ensure!(
        !req.request_id.is_empty()
            && req.request_id.len() <= 128
            && req
                .request_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b)),
        "invalid client request ID"
    );
    let width = if net == Network::MangoTestnet { 32 } else { 20 };
    ensure!(
        req.recipient.len() == width && req.recipient.iter().any(|b| *b != 0),
        "invalid recipient for network"
    );
    let mut tx = db.begin().await?;
    // The user row serializes all debits of this demo user. Reads of the
    // journal and existing request must happen *after* obtaining this lock.
    sqlx::query("SELECT id FROM demo_users WHERE id=$1 FOR UPDATE")
        .bind(req.user)
        .fetch_one(&mut *tx)
        .await
        .context("unknown demo user")?;
    let decimals: i16 =
        sqlx::query_scalar("SELECT decimals FROM assets WHERE network=$1 AND asset=$2")
            .bind(network)
            .bind(&asset)
            .fetch_one(&mut *tx)
            .await
            .context("unsupported asset")?;
    let raw = raw_amount(req.amount, decimals.try_into()?, net)?;
    let existing:Option<(String,String,bool)>=sqlx::query_as(
        "SELECT id::text,status,(network=$3 AND asset=$4 AND recipient=$5 AND amount=$6::text::numeric) FROM withdrawals WHERE user_id=$1 AND request_id=$2")
        .bind(req.user).bind(req.request_id).bind(network).bind(&asset).bind(req.recipient).bind(&raw).fetch_optional(&mut *tx).await?;
    if let Some((id, status, same)) = existing {
        ensure!(
            same,
            "client request ID already used for a different withdrawal"
        );
        tx.commit().await?;
        return Ok(Order { id, status });
    }
    let internal: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM wallets WHERE network=$1 AND address=$2)")
            .bind(network)
            .bind(req.recipient)
            .fetch_one(&mut *tx)
            .await?;
    ensure!(!internal, "withdrawals must target an external address");
    let available = format!("user:{}:available", req.user);
    let enough:bool=sqlx::query_scalar("SELECT COALESCE(sum(delta),0)>=$4::text::numeric FROM journal_entries WHERE network=$1 AND asset=$2 AND account=$3")
        .bind(network).bind(&asset).bind(&available).bind(&raw).fetch_one(&mut *tx).await?;
    ensure!(enough, "insufficient available balance");
    let freeze:i64=sqlx::query_scalar("INSERT INTO journal(network,asset,debit_account,credit_account,amount) VALUES($1,$2,$3,$4,$5::text::positive_raw) RETURNING id")
        .bind(network).bind(&asset).bind(&available).bind(format!("user:{}:frozen",req.user)).bind(&raw).fetch_one(&mut *tx).await?;
    let (id,status)=sqlx::query_as("INSERT INTO withdrawals(user_id,request_id,network,asset,recipient,amount,freeze_id) VALUES($1,$2,$3,$4,$5,$6::text::positive_raw,$7) RETURNING id::text,status")
        .bind(req.user).bind(req.request_id).bind(network).bind(&asset).bind(req.recipient).bind(raw).bind(freeze).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(Order { id, status })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_amounts_only() {
        let net = Network::MangoTestnet;
        assert_eq!(raw_amount("0.000000001", 9, net).unwrap(), "1");
        assert_eq!(raw_amount("00010.20", 9, net).unwrap(), "10200000000");
        assert_eq!(
            raw_amount("18446744073.709551615", 9, net).unwrap(),
            MAX_MANGO
        );
        for s in [
            "18446744073.709551616",
            "1e-3",
            "-1",
            "0",
            "0.0000000001",
            "1.",
            ".1",
            " 10",
            "1.1.2",
        ] {
            assert!(raw_amount(s, 9, net).is_err(), "{s}");
        }
        assert_eq!(
            raw_amount(MAX_EVM, 0, Network::BscTestnet).unwrap(),
            MAX_EVM
        );
        assert!(raw_amount(&format!("{MAX_EVM}0"), 0, Network::BscTestnet).is_err());
    }
}
