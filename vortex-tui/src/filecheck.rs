// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright the Vortex contributors

//! Check the array encoding trees of a Vortex file with FileCheck-style directives.
//!
//! The file is flattened into *units*, one per flat layout (the leaves of the layout tree that
//! hold a serialized array). Each unit is rendered as text:
//!
//! ```text
//! unit: l_comment/data[3] rows=8192 layouts=vortex.struct/vortex.zoned/vortex.chunked/vortex.flat
//! root: vortex.fsst(utf8, len=8192)
//!   codes: vortex.varbin(binary, len=8192)
//!   ...
//! ```
//!
//! The unit path joins the layout child names: struct fields with `.`, chunks with `[i]`, and
//! other children (`data`, `zones`, `values`, `codes`, ...) with `/`.
//!
//! A check file contains [filecheck](https://docs.rs/filecheck) directives (`check:`,
//! `nextln:`, `sameln:`, `not:`, `unordered:`, `regex:`), which match partially and in order.
//! It may also contain `units: <glob>` lines, which start a section whose directives are
//! checked against *every* unit whose path matches the glob, separately. Directives before the
//! first `units:` line are checked once against the dump of the whole file.
//!
//! ```text
//! # Every chunk of l_comment must be FSST-compressed.
//! units: lineitem.l_comment/data[*]
//! check: root: vortex.fsst
//!
//! # No zone map should hold an uncompressed primitive array.
//! units: */zones
//! not: vortex.primitive
//! ```
//!
//! In globs, `*` matches any run of characters (including `/`), `?` matches one character, and
//! everything else, including `[` and `]`, matches literally.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use anyhow::bail;
use filecheck::CheckerBuilder;
use filecheck::NO_VARIABLES;
use futures::StreamExt as _;
use futures::TryStreamExt as _;
use regex::Regex;
use vortex::array::ArrayRef;
use vortex::array::display::BufferExtractor;
use vortex::array::display::EncodingSummaryExtractor;
use vortex::array::display::MetadataExtractor;
use vortex::array::display::NbytesExtractor;
use vortex::array::display::StatsExtractor;
use vortex::array::serde::SerializedArray;
use vortex::error::VortexResult;
use vortex::error::vortex_err;
use vortex::file::OpenOptionsSessionExt;
use vortex::file::VortexFile;
use vortex::layout::LayoutChildType;
use vortex::layout::LayoutRef;
use vortex::layout::layouts::flat::Flat;
use vortex::layout::segments::SegmentSource;
use vortex::session::VortexSession;

/// How many segments to fetch and decode concurrently.
const FETCH_CONCURRENCY: usize = 16;

/// How many trailing lines of a match explanation to print without `--explain`.
const EXPLAIN_TAIL_LINES: usize = 20;

/// How many failing units to report per section.
const MAX_REPORTED_FAILURES: usize = 5;

/// Command-line arguments for the filecheck command.
#[derive(Debug, clap::Parser)]
pub struct FilecheckArgs {
    /// Path to the Vortex file.
    pub file: PathBuf,
    /// File of check directives. Without it, the text being checked is printed instead.
    pub checks: Option<PathBuf>,
    /// Skip units whose path matches this glob. Can be repeated.
    #[arg(long)]
    pub exclude: Vec<String>,
    /// Include array metadata in the dump.
    #[arg(long)]
    pub metadata: bool,
    /// Include array sizes in the dump.
    #[arg(long)]
    pub nbytes: bool,
    /// Include array buffers in the dump.
    #[arg(long)]
    pub buffers: bool,
    /// Include array statistics in the dump.
    #[arg(long)]
    pub stats: bool,
    /// Print the full match explanation for each failure instead of its last lines.
    #[arg(long)]
    pub explain: bool,
}

/// Which details to render for each array node.
#[derive(Debug, Clone, Copy, Default)]
pub struct DumpOptions {
    /// Include array metadata.
    pub metadata: bool,
    /// Include array sizes.
    pub nbytes: bool,
    /// Include array buffers.
    pub buffers: bool,
    /// Include array statistics.
    pub stats: bool,
}

