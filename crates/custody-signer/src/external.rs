use crate::{
    vault::{create_unlock_file, Vault},
    Wallet,
};
use anyhow::{ensure, Context, Result};
use custody_core::Network;
use std::{fs, io::Write, os::unix::fs::DirBuilderExt, path::Path};
use uuid::Uuid;

/// Local acceptance wallets, never inserted into a custody database or mounted
/// in Compose. An incomplete or damaged installation must not rotate addresses.
pub fn external_wallets(dir: &Path) -> Result<Vec<Wallet>> {
    let fresh = match fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(e) => return Err(e.into()),
    };
    use std::os::unix::fs::PermissionsExt;
    let meta = fs::symlink_metadata(dir)?;
    ensure!(
        meta.is_dir() && meta.permissions().mode() & 0o077 == 0,
        "expected private acceptance directory"
    );
    let unlock = dir.join("credentials/unlock.key");
    let marker = dir.join("public.json");
    if fresh {
        create_unlock_file(&unlock)?;
    }
    let (install, entries): (Uuid, Vec<(Uuid, Vec<u8>)>) = if fresh {
        (
            Uuid::new_v4(),
            vec![(Uuid::new_v4(), vec![]), (Uuid::new_v4(), vec![])],
        )
    } else {
        let (install, rows): (String, Vec<(String, Vec<u8>)>) = serde_json::from_slice(
            &fs::read(&marker).context("incomplete external wallet set; not replacing keys")?,
        )?;
        (
            Uuid::parse_str(&install)?,
            rows.into_iter()
                .map(|(id, addr)| Ok((Uuid::parse_str(&id)?, addr)))
                .collect::<Result<Vec<_>>>()?,
        )
    };
    ensure!(
        entries.len() == 2,
        "expected exactly two external test wallets"
    );
    let vault = Vault::open(&dir.join("keystore"), &unlock, install, !fresh)?;
    let mut rows = Vec::new();
    let mut wallets = Vec::new();
    for (net, (id, expected)) in [Network::MangoTestnet, Network::BscTestnet]
        .into_iter()
        .zip(entries)
    {
        let address = vault.address(
            id,
            net,
            "external-acceptance",
            (!fresh).then_some(expected.as_slice()),
        )?;
        rows.push((id.to_string(), address.clone()));
        wallets.push(Wallet {
            network: net,
            slot: "external-acceptance".into(),
            address,
        });
    }
    if fresh {
        let mut f = tempfile::NamedTempFile::new_in(dir)?;
        f.write_all(&serde_json::to_vec(&(install.to_string(), rows))?)?;
        f.as_file().sync_all()?;
        f.persist_noclobber(marker)?;
        fs::File::open(dir)?.sync_all()?;
    }
    Ok(wallets)
}
