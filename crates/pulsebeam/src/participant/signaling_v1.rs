use ahash::{HashMap, HashMapExt, HashSet, HashSetExt};
use pulsebeam_proto::signaling_v1::{LocalTrack, SendIntent, TrackKind as WireTrackKind};

use crate::entity::TrackKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SendBindingError {
    RelabeledSender,
    MovedLabel,
}

#[derive(Default)]
pub(crate) struct SenderBindings {
    by_index: HashMap<u32, (TrackKind, String)>,
    by_label: HashMap<(TrackKind, String), u32>,
}

pub(crate) struct SendPlan {
    pub(crate) tracks: Vec<LocalTrack>,
    bindings: SenderBindings,
}

impl SenderBindings {
    /// Reconcile the entire send section without mutating established bindings.
    /// Only a successfully accepted complete Intent may commit the returned plan.
    pub(crate) fn plan(
        &self,
        intent: Option<&SendIntent>,
        negotiated: &HashMap<u32, TrackKind>,
    ) -> Result<SendPlan, SendBindingError> {
        let mut bindings = Self {
            by_index: self.by_index.clone(),
            by_label: self.by_label.clone(),
        };
        let mut tracks = Vec::new();
        let mut used_indices = HashSet::new();
        let mut used_labels = HashSet::new();
        for track in intent.into_iter().flat_map(|send| &send.tracks) {
            let kind = match WireTrackKind::try_from(track.kind) {
                Ok(WireTrackKind::Audio) => TrackKind::Audio,
                Ok(WireTrackKind::Video) => TrackKind::Video,
                _ => continue,
            };
            if track.label.is_empty()
                || track.label.len() > 64
                || negotiated.get(&track.sender_index) != Some(&kind)
            {
                continue;
            }
            let key = (kind, track.label.clone());
            if used_indices.contains(&track.sender_index) || used_labels.contains(&key) {
                continue;
            }
            if bindings
                .by_index
                .get(&track.sender_index)
                .is_some_and(|previous| previous != &key)
            {
                return Err(SendBindingError::RelabeledSender);
            }
            if bindings
                .by_label
                .get(&key)
                .is_some_and(|previous| previous != &track.sender_index)
            {
                return Err(SendBindingError::MovedLabel);
            }
            used_indices.insert(track.sender_index);
            used_labels.insert(key.clone());
            bindings.by_index.insert(track.sender_index, key.clone());
            bindings.by_label.insert(key, track.sender_index);
            tracks.push(track.clone());
        }
        Ok(SendPlan { tracks, bindings })
    }

    pub(crate) fn commit(&mut self, plan: SendPlan) -> Vec<LocalTrack> {
        *self = plan.bindings;
        plan.tracks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn media() -> HashMap<u32, TrackKind> {
        HashMap::from_iter([
            (0, TrackKind::Audio),
            (2, TrackKind::Video),
            (3, TrackKind::Audio),
        ])
    }

    fn track(sender_index: u32, kind: WireTrackKind, label: &str) -> LocalTrack {
        LocalTrack {
            sender_index,
            kind: kind.into(),
            label: label.to_owned(),
        }
    }

    #[test]
    fn first_usable_entries_win_and_omission_preserves_binding() {
        let mut state = SenderBindings::default();
        let intent = SendIntent {
            tracks: vec![
                track(0, WireTrackKind::Video, "wrong-kind"),
                track(0, WireTrackKind::Audio, "mic"),
                track(0, WireTrackKind::Audio, "later"),
                track(3, WireTrackKind::Audio, "mic"),
                track(2, WireTrackKind::Video, "camera"),
            ],
        };
        let plan = state.plan(Some(&intent), &media()).unwrap();
        assert_eq!(state.commit(plan).len(), 2);
        assert!(state.commit(state.plan(None, &media()).unwrap()).is_empty());
        let restored = state.plan(Some(&intent), &media()).unwrap();
        assert_eq!(restored.tracks.len(), 2);
    }

    #[test]
    fn established_relabel_or_rebind_is_fatal_without_partial_commit() {
        let mut state = SenderBindings::default();
        let first = SendIntent {
            tracks: vec![track(0, WireTrackKind::Audio, "mic")],
        };
        let plan = state.plan(Some(&first), &media()).unwrap();
        state.commit(plan);
        let relabel = SendIntent {
            tracks: vec![
                track(2, WireTrackKind::Video, "camera"),
                track(0, WireTrackKind::Audio, "other"),
            ],
        };
        assert!(matches!(
            state.plan(Some(&relabel), &media()),
            Err(SendBindingError::RelabeledSender)
        ));
        let moved = SendIntent {
            tracks: vec![track(3, WireTrackKind::Audio, "mic")],
        };
        assert!(matches!(
            state.plan(Some(&moved), &media()),
            Err(SendBindingError::MovedLabel)
        ));
        assert_eq!(state.plan(Some(&first), &media()).unwrap().tracks.len(), 1);
    }

    #[test]
    fn invalid_labels_and_unknown_indices_are_skipped() {
        let intent = SendIntent {
            tracks: vec![
                track(0, WireTrackKind::Audio, ""),
                track(0, WireTrackKind::Audio, &"é".repeat(33)),
                track(9, WireTrackKind::Audio, "missing"),
                track(0, WireTrackKind::Audio, "valid"),
            ],
        };
        assert_eq!(
            SenderBindings::default()
                .plan(Some(&intent), &media())
                .unwrap()
                .tracks
                .len(),
            1
        );
    }
}
