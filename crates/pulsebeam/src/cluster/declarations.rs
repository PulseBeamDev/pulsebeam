//! A routing declaration cut for one scoped interest on an authenticated stream.
//! These declarations are reachability hints, not publisher or tenant authority.

use std::{collections::BTreeSet, num::NonZeroU64};

use pulsebeam_core::identity::{ProjectId, RoomId};
use serde::{Deserialize, Serialize};

mod exchange;
pub(crate) use exchange::{Exchange, Finalized, MAX_INTERESTS};

const MAX_DECLARATIONS: usize = 256;

type Revision = NonZeroU64;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Edge,
    Router,
    Gateway,
}

impl Role {
    pub(crate) const fn wire_name(self) -> &'static str {
        match self {
            Self::Edge => "edge",
            Self::Router => "router",
            Self::Gateway => "gateway",
        }
    }

    pub(crate) fn adjacent(self, peer: Self) -> bool {
        matches!(
            (self, peer),
            (Self::Edge | Self::Gateway, Self::Router)
                | (Self::Router, Self::Edge | Self::Router | Self::Gateway)
        )
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Kind {
    Subscriber,
    Queryable,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub(crate) struct Declaration {
    pub(crate) project: ProjectId,
    pub(crate) room: RoomId,
    pub(crate) kind: Kind,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Mode {
    Current,
    CurrentFuture,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Interest {
    pub(crate) project: ProjectId,
    pub(crate) room: Option<RoomId>,
    pub(crate) kind: Kind,
    pub(crate) mode: Mode,
}

impl Interest {
    fn includes(self, item: Declaration) -> bool {
        self.project == item.project
            && self.room.is_none_or(|room| room == item.room)
            && self.kind == item.kind
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    Stale,
    Scope,
    Capacity,
    NeedsSnapshot,
}

struct Cut {
    revision: Revision,
    items: BTreeSet<Declaration>,
}

pub(crate) struct Receive {
    interest: Interest,
    // A higher watermark must survive a failed cut but never expose its items.
    watermark: Option<Revision>,
    staging: Option<Cut>,
    ready: Option<Cut>,
}

impl Receive {
    pub(crate) fn new(local: Role, peer: Role, interest: Interest) -> Result<Self, Error> {
        if !local.adjacent(peer) {
            return Err(Error::Scope);
        }
        Ok(Self {
            interest,
            watermark: None,
            staging: None,
            ready: None,
        })
    }

    pub(crate) fn start(&mut self, revision: Revision) -> Result<(), Error> {
        if self.watermark.is_some_and(|known| revision <= known) {
            return Err(Error::Stale);
        }
        self.withdraw(revision);
        self.staging = Some(Cut {
            revision,
            items: BTreeSet::new(),
        });
        Ok(())
    }

    pub(crate) fn item(&mut self, revision: Revision, item: Declaration) -> Result<(), Error> {
        let Some(cut) = self.staging.as_mut().filter(|cut| cut.revision == revision) else {
            return Err(Error::Stale);
        };
        let result = if !self.interest.includes(item) {
            Err(Error::Scope)
        } else if cut.items.contains(&item) {
            Err(Error::NeedsSnapshot)
        } else if cut.items.len() >= MAX_DECLARATIONS {
            Err(Error::Capacity)
        } else {
            cut.items.insert(item);
            Ok(())
        };
        if result.is_err() {
            self.withdraw(revision);
        }
        result
    }

    pub(crate) fn finish(&mut self, revision: Revision, count: usize) -> Result<(), Error> {
        if self
            .staging
            .as_ref()
            .is_none_or(|cut| cut.revision != revision)
        {
            return Err(Error::Stale);
        }
        let cut = self.staging.take().ok_or(Error::NeedsSnapshot)?;
        if cut.items.len() != count {
            return Err(Error::NeedsSnapshot);
        }
        self.ready = Some(cut);
        Ok(())
    }

    pub(crate) fn delta(
        &mut self,
        base: Revision,
        revision: Revision,
        item: Declaration,
        present: bool,
    ) -> Result<(), Error> {
        if !matches!(self.interest.mode, Mode::CurrentFuture) {
            return Err(Error::Scope);
        }
        if self.watermark.is_some_and(|known| revision <= known) {
            return Err(Error::Stale);
        }
        let result = self.apply_delta(base, revision, item, present);
        if result.is_err() {
            self.withdraw(revision);
        }
        result
    }

    fn apply_delta(
        &mut self,
        base: Revision,
        revision: Revision,
        item: Declaration,
        present: bool,
    ) -> Result<(), Error> {
        let cut = self.ready.as_mut().ok_or(Error::NeedsSnapshot)?;
        if cut.revision != base || base.get().checked_add(1) != Some(revision.get()) {
            return Err(Error::NeedsSnapshot);
        }
        if !self.interest.includes(item) {
            return Err(Error::Scope);
        }
        if present {
            if cut.items.len() >= MAX_DECLARATIONS && !cut.items.contains(&item) {
                return Err(Error::Capacity);
            }
            cut.items.insert(item);
        } else {
            cut.items.remove(&item);
        }
        cut.revision = revision;
        self.watermark = Some(revision);
        Ok(())
    }

    fn withdraw(&mut self, watermark: Revision) {
        self.watermark = Some(watermark);
        self.staging = None;
        self.ready = None;
    }

    /// None means unknown coverage, including while assembling the initial cut.
    pub(crate) fn ready(&self) -> Option<(Revision, &BTreeSet<Declaration>)> {
        self.ready.as_ref().map(|cut| (cut.revision, &cut.items))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declaration(room: &str) -> Declaration {
        Declaration {
            project: ProjectId::new(),
            room: RoomId::new(room).unwrap(),
            kind: Kind::Queryable,
        }
    }

    fn revision(value: u64) -> Revision {
        Revision::new(value).unwrap()
    }

    #[test]
    fn finalized_cut_then_concurrent_change_and_gap() {
        let a = declaration("a");
        let b = Declaration {
            room: RoomId::new("b").unwrap(),
            ..a
        };
        let interest = Interest {
            project: a.project,
            room: None,
            kind: a.kind,
            mode: Mode::CurrentFuture,
        };
        assert!(matches!(
            Receive::new(Role::Edge, Role::Edge, interest),
            Err(Error::Scope)
        ));
        let mut view = Receive::new(Role::Edge, Role::Router, interest).unwrap();
        view.start(revision(1)).unwrap();
        view.item(revision(1), a).unwrap();
        assert!(view.ready().is_none());
        view.finish(revision(1), 1).unwrap();
        view.delta(revision(1), revision(2), b, true).unwrap();
        assert_eq!(view.ready(), Some((revision(2), &BTreeSet::from([a, b]))));
        assert_eq!(
            view.delta(revision(3), revision(4), a, false),
            Err(Error::NeedsSnapshot)
        );
        assert!(view.ready().is_none());
        assert_eq!(view.start(revision(4)), Err(Error::Stale));
        view.start(revision(5)).unwrap();
        view.item(revision(5), b).unwrap();
        view.finish(revision(5), 1).unwrap();
        assert_eq!(view.ready(), Some((revision(5), &BTreeSet::from([b]))));
    }

    #[test]
    fn incomplete_oversized_or_out_of_scope_cuts_never_become_ready() {
        let a = declaration("a");
        let interest = Interest {
            project: a.project,
            room: None,
            kind: a.kind,
            mode: Mode::Current,
        };
        let mut view = Receive::new(Role::Router, Role::Gateway, interest).unwrap();
        view.start(revision(1)).unwrap();
        assert_eq!(view.finish(revision(1), 1), Err(Error::NeedsSnapshot));
        view.start(revision(2)).unwrap();
        let wrong_kind = Declaration {
            kind: Kind::Subscriber,
            ..a
        };
        assert_eq!(view.item(revision(2), wrong_kind), Err(Error::Scope));
        assert!(view.finish(revision(2), 0).is_err());
        view.start(revision(3)).unwrap();
        for index in 0..MAX_DECLARATIONS {
            view.item(
                revision(3),
                Declaration {
                    room: RoomId::new(&format!("room-{index}")).unwrap(),
                    ..a
                },
            )
            .unwrap();
        }
        assert_eq!(view.item(revision(3), a), Err(Error::Capacity));
        assert!(view.finish(revision(3), MAX_DECLARATIONS).is_err());
        assert!(view.ready().is_none());
        view.start(revision(4)).unwrap();
        view.finish(revision(4), 0).unwrap();
        assert_eq!(
            view.delta(revision(4), revision(5), a, true),
            Err(Error::Scope)
        );
        assert_eq!(view.ready(), Some((revision(4), &BTreeSet::new())));
    }
}
