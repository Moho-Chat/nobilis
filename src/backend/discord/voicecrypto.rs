//! Discord's voice packet encryption, for the connection this crate opens
//! itself.
//!
//! The channel connection is songbird's and encrypts its own packets. A Go
//! Live stream is a second connection with its own endpoint, token, key and
//! UDP session, which songbird has no part in - so its packets have to be
//! sealed here, in exactly the layout the other end expects.
//!
//! The layout, which is Discord's own variation on SRTP (RFC 3711 §3.1):
//!
//! ```text
//! [ 12-byte RTP header ][ ciphertext ][ 16-byte tag ][ 4-byte nonce ]
//!   authenticated         encrypted                    in the clear
//! ```
//!
//! The nonce on the wire is four bytes and counts up once per packet; the
//! cipher's real nonce is twelve or twenty-four, so those four go at the
//! front and the rest stay zero. The RTP header is authenticated but not
//! encrypted, because the far end has to read the sequence number and the
//! SSRC before it has any hope of decrypting anything.
//!
//! Written against songbird's own implementation of the same two modes rather
//! than from the description alone: where the tag goes, which bytes are the
//! additional data, and whether the counter is big-endian are all things that
//! produce a packet the far end silently drops.

use aes_gcm::aead::{AeadInPlace, KeyInit};
use aes_gcm::Aes256Gcm;
use chacha20poly1305::XChaCha20Poly1305;

/// How many bytes a sealed packet adds after the payload.
pub const TAG_LEN: usize = 16;
pub const NONCE_LEN: usize = 4;
pub const OVERHEAD: usize = TAG_LEN + NONCE_LEN;

/// The two schemes Discord offers a client that is not doing end-to-end
/// encryption, named as they appear in the voice server's `modes` list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Aes256Gcm,
    XChaCha20Poly1305,
}

impl Mode {
    pub fn wire_name(self) -> &'static str {
        match self {
            Mode::Aes256Gcm => "aead_aes256_gcm_rtpsize",
            Mode::XChaCha20Poly1305 => "aead_xchacha20_poly1305_rtpsize",
        }
    }

    /// Picks the best of what a voice server says it will accept.
    ///
    /// AES first where the server offers it: it is Discord's own preference
    /// and is hardware-accelerated on anything this runs on. Anything else in
    /// the list is a mode this client does not implement and is skipped
    /// rather than guessed at - a client that picked a name it did not
    /// understand would negotiate successfully and then send rubbish.
    pub fn negotiate<S: AsRef<str>>(offered: &[S]) -> Option<Mode> {
        let has = |name: &str| offered.iter().any(|m| m.as_ref() == name);
        if has(Mode::Aes256Gcm.wire_name()) {
            Some(Mode::Aes256Gcm)
        } else if has(Mode::XChaCha20Poly1305.wire_name()) {
            Some(Mode::XChaCha20Poly1305)
        } else {
            None
        }
    }
}

/// One connection's sealing key and packet counter.
pub struct Sealer {
    mode: Mode,
    aes: Option<Aes256Gcm>,
    cha: Option<XChaCha20Poly1305>,
    /// The four bytes on the wire, counting up once per packet.
    ///
    /// Started at a random point rather than zero, which is what songbird
    /// does and what the mode's description asks for: the counter is public,
    /// and one that always starts at zero tells an observer where a
    /// connection began.
    counter: u32,
}

impl Sealer {
    pub fn new(mode: Mode, key: &[u8]) -> anyhow::Result<Self> {
        if key.len() != 32 {
            anyhow::bail!("a voice key is 32 bytes, not {}", key.len());
        }
        Ok(Self {
            mode,
            aes: match mode {
                Mode::Aes256Gcm => Some(Aes256Gcm::new_from_slice(key).map_err(|_| anyhow::anyhow!("bad key"))?),
                _ => None,
            },
            cha: match mode {
                Mode::XChaCha20Poly1305 => {
                    Some(XChaCha20Poly1305::new_from_slice(key).map_err(|_| anyhow::anyhow!("bad key"))?)
                }
                _ => None,
            },
            counter: rand::random(),
        })
    }

