
use std::fmt::{Display, Debug};
use std::fmt;
use hex;
use serde::{Deserialize, Serialize};
// elliptic curve Diffie-Hellman
use k256::ecdh::EphemeralSecret;
use ecdh;

// FHE libs
use seal_fhe::{ToBytes, FromBytes};
use sunscreen::{
    fhe_program,
    types::{
        bfv::{Rational, Signed, Fractional},
        Cipher
    },
    PublicKey,
    PrivateKey,
    Params,
    Ciphertext,
    CompiledFheProgram,
    FheRuntime,
    Error,
    Compiler
};


#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Position {
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct EncryptedPosition {
    pub x: Ciphertext,
    pub y: Ciphertext
}

#[fhe_program(scheme="bfv")]
pub fn move_position(
    x1: Cipher<Rational>,
    y1: Cipher<Rational>,
    x2: Cipher<Rational>,
    y2: Cipher<Rational>
) -> (Cipher<Rational>, Cipher<Rational>) {
    (x1+x2, y1+y2)
}

pub struct User {
    pub fhe_public_key: PublicKey,
    fhe_private_key: PrivateKey,
    pub ecdh_public_key: k256::PublicKey,
    ecdh_private_key: EphemeralSecret,
    pub runtime: FheRuntime,
    pub name: Option<String>,
}
impl User {

    pub fn setup(params: &Params, name: &str) -> Result<User, Error> {

        let runtime = FheRuntime::new(params)?;
        let (fhe_public_key, fhe_private_key) = runtime.generate_keys()?;
        // ECDH keys for encrypting and sharing FHE private keys via shared secret
        let (
            ecdh_private_key,
            ecdh_public_key
        ) = ecdh::generate_ecdh_keys();

        Ok(User {
            fhe_public_key: fhe_public_key,
            fhe_private_key: fhe_private_key,
            ecdh_public_key: ecdh_public_key,
            ecdh_private_key: ecdh_private_key,
            runtime: runtime,
            name: Some(name.to_string()),
        })
    }

    pub fn encrypt_fhe_key_for_peer(&self, bob_public_key: &k256::PublicKey) -> Vec<u8> {

        let shared_secret_key = ecdh::compute_shared_secret(&self.ecdh_private_key, bob_public_key);
        let alice_pkey = bincode::serialize(&self.fhe_private_key)
            .expect("bincode::serialize(alice_pkey");

        ecdh::encrypt(&alice_pkey, &shared_secret_key)
    }

    pub fn create_move_transaction(&self, position: Position) -> Result<EncryptedPosition, Error> {
        Ok(EncryptedPosition {
            x: self.runtime.encrypt(Rational::try_from(position.x)?, &self.fhe_public_key)?,
            y: self.runtime.encrypt(Rational::try_from(position.y)?, &self.fhe_public_key)?,
        })
    }

    pub fn decrypt_position(&self, position: EncryptedPosition) -> Result<Position, Error> {

        let position_x: Rational = self.runtime
            .decrypt(&position.x, &self.fhe_private_key)?;
        // if Error with TooMuchNoise -> probably decrypting with wrong key.
        // or tried to decrypt ciphertexts that were chain-encrypted too many times.
        let position_y: Rational = self.runtime
            .decrypt(&position.y, &self.fhe_private_key)?;

        let x: f64 = position_x.into();
        let y: f64 = position_y.into();

        Ok(Position { x, y })
    }
}

pub struct AVS {
    pub compiled_move_position: CompiledFheProgram,
    pub ecdh_public_key: k256::PublicKey,
    ecdh_private_key: EphemeralSecret,
    pub encrypted_positions: std::collections::HashMap<String, EncryptedPosition>,
    pub user_decryption_keys: std::collections::HashMap<String, PrivateKey>,
    pub peer_public_keys: std::collections::HashMap<String, k256::PublicKey>,
    runtime: FheRuntime,
    pub peer_id: Option<libp2p::PeerId>,
    pub peer_ids: std::collections::HashMap<String, libp2p::PeerId>,
}
impl AVS {

