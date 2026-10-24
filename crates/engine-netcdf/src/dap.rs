//! The DAP2 formats an OPeNDAP server answers in (#1264): the DDS (a
//! dataset's variables, types and shapes), the DAS (their attributes), and
//! a `.dods` reply (the DDS of what a constraint selected, `Data:`, then
//! its values in XDR, big-endian), and the constraint expressions that ask
//! for one hyperslab. Pure functions; the requests are `crate::opendap`'s.
//!
//! What THREDDS serves for MEPS (measured 2026-10-23): fields as `Grid`s
//! whose maps are their coordinate variables, coordinates as plain arrays,
//! scalars as bare declarations, attributes typed as stored, a `Float32`
//! printed in its shortest round-trip form (`9.96921E36` is netCDF's
//! default fill exactly), and every value raw, unpacked. XDR widens
//! `Int16` and `UInt16` to 32 bits each; a `Byte` array is its bytes padded
//! to four, and every array carries its length twice. DAP2's `Byte` is
//! unsigned: a netCDF signed byte carries `_Unsigned = "false"`.
//!
//! Names are netCDF's, which may hold any character, a `.` included. A
//! server may write them `%`-escaped in a DDS or DAS (`a%2Eb`) or as they
//! are: both are read unescaped ([`unescape`]). A constraint escapes each
//! part of a projection on its own ([`identifier`]), so the `.` between a
//! Grid and its array is the only one the server reads as a separator,
//! and a reply's values are found by the array's own name
//! ([`Projection::leaf`]), never by splitting a projection.

use netcdf_reader::{
    NcAttrValue, NcAttribute, NcDimension, NcSliceInfo, NcSliceInfoElem, NcType, NcVariable,
};

use crate::source::Raw;

/// A DAP2 base type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DapType {
    Byte,
    Int16,
    UInt16,
    Int32,
    UInt32,
    Float32,
    Float64,
    String,
    Url,
}

impl DapType {
    fn parse(word: &str) -> Option<Self> {
        Some(match word.to_ascii_lowercase().as_str() {
            "byte" => Self::Byte,
            "int16" => Self::Int16,
            "uint16" => Self::UInt16,
            "int32" => Self::Int32,
            "uint32" => Self::UInt32,
            "float32" => Self::Float32,
            "float64" => Self::Float64,
            "string" => Self::String,
            "url" => Self::Url,
            _ => return None,
        })
    }
}

/// One declaration of a DDS.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Decl {
    /// A scalar (no dimensions) or an array, each dimension's name (if
    /// any) and size.
    Base {
        ty: DapType,
        name: String,
        dims: Vec<(Option<String>, u64)>,
    },
    /// An array and its map vectors.
    Grid {
        name: String,
        array: Box<Decl>,
        maps: Vec<Decl>,
    },
    Structure {
        name: String,
        members: Vec<Decl>,
    },
    Sequence {
        name: String,
        members: Vec<Decl>,
    },
}

/// A token of a DDS or DAS.
#[derive(Debug, Clone, PartialEq)]
enum Token<'a> {
    Word(&'a str),
    /// A quoted string, its escapes (`\"`, `\\`) resolved.
    Text(String),
    Punct(char),
}

/// The tokens of a DDS or DAS document.
#[derive(Clone)]
struct Tokens<'a> {
    text: &'a str,
    at: usize,
}

const PUNCT: &[u8] = b"{}[];=:,";

impl<'a> Tokens<'a> {
    fn new(text: &'a str) -> Self {
        Self { text, at: 0 }
    }

