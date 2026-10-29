//! WCMP2 discovery records named by notifications (#1356).
//!
//! A WIS2 notification's `properties.metadata_id` names the WCMP2 record
//! of the dataset it belongs to. A WIS2-fed collection learns those ids into
//! the server's [`LearnedRecords`], which its collection descriptions link
//! as `describedby`. Brokers are public: the registry accepts only ids that
//! parse as a record id, and at most `MAX_LEARNED_RECORDS` per collection.

use std::sync::{Arc, OnceLock};

use ds_core::discovery::{Learned, LearnedRecords, MAX_LEARNED_RECORDS};

use crate::Notification;

/// Where a WIS2 source learns records: the server's [`LearnedRecords`],
/// connected once the engine is built. Unconnected, it learns nothing.
#[derive(Debug, Default)]
pub struct RecordLearner(OnceLock<Arc<LearnedRecords>>);

impl RecordLearner {
    /// Learn into `records` from now on. A later call keeps the first:
    /// the server has one registry.
    pub fn connect(&self, records: Arc<LearnedRecords>) {
        let _ = self.0.set(records);
    }

    /// Learn the record `n` names for `collection`, if connected.
    pub fn learn(&self, collection: &str, n: &Notification) {
        if let Some(records) = self.0.get() {
            learn_record(records, collection, n);
        }
    }
}

/// Learn the WCMP2 record `n` names (`properties.metadata_id`) for
/// `collection`. A new record is logged at INFO, the first one turned away
/// by a full collection at WARN, an id that is not one at DEBUG.
pub fn learn_record(records: &LearnedRecords, collection: &str, n: &Notification) {
    let Some(id) = n.metadata_id.as_deref() else {
        return;
    };
    match records.learn(collection, id) {
        Learned::Known | Learned::Full { first: false } => {}
        Learned::New => {
            tracing::info!("[{collection}] wis2: learned WCMP2 record {}", id.trim());
        }
        Learned::Invalid => {
            let shown: String = id.chars().take(80).collect();
            tracing::debug!(
                "[{collection}] wis2: ignoring metadata_id {shown:?} of {}: not a WCMP2 record id",
                n.data_id
            );
        }
        Learned::Full { first: true } => {
            tracing::warn!(
                "[{collection}] wis2: {MAX_LEARNED_RECORDS} WCMP2 records learned; \
                 ignoring {} and every further new one",
                id.trim()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notification(metadata_id: Option<&str>) -> Notification {
        Notification {
            topic: "cache/a/wis2/c/data/core/weather".into(),
            centre_id: None,
            id: "id".into(),
            data_id: "d".into(),
            pubtime: chrono::Utc::now(),
            datetime: None,
            start_datetime: None,
            end_datetime: None,
            geometry: None,
            integrity: None,
            content: None,
            links: Vec::new(),
            metadata_id: metadata_id.map(String::from),
            wigos_station_identifier: None,
            global_cache: None,
            extra: Default::default(),
        }
    }

    #[derive(Clone, Default)]
    struct Lines(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Lines {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// What `run` logs at INFO and above.
    fn logged(run: impl FnOnce()) -> Vec<String> {
        let lines = Lines::default();
        let writer = lines.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, run);
        let bytes = lines.0.lock().unwrap();
        String::from_utf8_lossy(&bytes)
            .lines()
            .map(String::from)
            .collect()
    }

    /// A new record is logged once; a repeat, a missing or invalid id add
    /// nothing; an unconnected learner learns nothing.
    #[test]
    fn records_are_learned_from_notifications() {
        let records = Arc::new(LearnedRecords::default());
        let learner = RecordLearner::default();
        learner.learn("obs", &notification(Some("urn:wmo:md:se-smhi:synop")));
        assert!(records.get("obs").is_none());
        learner.connect(records.clone());
        let lines = logged(|| {
            for id in [
                Some("urn:wmo:md:se-smhi:synop"),
                Some("urn:wmo:md:se-smhi:synop"),
                None,
                Some("<script>"),
            ] {
                learner.learn("obs", &notification(id));
            }
        });
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("[obs] wis2: learned WCMP2 record urn:wmo:md:se-smhi:synop"));
        let learned = records.get("obs").unwrap();
        assert_eq!(learned.len(), 1);
        assert_eq!(learned[0].as_str(), "urn:wmo:md:se-smhi:synop");
    }

    /// Past the cap, one WARN names the first id turned away; the rest are
    /// silent.
    #[test]
    fn a_full_collection_warns_once() {
        let records = LearnedRecords::default();
        let id = |i: usize| format!("urn:wmo:md:c:r{i:03}");
        for i in 0..MAX_LEARNED_RECORDS {
            records.learn("cap", &id(i));
        }
        let lines = logged(|| {
            for i in MAX_LEARNED_RECORDS..MAX_LEARNED_RECORDS + 5 {
                learn_record(&records, "cap", &notification(Some(&id(i))));
            }
        });
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("WARN"), "{lines:?}");
        assert!(
            lines[0].contains(&format!("64 WCMP2 records learned; ignoring {}", id(64))),
            "{lines:?}"
        );
        assert_eq!(records.get("cap").unwrap().len(), MAX_LEARNED_RECORDS);
    }
}
