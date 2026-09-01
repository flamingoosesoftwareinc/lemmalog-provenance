//! Bounded streaming lexical search over persisted base facts.

use crate::snapshot::{SnapshotError, SnapshotReader, SnapshotRecord, SnapshotValue};
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{Searcher, Sink, SinkMatch};
use std::collections::{BTreeSet, BinaryHeap};
use std::fmt;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct SearchStats {
    pub(crate) peak_snapshot_record_bytes: usize,
    pub(crate) peak_retained_result_count: usize,
    pub(crate) peak_retained_result_bytes: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SearchOutput {
    pub(crate) rows: Vec<String>,
    pub(crate) truncated: bool,
    pub(crate) stats: SearchStats,
}

#[derive(Debug)]
pub(crate) enum SearchError {
    Snapshot(SnapshotError),
    Io(io::Error),
    Regex(String),
}

impl fmt::Display for SearchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SearchError::Snapshot(error) => error.fmt(f),
            SearchError::Io(error) => write!(f, "search I/O: {error}"),
            SearchError::Regex(error) => write!(f, "invalid regex: {error}"),
        }
    }
}

impl std::error::Error for SearchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SearchError::Snapshot(error) => Some(error),
            SearchError::Io(error) => Some(error),
            SearchError::Regex(_) => None,
        }
    }
}

impl From<SnapshotError> for SearchError {
    fn from(error: SnapshotError) -> Self {
        SearchError::Snapshot(error)
    }
}

impl From<io::Error> for SearchError {
    fn from(error: io::Error) -> Self {
        if error
            .get_ref()
            .is_some_and(|source| source.is::<SnapshotError>())
        {
            let source = error
                .into_inner()
                .expect("snapshot error source was present");
            let snapshot = source
                .downcast::<SnapshotError>()
                .expect("snapshot error source type was checked");
            return SearchError::Snapshot(*snapshot);
        }
        SearchError::Io(error)
    }
}

pub(crate) fn search_snapshot(
    path: &Path,
    pattern: &str,
    limit: usize,
    selected_scopes: Option<BTreeSet<String>>,
    legacy_scope: String,
    include_legacy: bool,
) -> Result<SearchOutput, SearchError> {
    let matcher = RegexMatcherBuilder::new()
        .case_smart(true)
        .build(pattern)
        .map_err(|error| SearchError::Regex(error.to_string()))?;
    if !path.exists() {
        return Ok(SearchOutput {
            rows: Vec::new(),
            truncated: false,
            stats: SearchStats::default(),
        });
    }
    let file = File::open(path)?;
    let records = SnapshotReader::new(BufReader::new(file))?;
    let mut facts = FactRowReader::new(records, selected_scopes, legacy_scope, include_legacy)?;
    let mut matches = BoundedMatches::new(limit.saturating_add(1));
    Searcher::new().search_reader(&matcher, &mut facts, &mut matches)?;

    let peak_snapshot_record_bytes = facts.peak_snapshot_record_bytes();
    let mut rows = matches.rows.into_vec();
    rows.sort();
    let truncated = rows.len() > limit;
    rows.truncate(limit);
    Ok(SearchOutput {
        rows,
        truncated,
        stats: SearchStats {
            peak_snapshot_record_bytes,
            peak_retained_result_count: matches.peak_count,
            peak_retained_result_bytes: matches.peak_bytes,
        },
    })
}

struct FactRowReader<R: BufRead> {
    records: SnapshotReader<R>,
    now: i64,
    selected_scopes: Option<BTreeSet<String>>,
    legacy_scope: String,
    include_legacy: bool,
    row: Vec<u8>,
    offset: usize,
}

impl<R: BufRead> FactRowReader<R> {
    fn new(
        mut records: SnapshotReader<R>,
        selected_scopes: Option<BTreeSet<String>>,
        legacy_scope: String,
        include_legacy: bool,
    ) -> Result<Self, SnapshotError> {
        let now = match records.next().transpose()? {
            Some(SnapshotRecord::Now(now)) => now,
            _ => return Err(SnapshotError::MissingNow),
        };
        Ok(FactRowReader {
            records,
            now,
            selected_scopes,
            legacy_scope,
            include_legacy,
            row: Vec::new(),
            offset: 0,
        })
    }