    fn next(&mut self) -> Result<Option<Token<'a>>, String> {
        let bytes = self.text.as_bytes();
        while self.at < bytes.len() && bytes[self.at].is_ascii_whitespace() {
            self.at += 1;
        }
        let Some(&first) = bytes.get(self.at) else {
            return Ok(None);
        };
        if PUNCT.contains(&first) {
            self.at += 1;
            return Ok(Some(Token::Punct(first as char)));
        }
        if first == b'"' {
            let mut text = String::new();
            let mut chars = self.text[self.at + 1..].char_indices();
            while let Some((i, c)) = chars.next() {
                match c {
                    '"' => {
                        self.at += i + 2;
                        return Ok(Some(Token::Text(text)));
                    }
                    '\\' => match chars.next() {
                        Some((_, escaped)) => text.push(escaped),
                        None => break,
                    },
                    c => text.push(c),
                }
            }
            return Err("a quoted string is not closed".into());
        }
        let start = self.at;
        while self.at < bytes.len()
            && !bytes[self.at].is_ascii_whitespace()
            && !PUNCT.contains(&bytes[self.at])
            && bytes[self.at] != b'"'
        {
            self.at += 1;
        }
        Ok(Some(Token::Word(&self.text[start..self.at])))
    }

    fn peek(&self) -> Result<Option<Token<'a>>, String> {
        self.clone().next()
    }

    fn word(&mut self) -> Result<&'a str, String> {
        match self.next()? {
            Some(Token::Word(word)) => Ok(word),
            other => Err(format!("expected a name, found {other:?}")),
        }
    }

    fn expect(&mut self, punct: char) -> Result<(), String> {
        match self.next()? {
            Some(Token::Punct(found)) if found == punct => Ok(()),
            other => Err(format!("expected '{punct}', found {other:?}")),
        }
    }

    fn keyword(&mut self, keyword: &str) -> Result<(), String> {
        match self.word()? {
            word if word.eq_ignore_ascii_case(keyword) => Ok(()),
            word => Err(format!("expected '{keyword}', found '{word}'")),
        }
    }
}

/// How deep a DDS's structures and Grids, or a DAS's containers, may nest.
/// The parsers recurse once per level: a document nested deeper fails,
/// rather than a hostile one overflowing the stack. NetCDF's need two.
pub(crate) const MAX_DEPTH: usize = 32;

/// The declarations of a DDS, `Dataset { … } name;`.
pub(crate) fn parse_dds(text: &str) -> Result<Vec<Decl>, String> {
    let mut tokens = Tokens::new(text);
    tokens.keyword("Dataset")?;
    tokens.expect('{')?;
    declarations(&mut tokens, 0).map_err(|e| format!("not a DAP2 DDS: {e}"))
}

/// The error of a document nested deeper than [`MAX_DEPTH`].
fn too_deep() -> String {
    format!("it nests more than {MAX_DEPTH} levels deep")
}

/// Declarations up to and including the closing `}`, `depth` levels of
/// structures and Grids deep.
fn declarations(tokens: &mut Tokens, depth: usize) -> Result<Vec<Decl>, String> {
    let mut decls = Vec::new();
    loop {
        match tokens.peek()? {
            Some(Token::Punct('}')) => {
                tokens.next()?;
                return Ok(decls);
            }
            None => return Err("the document ends inside a declaration list".into()),
            _ => decls.push(declaration(tokens, depth)?),
        }
    }
}

fn declaration(tokens: &mut Tokens, depth: usize) -> Result<Decl, String> {
    if depth > MAX_DEPTH {
        return Err(too_deep());
    }
    let word = tokens.word()?;
    let named = |tokens: &mut Tokens| -> Result<String, String> {
        let name = unescape(tokens.word()?);
        tokens.expect(';')?;
        Ok(name)
    };
    match word.to_ascii_lowercase().as_str() {
        "grid" => {
            tokens.expect('{')?;
            tokens.keyword("ARRAY")?;
            tokens.expect(':')?;
            let array = Box::new(declaration(tokens, depth + 1)?);
            tokens.keyword("MAPS")?;
            tokens.expect(':')?;
            let maps = declarations(tokens, depth + 1)?;
            Ok(Decl::Grid {
                name: named(tokens)?,
                array,
                maps,
            })
        }
        kind @ ("structure" | "sequence") => {
            tokens.expect('{')?;
            let members = declarations(tokens, depth + 1)?;
            let name = named(tokens)?;
            Ok(match kind {
                "structure" => Decl::Structure { name, members },
                _ => Decl::Sequence { name, members },
            })
        }
        _ => {
            let ty = DapType::parse(word).ok_or_else(|| format!("unknown type '{word}'"))?;
            let name = unescape(tokens.word()?);
            let mut dims = Vec::new();
            while tokens.peek()? == Some(Token::Punct('[')) {
                tokens.next()?;
                let first = tokens.word()?;
                let (dim, size) = if tokens.peek()? == Some(Token::Punct('=')) {
                    tokens.next()?;
                    (Some(unescape(first)), tokens.word()?)
                } else {
                    (None, first)
                };
                let size = size
                    .parse()
                    .map_err(|_| format!("'{name}' has a dimension of size '{size}'"))?;
                tokens.expect(']')?;
                dims.push((dim, size));
            }
            tokens.expect(';')?;
            Ok(Decl::Base { ty, name, dims })
        }
    }
}

