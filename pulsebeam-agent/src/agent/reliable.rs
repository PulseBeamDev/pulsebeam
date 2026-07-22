//! Reliable-mode ARQ layer, built entirely on top of the raw, best-effort
//! pub/sub data channels (`DataPublisher`/`DataSubscriber`). The server has
//! zero awareness of any of this — every byte here is opaque application
//! payload as far as `pulsebeam` is concerned; sequencing, acking, and
//! retransmission are purely a client-side concern layered on top.
//!
//! # Wire format
//!
//! Frames and acks are serialised as proto3 binary using the types defined in
//! `pulsebeam-proto/proto/arq.proto` (`ArqFrame` and `ArqAck`).  The server
//! treats every data-channel payload as opaque bytes and never parses them.
//!
//! # Scoped subscribe is required
//!
//! [`ReliableSubscriber`] must wrap a *scoped* subscribe channel
//! (`AgentDriver::declare_subscribe_topic(topic, Some(publisher_id))`), never
//! the wildcard (all-publishers) form. The ARQ sequence-number space is per
//! `(publisher, topic)`; the wildcard form interleaves multiple publishers'
//! independent sequence spaces into one channel, so "seq 5 from A" becomes
//! indistinguishable from "seq 5 from B". This is exactly why the scoped
//! subscribe grammar exists: it gives the ARQ layer an unambiguous stream to
//! reason about.
//!
//! # Wiring it up
//!
//! Acks flow on an ordinary sibling pub/sub topic named `"{topic}_ack"` —
//! this is just two more plain topic declarations using the same mechanism
//! as any other data topic, with the publish/subscribe roles inverted
//! relative to the data topic itself (each subscriber *publishes* acks;
//! the original publisher *subscribes* to them, unscoped, since every
//! scoped subscriber of `<topic>` shares this one ack topic):
//!
//! ```ignore
//! // Publisher side:
//! let data_pub = /* from AgentEvent::DataPublisherDeclared for "game-sync" */;
//! let ack_sub = /* from AgentEvent::DataSubscriberDeclared for "game-sync_ack" */;
//! driver.declare_publish_topic("game-sync")?;
//! driver.declare_subscribe_topic(&reliable::ack_topic_name("game-sync"), None)?;
//! let reliable_pub = ReliablePublisher::new(data_pub, ack_sub);
//!
//! // Subscriber side, scoped to a specific publisher:
//! let data_sub = /* from AgentEvent::DataSubscriberDeclared for "game-sync" scoped to publisher_id */;
//! let ack_pub = /* from AgentEvent::DataPublisherDeclared for "game-sync_ack" */;
//! driver.declare_subscribe_topic("game-sync", Some(&publisher_id))?;
//! driver.declare_publish_topic(&reliable::ack_topic_name("game-sync"))?;
//! let reliable_sub = ReliableSubscriber::new(data_sub, ack_pub, my_participant_id);
//! ```
//!
//! Both handles (data + ack) arrive asynchronously via `AgentEvent` from the
//! driver's poll loop, in no guaranteed order — the caller collects both
//! before constructing the wrapper.

use crate::agent::handles::{DataPublisher, DataSubscriber};
use crate::agent::mailbox;
use pulsebeam_proto::arq::{ArqAck, ArqFrame};
use pulsebeam_proto::prelude::Message;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Returns the sibling ack-topic name for a reliable-mode data topic.
/// `.` is not a legal topic character (only alnum, `-`, `_`), hence `_ack`.
pub fn ack_topic_name(topic: &str) -> String {
    format!("{topic}_ack")
}

// -- wire helpers -----------------------------------------------------------

fn encode_frame(seq: u64, is_retransmit: bool, payload: Vec<u8>) -> Vec<u8> {
    ArqFrame { seq, is_retransmit, payload }.encode_to_vec()
}

fn decode_frame(raw: &[u8]) -> Option<ArqFrame> {
    ArqFrame::decode(raw).ok()
}

fn encode_ack(ack: &ArqAck) -> Vec<u8> {
    ack.encode_to_vec()
}

fn decode_ack(raw: &[u8]) -> Option<ArqAck> {
    ArqAck::decode(raw).ok()
}

// -- reorder buffer (subscriber side) --------------------------------------

const DEFAULT_REORDER_CAP: usize = 256;
const DEFAULT_RETRANSMIT_CAP: usize = 512;
const ACK_EVERY_N_FRAMES: u32 = 16;
const ACK_TIMER_INTERVAL: Duration = Duration::from_millis(100);
const MAX_ACK_MISSING: usize = 32;

struct ReorderBuffer {
    next_expected: u64,
    buf: BTreeMap<u64, Vec<u8>>,
    cap: usize,
}

impl ReorderBuffer {
    fn new(cap: usize) -> Self {
        Self { next_expected: 0, buf: BTreeMap::new(), cap }
    }

