use alloy_primitives::keccak256;
use anyhow::{bail, ensure, Context, Result};
use chacha20poly1305::{
    aead::{AeadInPlace, KeyInit},
    XChaCha20Poly1305, XNonce,
};
use custody_core::Network;
use k256::elliptic_curve::sec1::ToEncodedPoint;
use rand_core::{OsRng, RngCore};
use std::{
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};
use tempfile::NamedTempFile;
use uuid::Uuid;
use zeroize::Zeroizing;

const HEADER: &[u8; 4] = b"MCK1";
const MARKER: &[u8] = b"testnet-wallet-store";

fn private_dir(path: &Path) -> Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => File::open(
            path.parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new(".")),
        )?
        .sync_all()?,
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_dir() && meta.permissions().mode() & 0o077 == 0,
        "expected a private directory (mode 0700): {}",
        path.display()
    );
    Ok(())
}

fn private_file(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    ensure!(
        meta.is_file() && meta.permissions().mode() & 0o077 == 0,
        "expected a private regular file (mode 0600): {}",
        path.display()
    );
    Ok(file)
}

// Publish complete, durable files once. A losing creator always reopens the
// winning file; it never uses its own unpersisted key or replaces the winner.
fn publish(path: &Path, bytes: &[u8]) -> Result<bool> {
    let dir = path.parent().context("missing storage directory")?;
    let mut tmp = NamedTempFile::new_in(dir)?;
    tmp.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    match tmp.persist_noclobber(path) {
        Ok(_) => {
            File::open(dir)?.sync_all()?;
            Ok(true)
        }
        Err(e) if e.error.kind() == ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e.error.into()),
    }
}

fn random32() -> Result<Zeroizing<[u8; 32]>> {
    let mut buf = Zeroizing::new([0; 32]);
    OsRng
        .try_fill_bytes(&mut *buf)
        .map_err(|_| anyhow::anyhow!("operating-system RNG unavailable"))?;
    Ok(buf)
}

pub fn create_unlock_file(path: &Path) -> Result<()> {
    let dir = path
        .parent()
        .context("use a separate private credentials directory")?;
    ensure!(
        !dir.as_os_str().is_empty(),
        "specify a private credentials directory"
    );
    private_dir(dir)?;
    ensure!(
        publish(path, &*random32()?)?,
        "unlock file already exists; not replacing it"
    );
    Ok(())
}

pub struct Vault {
    root: PathBuf,
    install: Uuid,
    master: Zeroizing<[u8; 32]>,
}

impl Vault {
    pub fn open(root: &Path, unlock: &Path, install: Uuid, registered: bool) -> Result<Self> {
        let mut file = private_file(unlock).context("cannot read keystore unlock file")?;
        ensure!(
            file.metadata()?.len() == 32,
            "unlock file must contain 32 random bytes"
        );
        let mut master = Zeroizing::new([0; 32]);
        file.read_exact(&mut *master)?;
        private_dir(root)?;
        let root = root.canonicalize()?;
        ensure!(
            !unlock.canonicalize()?.starts_with(&root),
            "store the unlock file outside the wallet directory"
        );
        let v = Self {
            root,
            install,
            master,
        };
        let marker = v.root.join("identity");
        let forbid_create = registered
            || fs::read_dir(&v.root)?
                .collect::<std::io::Result<Vec<_>>>()?
                .iter()
                .any(|e| e.path().extension().is_some_and(|x| x == "keybox"));
        ensure!(
            v.load(&marker, b"identity", forbid_create, || Ok(Zeroizing::new(
                MARKER.to_vec()
            )))?
            .as_slice()
                == MARKER,
            "invalid vault identity"
        );
        Ok(v)
    }