/// One flat layout of a file, rendered as text.
#[derive(Debug, Clone)]
pub struct Unit {
    /// Path of the unit in the layout tree, such as `l_comment/data[3]`.
    pub path: String,
    /// The rendered unit: a `unit:` header line followed by the array encoding tree.
    pub text: String,
}

/// A path glob where `*` matches any run of characters and `?` matches one character.
#[derive(Debug, Clone)]
pub struct Glob {
    pattern: String,
    regex: Regex,
}

impl Glob {
    /// Compile a glob pattern.
    ///
    /// # Errors
    ///
    /// Returns an error if the translated regular expression does not compile.
    pub fn new(pattern: &str) -> anyhow::Result<Self> {
        let mut rx = String::from("^");
        for c in pattern.chars() {
            match c {
                '*' => rx.push_str(".*"),
                '?' => rx.push('.'),
                c => rx.push_str(&regex::escape(c.encode_utf8(&mut [0; 4]))),
            }
        }
        rx.push('$');
        Ok(Self {
            pattern: pattern.to_string(),
            regex: Regex::new(&rx)?,
        })
    }

    /// Whether `path` matches the whole glob.
    pub fn matches(&self, path: &str) -> bool {
        self.regex.is_match(path)
    }
}

/// A group of directives from a check file.
#[derive(Debug)]
pub struct CheckSection {
    /// Units to check one by one, or `None` to check the dump of the whole file once.
    pub selector: Option<Glob>,
    /// The 1-based line of the `units:` directive, or 1 for the leading section.
    pub line: usize,
    /// The directive text of the section.
    pub directives: String,
}

impl CheckSection {
    fn label(&self) -> String {
        match &self.selector {
            Some(glob) => format!("line {}: units: {}", self.line, glob.pattern),
            None => "whole file".to_string(),
        }
    }
}

/// One failed check.
#[derive(Debug)]
pub struct Failure {
    /// The unit that failed, or `None` for a whole-file section.
    pub unit: Option<String>,
    /// The filecheck explanation of the match attempt.
    pub explanation: String,
}

/// The outcome of checking one section.
#[derive(Debug)]
pub struct SectionOutcome {
    /// A human-readable description of the section.
    pub label: String,
    /// How many texts were checked: matching units, or 1 for the whole file.
    pub checked: usize,
    /// The checks that failed.
    pub failures: Vec<Failure>,
}

impl SectionOutcome {
    /// Whether every check in the section passed.
    ///
    /// A `units:` section that matched no unit fails, so a typo in a glob cannot pass silently.
    pub fn passed(&self) -> bool {
        self.checked > 0 && self.failures.is_empty()
    }
}

/// Run `vx filecheck`.
///
/// # Errors
///
/// Returns an error if the file cannot be read, the check file is malformed, or a check fails.
pub async fn exec_filecheck(session: &VortexSession, args: FilecheckArgs) -> anyhow::Result<()> {
    let options = DumpOptions {
        metadata: args.metadata,
        nbytes: args.nbytes,
        buffers: args.buffers,
        stats: args.stats,
    };
    let excludes = args
        .exclude
        .iter()
        .map(|pattern| Glob::new(pattern))
        .collect::<anyhow::Result<Vec<_>>>()?;

    let file = session.open_options().open_path(&args.file).await?;
    let units: Vec<Unit> = collect_units(session, &file, options)
        .await?
        .into_iter()
        .filter(|unit| !excludes.iter().any(|glob| glob.matches(&unit.path)))
        .collect();

    let Some(checks_path) = args.checks else {
        for unit in &units {
            print!("{}", unit.text);
        }
        return Ok(());
    };

    let check_text = std::fs::read_to_string(&checks_path)
        .with_context(|| format!("reading check file {}", checks_path.display()))?;
    let outcomes = run_checks(&units, &parse_check_file(&check_text)?)?;

    let mut failed_sections = 0;
    for outcome in &outcomes {
        if outcome.passed() {
            println!("PASS {}: {} checked", outcome.label, outcome.checked);
            continue;
        }
        failed_sections += 1;
        if outcome.checked == 0 {
            println!("FAIL {}: matched no units", outcome.label);
            continue;
        }
        println!(
            "FAIL {}: {}/{} failed",
            outcome.label,
            outcome.failures.len(),
            outcome.checked
        );
        for failure in outcome.failures.iter().take(MAX_REPORTED_FAILURES) {
            if let Some(unit) = &failure.unit {
                println!("  unit {unit}:");
            }
            print_explanation(&failure.explanation, args.explain);
        }
        if outcome.failures.len() > MAX_REPORTED_FAILURES {
            println!(
                "  ... and {} more failing units",
                outcome.failures.len() - MAX_REPORTED_FAILURES
            );
        }
    }

    if failed_sections > 0 {
        bail!(
            "{failed_sections} of {} check sections failed",
            outcomes.len()
        );
    }
    Ok(())
}

