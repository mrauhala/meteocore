//! A decoder for BUFR meteorological data format.

mod descriptor;
mod reader;
pub mod sections;
pub mod tables;

pub use descriptor::*;
pub use reader::{DataEvent, DataReader, DataSpec};
pub use sections::{HeaderSections, ensure_end_section};
pub use tables::{TableBEntry, TableDEntry, Tables};

/// The error type used by this crate.
#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("Table error: {0}")]
    Table(String),
    #[error("Invalid data: {0}")]
    Invalid(String),
    #[error("Not supported: {0}")]
    NotSupported(String),
    #[error("Fatal error: {0}")]
    Fatal(String),
}

/// Primitive value in BUFR data.
#[derive(Clone, PartialEq, Eq)]
pub enum Value {
    /// Missing value
    Missing,
    /// Scaled decimal value
    Decimal(i128, i16),
    /// Integer value
    Integer(i128),
    /// String value
    String(String),
}

impl std::fmt::Debug for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Value::Missing => write!(f, "Missing"),
            &Value::Decimal(v, s) => {
                write!(
                    f,
                    "{:.1$}",
                    v as f64 * 10f64.powi(s as i32),
                    if s < 0 { -s } else { 0 } as usize
                )
            }
            Value::Integer(v) => write!(f, "{v}"),
            Value::String(s) => write!(f, "\"{s}\""),
        }
    }
}
