use std::num::NonZeroU64;

use pulsebeam_clock::domain::{Projection, Provenance, Quality};
use pulsebeam_core::fabric::{
    MAJOR, Profile, REGISTRY_REVISION,
    wan::{WINDOW_SPAN, Window},
};
use serde::{Deserialize, Serialize};

use crate::{
    cluster::mesh::Member,
    control::controller::ownership::observations::{EndpointFact, Snapshot},
    entity::{
        ConnectionId, EndpointKey, ProjectId, ProjectRoomKey, PublicationId, PublicationKey, RoomId,
    },
    route::fabric::local::Emission,
    rtp::monitor::MeasuredTargetLadder,
    shard_update::PublisherAnchor,
};

use super::fabric::WanLiveRouteAck;

pub(in crate::control) const WINDOWS_PER_SOURCE: usize = 3;

/// Keep the current position window for every eligible source first. Under
/// pressure, retain an advertised predecessor before prearming a successor.
pub(in crate::control) fn plan_windows(
    mut current: Vec<WanSourceWindow>,
    previous: &[WanSourceWindow],
    limit: usize,
) -> Box<[WanSourceWindow]> {
    current.sort_unstable_by_key(|source| source.scope());
    current.truncate(limit);
    let stable = |candidate: WanSourceWindow| {
        previous
            .iter()
            .find(|&source| {
                source.room == candidate.room
                    && source.origin == candidate.origin
                    && source.publisher == candidate.publisher
                    && source.window_start == candidate.window_start
            })
            .copied()
    };
    for source in &mut current {
        if let Some(previous) = stable(*source) {
            let timing = (previous.sequence_anchor == source.sequence_anchor)
                .then_some(source.timing)
                .flatten();
            *source = WanSourceWindow {
                timing,
                ladder: source.ladder,
                ..previous
            };
        }
    }
    let sources = current.len();
    for index in 0..sources {
        if current.len() >= limit {
            break;
        }
        if let Some(&source) = current.get(index)
            && let Some(window_start) = source.window_start.checked_sub(WINDOW_SPAN)
            && let Some(predecessor) = stable(WanSourceWindow {
                window_start,
                ..source
            })
        {
            current.push(WanSourceWindow {
                timing: None,
                ladder: None,
                ..predecessor
            });
        }
    }
    for index in 0..sources {
        if current.len() >= limit {
            break;
        }
        if let Some(&source) = current.get(index)
            && let Some(window_start) = source.window_start.checked_add(WINDOW_SPAN)
        {
            let successor = WanSourceWindow {
                window_start,
                offer_cut: None,
                ..source
            };
            current.push(WanSourceWindow {
                timing: None,
                ladder: None,
                ..stable(successor).unwrap_or(successor)
            });
        }
    }
    current.sort_unstable_by_key(|source| (source.scope(), source.window_start));
    current.into_boxed_slice()
}

/// Evidence for the exact sequence anchor, not a bound for other samples in its window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::control) struct WanTimingPoint {
    generation: u64,
    global_ns: i64,
    uncertainty_ns: Option<u32>,
    provenance: u8,
    quality: u8,
}

impl WanTimingPoint {
    fn from_projection(projection: Projection) -> Self {
        Self {
            generation: projection.binding.generation.0,
            global_ns: projection.global.as_nanos(),
            uncertainty_ns: projection
                .absolute_uncertainty_ns
                .and_then(|ns| u32::try_from(ns).ok()),
            provenance: match projection.provenance {
                Provenance::ArrivalEstimate => 0,
                Provenance::SenderReport => 1,
                Provenance::DataCapture => 2,
            },
            quality: match projection.quality {
                Quality::Acquiring => 0,
                Quality::Synchronized => 1,
                Quality::Degraded => 2,
                Quality::Holdover => 3,
                Quality::Recovering => 4,
            },
        }
    }

    fn valid(self) -> bool {
        self.provenance <= 2 && self.quality <= 4
    }
}

