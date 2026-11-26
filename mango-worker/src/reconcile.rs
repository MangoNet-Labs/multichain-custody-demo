use super::{log_progress, Node};
use anyhow::{ensure, Context, Result};
use custody_core::{pipeline::ordered_range_observed, Network::MangoTestnet};
use custody_store::{
    reconcile::{self, MangoInventory},
    Cursor, PgPool,
};
use std::{collections::HashSet, num::NonZeroUsize};

pub async fn run(
    db: &PgPool,
    node: &Node,
    parallel: NonZeroUsize,
    ready: NonZeroUsize,
) -> Result<()> {
    let s = reconcile::snapshot(db, MangoTestnet).await?;
    // Additions committed after the snapshot cannot be active at its height.
    // Audit holdings, not credit eligibility. An opening coin may last have
    // moved before listing. Restrict this independent replay to snapshot assets.
    let wanted: HashSet<_> = s.wallets.iter().map(|w| w.asset.as_slice()).collect();
    let registry = super::registry::stored(db).await?;
    let node = node.with_rules(mango_worker::rules::Rules::new(
        registry
            .entries()
            .filter(|(t, _)| wanted.contains(t.coin_type.as_bytes()))
            .map(|(t, _)| (t.clone(), 0)),
    )?);
    let owners = s
        .wallets
        .iter()
        .map(|w| w.address.as_slice().try_into())
        .collect::<Result<HashSet<[u8; 32]>, _>>()?;
    let owner_keys: Vec<_> = s.wallets.iter().map(|w| w.address.as_slice()).collect();
    let assets: Vec<_> = s.wallets.iter().map(|w| w.asset.as_slice()).collect();
    // Resume only an independently replayed, matched inventory of exactly this
    // wallet set. Never initialize it from the primary scanner's coin table.
    let previous:Option<(i64,i64,Vec<u8>)>=sqlx::query_as("SELECT r.id,r.height,r.digest::bytea FROM reconciliation_runs r WHERE r.network='mango-testnet' AND r.matched AND r.height<=$1
        AND (SELECT count(*) FROM reconciliation_wallets w WHERE w.run_id=r.id)=$2
        AND NOT EXISTS (SELECT address,asset FROM reconciliation_wallets WHERE run_id=r.id EXCEPT SELECT * FROM unnest($3::bytea[],$4::bytea[])) ORDER BY height DESC,id DESC LIMIT 1")
        .bind(s.cursor.height).bind(i64::try_from(s.wallets.len())?).bind(owner_keys).bind(assets).fetch_optional(db).await?;
    let mut coins = MangoInventory::new();
    let base = if let Some((id, height, d)) = previous {
        let rows:Vec<(Vec<u8>,Vec<u8>,Vec<u8>,String)>=sqlx::query_as("SELECT object_id::bytea,owner,asset,balance::text FROM reconciliation_mango_coins WHERE run_id=$1").bind(id).fetch_all(db).await?;
        for (obj, owner, asset, b) in rows {
            coins.insert(
                obj.as_slice().try_into()?,
                (owner.as_slice().try_into()?, asset, b.parse()?),
            );
        }
        Cursor {
            height,
            digest: d.as_slice().try_into()?,
        }
    } else {
        // Wallets on a fresh installation are newly random, not imported, and
        // cannot be issued until both scan anchors exist.
        let (first,parent):(i64,Vec<u8>)=sqlx::query_as("SELECT height,parent::bytea FROM scanned_blocks WHERE network='mango-testnet' ORDER BY height LIMIT 1").fetch_one(db).await?;
        Cursor {
            height: first - 1,
            digest: parent.as_slice().try_into()?,
        }
    };
    replay(
        &node,
        (parallel, ready),
        base,
        s.cursor,
        &owners,
        &mut coins,
    )
    .await?;
    let height = s.cursor.height;
    let (id, matched) = reconcile::finish_mango(db, s, &coins)
        .await
        .context("saving Mango reconciliation")?;
    println!("Mango reconciliation {id} at checkpoint {height}: matched={matched}");
    ensure!(
        matched,
        "Mango reconciliation mismatch; inspect Operations before resuming payments"
    );
    Ok(())
}

pub(super) async fn replay(
    node: &Node,
    limits: (NonZeroUsize, NonZeroUsize),
    base: Cursor,
    target: Cursor,
    owners: &HashSet<[u8; 32]>,
    coins: &mut MangoInventory,
) -> Result<()> {
    // Each fetched parent is checked back to the saved anchor; the sampled
    // target authenticates the other end, including a zero-length replay.
    ensure!(
        base.height <= target.height,
        "Mango audit anchor after target"
    );
    ensure!(
        node.header(Some(target.height.try_into()?)).await? == target,
        "Mango audit target mismatch"
    );
    let mut parent = base.digest;
    if base.height < target.height {
        ordered_range_observed(
            (base.height + 1) as u64..=target.height as u64,
            limits.0,
            limits.1,
            |h| node.fetch(h),
            |_, (b, _, changes)| {
                let result = (|| -> Result<()> {
                    ensure!(b.parent == parent, "Mango audit checkpoint gap");
                    for (id, out) in changes {
                        if let Some(c) = out.filter(|c| owners.contains(&c.owner)) {
                            coins.insert(id, (c.owner, c.asset, c.balance));
                        } else {
                            coins.remove(&id);
                        }
                    }
                    parent = b.digest;
                    Ok(())
                })();
                std::future::ready(result)
            },
            log_progress("Mango audit"),
        )
        .await?;
    }
    ensure!(
        parent == target.digest,
        "Mango audit did not reach snapshot"
    );
    Ok(())
}
