slotmap::new_key_type! {
    pub struct ParticipantHandle;
    pub struct TrackHandle;
    pub struct DownstreamSlotKey;
}

#[cfg(test)]
mod tests {
    use super::{ParticipantHandle, TrackHandle};
    use slotmap::{Key, SlotMap};

    #[test]
    fn participant_handles_are_arena_local() {
        let left = SlotMap::<ParticipantHandle, ()>::with_key().insert(());
        let right = SlotMap::<ParticipantHandle, ()>::with_key().insert(());

        assert_eq!(
            left, right,
            "a handle alone cannot identify its owning shard"
        );
    }

    #[test]
    fn reused_participant_slots_change_handle_generation() {
        let mut participants = SlotMap::<ParticipantHandle, ()>::with_key();
        let retired = participants.insert(());
        assert_eq!(participants.remove(retired), Some(()));
        let replacement = participants.insert(());

        assert_ne!(retired, replacement);
        assert_eq!(
            retired.data().as_ffi() & u64::from(u32::MAX),
            replacement.data().as_ffi() & u64::from(u32::MAX),
            "slot reuse should preserve density while the generation fences stale handles"
        );
    }

    #[test]
    fn track_handles_are_arena_local() {
        let left = SlotMap::<TrackHandle, ()>::with_key().insert(());
        let right = SlotMap::<TrackHandle, ()>::with_key().insert(());

        assert_eq!(
            left, right,
            "a handle alone cannot identify its owning shard"
        );
    }

    #[test]
    fn reused_track_slots_change_handle_generation() {
        let mut tracks = SlotMap::<TrackHandle, ()>::with_key();
        let retired = tracks.insert(());
        assert_eq!(tracks.remove(retired), Some(()));
        let replacement = tracks.insert(());

        assert_ne!(retired, replacement);
        assert_eq!(
            retired.data().as_ffi() & u64::from(u32::MAX),
            replacement.data().as_ffi() & u64::from(u32::MAX),
            "slot reuse should preserve density while the generation fences stale handles"
        );
    }
}
