//! Portable boot discovery. These records carry no WAN pins or media grants.

use std::num::NonZeroU64;

use pulsebeam_core::{
    auth::{ApiPublicKey, ApiSigningKey},
    identity::NodeId,
};

use super::mesh::Member;

const TAG: &[u8; 6] = b"PBAD\x01\x00";
const BODY: usize = TAG.len() + 16 + 32 + 16 + 8 + 8;
pub(crate) const FRAME_BYTES: usize = BODY + 64;
pub(crate) const VALID_FOR_MS: u64 = 12_000;
const FUTURE_SKEW_MS: u64 = 500;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Scope {
    pub(crate) epoch: [u8; 16],
    pub(crate) roster: [u8; 32],
}

#[derive(Clone, Copy)]
pub(crate) struct Announcement<'a> {
    scope: Scope,
    origin: Member,
    issued_ms: u64,
    signed: &'a [u8],
    signature: &'a [u8; 64],
}

pub(crate) struct VerifiedBoot {
    pub(crate) origin: Member,
    pub(crate) issued_ms: u64,
    pub(crate) expires_ms: u64,
}

impl Announcement<'_> {
    pub(crate) fn encode(
        scope: Scope,
        local: Member,
        issued_ms: u64,
        key: &ApiSigningKey,
    ) -> [u8; FRAME_BYTES] {
        let mut out = [0; FRAME_BYTES];
        let (body, signature) = out.split_at_mut(BODY);
        let mut slot = &mut *body;
        for field in [
            TAG.as_slice(),
            &scope.epoch,
            &scope.roster,
            local.node.as_bytes(),
            &local.generation.get().to_be_bytes(),
            &issued_ms.to_be_bytes(),
        ] {
            let (head, tail) = slot.split_at_mut(field.len());
            head.copy_from_slice(field);
            slot = tail;
        }
        signature.copy_from_slice(&key.sign_message(body));
        out
    }

    pub(crate) fn decode(frame: &[u8]) -> Option<Announcement<'_>> {
        if frame.len() != FRAME_BYTES {
            return None;
        }
        let (signed, signature) = frame.split_at(BODY);
        let (epoch, fields) = signed.strip_prefix(TAG)?.split_first_chunk::<16>()?;
        let (roster, fields) = fields.split_first_chunk::<32>()?;
        let (node, fields) = fields.split_first_chunk::<16>()?;
        let (generation, issued) = fields.split_first_chunk::<8>()?;
        Some(Announcement {
            scope: Scope {
                epoch: *epoch,
                roster: *roster,
            },
            origin: Member {
                node: NodeId::from_bytes(*node),
                generation: NonZeroU64::new(u64::from_be_bytes(*generation))?,
            },
            issued_ms: u64::from_be_bytes(issued.try_into().ok()?),
            signed,
            signature: signature.try_into().ok()?,
        })
    }

    pub(crate) fn origin(self) -> Member {
        self.origin
    }

    pub(crate) fn stamp(self) -> (NonZeroU64, u64) {
        (self.origin.generation, self.issued_ms)
    }

    /// `now_ms` must come from a clock qualified for bounded absolute skew.
    /// A relay can forward these exact bytes but cannot extend their validity.
    pub(crate) fn verify(
        self,
        scope: Scope,
        key: ApiPublicKey,
        now_ms: u64,
    ) -> Option<VerifiedBoot> {
        let expires_ms = self.issued_ms.checked_add(VALID_FOR_MS)?;
        if self.scope != scope
            || self.issued_ms > now_ms.checked_add(FUTURE_SKEW_MS)?
            || now_ms >= expires_ms
            || !key.verify_message(self.signed, self.signature)
        {
            return None;
        }
        Some(VerifiedBoot {
            origin: self.origin,
            issued_ms: self.issued_ms,
            expires_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_boot_proof_is_scoped_to_origin_key_epoch_roster_and_absolute_freshness() {
        let key = ApiSigningKey::from_seed([4; 32]);
        let public = key.public_key();
        let other = ApiSigningKey::from_seed([8; 32]).public_key();
        let origin = Member {
            node: NodeId::new(),
            generation: NonZeroU64::new(7).unwrap(),
        };
        let scope = Scope {
            epoch: [3; 16],
            roster: [1; 32],
        };
        let now = 10_000;
        let frame = Announcement::encode(scope, origin, now, &key);
        let record = Announcement::decode(&frame).unwrap();
        assert_eq!(record.origin(), origin);
        let proof = record.verify(scope, public, now).unwrap();
        assert_eq!(
            (proof.origin, proof.issued_ms, proof.expires_ms),
            (origin, now, now + VALID_FOR_MS)
        );
        assert!(record.verify(scope, other, now).is_none());
        assert!(
            record
                .verify(
                    Scope {
                        epoch: [7; 16],
                        ..scope
                    },
                    public,
                    now
                )
                .is_none()
        );
        assert!(
            record
                .verify(
                    Scope {
                        roster: [9; 32],
                        ..scope
                    },
                    public,
                    now
                )
                .is_none()
        );
        assert!(
            record
                .verify(scope, public, now - FUTURE_SKEW_MS - 1)
                .is_none()
        );
        assert_eq!(
            record.verify(scope, public, now + 250).unwrap().expires_ms,
            proof.expires_ms
        );
        assert!(record.verify(scope, public, proof.expires_ms).is_none());
        let mut changed = frame;
        changed[BODY - 9] ^= 1;
        assert!(
            Announcement::decode(&changed)
                .unwrap()
                .verify(scope, public, now)
                .is_none()
        );
        assert!(Announcement::decode(&frame[..frame.len() - 1]).is_none());
    }
}
