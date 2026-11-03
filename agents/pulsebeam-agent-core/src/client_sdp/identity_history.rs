use std::collections::{BTreeMap, BTreeSet};

use super::Error;

pub(super) type SenderOwner = (String, Option<String>, bool);

pub(super) struct IdentityHistory {
    issued: BTreeMap<u32, bool>,
    senders: BTreeMap<u32, SenderOwner>,
    bytes: usize,
    limit: usize,
}

pub(super) fn guard_bytes(count: usize) -> Option<usize> {
    count.checked_mul(size_of::<(u32, bool)>())
}

fn sender_bytes(owner: &SenderOwner) -> Option<usize> {
    size_of::<(u32, SenderOwner)>()
        .checked_add(owner.0.capacity())?
        .checked_add(owner.1.as_ref().map_or(0, String::capacity))
}

impl IdentityHistory {
    pub(super) fn new(
        used: BTreeSet<u32>,
        senders: BTreeMap<u32, SenderOwner>,
        limit: usize,
    ) -> Result<Self, Error> {
        if senders.keys().any(|id| !used.contains(id)) {
            return Err(Error::Collision);
        }
        let bytes = senders
            .values()
            .try_fold(
                guard_bytes(used.len()).ok_or(Error::Capacity)?,
                |bytes, owner| bytes.checked_add(sender_bytes(owner)?),
            )
            .filter(|bytes| *bytes <= limit)
            .ok_or(Error::Capacity)?;
        Ok(Self {
            issued: used.into_iter().map(|id| (id, false)).collect(),
            senders,
            bytes,
            limit,
        })
    }

    pub(super) fn len(&self) -> usize {
        self.issued.len()
    }

    pub(super) fn contains(&self, id: u32) -> bool {
        self.issued.contains_key(&id)
    }

    pub(super) fn retired(&self, id: u32) -> bool {
        self.issued.get(&id).copied().unwrap_or(false)
    }

    pub(super) fn same_sender(
        &self,
        id: u32,
        mid: &str,
        rid: &Option<String>,
        repair: bool,
    ) -> bool {
        self.senders.get(&id).is_some_and(|owner| {
            owner.0 == mid && owner.2 == repair && (owner.1.is_none() || owner.1 == *rid)
        })
    }

    pub(super) fn reserve(&mut self, id: u32) -> Result<(), Error> {
        if self.contains(id) {
            return Err(Error::Collision);
        }
        let bytes = self
            .bytes
            .checked_add(guard_bytes(1).ok_or(Error::Capacity)?)
            .filter(|bytes| *bytes <= self.limit)
            .ok_or(Error::Capacity)?;
        self.issued.insert(id, false);
        self.bytes = bytes;
        Ok(())
    }

    pub(super) fn reserve_sender(
        &mut self,
        id: u32,
        mid: &str,
        rid: &Option<String>,
        repair: bool,
    ) -> Result<(), Error> {
        self.reserve_senders([(id, mid, rid, repair)])
    }

