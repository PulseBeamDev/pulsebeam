mod binding;

#[cfg(test)]
pub(crate) use binding::tests::fixture as binding_fixture;

use alloc::{collections::BTreeMap, format, string::String, vec::Vec};

use crate::{
    MediaSlot,
    client_sdp::{Bindings, Context, Encoding, Error, MediaKey, MediaOwner, Preparation},
};

pub(crate) fn bootstrap_media_key(slot: &MediaSlot) -> MediaKey {
    MediaKey(match slot {
        MediaSlot::LocalVideo(name) => format!("local-video:{name}"),
        MediaSlot::LocalAudio(name) => format!("local-audio:{name}"),
        MediaSlot::RemoteVideo(index) => format!("remote-video:{index}"),
        MediaSlot::RemoteAudio(index) => format!("remote-audio:{index}"),
    })
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PreparedRtp {
    pub binding: pulsebeam_proto::state::TrackBinding,
    // Local execution controls are not transport coordinates in the wire schema.
    pub encodings: Vec<Encoding>,
}

pub(crate) type PreparedMedia = BTreeMap<MediaSlot, Result<PreparedRtp, Error>>;

pub(crate) fn prepare_bootstrap_bindings(
    bindings: Bindings,
    slots: &[crate::SlotBinding],
    capabilities: &pulsebeam_proto::state::RtpCapabilities,
) -> Result<PreparedMedia, Error> {
    pulsebeam_proto::state::validation::validate_capabilities(capabilities)
        .map_err(|_| Error::Envelope("capabilities".into()))?;
    let count = slots.len();
    let slots = slots
        .iter()
        .map(|slot| (slot.mid.as_str(), &slot.slot))
        .collect::<BTreeMap<_, _>>();
    if slots.len() != count {
        return Err(Error::Observation);
    }
    let mut prepared = BTreeMap::new();
    for (publication, bindings) in [(true, bindings.publications), (false, bindings.receptions)] {
        for binding in bindings {
            let slot = slots.get(binding.mid.as_str()).ok_or(Error::Observation)?;
            let local = matches!(slot, MediaSlot::LocalAudio(_) | MediaSlot::LocalVideo(_));
            let kind = match slot.kind() {
                crate::MediaKind::Audio => crate::client_sdp::Kind::Audio,
                crate::MediaKind::Video => crate::client_sdp::Kind::Video,
            };
            if local != publication
                || binding.kind != kind
                || binding.key != bootstrap_media_key(slot)
                || prepared.contains_key(*slot)
            {
                return Err(Error::Observation);
            }
            prepared.insert(
                (*slot).clone(),
                binding::prepared_rtp_binding(&binding, capabilities, publication).map(
                    |transport| PreparedRtp {
                        binding: transport,
                        encodings: binding.encodings,
                    },
                ),
            );
        }
    }
    Ok(prepared)
}

pub fn adopt_bootstrap_media(
    context: &mut Context,
    slots: &[(MediaSlot, String)],
) -> Result<Bindings, Error> {
    let mut requests = Vec::new();
    for (slot, mid) in slots {
        let Some(payload_type) = context.bootstrap_payload_type(mid)? else {
            continue;
        };
        let publication = matches!(slot, MediaSlot::LocalVideo(_) | MediaSlot::LocalAudio(_));
        let owner = MediaOwner {
            key: bootstrap_media_key(slot),
            group: None,
        };
        requests.push(Preparation {
            mid: mid.clone(),
            payload_type,
            publication: publication.then(|| owner.clone()),
            reception: (!publication).then_some(owner),
        });
    }
    context.adopt_bootstrap_media(&requests).cloned()
}
