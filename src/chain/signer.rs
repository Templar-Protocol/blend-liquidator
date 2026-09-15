//! The network a transaction is hashed for, and the key that signs it.
//!
//! A `Signer` is the one place in the crate that holds key material. It
//! renders as its public address and nothing else, and the secret is never
//! stored as text: `from_secret` decodes it and keeps only the 32-byte seed
//! inside `ed25519_dalek::SigningKey`.

use ed25519_dalek::{Signer as _, SigningKey};
use sha2::{Digest, Sha256};
use stellar_xdr::{
    AccountId, Asset, ContractIdPreimage, DecoratedSignature, Hash, HashIdPreimage,
    HashIdPreimageContractId, MuxedAccount, PublicKey, Signature, SignatureHint, Transaction,
    TransactionEnvelope, TransactionV1Envelope, Uint256, VecM, WriteXdr,
};

use crate::chain::xdr::encode::XDR_LIMITS;
use crate::chain::xdr::XdrError;
use crate::chain::ChainError;
use crate::config::{ChainConfig, NetworkName};

/// A Stellar network: its passphrase and the id every signature is bound to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Network {
    /// The passphrase.
    pub passphrase: String,
    /// `sha256(passphrase)`, mixed into every transaction hash so a
    /// signature for one network is invalid on every other.
    pub id: [u8; 32],
}

impl Network {
    /// The network with this passphrase.
    #[must_use]
    pub fn from_passphrase(passphrase: &str) -> Self {
        Self {
            passphrase: passphrase.to_string(),
            id: Sha256::digest(passphrase.as_bytes()).into(),
        }
    }

    /// Public Global Stellar Network.
    #[must_use]
    pub fn mainnet() -> Self {
        Self::from_passphrase(NetworkName::Mainnet.passphrase())
    }

    /// Test SDF Network.
    #[must_use]
    pub fn testnet() -> Self {
        Self::from_passphrase(NetworkName::Testnet.passphrase())
    }

    /// The network the configuration names.
    #[must_use]
    pub fn from_config(config: &ChainConfig) -> Self {
        Self::from_passphrase(&config.network_passphrase)
    }

    /// The native asset's (XLM's) Stellar Asset Contract on this network:
    /// `sha256` of the network id and the native asset, as the protocol
    /// derives it. Derived rather than configured, so a wrong address cannot
    /// be typed in — it is where the filler's fee reserve is held back from.
    ///
    /// # Errors
    ///
    /// Only if the fixed preimage fails to encode, which is a bug.
    pub fn native_asset_contract(&self) -> Result<String, ChainError> {
        let preimage = HashIdPreimage::ContractId(HashIdPreimageContractId {
            network_id: Hash(self.id),
            contract_id_preimage: ContractIdPreimage::Asset(Asset::Native),
        });
        let bytes = preimage.to_xdr(XDR_LIMITS).map_err(XdrError::Xdr)?;
        Ok(stellar_strkey::Contract(Sha256::digest(&bytes).into()).to_string())
    }
}

/// An Ed25519 signing key and the account it controls.
pub struct Signer {
    key: SigningKey,
    public: [u8; 32],
    address: String,
}

impl std::fmt::Debug for Signer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Signer")
            .field("address", &self.address)
            .finish_non_exhaustive()
    }
}