/// An attribute container of a DAS: its attributes and the containers
/// nested in it.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Container {
    pub attributes: Vec<NcAttribute>,
    pub children: Vec<(String, Container)>,
}

impl Container {
    pub(crate) fn child(&self, name: &str) -> Option<&Container> {
        self.children
            .iter()
            .find(|(found, _)| found == name)
            .map(|(_, child)| child)
    }

    fn text(&self, name: &str) -> Option<String> {
        self.attributes
            .iter()
            .find(|a| a.name == name)
            .and_then(|a| a.value.as_string())
    }
}

/// A DAS, `Attributes { … }`: one container per variable, plus
/// `NC_GLOBAL` and `DODS_EXTRA`. An attribute that does not parse as its
/// type is an error: a dropped `_FillValue` would serve fill as data.
pub(crate) fn parse_das(text: &str) -> Result<Container, String> {
    let mut tokens = Tokens::new(text);
    tokens.keyword("Attributes")?;
    tokens.expect('{')?;
    container(&mut tokens, 0).map_err(|e| format!("not a DAP2 DAS: {e}"))
}

/// A container's contents up to and including its closing `}`, `depth`
/// containers deep.
fn container(tokens: &mut Tokens, depth: usize) -> Result<Container, String> {
    if depth > MAX_DEPTH {
        return Err(too_deep());
    }
    let mut found = Container::default();
    loop {
        match tokens.next()? {
            Some(Token::Punct('}')) => return Ok(found),
            Some(Token::Word(word)) => {
                if tokens.peek()? == Some(Token::Punct('{')) {
                    tokens.next()?;
                    found
                        .children
                        .push((unescape(word), container(tokens, depth + 1)?));
                    continue;
                }
                let name = tokens.word()?;
                let mut values = Vec::new();
                loop {
                    match tokens.next()? {
                        Some(Token::Word(value)) => values.push(value.to_string()),
                        Some(Token::Text(value)) => values.push(value),
                        other => return Err(format!("attribute '{name}': found {other:?}")),
                    }
                    match tokens.next()? {
                        Some(Token::Punct(',')) => {}
                        Some(Token::Punct(';')) => break,
                        other => return Err(format!("attribute '{name}': found {other:?}")),
                    }
                }
                if let Some(value) = attribute_value(word, &values)
                    .map_err(|e| format!("attribute '{name}': {e}"))?
                {
                    found.attributes.push(NcAttribute {
                        name: unescape(name),
                        value,
                    });
                }
            }
            other => return Err(format!("found {other:?} in a container")),
        }
    }
}

/// An attribute's values as their DAS type states them; `None` for a type
/// not read (`Alias`, or an unknown one).
fn attribute_value(ty: &str, values: &[String]) -> Result<Option<NcAttrValue>, String> {
    fn all<T: std::str::FromStr>(values: &[String]) -> Result<Vec<T>, String> {
        values
            .iter()
            .map(|v| {
                v.trim()
                    .parse()
                    .map_err(|_| format!("'{v}' is not a number"))
            })
            .collect()
    }
    Ok(Some(match ty.to_ascii_lowercase().as_str() {
        "byte" => {
            let values: Vec<i64> = all(values)?;
            if values.iter().all(|v| (0..=255).contains(v)) {
                NcAttrValue::UBytes(values.into_iter().map(|v| v as u8).collect())
            } else {
                NcAttrValue::Bytes(values.into_iter().map(|v| v as i8).collect())
            }
        }
        "int16" => NcAttrValue::Shorts(all(values)?),
        "uint16" => NcAttrValue::UShorts(all(values)?),
        "int32" => NcAttrValue::Ints(all(values)?),
        "uint32" => NcAttrValue::UInts(all(values)?),
        "int64" => NcAttrValue::Int64s(all(values)?),
        "uint64" => NcAttrValue::UInt64s(all(values)?),
        "float32" => NcAttrValue::Floats(all(values)?),
        "float64" => NcAttrValue::Doubles(all(values)?),
        "string" | "url" => match values {
            [one] => NcAttrValue::Chars(one.clone()),
            many => NcAttrValue::Strings(many.to_vec()),
        },
        _ => return Ok(None),
    }))
}

