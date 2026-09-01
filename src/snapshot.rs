//! Streaming snapshot encoding and decoding.

use std::fmt;
use std::io::{self, BufRead};

pub(crate) const SNAPSHOT_MAGIC: &str = "LEMMALOG2";
const SNAPSHOT_MAGIC_V1: &str = "LEMMALOG1";
const SNAPSHOT_MAGIC_V0: &str = "CORTEXLOG1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SnapshotVersion {
    Legacy,
    V2,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SnapshotValue {
    Symbol(String),
    Integer(i64),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SnapshotRecord {
    Now(i64),
    Rules(String),
    Episode {
        id: String,
        timestamp: i64,
        speaker: Option<String>,
        text: String,
    },
    Escalation(String),
    Fact {
        predicate: String,
        confidence: f64,
        provenance: Vec<String>,
        arguments: Vec<SnapshotValue>,
    },
}

#[derive(Debug)]
pub(crate) enum SnapshotError {
    Io(io::Error),
    InvalidMagic(String),
    MissingNow,
    Malformed { line: usize, message: String },
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SnapshotError::Io(error) => write!(f, "snapshot I/O: {error}"),
            SnapshotError::InvalidMagic(magic) => {
                write!(f, "not a lemmalog snapshot: magic {magic:?}")
            }
            SnapshotError::MissingNow => write!(f, "snapshot is missing its NOW record"),
            SnapshotError::Malformed { line, message } => {
                write!(f, "malformed snapshot record at line {line}: {message}")
            }
        }
    }
}

impl std::error::Error for SnapshotError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SnapshotError::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for SnapshotError {
    fn from(error: io::Error) -> Self {
        SnapshotError::Io(error)
    }
}

pub(crate) struct SnapshotReader<R: BufRead> {
    reader: R,
    version: SnapshotVersion,
    line: String,
    line_number: usize,
    pending_now: Option<SnapshotRecord>,
    peak_record_buffer_bytes: usize,
}

impl<R: BufRead> SnapshotReader<R> {
    pub(crate) fn new(reader: R) -> Result<Self, SnapshotError> {
        let mut records = SnapshotReader {
            reader,
            version: SnapshotVersion::Legacy,
            line: String::new(),
            line_number: 0,
            pending_now: None,
            peak_record_buffer_bytes: 0,
        };
        if !records.read_line()? {
            return Err(SnapshotError::InvalidMagic(String::new()));
        }
        let magic = records.line.clone();
        records.version = match magic.as_str() {
            SNAPSHOT_MAGIC => SnapshotVersion::V2,
            SNAPSHOT_MAGIC_V1 | SNAPSHOT_MAGIC_V0 => SnapshotVersion::Legacy,
            _ => return Err(SnapshotError::InvalidMagic(magic)),
        };
        if !records.read_line()? {
            return Err(SnapshotError::MissingNow);
        }
        let now = parse_record(records.version, records.line_number, &records.line)?;
        if !matches!(now, SnapshotRecord::Now(_)) {
            return Err(SnapshotError::MissingNow);
        }
        records.pending_now = Some(now);
        Ok(records)
    }

    pub(crate) fn peak_record_buffer_bytes(&self) -> usize {
        self.peak_record_buffer_bytes
    }

    fn read_line(&mut self) -> Result<bool, SnapshotError> {
        self.line.clear();
        if self.reader.read_line(&mut self.line)? == 0 {
            return Ok(false);
        }
        self.line_number += 1;
        self.peak_record_buffer_bytes = self.peak_record_buffer_bytes.max(self.line.capacity());
        if self.line.ends_with('\n') {
            self.line.pop();
            if self.line.ends_with('\r') {
                self.line.pop();
            }
        }
        Ok(true)
    }
}

impl<R: BufRead> Iterator for SnapshotReader<R> {
    type Item = Result<SnapshotRecord, SnapshotError>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(now) = self.pending_now.take() {
            return Some(Ok(now));
        }
        match self.read_line() {
            Ok(true) => Some(parse_record(self.version, self.line_number, &self.line)),
            Ok(false) => None,
            Err(error) => Some(Err(error)),
        }
    }
}