    /// Feeds one decoded frame; returns any now-contiguous payloads to
    /// deliver, in order.
    fn ingest(&mut self, seq: u64, payload: Vec<u8>) -> Vec<Vec<u8>> {
        let mut delivered = Vec::new();
        if seq < self.next_expected {
            return delivered; // duplicate/already-delivered, drop
        }
        if seq == self.next_expected {
            delivered.push(payload);
            self.next_expected += 1;
        } else {
            self.buf.entry(seq).or_insert(payload);
        }
        while let Some(next) = self.buf.remove(&self.next_expected) {
            delivered.push(next);
            self.next_expected += 1;
        }

        if self.buf.len() > self.cap {
            // The gap never closed and the buffer is over budget: skip ahead
            // to the oldest buffered seq, permanently losing the
            // unrecoverable range, rather than growing without bound.
            if let Some(&oldest) = self.buf.keys().next() {
                self.next_expected = oldest;
                while let Some(next) = self.buf.remove(&self.next_expected) {
                    delivered.push(next);
                    self.next_expected += 1;
                }
            }
        }

        delivered
    }

    /// Seq numbers known to be missing between `next_expected` and the
    /// highest buffered seq, capped at `limit` entries.
    fn missing(&self, limit: usize) -> Vec<u64> {
        let mut out = Vec::new();
        if let Some(&max_seq) = self.buf.keys().next_back() {
            for s in self.next_expected..max_seq {
                if !self.buf.contains_key(&s) {
                    out.push(s);
                    if out.len() >= limit {
                        break;
                    }
                }
            }
        }
        out
    }
}

// -- retransmit buffer (publisher side) ------------------------------------

struct RetransmitBuffer {
    buf: BTreeMap<u64, Vec<u8>>,
    cap: usize,
    /// Per-subscriber next_expected watermarks (exclusive upper bound).
    watermarks: HashMap<String, u64>,
}

impl RetransmitBuffer {
    fn new(cap: usize) -> Self {
        Self { buf: BTreeMap::new(), cap, watermarks: HashMap::new() }
    }

    fn insert(&mut self, seq: u64, payload: Vec<u8>) {
        self.buf.insert(seq, payload);
        // Hard cap so one silent/disconnected subscriber can't pin the
        // buffer forever.
        while self.buf.len() > self.cap {
            let Some(&oldest) = self.buf.keys().next() else { break };
            self.buf.remove(&oldest);
        }
    }

    /// Records a subscriber's `next_expected` watermark and evicts all seqs
    /// strictly below the minimum watermark seen so far.
    ///
    /// `next_expected` is an *exclusive* upper bound ("I expect seq N next"),
    /// so seqs < N have been delivered and can be dropped from the buffer.
    /// A value of 0 means nothing has been delivered yet — nothing is evicted.
    ///
    /// Eviction is progressive per-ack: a subscriber that acks early with a
    /// high watermark can evict seqs a not-yet-heard-from subscriber will
    /// later need.  That subscriber simply can't recover those seqs — the
    /// same documented limitation as the hard size cap above.
    fn on_ack(&mut self, subscriber_id: String, next_expected: u64) {
        self.watermarks.insert(subscriber_id, next_expected);
        if let Some(&min_watermark) = self.watermarks.values().min() {
            // Retain everything at or above min_watermark.
            // min_watermark == 0 → retain all (nothing delivered yet).
            self.buf.retain(|&seq, _| seq >= min_watermark);
        }
    }

    fn get(&self, seq: u64) -> Option<&[u8]> {
        self.buf.get(&seq).map(Vec::as_slice)
    }
}

// -- public API ------------------------------------------------------------

struct PublisherState {
    next_seq: u64,
    retransmit: RetransmitBuffer,
}

/// Send-only reliable wrapper around a raw [`DataPublisher`]. Delivers each
/// `send`ed payload with a monotonic per-instance sequence number and
/// retransmits on request from subscribers' SACKs.
pub struct ReliablePublisher {
    publisher: DataPublisher,
    state: Arc<Mutex<PublisherState>>,
    _ack_task: tokio::task::JoinHandle<()>,
}

