//! CAP Update/Cancel resolution. Identity is scoped to the sender; references
//! address an exact `(sender, identifier, sent)` message, never a bare id.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};

use crate::parser::CapAlert;

/// A producer's identifier, shared by its in-place revisions.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct AlertKey {
    pub sender: String,
    pub identifier: String,
}

impl AlertKey {
    pub fn of(alert: &CapAlert) -> Self {
        Self {
            sender: alert.sender.clone().unwrap_or_default(),
            identifier: alert.identifier.clone(),
        }
    }

    /// Length-prefix the sender so arbitrary identifier punctuation cannot
    /// collide with the namespace separator. The API percent-encodes the id.
    pub fn feature_id(&self, info: usize, area: usize) -> String {
        format!(
            "cap:{}:{}{}.{}.{}",
            self.sender.len(),
            self.sender,
            self.identifier,
            info,
            area
        )
    }
}

/// Full CAP message identity. Missing sent is tolerated for renderable input,
/// but a cancellation must supply all three mandatory reference components.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct MessageKey {
    pub alert: AlertKey,
    pub sent: Option<DateTime<Utc>>,
}

impl MessageKey {
    pub fn of(alert: &CapAlert) -> Self {
        Self {
            alert: AlertKey::of(alert),
            sent: alert.sent,
        }
    }
}

pub(crate) fn accepts_status(alert: &CapAlert, filter: &[String]) -> bool {
    filter.is_empty()
        || alert.status.as_ref().is_some_and(|status| {
            filter
                .iter()
                .any(|s| s.trim().eq_ignore_ascii_case(status.trim()))
        })
}

/// Reject incomplete/malformed references instead of broadening a withdrawal
/// to another sender or revision. Timestamps compare as instants, not text.
fn parse_reference(value: &str) -> Option<MessageKey> {
    let mut parts = value.split(',');
    let sender = parts.next()?;
    let identifier = parts.next()?;
    let sent = DateTime::parse_from_rfc3339(parts.next()?)
        .ok()?
        .with_timezone(&Utc);
    if sender.is_empty() || identifier.is_empty() || parts.next().is_some() {
        return None;
    }
    Some(MessageKey {
        alert: AlertKey {
            sender: sender.into(),
            identifier: identifier.into(),
        },
        sent: Some(sent),
    })
}

pub fn is_renderable(msg_type: Option<&str>) -> bool {
    match msg_type.map(|s| s.trim().to_ascii_lowercase()) {
        None => true,
        Some(t) => matches!(t.as_str(), "alert" | "update"),
    }
}

/// Shared withdrawal decision for pull-source rebuilds and WIS2 ingestion.
pub(crate) fn references_withdrawn_by(alert: &CapAlert) -> Vec<MessageKey> {
    if !alert.msg_type.as_deref().is_some_and(|t| {
        t.trim().eq_ignore_ascii_case("update") || t.trim().eq_ignore_ascii_case("cancel")
    }) {
        return Vec::new();
    }
    let own = MessageKey::of(alert);
    alert
        .references
        .as_deref()
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|value| {
            let key = parse_reference(value);
            if key.is_none() {
                tracing::warn!(
                    "cap: ignoring malformed reference '{value}' in '{}'",
                    alert.identifier
                );
            }
            key
        })
        .filter(|key| key != &own)
        .collect()
}

/// Latest sent per sender/identifier, followed by exact-reference withdrawal.
/// Status filtering must precede this function (including duplicate selection).
pub(crate) fn resolve_references(alerts: Vec<CapAlert>) -> (Vec<CapAlert>, Vec<MessageKey>) {
    let mut newest: HashMap<AlertKey, usize> = HashMap::new();
    for (i, alert) in alerts.iter().enumerate() {
        let key = AlertKey::of(alert);
        match newest.get(&key) {
            Some(&j) if alerts[j].sent >= alert.sent => {}
            _ => {
                newest.insert(key, i);
            }
        }
    }
    let keep: HashSet<usize> = newest.values().copied().collect();
    let withdrawn: HashSet<_> = alerts
        .iter()
        .enumerate()
        .filter(|(i, _)| keep.contains(i))
        .flat_map(|(_, alert)| references_withdrawn_by(alert))
        .collect();
    let mut superseded = Vec::new();
    let survivors = alerts
        .into_iter()
        .enumerate()
        .filter_map(|(i, alert)| {
            if !keep.contains(&i) {
                return None;
            }
            let key = MessageKey::of(&alert);
            if withdrawn.contains(&key) {
                superseded.push(key);
                return None;
            }
            is_renderable(alert.msg_type.as_deref()).then_some(alert)
        })
        .collect();
    superseded.sort();
    superseded.dedup();
    (survivors, superseded)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alert(sender: &str, id: &str, sent: &str) -> CapAlert {
        CapAlert {
            sender: Some(sender.into()),
            identifier: id.into(),
            sent: Some(sent.parse().unwrap()),
            status: Some("Actual".into()),
            msg_type: Some("Alert".into()),
            ..Default::default()
        }
    }
    const T: &str = "2026-09-13T10:00:00Z";

    #[test]
    fn senders_and_reference_timestamps_are_distinct() {
        let a = alert("one", "A", T);
        let b = alert("two", "A", T);
        let mut cancel = alert("one", "C", T);
        cancel.msg_type = Some("Cancel".into());
        cancel.references = Some("one,A,2026-09-13T12:00:00+02:00".into());
        let (out, withdrawn) = resolve_references(vec![a, b.clone(), cancel.clone()]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].sender, b.sender);
        assert_eq!(withdrawn.len(), 1);
        let newer = alert("one", "A", "2026-09-13T11:00:00Z");
        let (out, _) = resolve_references(vec![newer, cancel]);
        assert_eq!(
            out.len(),
            1,
            "a reference to an old sent cannot cancel a reissue"
        );
    }

    #[test]
    fn invalid_references_never_broaden_to_identifier_only() {
        let mut cancel = alert("one", "C", T);
        cancel.msg_type = Some("Cancel".into());
        cancel.references = Some("A one,A one,A,invalid ,A,2026-09-13T10:00:00Z".into());
        assert!(references_withdrawn_by(&cancel).is_empty());
    }

    #[test]
    fn update_chain_withdraws_original_and_ack_does_not_withdraw_update() {
        let a = alert("one", "A", T);
        let mut update = alert("one", "U", T);
        update.msg_type = Some("Update".into());
        update.references = Some("one,A,2026-09-13T10:00:00Z one,U,2026-09-13T10:00:00Z".into());
        let mut ack = alert("one", "ACK", T);
        ack.msg_type = Some("Ack".into());
        ack.references = Some("one,U,2026-09-13T10:00:00Z".into());
        let (out, withdrawn) = resolve_references(vec![a, update, ack]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].identifier, "U");
        assert_eq!(withdrawn.len(), 1);
    }

    #[test]
    fn newest_revision_controls_references() {
        let a = alert("one", "A", T);
        let mut old = alert("one", "U", T);
        old.msg_type = Some("Update".into());
        old.references = Some("one,A,2026-09-13T10:00:00Z".into());
        let new = alert("one", "U", "2026-09-13T11:00:00Z");
        let (out, withdrawn) = resolve_references(vec![a, old, new]);
        assert_eq!(out.len(), 2);
        assert!(withdrawn.is_empty());
    }
}