fn parse_record(
    version: SnapshotVersion,
    line_number: usize,
    line: &str,
) -> Result<SnapshotRecord, SnapshotError> {
    let malformed = |message: String| SnapshotError::Malformed {
        line: line_number,
        message,
    };
    let (tag, rest) = line
        .split_once('\t')
        .ok_or_else(|| malformed("record has no tab-separated payload".to_string()))?;
    match tag {
        "NOW" => rest
            .parse()
            .map(SnapshotRecord::Now)
            .map_err(|_| malformed(format!("invalid NOW value {rest:?}"))),
        "RULES" => Ok(SnapshotRecord::Rules(unescape(rest, version))),
        "EP" => {
            let mut fields = rest.splitn(4, '\t');
            let (Some(id), Some(timestamp), Some(speaker), Some(text)) =
                (fields.next(), fields.next(), fields.next(), fields.next())
            else {
                return Err(malformed("bad EP record".to_string()));
            };
            Ok(SnapshotRecord::Episode {
                id: unescape(id, version),
                timestamp: timestamp
                    .parse()
                    .map_err(|_| malformed(format!("invalid EP timestamp {timestamp:?}")))?,
                speaker: if speaker.is_empty() {
                    None
                } else {
                    Some(unescape(speaker, version))
                },
                text: unescape(text, version),
            })
        }
        "ESC" => Ok(SnapshotRecord::Escalation(unescape(rest, version))),
        "FACT" => {
            let mut fields = rest.splitn(4, '\t');
            let (Some(predicate), Some(confidence), Some(provenance), Some(arguments)) =
                (fields.next(), fields.next(), fields.next(), fields.next())
            else {
                return Err(malformed("bad FACT record".to_string()));
            };
            let confidence = confidence
                .parse()
                .map_err(|_| malformed(format!("invalid FACT confidence {confidence:?}")))?;
            let provenance = match version {
                SnapshotVersion::Legacy => provenance
                    .split(',')
                    .filter(|value| !value.is_empty())
                    .map(|value| unescape(value, version))
                    .collect(),
                SnapshotVersion::V2 => decode_provenance(provenance)
                    .map_err(|message| malformed(format!("bad provenance: {message}")))?,
            };
            let mut decoded_arguments = Vec::new();
            for argument in arguments.split(' ').filter(|value| !value.is_empty()) {
                if let Some(symbol) = argument.strip_prefix("s:") {
                    decoded_arguments.push(SnapshotValue::Symbol(unescape(symbol, version)));
                } else if let Some(integer) = argument.strip_prefix("i:") {
                    decoded_arguments.push(SnapshotValue::Integer(integer.parse().map_err(
                        |_| malformed(format!("invalid integer argument {argument:?}")),
                    )?));
                } else {
                    return Err(malformed(format!("invalid FACT argument {argument:?}")));
                }
            }
            Ok(SnapshotRecord::Fact {
                predicate: predicate.to_string(),
                confidence,
                provenance,
                arguments: decoded_arguments,
            })
        }
        _ => Err(malformed(format!("unknown record tag {tag:?}"))),
    }
}

pub(crate) fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace(' ', "\\s")
}

fn unescape(value: &str, version: SnapshotVersion) -> String {
    let mut decoded = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(character) = chars.next() {
        if character != '\\' {
            decoded.push(character);
            continue;
        }
        match chars.next() {
            Some('t') => decoded.push('\t'),
            Some('n') => decoded.push('\n'),
            Some('r') if version == SnapshotVersion::V2 => decoded.push('\r'),
            Some('s') => decoded.push(' '),
            Some('\\') => decoded.push('\\'),
            Some(other) => {
                decoded.push('\\');
                decoded.push(other);
            }
            None => decoded.push('\\'),
        }
    }
    decoded
}

pub(crate) fn encode_provenance<'a>(values: impl IntoIterator<Item = &'a String>) -> String {
    let mut encoded = String::new();
    for value in values {
        let value = escape(value);
        encoded.push_str(&value.len().to_string());
        encoded.push(':');
        encoded.push_str(&value);
    }
    encoded
}