fn print_explanation(explanation: &str, full: bool) {
    let lines: Vec<&str> = explanation.lines().collect();
    let start = if full {
        0
    } else {
        lines.len().saturating_sub(EXPLAIN_TAIL_LINES)
    };
    if start > 0 {
        println!("    ... ({start} lines omitted, pass --explain for all)");
    }
    for line in &lines[start..] {
        println!("    {line}");
    }
}

/// Split a check file into sections at its `units: <glob>` lines.
///
/// # Errors
///
/// Returns an error if a glob is malformed.
pub fn parse_check_file(text: &str) -> anyhow::Result<Vec<CheckSection>> {
    let units_rx = Regex::new(r"\bunits:\s+(\S.*?)\s*$")?;
    let mut sections = vec![CheckSection {
        selector: None,
        line: 1,
        directives: String::new(),
    }];
    for (idx, line) in text.lines().enumerate() {
        if let Some(glob) = units_rx.captures(line).and_then(|caps| caps.get(1)) {
            sections.push(CheckSection {
                selector: Some(Glob::new(glob.as_str())?),
                line: idx + 1,
                directives: String::new(),
            });
        } else if let Some(section) = sections.last_mut() {
            section.directives.push_str(line);
            section.directives.push('\n');
        }
    }
    Ok(sections)
}

/// Check `units` against each section, skipping sections without directives.
///
/// # Errors
///
/// Returns an error if a directive is malformed.
pub fn run_checks(
    units: &[Unit],
    sections: &[CheckSection],
) -> anyhow::Result<Vec<SectionOutcome>> {
    let mut outcomes = Vec::new();
    for section in sections {
        let checker = CheckerBuilder::new()
            .text(&section.directives)
            .with_context(|| format!("parsing directives of {}", section.label()))?
            .finish();
        if checker.is_empty() {
            continue;
        }

        let targets: Vec<(Option<&str>, String)> = match &section.selector {
            None => vec![(None, units.iter().map(|unit| unit.text.as_str()).collect())],
            Some(glob) => units
                .iter()
                .filter(|unit| glob.matches(&unit.path))
                .map(|unit| (Some(unit.path.as_str()), unit.text.clone()))
                .collect(),
        };

        let mut failures = Vec::new();
        for (unit, text) in &targets {
            let (passed, explanation) = checker.explain(text, NO_VARIABLES)?;
            if !passed {
                failures.push(Failure {
                    unit: unit.map(str::to_string),
                    explanation,
                });
            }
        }
        outcomes.push(SectionOutcome {
            label: section.label(),
            checked: targets.len(),
            failures,
        });
    }
    Ok(outcomes)
}

