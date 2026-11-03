use super::*;

struct Sender {
    encodings: Vec<Encoding>,
    cname: String,
}

struct SenderIds<'a> {
    history: &'a IdentityHistory,
    next: u32,
    blocked: BTreeSet<u32>,
    owners: BTreeMap<u32, (String, bool)>,
}

impl SenderIds<'_> {
    fn reserve(&mut self, id: u32, mid: &str, repair: bool) -> Result<(), Error> {
        if self
            .owners
            .get(&id)
            .is_some_and(|(owner, role)| owner != mid || *role != repair)
        {
            return Err(Error::Collision);
        }
        self.owners
            .entry(id)
            .or_insert_with(|| (mid.into(), repair));
        Ok(())
    }

    fn allocate(&mut self, mid: &str, repair: bool) -> Result<u32, Error> {
        let probes = self
            .history
            .len()
            .saturating_add(self.blocked.len())
            .saturating_add(self.owners.len())
            .saturating_add(2);
        for _ in 0..probes {
            self.next = self.next.wrapping_add(1);
            let id = self.next;
            if id != 0
                && !self.history.contains(id)
                && !self.blocked.contains(&id)
                && !self.owners.contains_key(&id)
            {
                self.reserve(id, mid, repair)?;
                return Ok(id);
            }
        }
        Err(Error::Capacity)
    }
}

impl Context {
    pub(crate) fn prepare_sender_modes(
        &mut self,
        candidate: &mut Candidate,
        mid: &str,
        modes: &[Option<String>],
    ) -> Result<(), Error> {
        let (_, pending) = self
            .pending
            .as_mut()
            .filter(|(id, _)| *id == candidate.id())
            .ok_or(Error::CandidateState)?;
        let mut binding = pending
            .bindings
            .publications
            .iter()
            .find(|binding| binding.mid == mid)
            .cloned()
            .ok_or(Error::Observation)?;
        if modes.len() != binding.encodings.len()
            || modes.iter().flatten().any(|mode| !sdp::token(mode))
        {
            return Err(Error::Observation);
        }
        for (encoding, mode) in binding.encodings.iter_mut().zip(modes) {
            encoding.runtime.scalability_mode.clone_from(mode);
        }
        for target in pending
            .bindings
            .publications
            .iter_mut()
            .chain(&mut pending.retained.publications)
            .chain(&mut candidate.bindings.publications)
            .filter(|target| target.mid == mid)
        {
            target.encodings.clone_from(&binding.encodings);
        }
        Ok(())
    }

