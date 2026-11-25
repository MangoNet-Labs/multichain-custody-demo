use anyhow::{bail, Context, Result};
use custody_core::Network::{self, BscTestnet, MangoTestnet};
use custody_signer::{create_unlock_file, parse_network, Signer, Wallet};
use custody_store::{network_key, wallets::Purpose, PgPool, MIGRATIONS};
use std::{env, path::PathBuf, time::Duration};

fn show(w: Wallet) {
    println!("{} {} {}", network_key(w.network), w.slot, w.formatted());
}

async fn wallet_loop(actor: &Signer) -> Result<()> {
    loop {
        if actor.pending().await? < 32 {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

async fn load_rules(actor: &Signer, net: Network) -> Result<()> {
    let path =
        env::var_os("CUSTODY_SWEEP_CONFIG").unwrap_or_else(|| "config/sweep.example.toml".into());
    actor
        .configure_sweeps(
            net,
            &std::fs::read_to_string(path).context("reading signer sweep configuration")?,
        )
        .await
}

async fn wait_for_rules(actor: &Signer, db: &PgPool, net: Network) {
    loop {
        match custody_store::wallets::network_ready(db, net).await {
            Ok(true) => match load_rules(actor, net).await {
                Ok(()) => return,
                Err(e) => eprintln!("{} sweep configuration deferred: {e:#}", network_key(net)),
            },
            Ok(false) => (),
            Err(e) => eprintln!("{} cursor unavailable: {e:#}", network_key(net)),
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let unlock = PathBuf::from(
        env::var_os("CUSTODY_UNLOCK_FILE").unwrap_or_else(|| "secrets/unlock.key".into()),
    );
    let root = PathBuf::from(env::var_os("CUSTODY_KEYSTORE").unwrap_or_else(|| "keystore".into()));
    let args: Vec<_> = env::args().skip(1).collect();
    if let [cmd, path] = args.as_slice() {
        if cmd == "external-test-wallets" {
            for w in custody_signer::external_wallets(std::path::Path::new(path))? {
                show(w);
            }
            return Ok(());
        }
    }
    if args == ["init-unlock"] {
        create_unlock_file(&unlock)?;
        println!(
            "Created unlock file at {}. Back it up separately from the keystore.",
            unlock.display()
        );
        return Ok(());
    }
    if !(args == ["platform"]
        || args == ["serve"]
        || args == ["bsc-prepare-once"]
        || args == ["bsc-sweep-once"]
        || args == ["mango-prepare-once"]
        || args == ["mango-sweep-once"]
        || args.len() == 3 && args[0] == "deposit")
    {
        bail!("usage: custody-signer init-unlock | platform | deposit <user> <network> | bsc-prepare-once | mango-prepare-once | bsc-sweep-once | mango-sweep-once | serve");
    }
    let db =
        PgPool::connect(&env::var("DATABASE_URL").context("DATABASE_URL is required")?).await?;
    MIGRATIONS.run(&db).await?;
    let actor = Signer::open(db.clone(), &root, &unlock).await?;
    let max_fee = env::var("BSC_MAX_PAYOUT_FEE_WEI")
        .unwrap_or_else(|_| "1000000000000000".into())
        .parse()?;
    let node = evm_reader::rpc::BscRpc::from_env()?;
    if args == ["bsc-sweep-once"] {
        load_rules(&actor, BscTestnet).await?;
        println!(
            "prepared={}",
            actor.prepare_bsc_sweep(&node, max_fee).await?
        );
        return Ok(());
    }
    if args == ["mango-prepare-once"] {
        println!("prepared={}", actor.prepare_configured_mango().await?);
        return Ok(());
    }
    if args == ["mango-sweep-once"] {
        load_rules(&actor, MangoTestnet).await?;
        println!("prepared={}", actor.prepare_configured_mango_sweep().await?);
        return Ok(());
    }
    if args == ["bsc-prepare-once"] {
        println!("prepared={}", actor.prepare_bsc(&node, max_fee).await?);
        return Ok(());
    }
    if args[0] == "deposit" {
        show(
            actor
                .wallet(parse_network(&args[2])?, Purpose::Deposit(&args[1]))
                .await?,
        );
    } else if args == ["platform"] {
        let platform = actor.platform().await?;
        if platform.is_empty() {
            bail!("no initialized network yet; wait for its scanner before requesting wallets");
        }
        for w in platform {
            show(w);
        }
    }
    if args == ["serve"] {
        let payouts = async {
            loop {
                if let Err(e) = actor.prepare_bsc(&node, max_fee).await {
                    eprintln!("BSC signing deferred: {e:#}");
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        };
        let mango = async {
            loop {
                if let Err(e) = actor.prepare_configured_mango().await {
                    eprintln!("Mango signing deferred: {e:#}");
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        };
        let sweeps = async {
            wait_for_rules(&actor, &db, BscTestnet).await;
            loop {
                if let Err(e) = actor.prepare_bsc_sweep(&node, max_fee).await {
                    eprintln!("BSC sweep deferred: {e:#}");
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        };
        let sponsored = async {
            wait_for_rules(&actor, &db, MangoTestnet).await;
            loop {
                if let Err(e) = actor.prepare_configured_mango_sweep().await {
                    eprintln!("Mango sweep deferred: {e:#}");
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        };
        tokio::select! {r=wallet_loop(&actor)=>r?,_=payouts=>{},_=mango=>{},_=sweeps=>{},_=sponsored=>{}}
    }
    Ok(())
}