/// Decode every flat layout of `file` and render it as a [`Unit`], in layout order.
///
/// Decoding reads each segment but does not decompress any array.
///
/// # Errors
///
/// Returns an error if a segment cannot be read or decoded.
pub async fn collect_units(
    session: &VortexSession,
    file: &VortexFile,
    options: DumpOptions,
) -> VortexResult<Vec<Unit>> {
    let mut flats = Vec::new();
    collect_flat_layouts(file.footer().layout(), "", "", &mut flats)?;

    let segment_source = file.segment_source();
    futures::stream::iter(flats)
        .map(|flat| {
            let segment_source = Arc::clone(&segment_source);
            async move {
                let array = decode_flat(session, segment_source, &flat.layout).await?;
                VortexResult::Ok(Unit {
                    text: render_unit(&flat.path, &flat.layouts, &array, options),
                    path: flat.path,
                })
            }
        })
        .buffered(FETCH_CONCURRENCY)
        .try_collect()
        .await
}

struct FlatLeaf {
    path: String,
    layouts: String,
    layout: LayoutRef,
}

fn collect_flat_layouts(
    layout: &LayoutRef,
    path: &str,
    parent_layouts: &str,
    out: &mut Vec<FlatLeaf>,
) -> VortexResult<()> {
    let layouts = if parent_layouts.is_empty() {
        layout.encoding_id().to_string()
    } else {
        format!("{parent_layouts}/{}", layout.encoding_id())
    };
    if layout.is::<Flat>() {
        out.push(FlatLeaf {
            path: path.to_string(),
            layouts,
            layout: Arc::clone(layout),
        });
        return Ok(());
    }
    for (child, child_type) in layout.children()?.iter().zip(layout.child_types()) {
        collect_flat_layouts(child, &child_path(path, &child_type), &layouts, out)?;
    }
    Ok(())
}

fn child_path(parent: &str, child: &LayoutChildType) -> String {
    match child {
        LayoutChildType::Field(name) if parent.is_empty() => name.to_string(),
        LayoutChildType::Field(name) => format!("{parent}.{name}"),
        LayoutChildType::Chunk((idx, _)) => format!("{parent}[{idx}]"),
        LayoutChildType::Auxiliary(name) | LayoutChildType::Transparent(name) => {
            format!("{parent}/{name}")
        }
    }
}

async fn decode_flat(
    session: &VortexSession,
    segment_source: Arc<dyn SegmentSource>,
    layout: &LayoutRef,
) -> VortexResult<ArrayRef> {
    let flat = layout.as_::<Flat>();
    let row_count = flat.row_count();
    let len = usize::try_from(row_count)
        .map_err(|_| vortex_err!("row count {row_count} does not fit in usize"))?;
    let segment = segment_source.request(flat.segment_id()).await?;
    let parts = match flat.array_tree() {
        Some(array_tree) => {
            SerializedArray::from_flatbuffer_and_segment(array_tree.clone(), segment)?
        }
        None => SerializedArray::try_from(segment)?,
    };
    parts.decode(flat.dtype(), len, flat.array_ctx(), session)
}

fn render_unit(path: &str, layouts: &str, array: &ArrayRef, options: DumpOptions) -> String {
    let mut display = array.tree_display_builder().with(EncodingSummaryExtractor);
    if options.nbytes {
        display = display.with(NbytesExtractor);
    }
    if options.stats {
        display = display.with(StatsExtractor);
    }
    if options.metadata {
        display = display.with(MetadataExtractor);
    }
    if options.buffers {
        display = display.with(BufferExtractor {
            show_percent: false,
        });
    }

    format!(
        "unit: {path} rows={} layouts={layouts}\n{display}",
        array.len()
    )
}

#[cfg(test)]
mod tests {
    use vortex::VortexSessionDefault;
    use vortex::array::IntoArray;
    use vortex::array::arrays::PrimitiveArray;
    use vortex::array::arrays::StructArray;
    use vortex::array::arrays::VarBinArray;
    use vortex::buffer::ByteBufferMut;
    use vortex::file::WriteOptionsSessionExt;

    use super::*;