    fn peak_snapshot_record_bytes(&self) -> usize {
        self.records.peak_record_buffer_bytes()
    }

    fn next_row(&mut self) -> Result<bool, SnapshotError> {
        self.row.clear();
        self.offset = 0;
        while let Some(record) = self.records.next() {
            let SnapshotRecord::Fact {
                predicate,
                provenance,
                arguments,
                ..
            } = record?
            else {
                continue;
            };
            let candidate = match predicate.as_str() {
                "edge" if self.include_legacy => {
                    render_edge(&self.legacy_scope, &arguments, self.now, &provenance)
                }
                "scoped_edge" => render_scoped_edge(&arguments, self.now, &provenance).and_then(
                    |(scope, row)| {
                        if self.scope_selected(&scope) {
                            Some(row)
                        } else {
                            None
                        }
                    },
                ),
                _ => None,
            };
            if let Some(candidate) = candidate {
                self.row.extend_from_slice(candidate.as_bytes());
                self.row.push(b'\n');
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn scope_selected(&self, scope: &str) -> bool {
        self.selected_scopes
            .as_ref()
            .is_none_or(|selected| selected.contains(scope))
    }
}

impl<R: BufRead> Read for FactRowReader<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        if self.offset == self.row.len()
            && !self
                .next_row()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
        {
            return Ok(0);
        }
        let count = output.len().min(self.row.len() - self.offset);
        output[..count].copy_from_slice(&self.row[self.offset..self.offset + count]);
        self.offset += count;
        Ok(count)
    }
}

fn render_edge(
    scope: &str,
    arguments: &[SnapshotValue],
    now: i64,
    provenance: &[String],
) -> Option<String> {
    if arguments.len() != 6 || !temporally_valid(arguments, 3, 4, now) {
        return None;
    }
    Some(render_row(
        scope,
        &arguments[0],
        &arguments[1],
        &arguments[2],
        provenance,
    ))
}

fn render_scoped_edge(
    arguments: &[SnapshotValue],
    now: i64,
    provenance: &[String],
) -> Option<(String, String)> {
    if arguments.len() != 7 || !temporally_valid(arguments, 4, 5, now) {
        return None;
    }
    let SnapshotValue::Symbol(scope) = &arguments[0] else {
        return None;
    };
    Some((
        scope.clone(),
        render_row(
            scope,
            &arguments[1],
            &arguments[2],
            &arguments[3],
            provenance,
        ),
    ))
}

fn temporally_valid(
    arguments: &[SnapshotValue],
    valid_from_index: usize,
    valid_to_index: usize,
    now: i64,
) -> bool {
    matches!(
        (&arguments[valid_from_index], &arguments[valid_to_index]),
        (SnapshotValue::Integer(valid_from), SnapshotValue::Integer(valid_to))
            if *valid_from <= now && now < *valid_to
    )
}

fn render_row(
    scope: &str,
    subject: &SnapshotValue,
    predicate: &SnapshotValue,
    object: &SnapshotValue,
    provenance: &[String],
) -> String {
    format!(
        "{scope}\t{} --{}--> {}\tprovenance={}",
        display_value(subject),
        display_value(predicate),
        display_value(object),
        provenance.join(",")
    )
}

fn display_value(value: &SnapshotValue) -> String {
    match value {
        SnapshotValue::Symbol(symbol) => symbol.clone(),
        SnapshotValue::Integer(integer) => integer.to_string(),
    }
}

struct BoundedMatches {
    capacity: usize,
    rows: BinaryHeap<String>,
    retained_bytes: usize,
    peak_count: usize,
    peak_bytes: usize,
}

impl BoundedMatches {
    fn new(capacity: usize) -> Self {
        BoundedMatches {
            capacity,
            rows: BinaryHeap::new(),
            retained_bytes: 0,
            peak_count: 0,
            peak_bytes: 0,
        }
    }

    fn retain(&mut self, row: String) {
        if self.rows.iter().any(|existing| existing == &row) {
            return;
        }
        if self.rows.len() < self.capacity {
            self.retained_bytes += row.len();
            self.rows.push(row);
        } else if self.rows.peek().is_some_and(|largest| row < *largest) {
            if let Some(removed) = self.rows.pop() {
                self.retained_bytes -= removed.len();
                self.retained_bytes += row.len();
                self.rows.push(row);
            }
        }
        self.peak_count = self.peak_count.max(self.rows.len());
        self.peak_bytes = self.peak_bytes.max(self.retained_bytes);
    }
}

impl Sink for &mut BoundedMatches {
    type Error = io::Error;