/// One publisher-owned position window, before the receiver allocates its SSRC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::control) struct WanSourceWindow {
    pub room: ProjectRoomKey,
    pub publisher: PublisherAnchor,
    pub sequence_anchor: u64,
    pub window_start: u64,
    pub timing: Option<WanTimingPoint>,
    pub ladder: Option<MeasuredTargetLadder>,
    // Routing provenance, not the owner or signer of the publication.
    pub origin: Option<Member>,
    pub offer_cut: Option<NonZeroU64>,
}

impl WanSourceWindow {
    pub(in crate::control) fn from_sample(
        room: ProjectRoomKey,
        publisher: PublisherAnchor,
        sequence_anchor: u64,
    ) -> Self {
        Self {
            room,
            publisher,
            sequence_anchor,
            window_start: Window::start_for(sequence_anchor),
            timing: None,
            ladder: None,
            origin: None,
            offer_cut: None,
        }
    }

    pub(in crate::control) fn with_projection(mut self, projection: Option<Projection>) -> Self {
        self.timing = projection
            .filter(|value| value.binding.domain.0 == self.publisher.incarnation.get())
            .map(WanTimingPoint::from_projection);
        self
    }

    pub(in crate::control) fn with_ladder(mut self, ladder: Option<MeasuredTargetLadder>) -> Self {
        self.ladder = ladder.filter(|&ladder| {
            self.publisher.emission.profile() == Profile::Vp9
                && Window::start_for(ladder.through) == self.window_start
                && ladder.valid()
        });
        self
    }

    pub(in crate::control) fn refresh_track_ladder(
        self,
        track: &crate::track::Track,
        expires: pulsebeam_runtime::time::Instant,
        now: pulsebeam_runtime::time::Instant,
    ) {
        // Only the sampled window is authoritative; prearmed windows do not withdraw it.
        if self.publisher.emission.profile() != Profile::Vp9
            || Window::start_for(self.sequence_anchor) != self.window_start
        {
            return;
        }
        let Some(video) = track.as_video() else {
            return;
        };
        let _refreshed = video.refresh_wan_ladder(self.publisher, self.ladder, expires, now);
        #[cfg(feature = "sim")]
        if _refreshed && video.wan_ladder(now).is_some() {
            crate::sim_metrics::record_routing_counter("wan_vp9_ladder_installed");
        }
    }

    pub(in crate::control) fn scope(self) -> super::Key {
        super::Key::new(self.room, self.publisher.publication, false)
    }

    pub(in crate::control) fn forms(self) -> u8 {
        if self.publisher.emission.wire_profile().1 == 0 {
            0
        } else {
            3
        }
    }

    pub(in crate::control) fn window_timing(self) -> Option<crate::route::fabric::WindowTiming> {
        let point = self.timing?;
        let quality = match point.quality {
            0 => Quality::Acquiring,
            1 => Quality::Synchronized,
            2 => Quality::Degraded,
            3 => Quality::Holdover,
            4 => Quality::Recovering,
            _ => return None,
        };
        Some(crate::route::fabric::WindowTiming {
            position: self.sequence_anchor,
            capture: pulsebeam_clock::deadline::Timing {
                instant: pulsebeam_clock::time::GlobalInstant::from_nanos(point.global_ns),
                absolute_uncertainty_ns: point.uncertainty_ns.map(u64::from),
                quality,
            },
        })
    }

    pub(in crate::control) fn record_timing_quality(self) {
        let (quality, provenance, bounded) =
            self.timing
                .map_or(("unknown", "unknown", "false"), |point| {
                    let quality = match point.quality {
                        0 => "acquiring",
                        1 => "synchronized",
                        2 => "degraded",
                        3 => "holdover",
                        4 => "recovering",
                        _ => "unknown",
                    };
                    let provenance = match point.provenance {
                        0 => "arrival",
                        1 => "sender_report",
                        2 => "data_capture",
                        _ => "unknown",
                    };
                    let bounded = if point.uncertainty_ns.is_some() {
                        "true"
                    } else {
                        "false"
                    };
                    (quality, provenance, bounded)
                });
        metrics::counter!(
            "wan_source_timing_observations",
            "quality" => quality,
            "provenance" => provenance,
            "reported_bound" => bounded,
        )
        .increment(1);
    }

