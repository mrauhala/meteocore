//! CAP message-chain resolution: `msgType` Update / Cancel + `<references>`.
//!
//! CAP v1.2 §3.2.1: `<references>` holds the extended identifiers
//! (`sender,identifier,sent`) of earlier messages this one refers to,
//! whitespace-separated. An `Update` replaces them, a `Cancel` withdraws
//! them; `Ack` and `Error` are administrative and describe no hazard.
//!
//! Applied to the whole alert set on every catalog rebuild, for **every**
//! source mode: a directory or feed that keeps the original alert next to
//! its cancellation must not render both.

use std::collections::{HashMap, HashSet};

use crate::parser::CapAlert;

/// One `sender,identifier,sent` triple from `<references>`. Only the
/// identifier is used for matching — `sender` and `sent` are informational
/// (and frequently malformed in the wild).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    pub sender: String,
    pub identifier: String,
    pub sent: String,
}

/// Parse a `<references>` value. Tolerates the common deviations: missing
/// `sent`, bare identifiers, multiple whitespace.
pub fn parse_references(s: &str) -> Vec<Reference> {
    s.split_whitespace()
        .filter_map(|triple| {
            let mut parts = triple.splitn(3, ',');
            let a = parts.next()?.trim();
            match (parts.next(), parts.next()) {
                (Some(ident), sent) => {
                    let ident = ident.trim();
                    if ident.is_empty() {
                        return None;
                    }
                    Some(Reference {
                        sender: a.to_string(),
                        identifier: ident.to_string(),
                        sent: sent.map(|x| x.trim().to_string()).unwrap_or_default(),
                    })
                }
                // Bare identifier (non-conformant but seen).
                (None, _) if !a.is_empty() => Some(Reference {
                    sender: String::new(),
                    identifier: a.to_string(),
                    sent: String::new(),
                }),
                _ => None,
            }
        })
        .collect()
}

/// Whether an alert of this `msgType` describes a hazard and should be
/// rendered (Alert / Update; a missing msgType is treated as Alert).
pub fn is_renderable(msg_type: Option<&str>) -> bool {
    match msg_type.map(|s| s.trim().to_ascii_lowercase()) {
        None => true,
        Some(t) => matches!(t.as_str(), "alert" | "update"),
    }
}

/// Collapse a set of alerts to the ones still in force:
///
/// 1. one alert per `identifier` — the newest `sent` wins (re-issued documents);
/// 2. every identifier referenced by an `Update` or `Cancel` is withdrawn;
/// 3. `Cancel` / `Ack` / `Error` messages are themselves dropped.
///
/// Returns the survivors in input order plus the number withdrawn by (2).
pub fn resolve_references(alerts: Vec<CapAlert>) -> (Vec<CapAlert>, usize) {
    // (1) newest per identifier.
    let mut newest: HashMap<&str, usize> = HashMap::new();
    for (i, a) in alerts.iter().enumerate() {
        match newest.get(a.identifier.as_str()) {
            Some(&j) if alerts[j].sent >= a.sent => {}
            _ => {
                newest.insert(a.identifier.as_str(), i);
            }
        }
    }
    let keep: HashSet<usize> = newest.values().copied().collect();

    // (2) withdrawn identifiers, from the surviving messages' references.
    let mut withdrawn: HashSet<String> = HashSet::new();
    for (i, a) in alerts.iter().enumerate() {
        if !keep.contains(&i) {
            continue;
        }
        let t = a
            .msg_type
            .as_deref()
            .map(|s| s.trim().to_ascii_lowercase())
            .unwrap_or_default();
        if t == "update" || t == "cancel" {
            if let Some(refs) = &a.references {
                for r in parse_references(refs) {
                    if r.identifier != a.identifier {
                        withdrawn.insert(r.identifier);
                    }
                }
            }
        }
    }

    let mut superseded = 0usize;
    let survivors = alerts
        .into_iter()
        .enumerate()
        .filter_map(|(i, a)| {
            if !keep.contains(&i) {
                return None;
            }
            if withdrawn.contains(&a.identifier) {
                superseded += 1;
                return None;
            }
            if !is_renderable(a.msg_type.as_deref()) {
                return None;
            }
            Some(a)
        })
        .collect();
    (survivors, superseded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn alert(id: &str, msg_type: &str, refs: Option<&str>, sent_secs: i64) -> CapAlert {
        CapAlert {
            identifier: id.into(),
            sender: Some("s@x".into()),
            sent: Some(Utc.timestamp_opt(1_700_000_000 + sent_secs, 0).unwrap()),
            status: Some("Actual".into()),
            msg_type: Some(msg_type.into()),
            scope: Some("Public".into()),
            references: refs.map(str::to_string),
            infos: vec![],
        }
    }

    fn ids(v: &[CapAlert]) -> Vec<&str> {
        v.iter().map(|a| a.identifier.as_str()).collect()
    }

    #[test]
    fn parses_triples_and_deviations() {
        let r =
            parse_references("s@x,A1,2026-09-12T10:00:00+02:00  s@x,A2,2026-09-12T11:00:00Z\nB3");
        assert_eq!(r.len(), 3);
        assert_eq!(r[0].identifier, "A1");
        assert_eq!(r[0].sender, "s@x");
        assert_eq!(r[1].sent, "2026-09-12T11:00:00Z");
        assert_eq!(r[2].identifier, "B3");
        assert!(r[2].sender.is_empty());
        assert!(parse_references("   ").is_empty());
        assert!(parse_references("s@x,,t").is_empty());
    }

    #[test]
    fn update_and_cancel_chain() {
        let set = vec![
            alert("A", "Alert", None, 0),
            alert("B", "Alert", None, 0),
            alert("A2", "Update", Some("s@x,A,2026"), 10),
            alert("C", "Cancel", Some("s@x,B,2026"), 20),
            alert("D", "Ack", Some("s@x,A2,2026"), 30),
        ];
        let (out, superseded) = resolve_references(set);
        // A withdrawn by A2, B withdrawn by C; C (Cancel) and D (Ack) not rendered.
        assert_eq!(ids(&out), vec!["A2"]);
        assert_eq!(superseded, 2);
    }

    #[test]
    fn newest_sent_wins_per_identifier_and_stale_update_is_ignored() {
        let set = vec![
            alert("A", "Alert", None, 100), // re-issued, newer
            alert("A", "Alert", None, 0),
            alert("B", "Update", Some("s@x,A,x"), 50), // stale duplicate of B
            alert("B", "Alert", None, 60),             // newest B has no references
        ];
        let (out, superseded) = resolve_references(set);
        assert_eq!(ids(&out), vec!["A", "B"]);
        assert_eq!(out[0].sent.unwrap().timestamp() % 1000, 100);
        assert_eq!(superseded, 0);
    }

    #[test]
    fn self_reference_does_not_withdraw_itself_and_missing_msgtype_renders() {
        let mut a = alert("A", "Update", Some("s@x,A,x"), 0);
        a.msg_type = None;
        let b = alert("B", "Update", Some("s@x,B,x"), 0);
        let (out, superseded) = resolve_references(vec![a, b]);
        assert_eq!(ids(&out), vec!["A", "B"]);
        assert_eq!(superseded, 0);
    }
}