    fn load(
        &self,
        path: &Path,
        context: &[u8],
        existing: bool,
        new: impl FnOnce() -> Result<Zeroizing<Vec<u8>>>,
    ) -> Result<Zeroizing<Vec<u8>>> {
        let mut aad = self.install.as_bytes().to_vec();
        aad.extend_from_slice(HEADER);
        aad.extend_from_slice(context);
        let cipher = XChaCha20Poly1305::new((&*self.master).into());
        let mut file = match private_file(path) {
            Ok(f) => f,
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == ErrorKind::NotFound) =>
            {
                ensure!(
                    !existing,
                    "registered key material is missing: restore the matching backup"
                );
                let mut nonce = [0u8; 24];
                OsRng
                    .try_fill_bytes(&mut nonce)
                    .map_err(|_| anyhow::anyhow!("operating-system RNG unavailable"))?;
                let mut body = new()?;
                let tag = cipher
                    .encrypt_in_place_detached(&XNonce::from(nonce), &aad, &mut body)
                    .map_err(|_| anyhow::anyhow!("cannot seal key material"))?;
                let mut sealed = HEADER.to_vec();
                sealed.extend(nonce);
                sealed.extend_from_slice(&body);
                sealed.extend_from_slice(&tag);
                publish(path, &sealed)?;
                private_file(path)?
            }
            Err(e) => return Err(e),
        };
        let mut sealed = Vec::new();
        Read::by_ref(&mut file).take(129).read_to_end(&mut sealed)?;
        ensure!(
            (44..=128).contains(&sealed.len()) && sealed.starts_with(HEADER),
            "invalid encrypted file"
        );
        let mut plain = Zeroizing::new(sealed[28..sealed.len() - 16].to_vec());
        let nonce: [u8; 24] = sealed[4..28].try_into()?;
        cipher
            .decrypt_in_place_detached(
                &XNonce::from(nonce),
                &aad,
                &mut plain,
                (&sealed[sealed.len() - 16..]).into(),
            )
            .map_err(|_| {
                anyhow::anyhow!(
                    "vault authentication failed: verify the installation, unlock file and backup"
                )
            })?;
        // Another creator may have published the winning file just before it
        // synced the parent. Make it durable before returning any address.
        File::open(&self.root)?.sync_all()?;
        Ok(plain)
    }

    pub(crate) fn address(
        &self,
        id: Uuid,
        net: Network,
        slot: &str,
        expected: Option<&[u8]>,
    ) -> Result<Vec<u8>> {
        let context = format!("wallet/{id}/{}/{slot}", custody_store::network_key(net));
        let key = self.load(
            &self.root.join(format!("{id}.keybox")),
            context.as_bytes(),
            expected.is_some(),
            || loop {
                let candidate = random32()?;
                if net != Network::BscTestnet || k256::SecretKey::from_slice(&*candidate).is_ok() {
                    return Ok(Zeroizing::new(candidate.to_vec()));
                }
            },
        )?;
        let address = derive(
            net,
            key.as_slice()
                .try_into()
                .context("invalid wallet key length")?,
        )?;
        if expected.is_some_and(|e| e != address) {
            bail!("wallet file does not match the registered address");
        }
        Ok(address)
    }

    pub(crate) fn sign_hot_mango(
        &self,
        id: Uuid,
        sender: &[u8],
        intent: &[u8; 32],
    ) -> Result<Vec<u8>> {
        self.sign_mango(id, "hot", sender, intent)
    }
    pub(crate) fn sign_mango(
        &self,
        id: Uuid,
        slot: &str,
        sender: &[u8],
        intent: &[u8; 32],
    ) -> Result<Vec<u8>> {
        use ed25519_dalek::Signer as _;
        ensure!(
            slot == "hot" || slot == "gas" || slot.starts_with("deposit:"),
            "invalid Mango signing slot"
        );
        let context = format!("wallet/{id}/mango-testnet/{slot}");
        let key = self.load(
            &self.root.join(format!("{id}.keybox")),
            context.as_bytes(),
            true,
            || bail!("missing Mango wallet"),
        )?;
        let secret = key
            .as_slice()
            .try_into()
            .context("invalid Mango key length")?;
        ensure!(
            derive(Network::MangoTestnet, secret)? == sender,
            "Mango signing address mismatch"
        );
        let sk = ed25519_dalek::SigningKey::from_bytes(secret);
        let mut sig = vec![0];
        sig.extend_from_slice(&sk.sign(intent).to_bytes());
        sig.extend_from_slice(sk.verifying_key().as_bytes());
        Ok(sig)
    }
    pub(crate) fn sign_hot_bsc(
        &self,
        id: Uuid,
        expected: alloy_primitives::Address,
        tx: alloy_consensus::TxLegacy,
    ) -> Result<Vec<u8>> {
        self.sign_bsc(id, "hot", expected, tx)
    }
    pub(crate) fn sign_bsc(
        &self,
        id: Uuid,
        slot: &str,
        expected: alloy_primitives::Address,
        tx: alloy_consensus::TxLegacy,
    ) -> Result<Vec<u8>> {
        use alloy_consensus::SignableTransaction;
        ensure!(tx.chain_id == Some(97), "wrong signing domain");
        ensure!(
            slot == "hot" || slot == "gas" || slot.starts_with("deposit:"),
            "invalid BSC signing slot"
        );
        let context = format!("wallet/{id}/bsc-testnet/{slot}");
        let key = self.load(
            &self.root.join(format!("{id}.keybox")),
            context.as_bytes(),
            true,
            || bail!("missing BSC wallet"),
        )?;
        let key = k256::ecdsa::SigningKey::from_slice(&key)?;
        let (sig, recovery) = key.sign_prehash_recoverable(tx.signature_hash().as_slice())?;
        ensure!(!recovery.is_x_reduced(), "unsupported recovery id");
        let signed = tx.into_signed(alloy_primitives::Signature::from_signature_and_parity(
            sig,
            recovery.is_y_odd(),
        ));
        ensure!(
            signed.recover_signer()? == expected,
            "signed transaction recovered unexpected wallet"
        );
        let mut raw = Vec::new();
        signed.rlp_encode(&mut raw);
        Ok(raw)
    }
}

