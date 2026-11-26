use custody_store::PgPool;
use sqlx::FromRow;

pub const PAGE: usize = 20;

#[derive(FromRow)]
pub struct Asset {
    pub net: String,
    pub key: String,
    pub symbol: String,
    pub decimals: i16,
    pub available: String,
    pub frozen: String,
}
pub async fn assets(db: &PgPool, user: &str) -> sqlx::Result<Vec<Asset>> {
    sqlx::query_as(
        "WITH balances AS (
      SELECT network,asset,sum(delta) FILTER(WHERE account=$1) AS avail,
          sum(delta) FILTER(WHERE account=$2) AS frozen
      FROM journal_entries WHERE account IN ($1,$2) GROUP BY network,asset)
      SELECT a.network::text AS net,CASE WHEN a.network='mango-testnet' THEN convert_from(a.asset,'UTF8') ELSE encode(a.asset,'hex') END AS key,a.symbol,a.decimals,
        coalesce(b.avail,0)::text AS available,coalesce(b.frozen,0)::text AS frozen
      FROM assets a LEFT JOIN balances b USING(network,asset)
      ORDER BY a.network DESC,a.asset",
    )
    .bind(format!("user:{user}:available"))
    .bind(format!("user:{user}:frozen"))
    .fetch_all(db)
    .await
}
#[derive(FromRow)]
pub struct DepositAddress {
    pub net: String,
    pub ready: bool,
    pub requested: bool,
    pub address: Option<Vec<u8>>,
}
pub async fn deposit_addresses(db: &PgPool, user: &str) -> sqlx::Result<Vec<DepositAddress>> {
    sqlx::query_as(
        "SELECT n.network::text AS net,c.network IS NOT NULL AS ready,a.id IS NOT NULL AS requested,w.address
         FROM (SELECT DISTINCT network FROM assets) n
         LEFT JOIN scan_cursors c USING(network)
         LEFT JOIN wallet_allocations a ON a.network=n.network AND a.role='deposit' AND a.owner_id=$1
         LEFT JOIN wallets w ON w.network=a.network AND w.owner_id=a.owner_id AND w.role='deposit'
         ORDER BY n.network DESC",
    )
    .bind(user)
    .fetch_all(db)
    .await
}

