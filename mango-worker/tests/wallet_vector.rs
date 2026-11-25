use mgo_types::{
    base_types::MgoAddress,
    crypto::{PublicKey, SignatureScheme},
};

// RFC 8032 test key #1, also exercised from the private seed by custody-signer.
#[test]
fn official_mango_address_for_reference_ed25519_public_key() {
    let bytes=b"\xd7\x5a\x98\x01\x82\xb1\x0a\xb7\xd5\x4b\xfe\xd3\xc9\x64\x07\x3a\x0e\xe1\x72\xf3\xda\xa6\x23\x25\xaf\x02\x1a\x68\xf7\x07\x51\x1a";
    let public = PublicKey::try_from_bytes(SignatureScheme::ED25519, bytes).unwrap();
    assert_eq!(
        MgoAddress::from(&public),
        "0xab994f930e2f65202598085e41e8c9da9e875b440291efb1d9b4a7817309baf6"
            .parse()
            .unwrap()
    );
}