    fn matched(
        &mut self,
        _searcher: &Searcher,
        matched: &SinkMatch<'_>,
    ) -> Result<bool, Self::Error> {
        let bytes = matched
            .bytes()
            .strip_suffix(b"\n")
            .unwrap_or(matched.bytes());
        let row = String::from_utf8(bytes.to_vec())
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        self.retain(row);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgentMemory, Ann, MockExtractor, Value};
    use std::io::BufRead;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn snapshot_path(name: &str) -> Result<std::path::PathBuf, std::time::SystemTimeError> {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        Ok(std::env::temp_dir().join(format!("lemmalog-search-{name}-{nonce}.snapshot")))
    }

    fn declare_edge(
        memory: &mut AgentMemory<MockExtractor>,
        subject: &str,
        predicate: &str,
        object: &str,
        valid_from: i64,
        valid_to: i64,
        provenance: &[&str],
    ) {
        let arguments = [
            memory.engine.sym(subject),
            memory.engine.sym(predicate),
            memory.engine.sym(object),
            Value::Int(valid_from),
            Value::Int(valid_to),
            Value::Int(valid_from),
        ];
        memory.engine.declare(
            "edge",
            &arguments,
            Ann::base(0.9, provenance.iter().copied()),
        );
    }

    #[test]
    fn streaming_search_bounds_decoder_and_result_owners() -> Result<(), Box<dyn std::error::Error>>
    {
        // Kill claim: unbounded growth in SnapshotReader's record buffer or
        // BoundedMatches' heap exceeds its owner peak; losing lexical ranking
        // changes the exact rows.
        let path = snapshot_path("bounds")?;
        let mut memory = AgentMemory::new(MockExtractor::new(0.9), "")?;
        for index in 0..2_000 {
            declare_edge(
                &mut memory,
                &format!("item{index:04}"),
                "mentions",
                "needle",
                0,
                i64::MAX,
                &["ep1"],
            );
        }
        let oversized = "x".repeat(32 * 1024);
        declare_edge(
            &mut memory,
            "zzzz",
            "mentions",
            &format!("needle-{oversized}"),
            0,
            i64::MAX,
            &["ep2"],
        );
        memory.engine.set_now(10);
        memory.save(&path.display().to_string())?;

        let first = search_snapshot(
            &path,
            "needle",
            3,
            None,
            "repository:test".to_string(),
            true,
        )?;
        let second = search_snapshot(
            &path,
            "needle",
            3,
            None,
            "repository:test".to_string(),
            true,
        )?;
        assert_eq!(first, second);
        assert_eq!(
            first.rows,
            [
                "repository:test\titem0000 --mentions--> needle\tprovenance=ep1",
                "repository:test\titem0001 --mentions--> needle\tprovenance=ep1",
                "repository:test\titem0002 --mentions--> needle\tprovenance=ep1",
            ]
        );
        assert!(first.truncated);
        assert_eq!(first.stats.peak_retained_result_count, 4);

        let max_snapshot_line = std::io::BufReader::new(std::fs::File::open(&path)?)
            .lines()
            .try_fold(0usize, |peak, line| {
                line.map(|line| peak.max(line.len() + 1))
            })?;
        let max_row = oversized.len() + 128;
        assert!(
            first.stats.peak_snapshot_record_bytes <= max_snapshot_line * 2 + 1_024,
            "record buffer peak: {:?}, max line: {max_snapshot_line}",
            first.stats
        );
        assert!(
            first.stats.peak_retained_result_bytes <= 4 * max_row,
            "result owner peak: {:?}, max row: {max_row}",
            first.stats
        );
        std::fs::remove_file(path)?;
        Ok(())
    }

