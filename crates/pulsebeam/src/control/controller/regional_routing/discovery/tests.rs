use std::num::{NonZeroU64, NonZeroUsize};

use pulsebeam_link::StreamControl;
use pulsebeam_runtime::time::sleep;

use crate::{
    clock::reference::{ClockService, Source},
    cluster::{REGIONAL_FRAME_BYTES, admission::fixture},
};

use super::{super::Border, *};

const EPOCH: [u8; 16] = [11; 16];

struct Node {
    discovery: Discovery,
    journal: fixture::Journal,
    tree: RegionTree,
    borders: BTreeMap<Member, Border>,
    epoch: Instant,
}

impl Node {
    fn new(
        tree: &RegionTree,
        journal: fixture::Journal,
        port: DiscoveryPort,
        local: Member,
        key: ApiSigningKey,
        direct: &[NodeId],
        clock: Snapshot,
    ) -> anyhow::Result<Self> {
        let (_, clocks) = pulsebeam_queue::bounded(NonZeroUsize::MIN);
        let mut tree = tree.clone();
        tree.begin_boot_admission(local)?;
        Ok(Self {
            discovery: Discovery::new(
                local,
                Scope {
                    epoch: EPOCH,
                    roster: *tree.fingerprint(),
                },
                key,
                direct.iter().copied().collect(),
                clock,
                clocks,
                port,
            ),
            journal,
            tree,
            borders: BTreeMap::new(),
            epoch: Instant::now(),
        })
    }

    fn replace_lane(&mut self, peer: Member) -> StreamControl {
        self.borders.retain(|member, _| member.node != peer.node);
        assert!(self.tree.admit_boot(peer));
        let (control, stream) = StreamControl::new_authenticated(
            const { NonZeroUsize::new(REGIONAL_FRAME_BYTES).unwrap() },
        );
        let mut border = Border::default();
        border.install_lane(control);
        self.borders.insert(peer, border);
        stream
    }

    fn drive(&mut self) -> anyhow::Result<()> {
        let now = Instant::now();
        let mut inbox = VecDeque::new();
        for (&member, border) in &mut self.borders {
            let Some(stream) = NonZeroU64::new(border.revision) else {
                continue;
            };
            if let Some(lane) = &mut border.lane {
                while let Some(bytes) = lane.try_recv() {
                    inbox.push_back((Link { member, stream }, bytes));
                }
            }
        }
        let links = Connected {
            epoch: self.epoch,
            now,
            mesh: None,
            peers: &[],
            borders: &self.borders,
        };
        for change in self.discovery.drive(&self.tree, &links, &mut inbox, now) {
            match change {
                Change::Admit(member) => assert!(self.tree.admit_boot(member)),
                Change::Withdraw(member) => self.tree.withdraw_boot(member),
            }
        }
        self.journal.drive()
    }
}

async fn cross(from: &mut StreamControl, to: &mut StreamControl) {
    while let Some(Some((frame, _))) =
        futures_lite::future::poll_once(from.directions().0.recv()).await
    {
        assert!(to.directions().1.send(frame).await.is_ok());
    }
}

async fn step(
    nodes: [&mut Node; 3],
    lines: [&mut (StreamControl, StreamControl); 2],
) -> anyhow::Result<()> {
    for node in nodes {
        node.drive()?;
    }
    for (a, b) in lines {
        cross(a, b).await;
        cross(b, a).await;
    }
    sleep(Duration::from_millis(30)).await;
    Ok(())
}

#[pulsebeam_runtime::test(start_paused = true, crate = "pulsebeam_runtime::__harness")]
async fn learns_a_non_neighbor_through_borders_and_fences_its_prior_boot() -> anyhow::Result<()> {
    let clock = ClockService::start(Source::Virtual {
        unix_nanos: || 1_700_000_000_000_000_000,
    })
    .await
    .unwrap();
    let states = [
        fixture::State::new()?,
        fixture::State::new()?,
        fixture::State::new()?,
    ];
    let [west, hub, east] = states.each_ref().map(fixture::State::node);
    let keys = [12, 13, 14].map(|seed| ApiSigningKey::from_seed([seed; 32]));
    let member = |node| Member {
        node,
        generation: NonZeroU64::MIN,
    };
    let entry = |node, key: &ApiSigningKey, catalog: bool| {
        serde_json::json!({
            "node": node, "generation": 1, "role": "gateway", "boot_key": key.public_key(), "catalog": catalog,
        })
    };
    let policy: RegionTree = serde_json::from_value(serde_json::json!({
        "regions": [
            {"name":"hub", "members":[entry(hub, &keys[1], false)]},
            {"name":"west", "parent":"hub", "members":[entry(west, &keys[0], false)]},
            {"name":"east", "parent":"hub", "members":[entry(east, &keys[2], true)]}
        ],
        "borders": [{"child":west, "parent":hub}, {"child":east, "parent":hub}]
    }))
    .unwrap();
    let build = |index: usize, local, direct: &[NodeId], discovered: &[NodeId]| {
        let (journal, port) = states[index].discovery(EPOCH, direct, discovered)?;
        Node::new(
            &policy,
            journal,
            port,
            member(local),
            keys[index].clone(),
            direct,
            clock.snapshot(),
        )
    };
    let mut a = build(0, west, &[hub], &[east])?;
    let mut b = build(1, hub, &[west, east], &[])?;
    let mut c = build(2, east, &[hub], &[west])?;
    let mut ab = (a.replace_lane(member(hub)), b.replace_lane(member(west)));
    let mut bc = (b.replace_lane(member(east)), c.replace_lane(member(hub)));
    assert_eq!(a.tree.catalog_members().count(), 0);
    assert_eq!(
        a.tree.catalog_coverage_members().collect::<Vec<_>>(),
        [member(east)]
    );
    for _ in 0..10 {
        step([&mut a, &mut b, &mut c], [&mut ab, &mut bc]).await?;
    }
    assert_eq!(a.tree.catalog_members().collect::<Vec<_>>(), [member(east)]);
    assert!(
        a.tree.media_key(member(east)).is_none(),
        "discovery alone grants no media key"
    );
    assert_eq!(
        a.borders.len(),
        1,
        "discovery never installs a WAN association"
    );
    // Reconnect the same physical slot with a higher, independently signed boot.
    let reboot = Member {
        generation: NonZeroU64::new(2).unwrap(),
        ..member(east)
    };
    let Discovery { port, .. } = c.discovery;
    c = Node::new(
        &policy,
        c.journal,
        port,
        reboot,
        keys[2].clone(),
        &[hub],
        clock.snapshot(),
    )?;
    bc = (b.replace_lane(reboot), c.replace_lane(member(hub)));
    for _ in 0..10 {
        step([&mut a, &mut b, &mut c], [&mut ab, &mut bc]).await?;
    }
    assert_eq!(a.tree.catalog_members().collect::<Vec<_>>(), [reboot]);
    assert_eq!(a.tree.region(member(east)), None);
    assert_eq!(a.tree.region(member(hub)), Some("hub"));

    // Discovery fails closed without an attested clock, independent of WAN pins.
    sleep(Duration::from_secs(4)).await;
    a.drive()?;
    assert_eq!(a.tree.catalog_members().count(), 0);
    assert_eq!(a.tree.catalog_coverage_members().count(), 1);
    assert_eq!(a.tree.region(member(hub)), Some("hub"));
    Ok(())
}
