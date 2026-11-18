use super::*;
use agent_core::{
    TrackOwner,
    ffi::{ErrorCode, ErrorInfo, WatchConfig, WatchState, WatchStatus},
};

pub(super) type WatchOwners = RefCell<BTreeMap<u64, TrackOwner>>;
pub(crate) type WatchProjection = (u64, WatchState, bool);

impl Control {
    pub(super) fn collect_catalog_states(&self, agent: &Agent) -> Vec<WatchProjection> {
        let mut states = Vec::new();
        self.watch_owners.borrow_mut().retain(|id, owner| {
            let Some(state) = agent.catalog_watch_state(*owner) else {
                return false;
            };
            let closing = state.status == WatchStatus::Failed;
            states.push((*id, state, closing));
            !closing
        });
        states
    }
}

impl<H: Host> Actor<H> {
    pub(super) fn finalize_catalog_watches(&self) {
        let owners = std::mem::take(&mut *self.driver.control.watch_owners.borrow_mut());
        let states = {
            let mut agent = self.driver.control.agent.borrow_mut();
            owners
                .into_iter()
                .filter_map(|(id, owner)| {
                    agent
                        .close_catalog_watch(owner)
                        .or_else(|| agent.catalog_watch_state(owner))
                        .map(|state| (id, state))
                })
                .collect::<Vec<_>>()
        };
        let notifications = states
            .iter()
            .filter_map(|(id, state)| self.host.prepare_catalog_state(*id, state, true))
            .collect::<Vec<_>>();
        for notification in notifications {
            notification();
        }
    }
}

impl ActorHandle {
    pub(crate) fn open_catalog_watch(
        &self,
        room: String,
        config: WatchConfig,
    ) -> Result<(u64, WatchState), ErrorInfo> {
        if !self.control.accepting.get() || self.sender.is_closed() {
            return Err(ErrorInfo::new(ErrorCode::Closed, "Actor is not running"));
        }
        let (owner, state) = {
            let mut agent = self.control.agent.borrow_mut();
            let owner = agent.catalog_watch(room, config)?;
            let state = agent
                .catalog_watch_state(owner)
                .ok_or_else(|| ErrorInfo::new(ErrorCode::Internal, "Catalog state unavailable"))?;
            (owner, state)
        };
        self.control
            .watch_owners
            .borrow_mut()
            .insert(owner.id(), owner);
        if let Err(error) = self.schedule_core_change() {
            self.close_catalog_watch(owner.id());
            return Err(error);
        }
        Ok((owner.id(), state))
    }

    pub(crate) fn close_catalog_watch(&self, id: u64) -> Option<WatchState> {
        let owner = self.control.watch_owners.borrow_mut().remove(&id)?;
        let state = {
            let mut agent = self.control.agent.borrow_mut();
            agent
                .close_catalog_watch(owner)
                .or_else(|| agent.catalog_watch_state(owner))
        };
        let _ = self.schedule_core_change();
        state
    }

    pub(crate) fn catalog_states(&self) -> Vec<WatchProjection> {
        let agent = self.control.agent.borrow();
        self.control
            .watch_owners
            .borrow()
            .iter()
            .filter_map(|(id, owner)| {
                agent.catalog_watch_state(*owner).map(|state| {
                    let closing = state.status == WatchStatus::Failed;
                    (*id, state, closing)
                })
            })
            .collect()
    }

    pub(super) fn schedule_core_change(&self) -> Result<(), ErrorInfo> {
        if !self.control.wake_pending.replace(true) {
            match self.sender.try_send(Message::CoreChanged) {
                Ok(()) => {}
                Err(MpscTrySendError::Full(_)) => self.control.wake_pending.set(false),
                Err(MpscTrySendError::Closed(_)) => {
                    self.control.wake_pending.set(false);
                    return Err(ErrorInfo::new(ErrorCode::Closed, "Actor is not running"));
                }
            }
        }
        Ok(())
    }
}