    #[test]
    fn search_adapter_filters_relation_scope_and_temporal_boundaries(
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Kill claim: either temporal inequality, scope filtering, unrelated
        // relation filtering, or duplicate retention changes the whole list.
        let path = snapshot_path("policy")?;
        let mut memory = AgentMemory::new(MockExtractor::new(0.9), "")?;
        declare_edge(&mut memory, "at_now", "matches", "needle", 10, 20, &["a"]);
        declare_edge(&mut memory, "closed", "matches", "needle", 0, 10, &["b"]);
        declare_edge(&mut memory, "future", "matches", "needle", 11, 20, &["c"]);
        let scope = memory.engine.sym("workspace");
        let subject = memory.engine.sym("shared");
        let predicate = memory.engine.sym("matches");
        let object = memory.engine.sym("needle");
        memory.engine.declare(
            "scoped_edge",
            &[
                scope,
                subject,
                predicate,
                object,
                Value::Int(0),
                Value::Int(20),
                Value::Int(0),
            ],
            Ann::base(0.9, ["d"]),
        );
        memory.engine.declare(
            "mentions",
            &[subject, object],
            Ann::base(0.9, ["not-searchable"]),
        );
        memory.engine.set_now(10);
        memory.save(&path.display().to_string())?;

        let output = search_snapshot(
            &path,
            "needle",
            10,
            Some(["workspace".to_string()].into_iter().collect()),
            "repository:test".to_string(),
            false,
        )?;
        assert_eq!(
            output.rows,
            ["workspace\tshared --matches--> needle\tprovenance=d"]
        );
        assert!(!output.truncated);
        let all = search_snapshot(
            &path,
            "needle",
            10,
            None,
            "repository:test".to_string(),
            true,
        )?;
        assert_eq!(
            all.rows,
            [
                "repository:test\tat_now --matches--> needle\tprovenance=a",
                "workspace\tshared --matches--> needle\tprovenance=d",
            ]
        );
        std::fs::remove_file(path)?;
        Ok(())
    }

    #[test]
    fn load_and_search_share_snapshot_version_behavior() -> Result<(), Box<dyn std::error::Error>> {
        // Kill claim: adding a second decoder makes load/search disagree on a
        // compatibility magic or escaped symbol.
        let snapshots = [
            ("CORTEXLOG1", "ep1"),
            ("LEMMALOG1", "ep1"),
            ("LEMMALOG2", "3:ep1"),
        ];
        for (magic, provenance) in snapshots {
            let path = snapshot_path(magic)?;
            std::fs::write(
                &path,
                format!(
                    "{magic}\nNOW\t10\nRULES\t\nFACT\tedge\t0.9\t{provenance}\ts:two\\swords s:matches s:needle i:10 i:20 i:10\n"
                ),
            )
            ?;
            let loaded = AgentMemory::load(MockExtractor::new(0.9), &path.display().to_string())?;
            assert_eq!(
                loaded.ask("current(\"two words\", \"matches\", O)")?,
                ["O=needle"]
            );
            let searched = search_snapshot(
                &path,
                "needle",
                10,
                None,
                "repository:test".to_string(),
                true,
            )?;
            assert_eq!(
                searched.rows,
                ["repository:test\ttwo words --matches--> needle\tprovenance=ep1"]
            );
            std::fs::remove_file(path)?;
        }

        Ok(())
    }

    #[test]
    fn mid_stream_corruption_remains_a_snapshot_error() -> Result<(), Box<dyn std::error::Error>> {
        // Kill claim: converting decoder failures to generic reader I/O loses
        // the snapshot error category and its exact corruption location.
        let malformed = snapshot_path("malformed-mid-stream")?;
        std::fs::write(
            &malformed,
            "LEMMALOG2\nNOW\t10\nRULES\t\nFACT\tedge\t0.9\t3:ep1\ts:a s:matches s:needle i:0 i:20 i:0\nFACT\tedge\t0.9\t3:ep1\tx:bad\n",
        )?;
        let search_error = search_snapshot(
            &malformed,
            ".",
            10,
            None,
            "repository:test".to_string(),
            true,
        )
        .expect_err("malformed snapshot must fail search");
        assert_eq!(
            search_error.to_string(),
            "malformed snapshot record at line 5: invalid FACT argument \"x:bad\""
        );
        match search_error {
            SearchError::Snapshot(SnapshotError::Malformed { line, message }) => {
                assert_eq!(line, 5);
                assert_eq!(message, "invalid FACT argument \"x:bad\"");
            }
            other => panic!("expected malformed snapshot error, got {other:?}"),
        }
        std::fs::remove_file(malformed)?;
        Ok(())
    }
}