    /// Seals one packet: header in the clear and authenticated, payload
    /// encrypted, tag and nonce appended.
    pub fn seal(&mut self, header: &[u8], payload: &[u8]) -> anyhow::Result<Vec<u8>> {
        let counter = self.counter;
        self.counter = self.counter.wrapping_add(1);

        let mut body = payload.to_vec();
        let tag = match self.mode {
            Mode::Aes256Gcm => {
                let mut nonce = aes_gcm::Nonce::default();
                nonce[..NONCE_LEN].copy_from_slice(&counter.to_be_bytes());
                let cipher = self.aes.as_ref().expect("aes mode has an aes cipher");
                cipher
                    .encrypt_in_place_detached(&nonce, header, &mut body)
                    .map_err(|_| anyhow::anyhow!("sealing the packet failed"))?
                    .to_vec()
            }
            Mode::XChaCha20Poly1305 => {
                let mut nonce = chacha20poly1305::XNonce::default();
                nonce[..NONCE_LEN].copy_from_slice(&counter.to_be_bytes());
                let cipher = self.cha.as_ref().expect("chacha mode has a chacha cipher");
                cipher
                    .encrypt_in_place_detached(&nonce, header, &mut body)
                    .map_err(|_| anyhow::anyhow!("sealing the packet failed"))?
                    .to_vec()
            }
        };

        let mut out = Vec::with_capacity(header.len() + body.len() + OVERHEAD);
        out.extend_from_slice(header);
        out.extend_from_slice(&body);
        out.extend_from_slice(&tag);
        out.extend_from_slice(&counter.to_be_bytes());
        Ok(out)
    }
}

