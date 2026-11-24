//! PIN pairing with SPAKE2 (RustCrypto `spake2`, Ed25519 group), bound to both TLS
//! certificate fingerprints so a man-in-the-middle on the (unverified, first-contact)
//! TLS session cannot complete pairing.
//!
//! Flow (control stream, after `Hello`/`Welcome { paired: false }`):
//! 1. PC shows a 6-digit PIN. User types it in the headset.
//! 2. C->S `PairStart { spake: A }`, S->C `PairStart { spake: B }`; both derive `K`.
//! 3. C->S `PairConfirm { mac: confirm_mac(K, "client", fps) }`; server verifies.
//! 4. S->C `PairOk { mac: confirm_mac(K, "server", fps) }`; client verifies.
//! 5. Both persist the peer's certificate fingerprint. Later connections are accepted
//!    only if the peer fingerprint is pinned.

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use spake2::{Ed25519Group, Identity, Password, Spake2};

pub type Fingerprint = [u8; 32];

const ID_CLIENT: &[u8] = b"hyprvr-client";
const ID_SERVER: &[u8] = b"hyprvr-server";

/// Maximum wrong PIN attempts before the daemon generates a new PIN.
pub const MAX_ATTEMPTS: u32 = 3;

#[derive(Debug, thiserror::Error)]
pub enum PairError {
    #[error("spake2 failed: {0}")]
    Spake(String),
    #[error("confirmation MAC mismatch (wrong PIN or interception)")]
    BadMac,
}

/// Generate a uniformly random 6-digit PIN.
pub fn generate_pin() -> String {
    use rand::RngExt;
    format!("{:06}", rand::rng().random_range(0..1_000_000u32))
}

/// One side of a SPAKE2 exchange.
pub struct PairState(Spake2<Ed25519Group>);

impl PairState {
    /// Client side. Returns the state and the message to send.
    pub fn client(pin: &str) -> (Self, Vec<u8>) {
        let (s, m) = Spake2::<Ed25519Group>::start_a(
            &Password::new(pin.as_bytes()),
            &Identity::new(ID_CLIENT),
            &Identity::new(ID_SERVER),
        );
        (PairState(s), m)
    }

    /// Server side.
    pub fn server(pin: &str) -> (Self, Vec<u8>) {
        let (s, m) = Spake2::<Ed25519Group>::start_b(
            &Password::new(pin.as_bytes()),
            &Identity::new(ID_CLIENT),
            &Identity::new(ID_SERVER),
        );
        (PairState(s), m)
    }

    /// Consume the peer message and derive the shared key.
    pub fn finish(self, peer_msg: &[u8]) -> Result<Vec<u8>, PairError> {
        self.0.finish(peer_msg).map_err(|e| PairError::Spake(format!("{e:?}")))
    }
}

/// Which side computes a confirmation MAC.
#[derive(Clone, Copy)]
pub enum Role {
    Client,
    Server,
}

/// HMAC-SHA256(K, role || server_fp || client_fp).
pub fn confirm_mac(key: &[u8], role: Role, server_fp: &Fingerprint, client_fp: &Fingerprint) -> [u8; 32] {
    let mut m = <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("hmac accepts any key length");
    m.update(match role {
        Role::Client => b"hyprvr-confirm-client",
        Role::Server => b"hyprvr-confirm-server",
    });
    m.update(server_fp);
    m.update(client_fp);
    m.finalize().into_bytes().into()
}

/// Constant-time verification of a peer's confirmation MAC.
pub fn verify_mac(
    key: &[u8],
    role: Role,
    server_fp: &Fingerprint,
    client_fp: &Fingerprint,
    mac: &[u8; 32],
) -> Result<(), PairError> {
    let mut m = <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("hmac accepts any key length");
    m.update(match role {
        Role::Client => b"hyprvr-confirm-client",
        Role::Server => b"hyprvr-confirm-server",
    });
    m.update(server_fp);
    m.update(client_fp);
    m.verify_slice(mac).map_err(|_| PairError::BadMac)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_pin_pairs() {
        let (sfp, cfp) = ([1u8; 32], [2u8; 32]);
        let (c, cm) = PairState::client("123456");
        let (s, sm) = PairState::server("123456");
        let kc = c.finish(&sm).unwrap();
        let ks = s.finish(&cm).unwrap();
        let mac = confirm_mac(&kc, Role::Client, &sfp, &cfp);
        verify_mac(&ks, Role::Client, &sfp, &cfp, &mac).unwrap();
    }

    #[test]
    fn wrong_pin_or_mitm_fails() {
        let (sfp, cfp) = ([1u8; 32], [2u8; 32]);
        let (c, cm) = PairState::client("123456");
        let (s, sm) = PairState::server("654321");
        let kc = c.finish(&sm).unwrap();
        let ks = s.finish(&cm).unwrap();
        let mac = confirm_mac(&kc, Role::Client, &sfp, &cfp);
        assert!(verify_mac(&ks, Role::Client, &sfp, &cfp, &mac).is_err());
        // Right PIN, but the client saw a different server certificate (MITM).
        let (c, cm) = PairState::client("111111");
        let (s, sm) = PairState::server("111111");
        let kc = c.finish(&sm).unwrap();
        let ks = s.finish(&cm).unwrap();
        let mac = confirm_mac(&kc, Role::Client, &[9u8; 32], &cfp);
        assert!(verify_mac(&ks, Role::Client, &sfp, &cfp, &mac).is_err());
    }

    #[test]
    fn pin_format() {
        let p = generate_pin();
        assert_eq!(p.len(), 6);
        assert!(p.chars().all(|c| c.is_ascii_digit()));
    }
}