impl ReliablePublisher {
    /// `publisher` is the raw data-topic publish handle; `ack_subscriber` is
    /// the raw (unscoped) subscribe handle for that topic's `_ack` sibling.
    pub fn new(publisher: DataPublisher, ack_subscriber: DataSubscriber) -> Self {
        let state = Arc::new(Mutex::new(PublisherState {
            next_seq: 0,
            retransmit: RetransmitBuffer::new(DEFAULT_RETRANSMIT_CAP),
        }));

        let task_state = state.clone();
        let resend_publisher = publisher.clone();
        let mut ack_subscriber = ack_subscriber;
        let ack_task = tokio::spawn(async move {
            loop {
                let Ok(raw) = ack_subscriber.recv().await else { return };
                let Some(ack) = decode_ack(&raw) else { continue };

                let to_resend = {
                    let mut state = task_state.lock().unwrap();
                    state.retransmit.on_ack(ack.subscriber_id, ack.next_expected);
                    let mut resend: Vec<(u64, Vec<u8>)> = ack
                        .missing
                        .into_iter()
                        .filter_map(|seq| {
                            state.retransmit.get(seq).map(|p| (seq, p.to_vec()))
                        })
                        .collect();
                    // Tail-packet recovery: if the missing list is empty but the
                    // subscriber hasn't caught up to our next_seq, the last
                    // unacked frame was lost and can't appear in missing() (the
                    // reorder buffer has nothing buffered above it to expose the
                    // gap). Retransmit next_expected directly so the subscriber
                    // isn't stuck waiting forever.
                    if resend.is_empty() {
                        let sub_next = ack.next_expected;
                        if sub_next < state.next_seq {
                            if let Some(p) = state.retransmit.get(sub_next) {
                                resend.push((sub_next, p.to_vec()));
                            }
                        }
                    }
                    resend
                };
                for (seq, payload) in to_resend {
                    let _ =
                        resend_publisher.try_send(encode_frame(seq, true, payload));
                }
            }
        });

        Self { publisher, state, _ack_task: ack_task }
    }

    pub async fn send(&self, payload: Vec<u8>) -> Result<(), mailbox::SendError<Vec<u8>>> {
        let frame = {
            let mut state = self.state.lock().unwrap();
            let seq = state.next_seq;
            state.next_seq += 1;
            state.retransmit.insert(seq, payload.clone());
            encode_frame(seq, false, payload.clone())
        };
        self.publisher.send(frame).await.map_err(|_| mailbox::SendError(payload))
    }
}

/// Receive-only reliable wrapper around a raw, **scoped** [`DataSubscriber`].
/// Delivers payloads fully reordered and gap-recovered via retransmit.
pub struct ReliableSubscriber {
    rx: mailbox::Receiver<Vec<u8>>,
    _task: tokio::task::JoinHandle<()>,
}

impl ReliableSubscriber {
    /// `subscriber` must be scoped to exactly one publisher (see module
    /// docs); `ack_publisher` is the raw publish handle for that topic's
    /// `_ack` sibling; `local_id` is this agent's own participant id, used
    /// to stamp outgoing acks so the publisher can demux multiple acking
    /// subscribers sharing one ack topic.
    pub fn new(
        subscriber: DataSubscriber,
        ack_publisher: DataPublisher,
        local_id: String,
    ) -> Self {
        debug_assert!(
            subscriber.scope.is_some(),
            "ReliableSubscriber requires a scoped subscribe channel: the ARQ \
             sequence-number space is per-(publisher, topic), and a wildcard \
             subscribe interleaves multiple publishers' independent sequence \
             spaces into one indistinguishable stream"
        );

        let (tx, rx) = mailbox::bounded(64);
        let mut subscriber = subscriber;
        let task = tokio::spawn(async move {
            let mut reorder = ReorderBuffer::new(DEFAULT_REORDER_CAP);
            let mut since_last_ack: u32 = 0;
            let mut ack_timer = tokio::time::interval(ACK_TIMER_INTERVAL);
            ack_timer.tick().await; // consume immediate first tick

            loop {
                tokio::select! {
                    frame = subscriber.recv() => {
                        let Ok(raw) = frame else { return };
                        let Some(f) = decode_frame(&raw) else { continue };
                        for delivered in reorder.ingest(f.seq, f.payload) {
                            if tx.send(delivered).await.is_err() {
                                return;
                            }
                        }
                        since_last_ack += 1;
                        if since_last_ack >= ACK_EVERY_N_FRAMES {
                            since_last_ack = 0;
                            send_ack(&ack_publisher, &local_id, &reorder);
                        }
                    }
                    _ = ack_timer.tick() => {
                        // Always ack on the timer, even when since_last_ack == 0.
                        // This handles tail-packet loss: if the last seq in the
                        // stream was dropped and nothing higher is buffered,
                        // missing() returns [] and the subscriber has no other
                        // way to solicit a retransmit without this periodic ack.
                        since_last_ack = 0;
                        send_ack(&ack_publisher, &local_id, &reorder);
                    }
                }
            }
        });

        Self { rx, _task: task }
    }

    pub async fn recv(&mut self) -> Result<Vec<u8>, mailbox::RecvError> {
        self.rx.recv().await
    }
}