    pub(super) fn reserve_senders<'a>(
        &mut self,
        reservations: impl IntoIterator<Item = (u32, &'a str, &'a Option<String>, bool)>,
    ) -> Result<(), Error> {
        let mut seen = BTreeSet::new();
        let mut changes = Vec::new();
        let mut bytes = self.bytes;
        for (id, mid, rid, repair) in reservations {
            if self.retired(id) || !seen.insert(id) {
                return Err(Error::Collision);
            }
            let old = self.senders.get(&id);
            if let Some(old) = old {
                if old.0 != mid
                    || old.2 != repair
                    || old.1.is_some() && rid.is_some() && old.1 != *rid
                {
                    return Err(Error::Collision);
                }
                if rid.is_none() || old.1 == *rid {
                    continue;
                }
            } else if self.contains(id) {
                return Err(Error::Collision);
            }
            let owner = (mid.to_owned(), rid.clone(), repair);
            bytes = bytes
                .checked_sub(old.map_or(Some(0), sender_bytes).ok_or(Error::Capacity)?)
                .and_then(|bytes| bytes.checked_add(sender_bytes(&owner)?))
                .and_then(|bytes| {
                    bytes.checked_add(if old.is_none() { guard_bytes(1)? } else { 0 })
                })
                .ok_or(Error::Capacity)?;
            changes.push((id, owner));
        }
        if bytes > self.limit {
            return Err(Error::Capacity);
        }
        for (id, owner) in changes {
            self.issued.entry(id).or_insert(false);
            self.senders.insert(id, owner);
        }
        self.bytes = bytes;
        Ok(())
    }

    pub(super) fn retire(&mut self, id: u32) -> Result<(), Error> {
        let bytes = self
            .senders
            .get(&id)
            .map_or(Some(self.bytes), |owner| {
                self.bytes.checked_sub(sender_bytes(owner)?)
            })
            .ok_or(Error::CandidateState)?;
        let retired = self.issued.get_mut(&id).ok_or(Error::CandidateState)?;
        *retired = true;
        self.senders.remove(&id);
        self.bytes = bytes;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn funded_guards_preserve_nonreuse_beyond_former_history_quotas() {
        assert!(guard_bytes(usize::MAX).is_none());
        let mut history = IdentityHistory::new(
            BTreeSet::new(),
            BTreeMap::new(),
            super::super::Limits::default().max_identity_bytes,
        )
        .unwrap();
        for id in 0..=65_536 {
            history.reserve(id).unwrap();
        }
        let bytes = guard_bytes(65_537).unwrap();
        assert_eq!(history.retire(65_538), Err(Error::CandidateState));
        assert!(!history.contains(65_538));
        assert_eq!(history.bytes, bytes);
        for id in [0, 65_536] {
            history.retire(id).unwrap();
            assert!(history.retired(id));
            assert_eq!(history.reserve(id), Err(Error::Collision));
            assert_eq!(
                history.reserve_sender(id, "v", &None, false),
                Err(Error::Collision)
            );
        }
        assert_eq!(history.bytes, bytes);
        history.reserve(65_537).unwrap();
        assert_eq!(history.bytes, bytes + guard_bytes(1).unwrap());
    }

    #[test]
    fn sender_payload_preflight_is_atomic_and_retirement_is_prepaid() {
        let owner = ("v".to_owned(), Some("q".to_owned()), false);
        let used = BTreeSet::from([0, 1]);
        let senders = BTreeMap::from([(0, owner.clone())]);
        let bytes = guard_bytes(2).unwrap() + sender_bytes(&owner).unwrap();
        assert_eq!(
            IdentityHistory::new(used.clone(), senders.clone(), bytes - 1).err(),
            Some(Error::Capacity)
        );
        for rid in [false, true] {
            let mut inflated = senders.clone();
            let owner = inflated.get_mut(&0).unwrap();
            if rid {
                owner.1.as_mut().unwrap().reserve(4096);
            } else {
                owner.0.reserve(4096);
            }
            assert_eq!(
                IdentityHistory::new(used.clone(), inflated, bytes).err(),
                Some(Error::Capacity)
            );
        }
        let mut history = IdentityHistory::new(used, senders, bytes).unwrap();
        assert_eq!(history.bytes, bytes);
        history.reserve_sender(0, "v", &None, false).unwrap();
        assert_eq!(history.senders[&0], owner);
        assert_eq!(history.bytes, bytes);
        assert_eq!(
            history.reserve_senders([(2, "v", &None, false), (0, "other", &None, false)]),
            Err(Error::Collision)
        );
        assert!(!history.contains(2));
        assert_eq!(history.bytes, bytes);
        history.retire(0).unwrap();
        assert!(history.retired(0) && !history.senders.contains_key(&0));
        assert_eq!(history.bytes, guard_bytes(2).unwrap());
        assert_eq!(
            history.reserve_sender(0, "v", &owner.1, false),
            Err(Error::Collision)
        );
        history.reserve(2).unwrap();

        let required = guard_bytes(2).unwrap() + 2 * sender_bytes(&owner).unwrap();
        let mut short =
            IdentityHistory::new(BTreeSet::new(), BTreeMap::new(), required - 1).unwrap();
        assert_eq!(
            short.reserve_senders([(0, "v", &owner.1, false), (1, "v", &owner.1, true)]),
            Err(Error::Capacity)
        );
        assert!(short.issued.is_empty() && short.senders.is_empty());
        assert_eq!(short.bytes, 0);
        short.reserve_sender(1, "v", &owner.1, true).unwrap();
        let mut exact = IdentityHistory::new(BTreeSet::new(), BTreeMap::new(), required).unwrap();
        for _ in 0..2 {
            exact
                .reserve_senders([(0, "v", &owner.1, false), (1, "v", &owner.1, true)])
                .unwrap();
            assert_eq!(exact.bytes, required);
        }

        let unassigned = ("v".to_owned(), None, false);
        let base = guard_bytes(1).unwrap() + sender_bytes(&unassigned).unwrap();
        let mut promotion = IdentityHistory::new(BTreeSet::new(), BTreeMap::new(), base).unwrap();
        promotion.reserve_sender(0, "v", &None, false).unwrap();
        assert_eq!(
            promotion.reserve_sender(0, "v", &owner.1, false),
            Err(Error::Capacity)
        );
        assert!(promotion.senders[&0].1.is_none());
        assert_eq!(promotion.bytes, base);
        promotion.retire(0).unwrap();
        assert_eq!(promotion.bytes, guard_bytes(1).unwrap());
    }
}