impl Signer {
    /// Decodes an `S…` secret. Any other input, a `G…` public key included,
    /// is `SecretKey`, which carries no text.
    pub fn from_secret(secret: &str) -> Result<Self, ChainError> {
        let seed = stellar_strkey::ed25519::PrivateKey::from_string(secret)
            .map_err(|_| ChainError::SecretKey)?;
        let key = SigningKey::from_bytes(&seed.0);
        let public = key.verifying_key().to_bytes();
        let address = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(public))).to_string();
        Ok(Self {
            key,
            public,
            address,
        })
    }

    /// The `G…` strkey of the account this key controls.
    #[must_use]
    pub fn address(&self) -> &str {
        &self.address
    }

    /// The account id, for ledger keys.
    #[must_use]
    pub fn account_id(&self) -> AccountId {
        AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(self.public)))
    }

    /// The account as a transaction source.
    #[must_use]
    pub fn muxed(&self) -> MuxedAccount {
        MuxedAccount::Ed25519(Uint256(self.public))
    }

    /// Signs `tx` for `network` into a v1 envelope with one decorated
    /// signature. The hint is the last four bytes of the public key, which
    /// is how validators find the matching signer without trying each.
    pub fn sign(
        &self,
        tx: &Transaction,
        network: &Network,
    ) -> Result<TransactionEnvelope, ChainError> {
        let hash = tx.hash(network.id).map_err(XdrError::Xdr)?;
        let signature = self.key.sign(&hash);
        let mut hint = [0_u8; 4];
        hint.copy_from_slice(&self.public[28..32]);
        let decorated = DecoratedSignature {
            hint: SignatureHint(hint),
            signature: Signature(
                signature
                    .to_bytes()
                    .to_vec()
                    .try_into()
                    .map_err(XdrError::Xdr)?,
            ),
        };
        Ok(TransactionEnvelope::Tx(TransactionV1Envelope {
            tx: tx.clone(),
            signatures: VecM::try_from(vec![decorated]).map_err(XdrError::Xdr)?,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::xdr::encode::{address, invoke_contract_op};
    use ed25519_dalek::{SigningKey, VerifyingKey};
    use stellar_xdr::{Memo, Preconditions, SequenceNumber, TransactionExt, VecM};

    const POOL: &str = "CAJJZSGMMM3PD7N33TAPHGBUGTB43OC73HVIK2L2G6BNGGGYOSSYBXBD";

    fn hex(bytes: &[u8]) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        for byte in bytes {
            write!(out, "{byte:02x}").expect("write to string");
        }
        out
    }

    /// The well-known network ids: sha256 of the passphrase.
    #[test]
    fn the_network_id_is_the_sha256_of_the_passphrase() {
        assert_eq!(
            hex(&Network::mainnet().id),
            "7ac33997544e3175d266bd022439b22cdb16508c01163f26e5cb2a3e1045a979"
        );
        assert_eq!(
            hex(&Network::testnet().id),
            "cee0302d59844d32bdca915c8203dd44b33fbb7edc19051ea37abedf28ecd472"
        );
        assert_eq!(
            Network::from_passphrase("Test SDF Network ; September 2015"),
            Network::testnet()
        );
    }

    /// Derived from the network id, never configured, so it cannot be typed
    /// in wrong. The two published addresses pin the derivation.
    #[test]
    fn the_native_asset_contract_is_derived_from_the_network() {
        assert_eq!(
            Network::mainnet().native_asset_contract().unwrap(),
            "CAS3J7GYLGXMF6TDJBBYYSE3HQ6BBSMLNUQ34T6TZMYMW2EVH34XOWMA"
        );
        assert_eq!(
            Network::testnet().native_asset_contract().unwrap(),
            "CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSC"
        );
    }

    /// The contract-attested half of the check above: the mainnet fixture's
    /// first reserve is the native asset.
    #[test]
    fn the_fixtures_first_reserve_is_mainnets_native_asset() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/mainnet-fixed-v2.json"))
                .unwrap();
        assert_eq!(
            fixture["reserves"][0]["asset"],
            Network::mainnet().native_asset_contract().unwrap()
        );
    }

    fn secret() -> (String, SigningKey) {
        let key = SigningKey::from_bytes(&[7_u8; 32]);
        (
            stellar_strkey::ed25519::PrivateKey(key.to_bytes()).to_string(),
            key,
        )
    }

    #[test]
    fn a_signer_derives_its_account_from_the_secret() {
        let (secret, key) = secret();
        let signer = Signer::from_secret(&secret).unwrap();
        let expected =
            stellar_strkey::ed25519::PublicKey(key.verifying_key().to_bytes()).to_string();
        assert_eq!(signer.address(), expected);
        assert_eq!(signer.account_id().to_string(), expected);
        assert!(matches!(signer.muxed(), MuxedAccount::Ed25519(_)));
    }

    #[test]
    fn a_bad_secret_is_an_error_that_never_echoes_it() {
        let error = Signer::from_secret("SNOTAKEY").unwrap_err();
        assert!(matches!(error, ChainError::SecretKey));
        assert!(!error.to_string().contains("SNOTAKEY"));
        let (secret, _) = secret();
        // A public key is not a secret key.
        let public = Signer::from_secret(&secret).unwrap().address().to_string();
        assert!(matches!(
            Signer::from_secret(&public).unwrap_err(),
            ChainError::SecretKey
        ));
    }

    #[test]
    fn debug_shows_the_address_and_nothing_of_the_key() {
        let (secret, key) = secret();
        let signer = Signer::from_secret(&secret).unwrap();
        let rendered = format!("{signer:?}");
        assert!(rendered.contains(signer.address()));
        assert!(!rendered.contains(&secret));
        assert!(!rendered.contains(&hex(&key.to_bytes())));
        assert!(!rendered.contains("[7, 7, 7"));
    }

    #[test]
    fn a_signature_verifies_against_the_transaction_hash_and_carries_the_hint() {
        let (secret, key) = secret();
        let signer = Signer::from_secret(&secret).unwrap();
        let tx = Transaction {
            source_account: signer.muxed(),
            fee: 100,
            seq_num: SequenceNumber(42),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: VecM::try_from(vec![invoke_contract_op(
                POOL,
                "bad_debt",
                vec![address(signer.address()).unwrap()],
            )
            .unwrap()])
            .unwrap(),
            ext: TransactionExt::V0,
        };
        let network = Network::testnet();
        let envelope = signer.sign(&tx, &network).unwrap();
        let TransactionEnvelope::Tx(v1) = &envelope else {
            panic!("expected a v1 envelope");
        };
        assert_eq!(v1.tx, tx);
        assert_eq!(v1.signatures.len(), 1);
        let public = key.verifying_key().to_bytes();
        assert_eq!(v1.signatures[0].hint.0, public[28..32]);
        let hash = tx.hash(network.id).unwrap();
        let bytes: [u8; 64] = v1.signatures[0].signature.0.as_slice().try_into().unwrap();
        let signature = ed25519_dalek::Signature::from_bytes(&bytes);
        VerifyingKey::from_bytes(&public)
            .unwrap()
            .verify_strict(&hash, &signature)
            .unwrap();
        // The envelope hash is the transaction hash: what getTransaction is polled by.
        assert_eq!(envelope.hash(network.id).unwrap(), hash);
        // A different network gives a different hash, so a testnet signature never lands on mainnet.
        assert_ne!(tx.hash(Network::mainnet().id).unwrap(), hash);
    }
}
