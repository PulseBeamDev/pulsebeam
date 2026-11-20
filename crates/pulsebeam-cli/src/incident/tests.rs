use super::*;
use clap::Parser;

const FIRST: &str = "550e8400-e29b-41d4-a716-446655440000";
const SECOND: &str = "550e8400-e29b-41d4-a716-446655440001";

fn config() -> IncidentArgs {
    IncidentArgs {
        conn: None,
        publication: None,
        jti: None,
        project: None,
        from: timestamp("2026-04-15T10:25:00Z").unwrap(),
        to: timestamp("2026-04-15T10:30:00Z").unwrap(),
        kind: None,
    }
}

fn log(minute: u8, project: ProjectId, name: &str, fields: Value) -> Value {
    let mut fields = fields.as_object().unwrap().clone();
    fields.insert("event".into(), json!(name));
    fields.insert("project".into(), json!(project.to_string()));
    fields.insert("participant".into(), json!("sentinel-user-name"));
    json!({"target": "pulsebeam::incident", "fields": fields,
        "timestamp": format!("2026-04-15T10:{minute:02}:00Z")})
}

fn report(config: IncidentArgs, records: &[Value]) -> Value {
    let input = records
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    let mut output = Vec::new();
    config.run(input.as_bytes(), &mut output).unwrap();
    let output = String::from_utf8(output).unwrap();
    assert!(!output.contains("sentinel-user-name"));
    serde_json::from_str(&output).unwrap()
}

#[test]
fn matches_accepted_renewal_after_window_and_keeps_same_project_candidates() {
    let project = ProjectId::new();
    let first = ConnectionId::new().to_string();
    let second = ConnectionId::new().to_string();
    let expired = ConnectionId::new().to_string();
    let records = [
        log(
            31,
            project,
            "credential_renewed",
            json!({"jti": FIRST, "conn": first}),
        ),
        log(
            24,
            project,
            "connection_admitted",
            json!({"jti": SECOND, "conn": first}),
        ),
        log(
            24,
            project,
            "connection_admitted",
            json!({"jti": FIRST, "conn": second}),
        ),
        log(
            22,
            project,
            "connection_admitted",
            json!({"jti": FIRST, "conn": expired}),
        ),
        log(
            24,
            project,
            "connection_ended",
            json!({"conn": expired, "reason": "closed"}),
        ),
        log(
            24,
            ProjectId::new(),
            "connection_admitted",
            json!({"jti": FIRST, "conn": ConnectionId::new().to_string()}),
        ),
    ];
    let report = report(
        IncidentArgs {
            project: Some(project),
            jti: Some(FIRST.parse().unwrap()),
            ..config()
        },
        &records,
    );
    assert_eq!(report["ambiguous_token_match"], true);
    assert_eq!(report["connections"].as_array().unwrap().len(), 2);
    assert_eq!(report["intervals"].as_array().unwrap().len(), 2);
    assert_eq!(report["token_matches"][1]["event"], "credential_renewed");
    assert_eq!(report["events"].as_array().unwrap().len(), 0);
}

#[test]
fn clips_attachment_and_waiting_intervals_for_exact_publication() {
    let project = ProjectId::new();
    let conn = ConnectionId::new().to_string();
    let publication = PublicationId::mint();
    let records = [
        log(
            22,
            project,
            "connection_admitted",
            json!({"conn": conn, "jti": FIRST}),
        ),
        log(
            22,
            project,
            "subscription_admitted",
            json!({"conn": conn, "owned": 5, "kind": "video"}),
        ),
        log(
            22,
            project,
            "subscription_waiting",
            json!({"conn": conn, "owned": 5, "kind": "video", "reason": "source_unavailable"}),
        ),
        log(
            23,
            project,
            "subscription_bound",
            json!({"conn": conn, "owned": 5, "kind": "video", "pub": publication.to_string()}),
        ),
        log(
            27,
            project,
            "subscription_waiting",
            json!({"conn": conn, "owned": 5, "kind": "video", "reason": "source_unavailable"}),
        ),
        log(
            28,
            project,
            "connection_ended",
            json!({"conn": conn, "reason": "participant_replaced", "winner": ConnectionId::new().to_string()}),
        ),
    ];
    let report = report(
        IncidentArgs {
            publication: Some(publication),
            ..config()
        },
        &records,
    );
    let find = |name| {
        report["intervals"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["start_evidence"]["event"] == name)
            .unwrap()
    };
    let bound = find("subscription_bound");
    assert_eq!(bound["from"], "2026-04-15T10:25:00Z");
    assert_eq!(bound["to"], "2026-04-15T10:27:00Z");
    assert_eq!(bound["end_evidence"]["reason"], "source_unavailable");
    let waiting = find("subscription_waiting");
    assert_eq!(waiting["to"], "2026-04-15T10:28:00Z");
    assert_eq!(waiting["end_evidence"]["reason"], "participant_replaced");
}

#[test]
fn missing_lookback_is_partial_and_unknown_publication_is_missing() {
    let conn = ConnectionId::new();
    let partial = report(
        IncidentArgs {
            conn: Some(conn),
            ..config()
        },
        &[log(
            27,
            ProjectId::new(),
            "subscription_retired",
            json!({"conn": conn.to_string(), "kind": "data", "owned": 5, "reason": "sentinel-user-name"}),
        )],
    );
    assert_eq!(partial["evidence"], "partial");
    assert_eq!(partial["lifetimes_missing_start"], 1);
    assert!(partial["events"][0]["reason"].is_null());
    assert_eq!(
        report(
            IncidentArgs {
                publication: Some(PublicationId::mint()),
                ..config()
            },
            &[]
        )["evidence"],
        "missing"
    );
}

#[test]
fn command_requires_one_selector_and_both_window_bounds() {
    let conn = ConnectionId::new().to_string();
    let publication = PublicationId::mint().to_string();
    let project = ProjectId::new().to_string();
    let parse = |selector: &[&str]| {
        crate::Cli::try_parse_from(
            [
                "pulsebeam-cli",
                "incident",
                "--from",
                "2026-04-15T10:25:00Z",
                "--to",
                "2026-04-15T10:30:00Z",
            ]
            .into_iter()
            .chain(selector.iter().copied()),
        )
    };
    assert!(parse(&["--conn", &conn, "--kind", "audio"]).is_ok());
    assert!(parse(&["--publication", &publication]).is_ok());
    assert!(parse(&["--project", &project, "--jti", FIRST]).is_ok());
    assert!(parse(&["--jti", FIRST]).is_err());
    assert!(parse(&["--conn", &conn, "--publication", &publication]).is_err());
    assert!(parse(&[]).is_err());
    assert!(crate::Cli::try_parse_from(["pulsebeam-cli", "incident", "--conn", &conn]).is_err());
}