/// A dataset's variables, as netcdf-reader would describe them, and how a
/// constraint names each one's values ([`projection`](Metadata::projection)).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Metadata {
    pub variables: Vec<NcVariable>,
    /// Per variable, in `variables`' order.
    pub projections: Vec<Projection>,
}

impl Metadata {
    /// How a constraint names `name`'s values.
    pub(crate) fn projection(&self, name: &str) -> Option<&Projection> {
        let index = self.variables.iter().position(|v| v.name == name)?;
        Some(&self.projections[index])
    }
}

/// How a constraint selects a variable's values, and the name they carry
/// in a reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Projection {
    /// The path a constraint names, each part escaped ([`identifier`]):
    /// `grid.array` for a Grid's array (its maps are not wanted), else the
    /// variable's name.
    pub path: String,
    /// The name the values carry in a reply: the array's own, unescaped.
    pub leaf: String,
}

impl Projection {
    /// The projection of the parts of `path`, the values carrying the last
    /// one's name.
    fn of(path: &[&str]) -> Self {
        Self {
            path: path
                .iter()
                .map(|part| identifier(part))
                .collect::<Vec<_>>()
                .join("."),
            leaf: path.last().copied().unwrap_or_default().to_string(),
        }
    }
}

/// The variables of a DDS and its DAS: each top-level array, scalar or
/// Grid (by its name, with its array's type and dimensions) with its
/// container's attributes. Structures and sequences are not variables
/// netCDF fields come as and are left out, as is a dimension's name when
/// the DDS gives none (`[1069]`).
pub(crate) fn metadata(dds: &str, das: &str) -> Result<Metadata, String> {
    let decls = parse_dds(dds)?;
    let das = parse_das(das)?;
    let unlimited = das
        .child("DODS_EXTRA")
        .and_then(|extra| extra.text("Unlimited_Dimension"));
    let mut variables = Vec::new();
    let mut projections = Vec::new();
    for decl in &decls {
        let (name, ty, dims, projection) = match decl {
            Decl::Base { ty, name, dims } => (name, *ty, dims, Projection::of(&[name])),
            Decl::Grid { name, array, .. } => match &**array {
                Decl::Base {
                    ty,
                    name: member,
                    dims,
                } => (name, *ty, dims, Projection::of(&[name, member])),
                _ => return Err(format!("Grid '{name}' has no array")),
            },
            Decl::Structure { .. } | Decl::Sequence { .. } => continue,
        };
        let attributes = das
            .child(name)
            .map(|c| c.attributes.clone())
            .unwrap_or_default();
        let signed = attributes
            .iter()
            .any(|a| a.name == "_Unsigned" && a.value.as_string().as_deref() == Some("false"));
        let dtype = match ty {
            DapType::Byte if signed => NcType::Byte,
            DapType::Byte => NcType::UByte,
            DapType::Int16 => NcType::Short,
            DapType::UInt16 => NcType::UShort,
            DapType::Int32 => NcType::Int,
            DapType::UInt32 => NcType::UInt,
            DapType::Float32 => NcType::Float,
            DapType::Float64 => NcType::Double,
            DapType::String | DapType::Url => NcType::String,
        };
        // A signed byte's attributes in its own type: `_FillValue 129`
        // printed unsigned is -127.
        let attributes = attributes
            .into_iter()
            .map(|a| match (signed, a.value) {
                (true, NcAttrValue::UBytes(v)) => NcAttribute {
                    name: a.name,
                    value: NcAttrValue::Bytes(v.into_iter().map(|b| b as i8).collect()),
                },
                (_, value) => NcAttribute {
                    name: a.name,
                    value,
                },
            })
            .collect();
        let dimensions = dims
            .iter()
            .map(|(dim, size)| NcDimension {
                name: dim.clone().unwrap_or_default(),
                size: *size,
                is_unlimited: dim.is_some() && *dim == unlimited,
            })
            .collect();
        variables.push(NcVariable {
            name: name.clone(),
            dimensions,
            dtype,
            attributes,
            data_offset: 0,
            _data_size: 0,
            is_record_var: false,
            record_size: 0,
        });
        projections.push(projection);
    }
    Ok(Metadata {
        variables,
        projections,
    })
}

