//! Cache and budget sizes in config (#1313). `[memory.caches]` and the
//! per-collection size keys (`[geotiff] tile_cache_mb`, `[grib]
//! grid_cache_mb`, …) take a whole number of MiB, which pins the size, or
//! `{ min = …, max = … }`, which leaves the size to the memory plan within a
//! floor and a ceiling.

use std::fmt;

use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde::Deserialize;

/// A cache or budget size as config writes it, in MiB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CacheSize {
    /// Pinned: the memory plan leaves it alone. `0` disables a cache.
    Mib(u64),
    /// Left to the memory plan, never below `min` nor above `max`. Until the
    /// plan sizes it, and in `[memory] mode = "report"`, the size is the
    /// built-in default clamped into the range.
    Range { min: u64, max: u64 },
}

impl From<u64> for CacheSize {
    fn from(mib: u64) -> Self {
        CacheSize::Mib(mib)
    }
}

impl CacheSize {
    /// The size in MiB this gives a cache whose built-in default is
    /// `default_mb`: the pinned size, or the default clamped into the range.
    pub fn mb(self, default_mb: u64) -> u64 {
        match self {
            CacheSize::Mib(mib) => mib,
            CacheSize::Range { min, max } => default_mb.clamp(min, max),
        }
    }

    /// Whether a number pins the size.
    pub fn is_pinned(self) -> bool {
        matches!(self, CacheSize::Mib(_))
    }

    /// The range's bounds in MiB, `None` for a pinned size.
    pub fn range(self) -> Option<(u64, u64)> {
        match self {
            CacheSize::Mib(_) => None,
            CacheSize::Range { min, max } => Some((min, max)),
        }
    }

    /// A range whose `min` is above its `max`, or whose `max` is 0, is an
    /// error naming `key`: write `0` to disable a cache.
    pub fn validate(self, key: &str) -> Result<(), String> {
        match self {
            CacheSize::Mib(_) => Ok(()),
            CacheSize::Range { max: 0, .. } => Err(format!(
                "{key}: max must be at least 1 MiB; write {key} = 0 to disable the cache"
            )),
            CacheSize::Range { min, max } if min > max => {
                Err(format!("{key}: min ({min}) must not be above max ({max})"))
            }
            CacheSize::Range { .. } => Ok(()),
        }
    }
}

/// The size in MiB `setting` gives a cache whose built-in default is
/// `default_mb`: the default when unset.
pub fn resolve(setting: Option<CacheSize>, default_mb: u64) -> u64 {
    setting.map_or(default_mb, |size| size.mb(default_mb))
}

impl fmt::Display for CacheSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CacheSize::Mib(mib) => write!(f, "{mib}"),
            CacheSize::Range { min, max } => write!(f, "{{ min = {min}, max = {max} }}"),
        }
    }
}

const EXPECTED: &str = "a size in MiB, a whole number, or { min = …, max = … } in MiB";

impl<'de> Deserialize<'de> for CacheSize {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct SizeVisitor;

        impl<'de> Visitor<'de> for SizeVisitor {
            type Value = CacheSize;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(EXPECTED)
            }

            fn visit_u64<E: de::Error>(self, mib: u64) -> Result<CacheSize, E> {
                Ok(CacheSize::Mib(mib))
            }

            fn visit_i64<E: de::Error>(self, mib: i64) -> Result<CacheSize, E> {
                u64::try_from(mib)
                    .map(CacheSize::Mib)
                    .map_err(|_| E::invalid_value(de::Unexpected::Signed(mib), &EXPECTED))
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<CacheSize, A::Error> {
                let (mut min, mut max) = (None, None);
                while let Some(key) = map.next_key::<String>()? {
                    let slot = match key.as_str() {
                        "min" => &mut min,
                        "max" => &mut max,
                        other => return Err(de::Error::unknown_field(other, &["min", "max"])),
                    };
                    if slot.is_some() {
                        return Err(de::Error::custom(format_args!("duplicate field `{key}`")));
                    }
                    *slot = Some(map.next_value::<u64>()?);
                }
                match (min, max) {
                    (Some(min), Some(max)) => Ok(CacheSize::Range { min, max }),
                    (None, _) => Err(de::Error::missing_field("min")),
                    (_, None) => Err(de::Error::missing_field("max")),
                }
            }
        }

        deserializer.deserialize_any(SizeVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Deserialize)]
    struct Holder {
        size: CacheSize,
    }

    fn parse(value: &str) -> Result<CacheSize, String> {
        toml::from_str::<Holder>(&format!("size = {value}"))
            .map(|h| h.size)
            .map_err(|e| e.to_string())
    }

    #[test]
    fn a_number_pins_and_a_table_bounds() {
        assert_eq!(parse("8192"), Ok(CacheSize::Mib(8192)));
        assert_eq!(parse("0"), Ok(CacheSize::Mib(0)));
        assert_eq!(
            parse("{ min = 2048, max = 8192 }"),
            Ok(CacheSize::Range {
                min: 2048,
                max: 8192
            })
        );
    }

    #[test]
    fn other_forms_are_errors_that_say_what_is_expected() {
        for (value, says) in [
            ("-1", "a size in MiB"),
            ("\"2G\"", "a size in MiB"),
            ("1.5", "a size in MiB"),
            ("{ min = 1 }", "missing field `max`"),
            ("{ max = 1 }", "missing field `min`"),
            ("{ min = 1, max = 2, step = 1 }", "unknown field `step`"),
            ("{ min = -1, max = 2 }", "invalid value"),
        ] {
            let err = parse(value).unwrap_err();
            assert!(err.contains(says), "{value}: {err}");
        }
    }

    #[test]
    fn a_range_takes_the_default_clamped_and_validates() {
        let range = CacheSize::Range { min: 64, max: 512 };
        assert_eq!(range.mb(256), 256);
        assert_eq!(range.mb(1024), 512);
        assert_eq!(range.mb(0), 64);
        assert_eq!(CacheSize::Mib(7).mb(256), 7);
        assert_eq!(resolve(None, 256), 256);
        assert_eq!(resolve(Some(range), 16), 64);
        assert!(range.validate("k").is_ok());
        let reversed = CacheSize::Range { min: 9, max: 8 }.validate("[grib] grid_cache_mb");
        assert_eq!(
            reversed.unwrap_err(),
            "[grib] grid_cache_mb: min (9) must not be above max (8)"
        );
        assert!(CacheSize::Range { min: 0, max: 0 }
            .validate("k")
            .unwrap_err()
            .contains("k = 0 to disable"));
        assert_eq!(range.to_string(), "{ min = 64, max = 512 }");
    }
}
