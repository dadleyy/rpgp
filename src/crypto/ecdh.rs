use block_padding::{Padding, Pkcs7};
use generic_array::{typenum::U8, GenericArray};
use rand::{CryptoRng, Rng};
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::{Zeroize, Zeroizing};

use crate::crypto::{
    aes_kw, ecc_curve::ECCCurve, public_key::PublicKeyAlgorithm, sym::SymmetricKeyAlgorithm,
};
use crate::errors::{Error, Result};
use crate::types::{ECDHSecretKey, Mpi, PlainSecretParams, PublicParams};

use super::hash::HashAlgorithm;

/// 20 octets representing "Anonymous Sender    ".
const ANON_SENDER: [u8; 20] = [
    0x41, 0x6E, 0x6F, 0x6E, 0x79, 0x6D, 0x6F, 0x75, 0x73, 0x20, 0x53, 0x65, 0x6E, 0x64, 0x65, 0x72,
    0x20, 0x20, 0x20, 0x20,
];

const SECRET_KEY_LENGTH: usize = 32;

/// Generate an ECDH KeyPair.
/// Currently only support ED25519.
pub fn generate_key<R: Rng + CryptoRng>(rng: &mut R) -> (PublicParams, PlainSecretParams) {
    let mut secret_key_bytes = Zeroizing::new([0u8; SECRET_KEY_LENGTH]);
    rng.fill_bytes(&mut *secret_key_bytes);

    let secret = StaticSecret::from(*secret_key_bytes);
    let public = PublicKey::from(&secret);

    // public key
    let p_raw = public.to_bytes();

    let mut p = Vec::with_capacity(33);
    p.push(0x40);
    p.extend_from_slice(&p_raw);

    // secret key
    // Clamp, as `to_bytes` does not clamp.
    let q_raw = curve25519_dalek::scalar::clamp_integer(secret.to_bytes());
    // Big Endian
    let q = q_raw.into_iter().rev().collect::<Vec<u8>>();

    // TODO: make these configurable and/or check for good defaults
    let hash = HashAlgorithm::default();
    let alg_sym = SymmetricKeyAlgorithm::AES128;
    (
        PublicParams::ECDH {
            curve: ECCCurve::Curve25519,
            p: p.into(),
            hash,
            alg_sym,
        },
        PlainSecretParams::ECDH(Mpi::from_raw(q)),
    )
}

/// Build param for ECDH algorithm (as defined in RFC 6637)
/// https://tools.ietf.org/html/rfc6637#section-8
pub fn build_ecdh_param(
    oid: &[u8],
    alg_sym: SymmetricKeyAlgorithm,
    hash: HashAlgorithm,
    fingerprint: &[u8],
) -> Vec<u8> {
    let kdf_params = vec![
        0x03, // length of the following fields
        0x01, // reserved for future extensions
        hash.into(),
        u8::from(alg_sym),
    ];

    let oid_len = [oid.len() as u8];

    let pkalgo = [u8::from(PublicKeyAlgorithm::ECDH)];

    let values: Vec<&[u8]> = vec![
        &oid_len,
        oid,
        &pkalgo,
        &kdf_params,
        &ANON_SENDER[..],
        fingerprint,
    ];

    values.concat()
}