/// Opens a packet sealed the same way.
///
/// Here for the other half of a stream - watching one - and used by the tests
/// either way: a sealer with nothing that can open what it makes is a sealer
/// nothing can check.
pub fn open(mode: Mode, key: &[u8], packet: &[u8], header_len: usize) -> anyhow::Result<Vec<u8>> {
    if packet.len() < header_len + OVERHEAD {
        anyhow::bail!("packet too short to hold a tag and a nonce");
    }
    let (header, rest) = packet.split_at(header_len);
    let nonce_at = rest.len() - NONCE_LEN;
    let counter = &rest[nonce_at..];
    let tag = &rest[nonce_at - TAG_LEN..nonce_at];
    let mut body = rest[..nonce_at - TAG_LEN].to_vec();

    match mode {
        Mode::Aes256Gcm => {
            let mut nonce = aes_gcm::Nonce::default();
            nonce[..NONCE_LEN].copy_from_slice(counter);
            let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| anyhow::anyhow!("bad key"))?;
            cipher
                .decrypt_in_place_detached(&nonce, header, &mut body, tag.into())
                .map_err(|_| anyhow::anyhow!("the packet did not open"))?;
        }
        Mode::XChaCha20Poly1305 => {
            let mut nonce = chacha20poly1305::XNonce::default();
            nonce[..NONCE_LEN].copy_from_slice(counter);
            let cipher = XChaCha20Poly1305::new_from_slice(key).map_err(|_| anyhow::anyhow!("bad key"))?;
            cipher
                .decrypt_in_place_detached(&nonce, header, &mut body, tag.into())
                .map_err(|_| anyhow::anyhow!("the packet did not open"))?;
        }
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::discord::rtp;

    fn key() -> [u8; 32] {
        let mut k = [0u8; 32];
        for (i, b) in k.iter_mut().enumerate() {
            *b = i as u8;
        }
        k
    }

    /// The claim the whole module rests on, for both modes: what is sealed
    /// opens, and opens to exactly what went in.
    #[test]
    fn a_sealed_packet_opens_to_what_went_in() {
        for mode in [Mode::Aes256Gcm, Mode::XChaCha20Poly1305] {
            let packet = rtp::Packet {
                payload_type: rtp::PAYLOAD_TYPE_VP8,
                sequence: 42,
                timestamp: 90_000,
                ssrc: 0xCAFE,
                marker: true,
                payload: b"a frame of something".to_vec(),
            };
            let header = rtp::header(&packet);
            let mut sealer = Sealer::new(mode, &key()).expect("a sealer");
            let sealed = sealer.seal(&header, &packet.payload).expect("sealing");

            // The header travels in the clear - the far end reads the
            // sequence number and the SSRC before it can decrypt anything.
            assert_eq!(&sealed[..12], &header, "{mode:?}");
            assert_eq!(sealed.len(), 12 + packet.payload.len() + OVERHEAD, "{mode:?}");

            let opened = open(mode, &key(), &sealed, 12).expect("opening");
            assert_eq!(opened, packet.payload, "{mode:?}");
        }
    }

    /// The header is authenticated, not merely prepended. A relay that
    /// rewrote the sequence number would otherwise go unnoticed.
    #[test]
    fn a_changed_header_stops_the_packet_opening() {
        let header = [0x80u8, 103, 0, 1, 0, 0, 0, 0, 0, 0, 0, 9];
        let mut sealer = Sealer::new(Mode::Aes256Gcm, &key()).unwrap();
        let mut sealed = sealer.seal(&header, b"hello").unwrap();
        assert!(open(Mode::Aes256Gcm, &key(), &sealed, 12).is_ok());

        // One bit of the sequence number.
        sealed[3] ^= 0x01;
        assert!(open(Mode::Aes256Gcm, &key(), &sealed, 12).is_err());
    }

    /// Every packet gets its own nonce and it counts up. Two packets sealed
    /// with the same key and the same nonce is the failure that takes the
    /// key with it.
    #[test]
    fn the_nonce_counts_up_once_per_packet() {
        let header = [0x80u8, 103, 0, 1, 0, 0, 0, 0, 0, 0, 0, 9];
        let mut sealer = Sealer::new(Mode::Aes256Gcm, &key()).unwrap();
        let first = sealer.seal(&header, b"one").unwrap();
        let second = sealer.seal(&header, b"two").unwrap();

        let nonce_of = |p: &[u8]| u32::from_be_bytes(p[p.len() - 4..].try_into().unwrap());
        assert_eq!(nonce_of(&second), nonce_of(&first).wrapping_add(1));

        // And the same plaintext twice does not seal to the same bytes,
        // which is what a repeated nonce would look like.
        let a = sealer.seal(&header, b"same").unwrap();
        let b = sealer.seal(&header, b"same").unwrap();
        assert_ne!(a, b);
    }

    /// A mode named but not implemented is not one to pick: negotiating
    /// successfully and then sending rubbish is worse than refusing.
    #[test]
    fn only_a_mode_this_client_can_actually_speak_is_chosen() {
        assert_eq!(
            Mode::negotiate(&["aead_aes256_gcm_rtpsize", "aead_xchacha20_poly1305_rtpsize"]),
            Some(Mode::Aes256Gcm),
            "AES is Discord's own preference where it is offered"
        );
        assert_eq!(
            Mode::negotiate(&["aead_xchacha20_poly1305_rtpsize"]),
            Some(Mode::XChaCha20Poly1305)
        );
        // The deprecated ones, and anything invented since.
        assert_eq!(Mode::negotiate(&["xsalsa20_poly1305", "something_new"]), None);
        assert_eq!(Mode::negotiate::<&str>(&[]), None);
    }

    #[test]
    fn a_key_that_is_not_a_key_is_refused_rather_than_padded() {
        assert!(Sealer::new(Mode::Aes256Gcm, &[0u8; 16]).is_err());
        assert!(Sealer::new(Mode::Aes256Gcm, &[]).is_err());
        assert!(Sealer::new(Mode::Aes256Gcm, &key()).is_ok());
    }

    #[test]
    fn a_runt_packet_is_refused_rather_than_indexed_past_its_end() {
        assert!(open(Mode::Aes256Gcm, &key(), &[0u8; 12], 12).is_err());
        assert!(open(Mode::Aes256Gcm, &key(), &[0u8; 4], 12).is_err());
    }
}