    pub(crate) fn prepare_local_offer(&mut self, offer: &str) -> Result<String, Error> {
        if self.pending.is_some() {
            return Err(Error::CandidateState);
        }
        let document = Document::parse(offer)?;
        if !active_transport(&document)?.same_peer(&self.local_transport) {
            return Err(Error::TransportChange);
        }
        let cname = self
            .accepted
            .bindings
            .publications
            .iter()
            .chain(&self.accepted.retained.publications)
            .find_map(|binding| binding.rtcp.cname.clone())
            .or_else(|| {
                document
                    .media
                    .iter()
                    .filter(|media| media.section.direction.sends() && !media.section.stopped)
                    .find_map(|media| media.section.rtcp.cname.clone())
            })
            .unwrap_or_else(|| format!("pulsebeam-{:08x}", self.next_identity));
        let mut ids = SenderIds {
            history: &self.identities,
            next: self.next_identity,
            blocked: document
                .media
                .iter()
                .flat_map(|media| &media.signaled)
                .flat_map(|(primary, repair)| [Some(*primary), *repair])
                .flatten()
                .collect(),
            owners: BTreeMap::new(),
        };
        let mut senders = BTreeMap::new();
        for media in &document.media {
            let section = &media.section;
            if section.kind == Kind::Application || section.stopped || !section.direction.sends() {
                continue;
            }
            let previous = self
                .accepted
                .bindings
                .publications
                .iter()
                .chain(&self.accepted.retained.publications)
                .find(|binding| binding.mid == section.mid && !self.stopped(&section.mid));
            if previous.is_some_and(|binding| {
                !section.encodings.is_empty() && binding.encodings.len() != section.encodings.len()
            }) {
                return Err(Error::Envelope(section.mid.clone()));
            }
            let mut encodings = previous
                .map(|binding| binding.encodings.clone())
                .unwrap_or_else(|| section.encodings.clone());
            if encodings.is_empty() {
                encodings.push(Encoding {
                    rid: None,
                    primary: None,
                    repair: None,
                    paused: false,
                    runtime: RuntimeEncoding::default(),
                });
            }
            let repair = section.codecs.iter().any(|codec| codec.repair.is_some());
            for encoding in &mut encodings {
                // libwebrtc rejects send streams containing both SSRCs and RIDs;
                // RID-only streams allocate their own SSRCs. Use SSRC/SIM/FID to
                // install these prepared identities; host controls keep their order.
                encoding.rid = None;
                if encoding.primary.is_none() {
                    encoding.primary = Some(ids.allocate(&section.mid, false)?);
                }
                if repair && encoding.repair.is_none() {
                    encoding.repair = Some(ids.allocate(&section.mid, true)?);
                }
                if !repair {
                    encoding.repair = None;
                }
                ids.reserve(
                    encoding.primary.ok_or(Error::Observation)?,
                    &section.mid,
                    false,
                )?;
                if let Some(repair) = encoding.repair {
                    ids.reserve(repair, &section.mid, true)?;
                }
            }
            senders.insert(
                section.mid.clone(),
                Sender {
                    encodings,
                    cname: cname.clone(),
                },
            );
        }
        let mut output = String::new();
        let mut media = document.media.iter();
        let mut current = None;
        for line in offer.lines() {
            if line.starts_with("m=") {
                if let Some(sender) = current.take() {
                    append_sender(&mut output, sender)?;
                }
                let section = media.next().ok_or(Error::Malformed("media"))?;
                current = senders.get(&section.section.mid);
            }
            if current.is_some() {
                if line.starts_with("a=ssrc:") || line.starts_with("a=ssrc-group:") {
                    continue;
                }
                if line
                    .strip_prefix("a=rid:")
                    .is_some_and(|value| value.split_whitespace().nth(1) == Some("send"))
                {
                    continue;
                }
                if let Some(value) = line.strip_prefix("a=simulcast:") {
                    let fields = value.split_whitespace().collect::<Vec<_>>();
                    let remaining = fields
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .filter(|[direction, _]| *direction != "send")
                        .flatten()
                        .copied()
                        .collect::<Vec<_>>();
                    if !remaining.is_empty() {
                        append_line(&mut output, &format!("a=simulcast:{}", remaining.join(" ")))?;
                    }
                    continue;
                }
            }
            append_line(&mut output, line)?;
        }
        if let Some(sender) = current {
            append_sender(&mut output, sender)?;
        }
        Document::parse(&output)?;
        // No offer escaped on failure. Commit history only after rendering and
        // validation, using the existing atomic identity-history admission.
        let SenderIds { next, owners, .. } = ids;
        self.identities.reserve_senders(
            owners
                .iter()
                .map(|(id, (mid, repair))| (*id, mid.as_str(), &None, *repair)),
        )?;
        self.next_identity = next;
        Ok(output)
    }
}

fn append_line(output: &mut String, line: &str) -> Result<(), Error> {
    if output.len().saturating_add(line.len()).saturating_add(2) > super::super::MAX_SDP_BYTES {
        return Err(Error::Capacity);
    }
    output.push_str(line);
    output.push_str("\r\n");
    Ok(())
}

fn append_sender(output: &mut String, sender: &Sender) -> Result<(), Error> {
    if sender.encodings.len() > 1 {
        let primaries = sender
            .encodings
            .iter()
            .map(|encoding| {
                encoding
                    .primary
                    .ok_or(Error::Observation)
                    .map(|id| id.to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        append_line(output, &format!("a=ssrc-group:SIM {}", primaries.join(" ")))?;
    }
    for encoding in &sender.encodings {
        let primary = encoding.primary.ok_or(Error::Observation)?;
        append_line(output, &format!("a=ssrc:{primary} cname:{}", sender.cname))?;
        if let Some(repair) = encoding.repair {
            append_line(output, &format!("a=ssrc:{repair} cname:{}", sender.cname))?;
            append_line(output, &format!("a=ssrc-group:FID {primary} {repair}"))?;
        }
    }
    Ok(())
}