    async fn units(session: &VortexSession, excludes: &[&str]) -> anyhow::Result<Vec<Unit>> {
        let strings: Vec<&str> = ["alpha", "beta", "gamma"]
            .into_iter()
            .cycle()
            .take(10_000)
            .collect();
        let array = StructArray::from_fields(&[
            ("ints", PrimitiveArray::from_iter(0i64..10_000).into_array()),
            ("strs", VarBinArray::from(strings).into_array()),
        ])?
        .into_array();

        let mut buf = ByteBufferMut::empty();
        session
            .write_options()
            .write(&mut buf, array.to_array_stream())
            .await?;
        let file = session.open_options().open_buffer(buf.freeze())?;

        let excludes = excludes
            .iter()
            .map(|pattern| Glob::new(pattern))
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(collect_units(session, &file, DumpOptions::default())
            .await?
            .into_iter()
            .filter(|unit| !excludes.iter().any(|glob| glob.matches(&unit.path)))
            .collect())
    }

    async fn check(checks: &str, excludes: &[&str]) -> anyhow::Result<Vec<SectionOutcome>> {
        let session = VortexSession::default();
        let units = units(&session, excludes).await?;
        run_checks(&units, &parse_check_file(checks)?)
    }

    #[test]
    fn glob_matches_whole_path_with_literal_brackets() -> anyhow::Result<()> {
        let glob = Glob::new("lineitem.l_comment/data[*]")?;
        assert!(glob.matches("lineitem.l_comment/data[0]"));
        assert!(glob.matches("lineitem.l_comment/data[12]"));
        assert!(!glob.matches("lineitem.l_comment/zones"));
        assert!(!glob.matches("xlineitem.l_comment/data[0]"));
        assert!(Glob::new("*/zones")?.matches("a.b/zones"));
        assert!(Glob::new("ints?zones")?.matches("ints/zones"));
        Ok(())
    }

    #[test]
    fn check_file_splits_into_sections() -> anyhow::Result<()> {
        let sections =
            parse_check_file("check: a\n# units: x[*]\nnot: b\n\nunits: */zones\ncheck: c\n")?;
        assert_eq!(sections.len(), 3);
        assert!(sections[0].selector.is_none());
        assert_eq!(sections[0].directives, "check: a\n");
        assert_eq!(sections[1].line, 2);
        assert_eq!(sections[1].directives, "not: b\n\n");
        assert_eq!(sections[2].line, 5);
        assert!(
            sections[2]
                .selector
                .as_ref()
                .is_some_and(|glob| glob.matches("x/zones"))
        );
        Ok(())
    }

    #[tokio::test]
    async fn whole_file_checks_match_partially_in_order() -> anyhow::Result<()> {
        let outcomes = check(
            "check: unit: ints\ncheck: root: vortex.\ncheck: unit: strs\n",
            &[],
        )
        .await?;
        assert_eq!(outcomes.len(), 1);
        assert!(outcomes[0].passed(), "{outcomes:?}");

        let outcomes = check("check: unit: strs\ncheck: unit: ints\n", &[]).await?;
        assert!(!outcomes[0].passed());
        Ok(())
    }

    #[tokio::test]
    async fn unit_sections_check_every_matching_unit() -> anyhow::Result<()> {
        let outcomes = check(
            "units: strs*\ncheck: root: vortex.\n\nunits: ints*\ncheck: vortex.no_such_encoding\n",
            &[],
        )
        .await?;
        assert_eq!(outcomes.len(), 2);
        assert!(outcomes[0].passed(), "{outcomes:?}");
        assert!(outcomes[0].checked > 0);
        assert_eq!(outcomes[1].failures.len(), outcomes[1].checked);
        assert!(outcomes[1].failures.iter().all(|f| f.unit.is_some()));
        Ok(())
    }

    #[tokio::test]
    async fn sections_matching_no_units_fail() -> anyhow::Result<()> {
        let outcomes = check("units: no_such_column*\ncheck: root:\n", &[]).await?;
        assert_eq!(outcomes[0].checked, 0);
        assert!(!outcomes[0].passed());
        Ok(())
    }

    #[tokio::test]
    async fn excluded_units_are_not_checked() -> anyhow::Result<()> {
        let outcomes = check("not: unit: ints\n", &["ints*"]).await?;
        assert!(outcomes[0].passed(), "{outcomes:?}");
        Ok(())
    }
}
