#![allow(clippy::unwrap_used)]

use pulsebeam_auth::grants::{Action, GrantSet, TrackKind};
use serde_json::{Value, json};

#[test]
fn shared_grants_round_trip_the_rfc_shape_and_match_concrete_addresses() {
    let wire = json!([
        {"rooms": ["meeting/123/*"], "allow": ["join", "publish", "discover", "subscribe"]},
        {
            "rooms": ["robots"],
            "participant": "robot-42",
            "kind": "video",
            "label": "front",
            "allow": ["subscribe"]
        }
    ]);
    let grants: GrantSet = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(serde_json::to_value(&grants).unwrap(), wire);
    assert!(grants.allows(Action::Join, "meeting/123/main", None, None, None));
    assert!(!grants.allows(Action::Join, "robots", None, None, None));
    assert!(grants.allows(
        Action::Subscribe,
        "robots",
        Some("robot-42"),
        Some(TrackKind::Video),
        Some("front"),
    ));
    assert!(!grants.allows(
        Action::Discover,
        "robots",
        Some("robot-42"),
        Some(TrackKind::Video),
        Some("front"),
    ));
    let boundary = json!([{
        "rooms": ["é".repeat(128)],
        "allow": ["subscribe"],
        "participant": "é".repeat(128),
        "label": "é".repeat(128)
    }]);
    let accepted: GrantSet = serde_json::from_value(boundary.clone()).unwrap();
    assert_eq!(serde_json::to_value(accepted).unwrap(), boundary);

    let empty: GrantSet = serde_json::from_str("[]").unwrap();
    assert!(!empty.allows(Action::Join, "main", None, None, None));
}

#[test]
fn shared_grants_reject_malformed_schema_duplicates_and_received_budget_overflow() {
    for wire in [
        r#"null"#,
        r#"{}"#,
        r#"[null]"#,
        r#"[{"rooms":["main"],"allow":["join"],"allow":["subscribe"]}]"#,
        r#"[{"rooms":["main"],"rooms":["robots"],"allow":["join"]}]"#,
        r#"[{"rooms":["main"],"allow":["subscribe"],"kind":"audio","kind":"video"}]"#,
        r#"[{"rooms":["main"],"allow":["subscribe"],"label":"a","label":"b"}]"#,
        r#"[{"rooms":["main"],"allow":["subscribe"],"participant":"a","participant":"b"}]"#,
        r#"[{"rooms":["main"],"allow":["join"],"roomPrefix":"other"}]"#,
        r#"[{"rooms":["main"],"allow":["receive"]}]"#,
        r#"[{"rooms":["main"],"allow":["Join"]}]"#,
        r#"[{"rooms":["main"],"allow":["join","join"]}]"#,
        r#"[{"rooms":["main"],"allow":[]}]"#,
        r#"[{"rooms":[],"allow":["join"]}]"#,
        r#"[{"rooms":["main*"],"allow":["join"]}]"#,
        r#"[{"rooms":["main"],"allow":["join"],"participant":"self"}]"#,
        r#"[{"rooms":["main"],"allow":["publish","subscribe"],"participant":"self"}]"#,
        r#"[{"rooms":["main"],"allow":["join","discover"],"kind":"video"}]"#,
        r#"[{"rooms":["main"],"allow":["discover"],"kind":"screen"}]"#,
        r#"[{"rooms":["main"],"allow":["discover"],"label":""}]"#,
        r#"[{"rooms":["main"],"allow":["discover"],"participant":null}]"#,
        r#"[{"rooms":["main"],"allow":["discover"],"kind":null}]"#,
        r#"[{"rooms":["main"],"allow":["discover"],"label":null}]"#,
        r#"[{"rooms":[{"nested":"room"}],"allow":["join"]}]"#,
        r#"[{"rooms":["main"],"allow":[["join"]]}]"#,
        r#"[{"rooms":["main"],"allow":[{"join":null}]}]"#,
        r#"[{"rooms":["main"],"allow":["subscribe"],"kind":{"audio":null}}]"#,
        r#"[[["main"],["subscribe"],"alice","audio","camera"]]"#,
    ] {
        assert!(serde_json::from_str::<GrantSet>(wire).is_err(), "{wire}");
    }

    let nested = format!("{}0{}", "[".repeat(256), "]".repeat(256));
    let grant_items = vec![r#"{"rooms":["main"],"allow":["join"]}"#; 32].join(",");
    let room_items = vec![r#""main""#; 64].join(",");
    for wire in [
        format!("[{grant_items},{nested}]"),
        format!("[{{\"rooms\":[{room_items},{nested}],\"allow\":[\"join\"]}}]"),
        format!(
            "[{{\"rooms\":[\"main\"],\"allow\":[\"join\",\"publish\",\"discover\",\"subscribe\",{nested}]}}]"
        ),
    ] {
        let error = serde_json::from_str::<GrantSet>(&wire).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("authorization array exceeds its limit")
        );
    }

    let grant = json!({"rooms": ["main"], "allow": ["join"]});
    let at_limit = json!(vec![grant.clone(); 32]);
    assert!(serde_json::from_value::<GrantSet>(at_limit).is_ok());
    assert!(serde_json::from_value::<GrantSet>(json!(vec![grant; 33])).is_err());

    let rooms = json!(vec!["main"; 64]);
    let at_limit = json!([{"rooms": rooms, "allow": ["join"]}]);
    assert!(serde_json::from_value::<GrantSet>(at_limit.clone()).is_ok());
    let mut overflow: Vec<Value> = serde_json::from_value(at_limit).unwrap();
    overflow.push(json!({"rooms": ["main"], "allow": ["join"]}));
    assert!(serde_json::from_value::<GrantSet>(json!(overflow)).is_err());
    assert!(
        serde_json::from_value::<GrantSet>(json!([
            {"rooms": vec!["main"; 65], "allow": ["join"]}
        ]))
        .is_err()
    );
    assert!(
        serde_json::from_value::<GrantSet>(json!([
            {"rooms": ["main"], "allow": ["join", "publish", "discover", "subscribe", "join"]}
        ]))
        .is_err()
    );
    for field in ["rooms", "participant", "label"] {
        let mut grant = json!({"rooms": ["main"], "allow": ["subscribe"]});
        grant[field] = if field == "rooms" {
            json!(["é".repeat(129)])
        } else {
            json!("é".repeat(129))
        };
        assert!(serde_json::from_value::<GrantSet>(json!([grant])).is_err());
    }
}
