//! Bounded reporting of sidecar ambiguities during one source scan.

use ds_core::config::GribLevelType;

use crate::catalog::{duplicate_message_keys, MessageEntry};

#[derive(Default)]
pub(crate) struct IndexAmbiguities {
    files: usize,
    records: usize,
    example: Option<(String, MessageEntry, MessageEntry)>,
}

impl IndexAmbiguities {
    /// Called after the parameter filter. Disabled level families must not
    /// produce warnings for data that this source will never serve.
    pub fn record(
        &mut self,
        source: &str,
        messages: &[MessageEntry],
        enabled: Option<&[GribLevelType]>,
    ) {
        let mut count = 0;
        let eligible = messages.iter().filter(|m| {
            enabled.is_none_or(|families| m.level_type().is_some_and(|t| families.contains(&t)))
        });
        for (first, duplicate) in duplicate_message_keys(eligible) {
            tracing::debug!(
                source,
                parameter = %first.param,
                level_type = %first.levtype,
                grib_level = first.level,
                first_offset = first.offset,
                duplicate_offset = duplicate.offset,
                "ambiguous wgrib2 catalog key; queries select the first record; payload equivalence is unknown"
            );
            self.example
                .get_or_insert_with(|| (source.to_owned(), first.clone(), duplicate.clone()));
            count += 1;
        }
        self.files += usize::from(count > 0);
        self.records += count;
    }

    pub fn emit(&self, collection: &str) {
        if let Some((source, first, duplicate)) = &self.example {
            tracing::warn!(
                collection,
                ambiguous_files = self.files,
                ambiguous_records = self.records,
                example_source = source,
                parameter = %first.param,
                level_type = %first.levtype,
                // `level` is reserved for severity in flattened JSON logs.
                grib_level = first.level,
                first_offset = first.offset,
                duplicate_offset = duplicate.offset,
                "ambiguous wgrib2 catalog keys in scan; queries select the first record; payload equivalence is unknown; per-record details at DEBUG"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{message, TestSource};
    use crate::GribEngine;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct LogBuffer(Arc<Mutex<Vec<u8>>>);

    impl Write for LogBuffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn scan_summarizes_real_ambiguities_after_parameter_and_family_filters() {
        let source = TestSource::new();
        for step in 0..8 {
            source.write(
                &format!("f{step:03}"),
                &[
                    ("TMP", "surface", message(0, 280.0, [0; 4], 1, 0)),
                    ("TMP", "surface", message(0, 290.0, [0; 4], 1, 0)),
                    ("UPPER", "500 mb", message(0, 240.0, [0; 4], 100, 50000)),
                    ("UPPER", "500 mb", message(0, 250.0, [0; 4], 100, 50000)),
                ],
                step,
            );
        }
        for (parameter, family, expected_records) in [
            (None, None, 16),
            (Some("TMP"), Some(GribLevelType::Single), 8),
            (Some("TMP"), Some(GribLevelType::Pressure), 0),
            (Some("OTHER"), None, 0),
        ] {
            let log = LogBuffer::default();
            let writer = log.clone();
            let subscriber = tracing_subscriber::fmt()
                .json()
                .flatten_event(true)
                .without_time()
                .with_max_level(tracing::Level::WARN)
                .with_writer(move || writer.clone())
                .finish();
            let mut config = source.config();
            config.parameters = parameter.map(|p| vec![p.into()]);
            config.level_types = family.map(|f| vec![f]);
            tracing::subscriber::with_default(subscriber, || {
                let engine = GribEngine::new("diagnostics", &config).unwrap();
                // Already-known files must not produce another summary.
                engine.scan_once().unwrap();
            });
            let bytes = log.0.lock().unwrap();
            let lines: Vec<serde_json::Value> = std::str::from_utf8(&bytes)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(lines.len(), usize::from(expected_records > 0), "{lines:?}");
            if let Some(warning) = lines.first() {
                assert_eq!(warning["level"], "WARN");
                assert_eq!(warning["ambiguous_files"], 8);
                assert_eq!(warning["ambiguous_records"], expected_records);
                assert_eq!(warning["parameter"], "TMP");
                assert_eq!(warning["first_offset"], 0);
                assert!(warning["duplicate_offset"].as_u64().unwrap() > 0);
                assert!(warning["example_source"]
                    .as_str()
                    .unwrap()
                    .ends_with("f000.grib2"));
            }
        }
    }
}