    pub(in crate::control) fn same_window(self, other: Self) -> bool {
        self.room == other.room
            && self.publisher == other.publisher
            && self.sequence_anchor == other.sequence_anchor
            && self.window_start == other.window_start
    }

    pub(in crate::control) fn matches_ack(self, ack: WanLiveRouteAck) -> bool {
        self.room == ack.room
            && self.publisher.publication == ack.publication
            && self.publisher.incarnation == ack.incarnation
            && self.sequence_anchor == ack.sequence_anchor
            && self.window_start == ack.window.start()
            && self.offer_cut == ack.offer_cut
            && self.forms() == ack.window.forms()
            && self.publisher.emission.profile() == ack.window.profile()
            && self.publisher.emission.wire_profile().1 == ack.window.emission()
    }

    pub(in crate::control) fn present_in(self, snapshot: &Snapshot) -> bool {
        snapshot
            .iter()
            .any(|(key, endpoint)| self.matches_fact(*key, endpoint))
    }

    pub(in crate::control) fn matches_fact(
        self,
        key: EndpointKey,
        endpoint: &EndpointFact,
    ) -> bool {
        key.room == self.room
            && endpoint.claim.connection() == self.publisher.connection
            && endpoint
                .publications
                .iter()
                .any(|fact| match self.publisher.emission.profile() {
                    Profile::Data => fact
                        .data_publisher(self.publisher.connection)
                        .is_some_and(|data| PublisherAnchor::from(data) == self.publisher),
                    profile => {
                        self.publisher.emission.supports_wan_rtp()
                            && fact.wan_rtp_publication()
                                == Some((self.publisher.publication, profile))
                    }
                })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::control) struct WireWanSourceWindow {
    project: ProjectId,
    room: RoomId,
    publication: PublicationId,
    connection: ConnectionId,
    incarnation: NonZeroU64,
    sequence_anchor: u64,
    window_start: u64,
    profile: u8,
    extensions: u16,
    forms: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    timing: Option<WanTimingPoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ladder: Option<MeasuredTargetLadder>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    origin: Option<Member>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    offer_cut: Option<NonZeroU64>,
}

impl From<WanSourceWindow> for WireWanSourceWindow {
    fn from(source: WanSourceWindow) -> Self {
        let (profile, extensions) = source.publisher.emission.wire_profile();
        Self {
            project: source.room.project,
            room: source.room.room,
            publication: source.publisher.publication.publication_id(),
            connection: source.publisher.connection,
            incarnation: source.publisher.incarnation,
            sequence_anchor: source.sequence_anchor,
            window_start: source.window_start,
            profile,
            extensions,
            forms: source.forms(),
            timing: source.timing,
            ladder: source.ladder,
            origin: source.origin,
            offer_cut: source.offer_cut,
        }
    }
}

impl WireWanSourceWindow {
    pub(in crate::control) fn decode(self) -> Option<WanSourceWindow> {
        let emission =
            Emission::from_wire_profile(self.profile, self.extensions, MAJOR, REGISTRY_REVISION)?;
        let publication = match emission.profile() {
            Profile::Data => PublicationKey::Data(self.publication),
            Profile::Opus => PublicationKey::Audio(self.publication),
            profile if emission.supports_wan_rtp() && profile.is_video() => {
                PublicationKey::Video(self.publication)
            }
            _ => return None,
        };
        let source = WanSourceWindow {
            room: ProjectRoomKey {
                project: self.project,
                room: self.room,
            },
            publisher: PublisherAnchor {
                publication,
                connection: self.connection,
                incarnation: self.incarnation,
                emission,
            },
            sequence_anchor: self.sequence_anchor,
            window_start: self.window_start,
            origin: self.origin,
            offer_cut: self.offer_cut,
            ladder: None,
            timing: self.timing.filter(|point| {
                Window::start_for(self.sequence_anchor) == self.window_start && point.valid()
            }),
        }
        .with_ladder(self.ladder);
        (Window::start_for(self.window_start) == self.window_start && source.forms() == self.forms)
            .then_some(source)
    }
}