fn send_ack(ack_publisher: &DataPublisher, subscriber_id: &str, reorder: &ReorderBuffer) {
    let ack = ArqAck {
        subscriber_id: subscriber_id.to_string(),
        // next_expected (exclusive upper bound): "I expect seq N next", meaning
        // seqs 0..N-1 have been delivered.  Using next_expected directly avoids
        // the saturating_sub(1) ambiguity where 0 would wrongly signal seq 0 as
        // already delivered when nothing has been received yet.
        next_expected: reorder.next_expected,
        missing: reorder.missing(MAX_ACK_MISSING),
    };
    let _ = ack_publisher.try_send(encode_ack(&ack));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trip() {
        let frame = encode_frame(42, false, b"hello".to_vec());
        let f = decode_frame(&frame).unwrap();
        assert_eq!(f.seq, 42);
        assert!(!f.is_retransmit);
        assert_eq!(f.payload, b"hello");

        let frame = encode_frame(7, true, b"world".to_vec());
        let f = decode_frame(&frame).unwrap();
        assert_eq!(f.seq, 7);
        assert!(f.is_retransmit);
        assert_eq!(f.payload, b"world");
    }

    #[test]
    fn decode_frame_rejects_garbage() {
        assert!(decode_frame(&[0xFF, 0x00, 0xAB]).is_none());
    }

    #[test]
    fn ack_round_trip() {
        let ack = ArqAck {
            subscriber_id: "pa_TESTSUBSCRIBERID0000000000".to_string(),
            next_expected: 101,
            missing: vec![101, 103, 105],
        };
        let raw = encode_ack(&ack);
        let decoded = decode_ack(&raw).unwrap();
        assert_eq!(decoded.subscriber_id, ack.subscriber_id);
        assert_eq!(decoded.next_expected, 101);
        assert_eq!(decoded.missing, vec![101, 103, 105]);
    }

    #[test]
    fn ack_missing_list_is_capped_by_caller() {
        // The cap is enforced by the caller (send_ack passes MAX_ACK_MISSING to
        // missing()); proto itself has no field-level size limit, so test that
        // a large list round-trips correctly.
        let missing: Vec<u64> = (0..64).collect();
        let ack = ArqAck {
            subscriber_id: "pa_X".to_string(),
            next_expected: 0,
            missing: missing.clone(),
        };
        let raw = encode_ack(&ack);
        let decoded = decode_ack(&raw).unwrap();
        assert_eq!(decoded.missing.len(), 64);
    }

    #[test]
    fn reorder_buffer_delivers_in_order_despite_scrambling() {
        let mut buf = ReorderBuffer::new(16);
        let script = [0u64, 1, 3, 2, 5, 4];
        let mut delivered = Vec::new();
        for &seq in &script {
            delivered.extend(buf.ingest(seq, seq.to_be_bytes().to_vec()));
        }
        let expected: Vec<Vec<u8>> = (0u64..6).map(|s| s.to_be_bytes().to_vec()).collect();
        assert_eq!(delivered, expected);
    }

    #[test]
    fn reorder_buffer_drops_duplicates() {
        let mut buf = ReorderBuffer::new(16);
        assert_eq!(buf.ingest(0, vec![0]).len(), 1);
        assert_eq!(buf.ingest(0, vec![0]).len(), 0); // duplicate, dropped
        assert_eq!(buf.ingest(1, vec![1]).len(), 1);
    }

    #[test]
    fn reorder_buffer_skips_ahead_when_gap_never_closes() {
        let mut buf = ReorderBuffer::new(4);
        for seq in 1..=6u64 {
            buf.ingest(seq, vec![seq as u8]);
        }
        assert!(buf.next_expected > 0);
        assert!(buf.buf.len() <= 4);
    }

    #[test]
    fn retransmit_buffer_evicts_below_min_watermark() {
        let mut buf = RetransmitBuffer::new(16);
        for seq in 0..10u64 {
            buf.insert(seq, vec![seq as u8]);
        }
        // Watermarks are next_expected (exclusive upper bound): 6 means seqs
        // 0-5 delivered, evict them.
        buf.on_ack("sub-a".to_string(), 6);
        assert!(buf.get(5).is_none()); // seq 5 evicted (< 6)
        assert!(buf.get(6).is_some()); // seq 6 kept (>= 6)

        buf.on_ack("sub-b".to_string(), 4);
        // min_watermark = min(6, 4) = 4; seqs 0-5 already gone, 6..9 are >= 4.
        assert!(buf.get(6).is_some());
        assert!(buf.get(9).is_some());
    }

    #[test]
    fn retransmit_buffer_caps_size_regardless_of_acks() {
        let mut buf = RetransmitBuffer::new(4);
        for seq in 0..10u64 {
            buf.insert(seq, vec![seq as u8]);
        }
        assert_eq!(buf.buf.len(), 4);
        assert!(buf.get(9).is_some());
        assert!(buf.get(0).is_none());
    }
}