fn derive(net: Network, secret: &[u8; 32]) -> Result<Vec<u8>> {
    Ok(match net {
        Network::MangoTestnet => {
            let public = ed25519_dalek::SigningKey::from_bytes(secret).verifying_key();
            let mut input = [0u8; 33]; // Mango Ed25519 scheme byte is zero.
            input[1..].copy_from_slice(public.as_bytes());
            keccak256(input).to_vec()
        }
        Network::BscTestnet => {
            let public = k256::SecretKey::from_slice(secret)?
                .public_key()
                .to_encoded_point(false);
            keccak256(&public.as_bytes()[1..])[12..].to_vec()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_match_chain_vectors() {
        let mut ed = Zeroizing::new([0; 32]);
        hex::decode_to_slice(
            "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
            &mut *ed,
        )
        .unwrap();
        assert_eq!(
            hex::encode(derive(Network::MangoTestnet, &ed).unwrap()),
            "ab994f930e2f65202598085e41e8c9da9e875b440291efb1d9b4a7817309baf6"
        );
        let mut secp = [0; 32];
        secp[31] = 1;
        assert_eq!(
            hex::encode(derive(Network::BscTestnet, &secp).unwrap()),
            "7e5f4552091a69125d5dfcb7b8c2659029395bdf"
        );
    }

    #[test]
    fn fresh_install_and_restored_volume_have_distinct_behaviour() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let unlock = tmp.path().join("credentials/key");
        create_unlock_file(&unlock)?;
        let first = tmp.path().join("first");
        let replica = tmp.path().join("restored");
        let second = tmp.path().join("independent");
        let first_id = Uuid::from_u128(11);
        let slot = Uuid::from_u128(21);
        let original = Vault::open(&first, &unlock, first_id, false)?;
        let wallet = original.address(slot, Network::BscTestnet, "hot", None)?;
        let mango_slot = Uuid::from_u128(22);
        let mango = original.address(mango_slot, Network::MangoTestnet, "deposit:alice", None)?;
        drop(original);
        private_dir(&replica)?;
        for e in fs::read_dir(&first)? {
            let e = e?;
            fs::copy(e.path(), replica.join(e.file_name()))?;
        }
        let restored = Vault::open(&replica, &unlock, first_id, true)?;
        let tx = alloy_consensus::TxLegacy {
            chain_id: Some(97),
            ..Default::default()
        };
        let raw =
            restored.sign_hot_bsc(slot, alloy_primitives::Address::from_slice(&wallet), tx)?;
        let signed =
            alloy_consensus::Signed::<alloy_consensus::TxLegacy>::rlp_decode(&mut raw.as_slice())?;
        assert_eq!(signed.recover_signer()?.as_slice(), wallet);
        let intent = [17; 32];
        let sig = restored.sign_mango(mango_slot, "deposit:alice", &mango, &intent)?;
        let pk = ed25519_dalek::VerifyingKey::from_bytes(sig[65..].try_into()?)?;
        pk.verify_strict(&intent, &ed25519_dalek::Signature::from_slice(&sig[1..65])?)?;
        let mut identity = [0; 33];
        identity[1..].copy_from_slice(pk.as_bytes());
        assert_eq!(keccak256(identity).as_slice(), mango);
        assert_ne!(
            Vault::open(&second, &unlock, Uuid::from_u128(12), false)?.address(
                slot,
                Network::BscTestnet,
                "hot",
                None
            )?,
            wallet
        );
        Ok(())
    }

    #[test]
    fn encrypted_files_are_bound_to_installation_slot_and_unlock_key() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let unlock = tmp.path().join("credentials/key");
        let root = tmp.path().join("vault");
        create_unlock_file(&unlock)?;
        assert!(create_unlock_file(&unlock).is_err());
        let v = Vault::open(&root, &unlock, Uuid::from_u128(1), false)?;
        let id = Uuid::from_u128(2);
        let address = v.address(id, Network::MangoTestnet, "deposit:alice", None)?;
        drop(v);
        assert!(Vault::open(&root, &unlock, Uuid::from_u128(3), true).is_err());
        let other = tmp.path().join("other/key");
        create_unlock_file(&other)?;
        assert!(Vault::open(&root, &other, Uuid::from_u128(1), true).is_err());
        let v = Vault::open(&root, &unlock, Uuid::from_u128(1), true)?;
        assert_eq!(
            v.address(id, Network::MangoTestnet, "deposit:alice", Some(&address))?,
            address
        );
        assert!(v
            .address(id, Network::MangoTestnet, "deposit:bob", None)
            .is_err());
        let file = root.join(format!("{id}.keybox"));
        assert_eq!(fs::metadata(&file)?.permissions().mode() & 0o777, 0o600);
        let mut data = fs::read(&file)?;
        assert_eq!(data.len(), 76);
        data[32] ^= 1;
        fs::write(&file, &data)?;
        assert!(v
            .address(id, Network::MangoTestnet, "deposit:alice", Some(&address))
            .is_err());
        fs::remove_file(&file)?;
        assert!(v
            .address(id, Network::MangoTestnet, "deposit:alice", Some(&address))
            .is_err());
        assert!(!file.exists());
        Ok(())
    }
}