/// The values of a `.dods` reply: each base variable it carries (a Grid's
/// array and maps, a structure's members), by its own name, in reply
/// order. A reply that is no data (no `Data:` line) is an error, with the
/// server's message when it sent a DAP2 `Error`.
pub(crate) fn decode(reply: &[u8]) -> Result<Vec<(String, Raw)>, String> {
    let (dds, data) = split(reply)?;
    let decls = parse_dds(dds)?;
    let mut xdr = Xdr { bytes: data };
    let mut values = Vec::new();
    for decl in &decls {
        read_decl(&mut xdr, decl, &mut values)?;
    }
    Ok(values)
}

/// Whether `reply` is data: an error a server sent with a 200 is not.
pub(crate) fn check(reply: &[u8]) -> Result<(), String> {
    split(reply).map(|_| ())
}

/// A `.dods` reply's DDS and its data, either side of `Data:`.
fn split(reply: &[u8]) -> Result<(&str, &[u8]), String> {
    const SEPARATOR: &[u8] = b"\nData:\n";
    let Some(at) = reply.windows(SEPARATOR.len()).position(|w| w == SEPARATOR) else {
        return Err(server_error(reply));
    };
    let dds = std::str::from_utf8(&reply[..at]).map_err(|e| format!("reply DDS: {e}"))?;
    Ok((dds, &reply[at + SEPARATOR.len()..]))
}

/// What a reply without data says: a DAP2 `Error { … message = "…"; }`.
fn server_error(reply: &[u8]) -> String {
    let text = String::from_utf8_lossy(&reply[..reply.len().min(4096)]);
    let message = text
        .split_once("message")
        .and_then(|(_, rest)| rest.split_once('"'))
        .and_then(|(_, rest)| rest.split_once("\";"))
        .map(|(message, _)| message.replace("\\\"", "\""));
    match message {
        Some(message) => format!("the server answered an error: {message}"),
        None => "the reply carries no data".to_string(),
    }
}

/// XDR values, read from the front.
struct Xdr<'a> {
    bytes: &'a [u8],
}

impl<'a> Xdr<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        if n > self.bytes.len() {
            return Err(format!(
                "the reply ends {} bytes short",
                n - self.bytes.len()
            ));
        }
        let (taken, rest) = self.bytes.split_at(n);
        self.bytes = rest;
        Ok(taken)
    }

    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
}

fn read_decl(xdr: &mut Xdr, decl: &Decl, out: &mut Vec<(String, Raw)>) -> Result<(), String> {
    match decl {
        Decl::Base { ty, name, dims } => {
            let values = read_base(xdr, *ty, dims).map_err(|e| format!("'{name}': {e}"))?;
            out.push((name.clone(), values));
        }
        Decl::Grid { array, maps, .. } => {
            read_decl(xdr, array, out)?;
            for map in maps {
                read_decl(xdr, map, out)?;
            }
        }
        Decl::Structure { members, .. } => {
            for member in members {
                read_decl(xdr, member, out)?;
            }
        }
        Decl::Sequence { name, .. } => return Err(format!("sequence '{name}' is not read")),
    }
    Ok(())
}