/// ECDH decryption.
pub fn decrypt(priv_key: &ECDHSecretKey, mpis: &[Mpi], fingerprint: &[u8]) -> Result<Vec<u8>> {
    debug!("ECDH decrypt");

    let param = build_ecdh_param(&priv_key.oid, priv_key.alg_sym, priv_key.hash, fingerprint);

    // 33 = 0x40 + 32bits
    ensure_eq!(mpis.len(), 3);
    ensure_eq!(mpis[0].len(), 33, "invalid public point");
    ensure_eq!(priv_key.secret.len(), 32, "invalid secret point");

    // encrypted and wrapped value derived from the session key
    let encrypted_session_key = mpis[2].as_bytes();

    let their_public = {
        // public part of the ephemeral key (removes 0x40 prefix)
        let ephemeral_public_key = &mpis[0].as_bytes()[1..];

        // create montgomery point
        let mut ephemeral_public_key_arr = [0u8; 32];
        ephemeral_public_key_arr[..].copy_from_slice(ephemeral_public_key);

        x25519_dalek::PublicKey::from(ephemeral_public_key_arr)
    };

    let our_secret = {
        // private key of the recipient.
        let private_key = &priv_key.secret[..];

        // create scalar and reverse to little endian
        let mut private_key_le = private_key.iter().rev().cloned().collect::<Vec<u8>>();
        let mut private_key_arr = [0u8; 32];
        private_key_arr[..].copy_from_slice(&private_key_le);
        private_key_le.zeroize();

        StaticSecret::from(private_key_arr)
    };

    // derive shared secret
    let shared_secret = our_secret.diffie_hellman(&their_public);

    // Perform key derivation
    let z = kdf(
        priv_key.hash,
        shared_secret.as_bytes(),
        priv_key.alg_sym.key_size(),
        &param,
    )?;

    // Peform AES Key Unwrap
    let encrypted_key_len: usize = match mpis[1].first() {
        Some(l) => *l as usize,
        None => 0,
    };

    let mut encrypted_session_key_vec = vec![0; encrypted_key_len];
    encrypted_session_key_vec[(encrypted_key_len - encrypted_session_key.len())..]
        .copy_from_slice(encrypted_session_key);

    let mut decrypted_key_padded = aes_kw::unwrap(&z, &encrypted_session_key_vec)?;
    // PKCS5-style unpadding (PKCS5 is PKCS7 with a blocksize of 8).
    //
    // RFC 6637 describes the padding:
    // a) "The result is padded using the method described in [PKCS5] to the 8-byte granularity."
    // b) "For example, assuming that an AES algorithm is used for the session key, the sender MAY
    // use 21, 13, and 5 bytes of padding for AES-128, AES-192, and AES-256, respectively, to
    // provide the same number of octets, 40 total, as an input to the key wrapping method."
    //
    // So while the padding ensures that the length of the padded message is a multiple of 8, the
    // padding may exceed 8 bytes in size.
    {
        let len = decrypted_key_padded.len();
        let block_size = 8;
        ensure!(len % block_size == 0, "invalid key length {}", len);
        ensure!(!decrypted_key_padded.is_empty(), "empty key is not valid");

        // The last byte should contain the padding symbol, which is also the padding length
        let pad = decrypted_key_padded.last().expect("is not empty");

        // Padding length seems to exceed size of the padded message
        if *pad as usize > len {
            return Err(Error::UnpadError);
        }

        // Expected length of the unpadded message
        let unpadded_len = len - *pad as usize;

        // All bytes that constitute the padding must have the value of `pad`
        if decrypted_key_padded[unpadded_len..]
            .iter()
            .any(|byte| byte != pad)
        {
            return Err(Error::UnpadError);
        }

        decrypted_key_padded.truncate(unpadded_len);
    }

    ensure!(
        !decrypted_key_padded.is_empty(),
        "empty unpadded key is not valid"
    );

    Ok(decrypted_key_padded)
}

/// Key Derivation Function for ECDH (as defined in RFC 6637).
/// https://tools.ietf.org/html/rfc6637#section-7
fn kdf(hash: HashAlgorithm, x: &[u8; 32], length: usize, param: &[u8]) -> Result<Vec<u8>> {
    let prefix = vec![0, 0, 0, 1];

    let values: Vec<&[u8]> = vec![&prefix, x, param];
    let data = values.concat();

    let mut digest = hash.digest(&data)?;
    digest.truncate(length);

    Ok(digest)
}