#[derive(FromRow)]
pub struct Entry {
    pub coin_type: Option<String>,
    pub net: String,
    pub symbol: String,
    pub decimals: i16,
    pub amount: String,
    pub address: Option<Vec<u8>>,
    pub tx: Option<Vec<u8>>,
    pub confirmed_tx: Option<Vec<u8>>,
    pub merge_tx: Option<Vec<u8>>,
    pub height: Option<i64>,
    pub id: String,
    pub status: String,
    pub detail: Option<String>,
    pub gas: Option<String>,
}
pub async fn history(
    db: &PgPool,
    user: &str,
    deposits: bool,
    page: u32,
) -> sqlx::Result<Vec<Entry>> {
    let query = if deposits {
        "SELECT d.network::text AS net,a.symbol,a.decimals,j.amount::text,
         CASE WHEN d.network='mango-testnet' AND d.asset<>'' THEN convert_from(d.asset,'UTF8') END AS coin_type,
         d.sender AS address,d.tx::bytea,d.height,d.journal_id::text AS id,
         'credited'::text AS status,NULL::text AS detail,NULL::text AS gas,NULL::bytea AS confirmed_tx,NULL::bytea AS merge_tx
         FROM deposits d JOIN journal j ON j.id=d.journal_id JOIN assets a ON (a.network,a.asset)=(d.network,d.asset)
         JOIN wallets w ON w.network=d.network AND w.address=d.recipient
         WHERE w.owner_id=$1 ORDER BY d.journal_id DESC LIMIT $2 OFFSET $3"
    } else {
        "SELECT w.network::text AS net,a.symbol,a.decimals,w.amount::text,CASE WHEN w.network='mango-testnet' AND w.asset<>'' THEN convert_from(w.asset,'UTF8') END AS coin_type,
         w.recipient AS address,coalesce(p.digest,m.digest)::bytea AS tx,coalesce(p.confirmed_height,m.confirmed_height) AS height,
         w.id::text,w.status,w.detail,CASE WHEN m.net_gas IS NOT NULL OR prep.fees IS NOT NULL
         THEN (coalesce(m.net_gas,0)+coalesce(prep.fees,0))::text ELSE g.amount::text END AS gas,
         coalesce(p.observed_tx,m.observed_tx)::bytea AS confirmed_tx,prep.digest::bytea AS merge_tx
         FROM withdrawals w JOIN assets a ON (a.network,a.asset)=(w.network,w.asset)
         LEFT JOIN evm_payouts p ON p.withdrawal_id=w.id LEFT JOIN journal g ON g.id=p.gas_journal
         LEFT JOIN mango_payouts m ON m.withdrawal_id=w.id
         LEFT JOIN LATERAL (SELECT digest,sum(net_gas) OVER () AS fees FROM mango_payouts
           WHERE merge_for=w.id ORDER BY confirmed_height DESC NULLS FIRST LIMIT 1) prep ON true
         WHERE w.user_id=$1 ORDER BY w.created_at DESC,w.id DESC LIMIT $2 OFFSET $3"
    };
    sqlx::query_as(query)
        .bind(user)
        .bind(PAGE as i64 + 1)
        .bind(i64::from(page - 1) * PAGE as i64)
        .fetch_all(db)
        .await
}
#[derive(FromRow)]
pub struct PlatformWallet {
    pub net: String,
    pub role: String,
    pub address: Vec<u8>,
}
pub struct Operations {
    pub openings: Vec<Opening>,
    pub paused: bool,
    pub cursors: Vec<Scan>,
    pub wallets: Vec<PlatformWallet>,
    pub orders: Vec<(String, String, i64)>,
    pub sweeps: Vec<Sweep>,
    pub checks: Vec<(String, Vec<u8>, String)>,
    pub audits: Vec<Audit>,
    pub observed: Vec<Observed>,
}
#[derive(FromRow)]
pub struct Opening {
    pub net: String,
    pub address: Vec<u8>,
    pub asset: Vec<u8>,
    pub symbol: String,
    pub decimals: i16,
    pub height: i64,
    pub amount: String,
}
#[derive(FromRow)]
pub struct Audit {
    pub opening: String,
    pub coin_type: Option<String>,
    pub net: String,
    pub height: i64,
    pub matched: bool,
    pub symbol: String,
    pub decimals: i16,
    pub available: String,
    pub frozen: String,
    pub capital: String,
    pub fees: String,
    pub book: String,
    pub observed: String,
}
#[derive(FromRow)]
pub struct Observed {
    pub coin_type: Option<String>,
    pub net: String,
    pub address: Vec<u8>,
    pub role: String,
    pub symbol: String,
    pub decimals: i16,
    pub expected: String,
    pub observed: String,
    pub matched: bool,
}
#[derive(FromRow)]
pub struct Sweep {
    pub net: String,
    pub kind: String,
    pub symbol: String,
    pub decimals: i16,
    pub amount: String,
    pub sender: Vec<u8>,
    pub recipient: Vec<u8>,
    pub status: String,
    pub detail: Option<String>,
    pub fee: Option<String>,
}
#[derive(FromRow)]
pub struct Scan {
    pub net: String,
    pub height: Option<i64>,
    pub digest: Option<Vec<u8>>,
    pub head: Option<i64>,
    pub lag: Option<i64>,
    pub measured: Option<String>,
    pub stale: bool,
    pub advancing: bool,
    pub issue: Option<String>,
    pub next_height: Option<i64>,
    pub issue_at: Option<String>,
}
pub async fn operations(db: &PgPool) -> sqlx::Result<Operations> {
    Ok(Operations{
        openings:sqlx::query_as("SELECT o.network::text net,o.address,a.asset,a.symbol,a.decimals,a.enabled_from-1 AS height,o.amount::text
          FROM opening_balances o JOIN assets a USING(network,asset) WHERE a.opening_digest IS NOT NULL AND o.amount<>0
          ORDER BY a.enabled_from DESC,o.network,a.asset,o.address LIMIT 64").fetch_all(db).await?,
        paused:sqlx::query_scalar("SELECT paused FROM payment_control WHERE singleton").fetch_one(db).await?,
        cursors:sqlx::query_as("SELECT n.network::text AS net,c.height,c.digest::bytea,s.head,
            CASE WHEN s.head IS NOT NULL THEN greatest(s.head-c.height,0) END AS lag,
            to_char(s.sampled_at AT TIME ZONE 'UTC','YYYY-MM-DD HH24:MI:SS') AS measured,
            coalesce(now()-s.sampled_at>interval '1 minute',true) AS stale,
            coalesce(now()-c.last_advanced_at<=interval '1 minute',false) AS advancing,
            i.stage AS issue,i.next_height,to_char(i.observed_at AT TIME ZONE 'UTC','YYYY-MM-DD HH24:MI:SS') AS issue_at
            FROM (SELECT DISTINCT network FROM assets) n LEFT JOIN scan_cursors c USING(network)
            LEFT JOIN scan_head_samples s ON s.network=n.network
            LEFT JOIN scan_issues i ON i.network=n.network AND
            ((i.stage='finality' AND (s.sampled_at IS NULL OR i.observed_at>s.sampled_at))
            OR (i.stage='range' AND c.height<i.next_height)) ORDER BY n.network DESC").fetch_all(db).await?,
        wallets:sqlx::query_as("SELECT network::text AS net,role,address FROM wallets WHERE role IN ('hot','gas') ORDER BY network DESC,role DESC").fetch_all(db).await?,
        orders:sqlx::query_as("SELECT network::text,status,count(*) FROM withdrawals GROUP BY network,status ORDER BY network,status").fetch_all(db).await?,
        sweeps:sqlx::query_as("SELECT s.network::text AS net,s.kind,a.symbol,a.decimals,s.amount::text,s.sender,s.recipient,s.status,s.detail,coalesce(j.amount::text,m.net_gas::text) AS fee
            FROM sweep_orders s JOIN assets a USING(network,asset) LEFT JOIN evm_payouts p ON p.sweep_id=s.id LEFT JOIN journal j ON j.id=p.gas_journal
            LEFT JOIN mango_payouts m ON m.sweep_id=s.id ORDER BY s.created_at DESC,s.id DESC LIMIT 20").fetch_all(db).await?,
        checks:sqlx::query_as("SELECT network::text,address,detail FROM sweep_checks ORDER BY next_check NULLS LAST,address LIMIT 20").fetch_all(db).await?,
        audits:sqlx::query_as("WITH latest AS(SELECT DISTINCT ON(network) * FROM reconciliation_runs ORDER BY network,height DESC,id DESC)
          SELECT r.network::text net,r.height,(x.book_assets=x.observed AND x.book_assets=x.available+x.frozen+x.capital-x.net_fees+x.opening AND NOT EXISTS(SELECT 1 FROM reconciliation_wallets v WHERE v.run_id=r.id AND v.asset=x.asset AND v.expected<>v.observed)) AS matched,x.opening::text,a.symbol,a.decimals,x.available::text,x.frozen::text,x.capital::text,x.net_fees::text fees,x.book_assets::text book,x.observed::text,
          CASE WHEN a.network='mango-testnet' AND a.asset<>'' THEN convert_from(a.asset,'UTF8') END coin_type
          FROM latest r JOIN reconciliation_assets x ON x.run_id=r.id JOIN assets a ON a.network=r.network AND a.asset=x.asset ORDER BY r.network,a.asset").fetch_all(db).await?,
        observed:sqlx::query_as("WITH latest AS(SELECT DISTINCT ON(network) * FROM reconciliation_runs ORDER BY network,height DESC,id DESC)
          SELECT r.network::text net,x.address,w.role,a.symbol,a.decimals,x.expected::text,x.observed::text,x.expected=x.observed AS matched,
          CASE WHEN a.network='mango-testnet' AND a.asset<>'' THEN convert_from(a.asset,'UTF8') END coin_type
          FROM latest r JOIN reconciliation_wallets x ON x.run_id=r.id JOIN wallets w ON w.network=r.network AND w.address=x.address JOIN assets a ON a.network=r.network AND a.asset=x.asset
          WHERE w.role IN ('hot','gas') OR x.observed<>x.expected ORDER BY matched,r.network,w.role,a.asset LIMIT 64").fetch_all(db).await?,
    })
}
