use std::{
    collections::{BTreeMap, BTreeSet},
    io::{BufRead, Write},
    str::FromStr,
};

use anyhow::{Context, Result, ensure};
use clap::{Args, ValueEnum};
use pulsebeam_core::{
    auth::TokenId,
    identity::{ConnectionId, NodeId, ProjectId, PublicationId},
};
use serde_json::{Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

#[cfg(test)]
mod tests;

#[derive(Args)]
#[group(skip)]
#[command(group(clap::ArgGroup::new("selector").required(true).args(["conn", "publication", "jti"])))]
pub(crate) struct IncidentArgs {
    #[arg(long)]
    conn: Option<ConnectionId>,
    #[arg(long)]
    publication: Option<PublicationId>,
    #[arg(long, requires = "project")]
    jti: Option<TokenId>,
    /// Project scope (required for token lookup)
    #[arg(long)]
    project: Option<ProjectId>,
    /// Inclusive incident start (RFC 3339)
    #[arg(long, value_parser = timestamp)]
    from: OffsetDateTime,
    /// Exclusive incident end (RFC 3339)
    #[arg(long, value_parser = timestamp)]
    to: OffsetDateTime,
    #[arg(long, value_enum)]
    kind: Option<Kind>,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Kind {
    Audio,
    Video,
    Data,
}

impl Kind {
    fn parse(value: &Value) -> Option<Self> {
        Self::from_str(value.as_str()?, false).ok()
    }

    fn code(self) -> &'static str {
        match self {
            Self::Audio => "audio",
            Self::Video => "video",
            Self::Data => "data",
        }
    }
}

fn timestamp(input: &str) -> Result<OffsetDateTime, time::error::Parse> {
    OffsetDateTime::parse(input, &Rfc3339)
}

fn render_time(time: OffsetDateTime) -> Result<String> {
    Ok(time.to_offset(time::UtcOffset::UTC).format(&Rfc3339)?)
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Relay {
    node: NodeId,
    incarnation: u64,
    epoch: u64,
    label: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    Connection(ConnectionId),
    Publication(ConnectionId, PublicationId),
    Subscription(ConnectionId, u64),
    Relay(Relay),
}

impl Key {
    fn connection(self) -> Option<ConnectionId> {
        match self {
            Self::Connection(conn) | Self::Publication(conn, _) | Self::Subscription(conn, _) => {
                Some(conn)
            }
            Self::Relay(_) => None,
        }
    }

    fn is_connection(self) -> bool {
        matches!(self, Self::Connection(_))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Effect {
    Begin,
    Change,
    End,
    Note,
}

#[derive(Clone, PartialEq, Eq)]
struct Event {
    at: OffsetDateTime,
    project: ProjectId,
    key: Key,
    name: &'static str,
    effect: Effect,
    jti: Option<TokenId>,
    publication: Option<PublicationId>,
    kind: Option<Kind>,
    reason: Option<&'static str>,
    winner: Option<ConnectionId>,
    relay: Option<RelayObservation>,
}

#[derive(Clone, PartialEq, Eq)]
struct RelayObservation {
    peer: Option<NodeId>,
    destinations: u64,
    direction: &'static str,
}

fn id<T: FromStr>(value: &Value) -> Option<T> {
    value.as_str()?.parse().ok()
}

fn nonzero(value: &Value) -> Option<u64> {
    value.as_u64().filter(|value| *value > 0)
}

impl Event {
    fn parse(record: &Value) -> Option<Self> {
        let at = timestamp(record["timestamp"].as_str()?).ok()?;
        let f = &record["fields"];
        let project = id(&f["project"])?;
        let (mut jti, mut publication, mut kind, mut winner, mut relay) =
            (None, None, None, None, None);
        let name = f["event"].as_str()?;
        let (name, effect) = match name {
            "connection_admitted" => ("connection_admitted", Effect::Begin),
            "credential_renewed" => ("credential_renewed", Effect::Note),
            "connection_ended" => ("connection_ended", Effect::End),
            "publication_admitted" => ("publication_admitted", Effect::Begin),
            "publication_retired" => ("publication_retired", Effect::End),
            "subscription_admitted" => ("subscription_admitted", Effect::Begin),
            "subscription_waiting" => ("subscription_waiting", Effect::Change),
            "subscription_bound" => ("subscription_bound", Effect::Change),
            "subscription_replaced" => ("subscription_replaced", Effect::Change),
            "subscription_retired" => ("subscription_retired", Effect::End),
            "relay_installed" => ("relay_installed", Effect::Begin),
            "relay_changed" => ("relay_changed", Effect::Change),
            "relay_removed" => ("relay_removed", Effect::End),
            _ => return None,
        };
        let key = if name.starts_with("relay_") {
            publication = Some(id(&f["pub"])?);
            kind = Some(Kind::parse(&f["kind"])?);
            relay = Some(RelayObservation {
                peer: id(&f["peer"]),
                destinations: f["destinations"].as_u64()?,
                direction: match f["direction"].as_str()? {
                    "forward" => "forward",
                    "reverse" => "reverse",
                    _ => return None,
                },
            });
            Key::Relay(Relay {
                node: id(&f["node"])?,
                incarnation: nonzero(&f["incarnation"])?,
                epoch: nonzero(&f["epoch"])?,
                label: nonzero(&f["label"])?,
            })
        } else {
            let conn = id(&f["conn"])?;
            if name.starts_with("subscription_") {
                kind = Some(Kind::parse(&f["kind"])?);
                if matches!(name, "subscription_bound" | "subscription_replaced") {
                    publication = Some(id(&f["pub"])?);
                }
                Key::Subscription(conn, nonzero(&f["owned"])?)
            } else if name.starts_with("publication_") {
                let id = id(&f["pub"])?;
                publication = Some(id);
                kind = Some(Kind::parse(&f["kind"])?);
                Key::Publication(conn, id)
            } else {
                if matches!(name, "connection_admitted" | "credential_renewed") {
                    jti = Some(id(&f["jti"])?);
                }
                if name == "connection_ended" {
                    winner = id(&f["winner"]);
                }
                Key::Connection(conn)
            }
        };
        let reason = match f["reason"].as_str() {
            Some("closed") => Some("closed"),
            Some("authorization_expired") => Some("authorization_expired"),
            Some("authorization_revoked") => Some("authorization_revoked"),
            Some("participant_replaced") => Some("participant_replaced"),
            Some("unpublished") => Some("unpublished"),
            Some("connection_ended") => Some("connection_ended"),
            Some("source_unavailable") => Some("source_unavailable"),
            Some("execution_failed") => Some("execution_failed"),
            Some("preparing") => Some("preparing"),
            Some("disabled") => Some("disabled"),
            Some("removed") => Some("removed"),
            Some("source_ended") => Some("source_ended"),
            Some("permission_revoked") => Some("permission_revoked"),
            Some("gone") => Some("gone"),
            _ => None,
        };
        Some(Self {
            at,
            project,
            key,
            name,
            effect,
            jti,
            publication,
            kind,
            reason,
            winner,
            relay,
        })
    }

    fn render(&self) -> Result<Value> {
        let mut result = serde_json::Map::new();
        let mut field = |key: &str, value| {
            result.insert(key.to_owned(), value);
        };
        field("at", json!(render_time(self.at)?));
        field("event", json!(self.name));
        field("project", json!(self.project.to_string()));
        if let Some(conn) = self.key.connection() {
            field("conn", json!(conn.to_string()));
        }
        match self.key {
            Key::Subscription(_, owned) => field("owned", json!(owned)),
            Key::Relay(relay) => {
                field("node", json!(relay.node.to_string()));
                field("incarnation", json!(relay.incarnation));
                field("epoch", json!(relay.epoch));
                field("label", json!(relay.label));
            }
            _ => {}
        }
        if let Some(relay) = &self.relay {
            if let Some(peer) = relay.peer {
                field("peer", json!(peer.to_string()));
            }
            field("destinations", json!(relay.destinations));
            field("direction", json!(relay.direction));
        }
        if let Some(publication) = self.publication {
            field("pub", json!(publication.to_string()));
        }
        if let Some(kind) = self.kind {
            field("kind", json!(kind.code()));
        }
        if let Some(reason) = self.reason {
            field("reason", json!(reason));
        }
        if let Some(winner) = self.winner {
            field("winner", json!(winner.to_string()));
        }
        Ok(Value::Object(result))
    }
}

impl IncidentArgs {
    pub(crate) fn run(self, input: impl BufRead, mut output: impl Write) -> Result<()> {
        ensure!(self.from < self.to, "--from must be before --to");
        let mut skipped = 0usize;
        let mut events = Vec::new();
        for line in input.lines() {
            let line = line.context("reading the log export")?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(&line) {
                Ok(record)
                    if record.get("target").and_then(Value::as_str)
                        == Some("pulsebeam::incident") =>
                {
                    match Event::parse(&record) {
                        Some(event) if self.project.is_none_or(|p| p == event.project) => {
                            events.push(event);
                        }
                        Some(_) => {}
                        None => skipped = skipped.saturating_add(1),
                    }
                }
                Ok(_) => {}
                Err(_) => skipped = skipped.saturating_add(1),
            }
        }
        events.sort_by_key(|event| event.at);
        events.dedup();
        serde_json::to_writer_pretty(&mut output, &self.report(&events, skipped)?)?;
        writeln!(output)?;
        Ok(())
    }

    fn report(&self, events: &[Event], skipped: usize) -> Result<Value> {
        // Credential matches intentionally use the whole export, not the incident window.
        let mut candidates = BTreeSet::new();
        let mut related = BTreeSet::new();
        let mut outside_window = BTreeSet::new();
        let kind_matches = |event: &Event| {
            self.kind
                .is_none_or(|wanted| event.kind.is_none_or(|actual| wanted == actual))
        };
        for event in events {
            let Some(conn) = event.key.connection() else {
                continue;
            };
            if event.key.is_connection()
                && ((event.effect == Effect::Begin && event.at >= self.to)
                    || (event.effect == Effect::End && event.at <= self.from))
            {
                outside_window.insert((event.project, conn));
            }
            if self.conn == Some(conn) || self.jti.is_some_and(|jti| event.jti == Some(jti)) {
                candidates.insert((event.project, conn));
            }
            if event.at < self.to
                && kind_matches(event)
                && self
                    .publication
                    .is_some_and(|publication| event.publication == Some(publication))
            {
                candidates.insert((event.project, conn));
                related.insert((event.project, event.key));
            }
        }
        if self.jti.is_some() {
            candidates.retain(|key| !outside_window.contains(key));
        }
        let publications: BTreeSet<_> = events
            .iter()
            .filter_map(|event| {
                (event.at < self.to
                    && kind_matches(event)
                    && event
                        .key
                        .connection()
                        .is_some_and(|conn| candidates.contains(&(event.project, conn))))
                .then_some((event.project, event.publication?))
            })
            .collect();
        let matches_publication = |event: &Event| {
            event.publication.is_some_and(|pub_id| {
                self.publication.map_or_else(
                    || publications.contains(&(event.project, pub_id)),
                    |selected| selected == pub_id,
                )
            })
        };
        let linked_publishers: BTreeSet<_> = events
            .iter()
            .filter_map(|event| match event.key {
                Key::Publication(conn, _) if event.at < self.to && matches_publication(event) => {
                    Some((event.project, conn))
                }
                _ => None,
            })
            .collect();
        let belongs = |event: &Event| {
            if !kind_matches(event) {
                return false;
            }
            match event.key {
                Key::Relay(_) | Key::Publication(_, _) if matches_publication(event) => {
                    return true;
                }
                Key::Connection(conn) if linked_publishers.contains(&(event.project, conn)) => {
                    return true;
                }
                _ => {}
            }
            event
                .key
                .connection()
                .is_some_and(|conn| candidates.contains(&(event.project, conn)))
                && (self.publication.is_none()
                    || event.key.is_connection()
                    || related.contains(&(event.project, event.key)))
        };
        let mut timeline = Vec::new();
        let mut intervals = Vec::new();
        let mut active = BTreeMap::<_, &Event>::new();
        let mut known = BTreeSet::new();
        let mut incomplete = BTreeSet::new();
        for event in events
            .iter()
            .take_while(|event| event.at < self.to)
            .filter(|event| belongs(event))
        {
            if event.at >= self.from {
                timeline.push(event.render()?);
            }
            let key = (event.project, event.key);
            match event.effect {
                Effect::Begin => {
                    known.insert(key);
                    active.entry(key).or_insert(event);
                }
                Effect::Change | Effect::End => {
                    if !known.contains(&key) {
                        incomplete.insert(key);
                    }
                    if let Some(previous) = active.remove(&key) {
                        self.interval(&mut intervals, previous, Some(event))?;
                    }
                    if event.effect == Effect::Change {
                        active.insert(key, event);
                    }
                    if event.effect == Effect::End
                        && let Key::Connection(conn) = event.key
                    {
                        for (_, previous) in active.extract_if(.., |(project, key), _| {
                            *project == event.project && key.connection() == Some(conn)
                        }) {
                            self.interval(&mut intervals, previous, Some(event))?;
                        }
                    }
                }
                Effect::Note if !known.contains(&key) => {
                    incomplete.insert(key);
                }
                Effect::Note => {}
            }
        }
        for event in active.into_values() {
            self.interval(&mut intervals, event, None)?;
        }
        let mut token_matches = Vec::new();
        if let Some(jti) = self.jti {
            for event in events.iter().filter(|e| {
                e.jti == Some(jti)
                    && e.key
                        .connection()
                        .is_some_and(|conn| candidates.contains(&(e.project, conn)))
            }) {
                token_matches.push(event.render()?);
            }
        }
        let connections: Vec<_> = candidates
            .iter()
            .map(|&(project, conn)| {
                json!({
                    "project": project.to_string(), "conn": conn.to_string(),
                    "admission_observed": known.contains(&(project, Key::Connection(conn))),
                })
            })
            .collect();
        let missing = intervals.is_empty() && timeline.is_empty();
        let partial = skipped > 0
            || !incomplete.is_empty()
            || candidates
                .iter()
                .any(|&(project, conn)| !known.contains(&(project, Key::Connection(conn))));
        Ok(json!({
            "from": render_time(self.from)?, "to": render_time(self.to)?,
            "evidence": if missing { "missing" } else if partial { "partial" } else { "observed" },
            "ambiguous_token_match": self.jti.is_some() && candidates.len() > 1,
            "skipped_records": skipped, "lifetimes_missing_start": incomplete.len(),
            "connections": connections, "token_matches": token_matches,
            "events": timeline, "intervals": intervals,
            "note": "Observed control-plane events only. Missing records and packet delivery cannot be inferred.",
        }))
    }

    fn interval(&self, out: &mut Vec<Value>, start: &Event, end: Option<&Event>) -> Result<()> {
        let from = start.at.max(self.from);
        let to = end.map_or(self.to, |event| event.at);
        if from < to {
            out.push(json!({
                "from": render_time(from)?, "to": render_time(to)?,
                "start_evidence": start.render()?,
                "end_evidence": end.map(Event::render).transpose()?,
            }));
        }
        Ok(())
    }
}