/// ECDH encryption.
pub fn encrypt<R: CryptoRng + Rng>(
    rng: &mut R,
    curve: &ECCCurve,
    alg_sym: SymmetricKeyAlgorithm,
    hash: HashAlgorithm,
    fingerprint: &[u8],
    q: &[u8],
    plain: &[u8],
) -> Result<Vec<Vec<u8>>> {
    debug!("ECDH encrypt");

    // can't fit more size wise
    let max_size = 239;
    ensure!(
        plain.len() < max_size,
        "unable to encrypt larger than {} bytes",
        max_size
    );

    let param = build_ecdh_param(&curve.oid(), alg_sym, hash, fingerprint);

    ensure_eq!(q.len(), 33, "invalid public key");

    let their_public = {
        // public part of the ephemeral key (removes 0x40 prefix)
        let public_key = &q[1..];

        // create montgomery point
        let mut public_key_arr = [0u8; 32];
        public_key_arr[..].copy_from_slice(public_key);

        x25519_dalek::PublicKey::from(public_key_arr)
    };

    let mut our_secret_key_bytes = Zeroizing::new([0u8; SECRET_KEY_LENGTH]);
    rng.fill_bytes(&mut *our_secret_key_bytes);
    let our_secret = StaticSecret::from(*our_secret_key_bytes);

    // derive shared secret
    let shared_secret = our_secret.diffie_hellman(&their_public);

    // Perform key derivation
    let z = kdf(hash, shared_secret.as_bytes(), alg_sym.key_size(), &param)?;

    // PKCS5 padding (PKCS5 is PKCS7 with a blocksize of 8)
    let len = plain.len();
    let mut plain_padded = plain.to_vec();
    plain_padded.resize(len + 8, 0);

    let plain_padded_ref = {
        let pos = len;
        let block_size = 8;
        let bs = block_size * (pos / block_size);
        if plain_padded.len() < bs || plain_padded.len() - bs < block_size {
            bail!("unable to pad");
        }
        let buf = GenericArray::<u8, U8>::from_mut_slice(&mut plain_padded[bs..bs + block_size]);
        Pkcs7::pad(buf, pos - bs);
        &plain_padded[..bs + block_size]
    };

    // Peform AES Key Wrap
    let encrypted_key = aes_kw::wrap(&z, plain_padded_ref)?;

    // Encode public point: prefix with 0x40
    let mut encoded_public = Vec::with_capacity(33);
    encoded_public.push(0x40);
    encoded_public.extend(x25519_dalek::PublicKey::from(&our_secret).as_bytes().iter());

    let encrypted_key_len = vec![u8::try_from(encrypted_key.len())?];

    Ok(vec![encoded_public, encrypted_key_len, encrypted_key])
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::fs;

    use crate::{Deserializable, Message, SignedSecretKey};
    use rand::{RngCore, SeedableRng};
    use rand_chacha::ChaChaRng;

    use crate::types::{PublicParams, SecretKeyRepr};

    #[test]
    fn test_encrypt_decrypt() {
        let mut rng = ChaChaRng::from_seed([0u8; 32]);

        let (pkey, skey) = generate_key(&mut rng);

        for text_size in 1..239 {
            for _i in 0..10 {
                let mut fingerprint = vec![0u8; 20];
                rng.fill_bytes(&mut fingerprint);

                let mut plain = vec![0u8; text_size];
                rng.fill_bytes(&mut plain);

                let mpis = match pkey {
                    PublicParams::ECDH {
                        ref curve,
                        ref p,
                        hash,
                        alg_sym,
                    } => encrypt(
                        &mut rng,
                        curve,
                        alg_sym,
                        hash,
                        &fingerprint,
                        p.as_bytes(),
                        &plain[..],
                    )
                    .unwrap(),
                    _ => panic!("invalid key generated"),
                };

                let mpis = mpis.into_iter().map(Into::into).collect::<Vec<Mpi>>();

                let decrypted = match skey.as_ref().as_repr(&pkey).unwrap() {
                    SecretKeyRepr::ECDH(ref skey) => decrypt(skey, &mpis, &fingerprint).unwrap(),
                    _ => panic!("invalid key generated"),
                };

                assert_eq!(&plain[..], &decrypted[..]);
            }
        }
    }

    #[test]
    fn test_decrypt_padding() {
        let (decrypt_key, _headers) = SignedSecretKey::from_armor_single(
            fs::File::open("./tests/unit-tests/padding/alice.key").unwrap(),
        )
        .expect("failed to read decryption key");

        for msg_file in [
            "./tests/unit-tests/padding/msg-short-padding.pgp",
            "./tests/unit-tests/padding/msg-long-padding.pgp",
        ] {
            let (message, _headers) = Message::from_armor_single(fs::File::open(msg_file).unwrap())
                .expect("failed to parse message");

            let (mut decrypter, _ids) = message
                .decrypt(String::default, &[&decrypt_key])
                .expect("failed to init decryption");

            let msg = decrypter.next().unwrap().unwrap();
            let data = msg.get_literal().unwrap().data();

            assert_eq!(data, "hello\n".as_bytes());
        }
    }
}