/// One base variable's values: a scalar's alone, an array's after its
/// length, sent twice. Every number takes 4 bytes but a `Float64`'s 8;
/// an array of `Byte`s is its bytes, padded to a multiple of 4.
fn read_base(xdr: &mut Xdr, ty: DapType, dims: &[(Option<String>, u64)]) -> Result<Raw, String> {
    let n = if dims.is_empty() {
        1
    } else {
        let declared = dims
            .iter()
            .try_fold(1u64, |n, (_, size)| n.checked_mul(*size))
            .ok_or("its size overflows")?;
        let (count, again) = (xdr.u32()?, xdr.u32()?);
        if u64::from(count) != declared || again != count {
            return Err(format!(
                "{count} and {again} values sent for a shape of {declared}"
            ));
        }
        count as usize
    };
    macro_rules! numbers {
        ($variant:ident, $width:expr, $convert:expr) => {{
            let bytes = n.checked_mul($width).ok_or("its size overflows")?;
            Raw::$variant(
                xdr.take(bytes)?
                    .chunks_exact($width)
                    .map(|word| $convert(word.try_into().unwrap()))
                    .collect(),
            )
        }};
    }
    Ok(match ty {
        DapType::Byte if dims.is_empty() => Raw::U8(vec![xdr.u32()? as u8]),
        DapType::Byte => {
            let bytes = xdr.take(n)?.to_vec();
            xdr.take(n.next_multiple_of(4) - n)?;
            Raw::U8(bytes)
        }
        DapType::Int16 => numbers!(I16, 4, |w: [u8; 4]| i32::from_be_bytes(w) as i16),
        DapType::UInt16 => numbers!(U16, 4, |w: [u8; 4]| u32::from_be_bytes(w) as u16),
        DapType::Int32 => numbers!(I32, 4, i32::from_be_bytes),
        DapType::UInt32 => numbers!(U32, 4, u32::from_be_bytes),
        DapType::Float32 => numbers!(F32, 4, f32::from_be_bytes),
        DapType::Float64 => numbers!(F64, 8, f64::from_be_bytes),
        DapType::String | DapType::Url => return Err("string values are not read".into()),
    })
}

/// The constraint expression selecting `selection` of `projection`,
/// ready for a URL's query: `[i]` for one index, `[start:stride:stop]`
/// (`stop` inclusive) for a range.
pub(crate) fn constraint(projection: &Projection, selection: &NcSliceInfo) -> String {
    let mut text = query_name(&projection.path);
    for elem in &selection.selections {
        let slab = match *elem {
            NcSliceInfoElem::Index(i) => format!("[{i}]"),
            NcSliceInfoElem::Slice { start, end, step } => {
                format!("[{start}:{step}:{}]", end.saturating_sub(1))
            }
        };
        text.push_str(&slab.replace('[', "%5B").replace(']', "%5D"));
    }
    text
}

/// Values `selection` selects: one per index, each range's count, the
/// product saturating, whatever sizes the selection names.
pub(crate) fn selected(selection: &NcSliceInfo) -> u64 {
    selection.selections.iter().fold(1u64, |n, elem| {
        n.saturating_mul(match *elem {
            NcSliceInfoElem::Index(_) => 1,
            NcSliceInfoElem::Slice { start, end, step } => {
                end.saturating_sub(start).div_ceil(step.max(1))
            }
        })
    })
}

/// Several whole variables, by their projections, as one constraint.
pub(crate) fn whole(projections: &[&Projection]) -> String {
    projections
        .iter()
        .map(|p| query_name(&p.path))
        .collect::<Vec<_>>()
        .join(",")
}

/// One name in a constraint: every byte but an ASCII letter or digit, `_`,
/// `-`, `+` or `/` as `%XX`, which DAP2 servers decode inside an
/// identifier. So a `.`, `,`, `[` or `%` in a name is the name's, never
/// the constraint's syntax.
fn identifier(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || b"_-+/".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// A name a DDS or DAS writes, its `%XX` escapes decoded: `a%2Eb` is
/// `a.b`. A `%` not followed by two hex digits is kept.
pub(crate) fn unescape(name: &str) -> String {
    if !name.contains('%') {
        return name.to_string();
    }
    let bytes = name.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = name
            .get(i + 1..i + 3)
            .filter(|_| bytes[i] == b'%')
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match hex {
            Some(byte) => {
                out.push(byte);
                i += 3;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A projection as a query names it: every byte but an unreserved one or
/// a `.` percent-encoded, so an identifier's `%XX` reaches the server
/// whole.
fn query_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Bytes a reply of `n` values of `ty` carries in XDR, its DDS aside;
/// `u64::MAX` past it, whatever size a DDS declares.
pub(crate) fn xdr_bytes(ty: &NcType, n: u64) -> u64 {
    let values = match ty {
        NcType::Byte | NcType::UByte => n.checked_next_multiple_of(4).unwrap_or(u64::MAX),
        NcType::Double => n.saturating_mul(8),
        _ => n.saturating_mul(4),
    };
    values.saturating_add(8)
}

/// The values of `name` among a reply's, the first by that name.
pub(crate) fn take(values: Vec<(String, Raw)>, name: &str) -> Option<Raw> {
    values
        .into_iter()
        .find(|(found, _)| found == name)
        .map(|(_, raw)| raw)
}

#[cfg(test)]
mod tests;