    pub fn setup() -> Result<AVS, Error> {

        let app = Compiler::new()
            .fhe_program(move_position)
            .compile()?;

        let runtime= FheRuntime::new(app.params())?;
        let (
            bob_secret,
            bob_public
        ) = ecdh::generate_ecdh_keys();

        Ok(AVS {
            compiled_move_position: app.get_fhe_program(move_position).unwrap().clone(),
            ecdh_public_key: bob_public,
            ecdh_private_key: bob_secret,
            encrypted_positions: std::collections::HashMap::new(),
            user_decryption_keys: std::collections::HashMap::new(),
            peer_public_keys: std::collections::HashMap::new(),
            runtime: runtime,
            peer_id: None,
            peer_ids: std::collections::HashMap::new(),
        })
    }

    pub fn set_peer_id(&mut self, peer_id: Option<libp2p::PeerId>) {
        self.peer_id = peer_id;
    }

    pub fn decrypt_key_from_alice(
        &self,
        encrypted_fhe_private_key: &[u8],
        alice_public_key: &k256::PublicKey
    ) -> PrivateKey {

        let shared_secret_key = ecdh::compute_shared_secret(&self.ecdh_private_key, alice_public_key);
        let alice_private_key_bytes = ecdh::decrypt(&encrypted_fhe_private_key, &shared_secret_key);
        let alice_private_key = bincode::deserialize(&alice_private_key_bytes)
            .expect("bincode::deserialize(alice_pkey");

        return alice_private_key
    }

    pub fn get_public_key_hex(&self, public_key: &PublicKey) -> String {
        hex::encode(public_key.public_key.as_bytes().expect("could not parse public_key.as_bytes"))
    }

    pub fn get_prev_position(&self, public_key: &PublicKey) -> Result<EncryptedPosition, Error> {

        let pubkey_str = self.get_public_key_hex(public_key);

        match self.encrypted_positions.get(&pubkey_str) {
            Some(p) => Ok(p.clone()),
            None => {
                let x_encrypted  = self.runtime.encrypt(Rational::try_from(0.0)?, &public_key)?;
                let y_encrypted  = self.runtime.encrypt(Rational::try_from(0.0)?, &public_key)?;
                Ok(EncryptedPosition {
                    x: x_encrypted,
                    y: y_encrypted
                })
            }
        }
    }

    pub fn decrypt_position_admin(&self, position: EncryptedPosition, peer_id: &str) -> Result<Position, Error> {

        let decryption_key = self.user_decryption_keys.get(peer_id)
            .expect("peer_id does not have a private key stored in AVS");

        let position_x: Rational = self
            .runtime
            .decrypt(&position.x, decryption_key)?;

        let position_y: Rational = self
            .runtime
            .decrypt(&position.y, decryption_key)?;

        let x: f64 = position_x.into();
        let y = position_y.into();

        Ok(Position { x, y })
    }

    pub fn run_contract(
        &mut self,
        new_position: EncryptedPosition,
        public_key: &PublicKey
    ) -> Result<EncryptedPosition, Error> {

        // get user's prev position
        let prev_position: EncryptedPosition = self.get_prev_position(public_key)?;

        // run movement function on encrypted position
        let results = self.runtime.run(
            &self.compiled_move_position,
            vec![prev_position.x, prev_position.y, new_position.x, new_position.y],
            public_key
        )?;

        let new_encrypted_position = EncryptedPosition {
            x: results[0].clone(),
            y: results[1].clone()
        };
        println!("{}", WrapperCiphertext(&new_encrypted_position.x));

        // save new encrypted position to state
        let pubkey_str = self.get_public_key_hex(public_key);
        self.encrypted_positions.insert(pubkey_str, new_encrypted_position.clone());

        Ok(new_encrypted_position)
    }
}

pub struct WrapperCiphertext<'a>(pub &'a Ciphertext);
pub struct WrapperPrivateKey<'a>(pub &'a PrivateKey);

impl <'a>Display for WrapperCiphertext<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> fmt::Result {

        let ciphertext_bytes = bincode::serialize(&self.0)
            .expect("bincode::serialize");

        write!(f, "{}", hex::encode(&ciphertext_bytes))?;
        write!(f, "\n>>> Encrypted Ciphertext length: {}", ciphertext_bytes.len())?;
        Ok(())
    }
}

impl <'a>Display for WrapperPrivateKey<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> fmt::Result {

        let privatekey_bytes = bincode::serialize(&self.0)
            .expect("bincode::serialize");

        write!(f, "{}", hex::encode(&privatekey_bytes))?;
        Ok(())
    }
}