fn decode_provenance(mut encoded: &str) -> Result<Vec<String>, String> {
    let mut values = Vec::new();
    while !encoded.is_empty() {
        let colon = encoded
            .find(':')
            .ok_or_else(|| "length has no colon".to_string())?;
        let length: usize = encoded[..colon]
            .parse()
            .map_err(|_| format!("invalid length {:?}", &encoded[..colon]))?;
        encoded = &encoded[colon + 1..];
        if encoded.len() < length || !encoded.is_char_boundary(length) {
            return Err(format!("length {length} exceeds encoded value"));
        }
        let (value, remaining) = encoded.split_at(length);
        values.push(unescape(value, SnapshotVersion::V2));
        encoded = remaining;
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Cursor};

    fn records(input: String) -> Result<Vec<SnapshotRecord>, SnapshotError> {
        SnapshotReader::new(BufReader::with_capacity(1, Cursor::new(input)))?.collect()
    }

    fn legacy(magic: &str) -> String {
        format!(
            "{magic}\nNOW\t7\nRULES\tp(X)\\s:-\\sq(X).\nEP\tep1\t6\tagent\tline\\ntext\nESC\tcheck\\sthis\nFACT\tedge\t0.5\tep1,ref\\\\n\ts:two\\swords i:7\n"
        )
    }

    #[test]
    fn decoder_streams_both_legacy_magics_with_identical_typed_records() -> Result<(), SnapshotError>
    {
        // Kill claim: changing either compatibility magic, one-byte reads,
        // escaping, or any record decoder changes this complete record list.
        let v0 = records(legacy(SNAPSHOT_MAGIC_V0))?;
        let v1 = records(legacy(SNAPSHOT_MAGIC_V1))?;
        let expected = vec![
            SnapshotRecord::Now(7),
            SnapshotRecord::Rules("p(X) :- q(X).".to_string()),
            SnapshotRecord::Episode {
                id: "ep1".to_string(),
                timestamp: 6,
                speaker: Some("agent".to_string()),
                text: "line\ntext".to_string(),
            },
            SnapshotRecord::Escalation("check this".to_string()),
            SnapshotRecord::Fact {
                predicate: "edge".to_string(),
                confidence: 0.5,
                provenance: vec!["ep1".to_string(), r"ref\n".to_string()],
                arguments: vec![
                    SnapshotValue::Symbol("two words".to_string()),
                    SnapshotValue::Integer(7),
                ],
            },
        ];
        assert_eq!(v0, expected);
        assert_eq!(v1, expected);
        Ok(())
    }

    #[test]
    fn version_two_provenance_codec_is_lossless_and_length_prefixed() -> Result<(), SnapshotError> {
        // Kill claim: comma splitting or double-unescaping opaque references
        // fails either the pinned bytes or the decoded record equality.
        assert_eq!(encode_provenance([&"ref,one".to_string()]), "7:ref,one");
        let provenance = vec![
            "ep1".to_string(),
            "ref,one".to_string(),
            r"literal\n\t\".to_string(),
        ];
        let input = format!(
            "{SNAPSHOT_MAGIC}\nNOW\t9\nRULES\t\nFACT\tedge\t0.9\t{}\ts:a s:p s:o i:0 i:10 i:0\n",
            encode_provenance(provenance.iter())
        );
        let decoded = records(input)?;
        assert_eq!(
            decoded,
            vec![
                SnapshotRecord::Now(9),
                SnapshotRecord::Rules(String::new()),
                SnapshotRecord::Fact {
                    predicate: "edge".to_string(),
                    confidence: 0.9,
                    provenance,
                    arguments: vec![
                        SnapshotValue::Symbol("a".to_string()),
                        SnapshotValue::Symbol("p".to_string()),
                        SnapshotValue::Symbol("o".to_string()),
                        SnapshotValue::Integer(0),
                        SnapshotValue::Integer(10),
                        SnapshotValue::Integer(0),
                    ],
                },
            ]
        );
        Ok(())
    }

    #[test]
    fn decoder_rejects_malformed_snapshot_boundaries() {
        // Kill claim: permissive header, record, number, provenance, or
        // argument parsing makes at least one malformed case succeed.
        let cases = [
            "NOPE\nNOW\t0\n",
            "LEMMALOG2\nRULES\t\n",
            "LEMMALOG2\nNOW\tx\n",
            "LEMMALOG2\nNOW\t0\nEP\tonly\ttwo\n",
            "LEMMALOG2\nNOW\t0\nFACT\tedge\t0.9\t4:x\ts:a\n",
            "LEMMALOG2\nNOW\t0\nFACT\tedge\t0.9\t\tx:a\n",
        ];
        for input in cases {
            let result =
                SnapshotReader::new(BufReader::with_capacity(1, Cursor::new(input.as_bytes())))
                    .and_then(|reader| reader.collect::<Result<Vec<_>, _>>());
            assert!(result.is_err(), "malformed input accepted: {input:?}");
        }
    }
}
