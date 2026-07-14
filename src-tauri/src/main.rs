// Prevents an extra console window on Windows in release. No effect on macOS.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::cmp::Ordering;
use std::collections::HashMap;
use std::fs::File;
use std::sync::Mutex;

use arrow::array::{new_empty_array, Array, ArrayRef, UInt32Array};
use arrow::compute::{concat, sort_to_indices, take, SortOptions};
use arrow::datatypes::{DataType, SchemaRef};
use arrow::record_batch::RecordBatch;
use arrow::util::display::{ArrayFormatter, FormatOptions};
use parquet::arrow::arrow_reader::{
    ArrowReaderMetadata, ArrowReaderOptions, ParquetRecordBatchReaderBuilder, RowSelection,
    RowSelector,
};
use parquet::arrow::ProjectionMask;
use serde::{Deserialize, Serialize};
use tauri::{Emitter, Manager, State};

// Cap on how many matching rows a search will collect, to bound memory/time on
// huge files. Beyond this the result set is marked truncated.
const SEARCH_CAP: usize = 100_000;

// Upper bound on rows fetched in a single `get_rows` call (defense in depth
// against an over-large `limit`).
const MAX_PAGE: usize = 10_000;

// Cap on a single rendered cell's length, so a huge binary/blob value can't
// produce a multi-megabyte string in the UI.
const CELL_MAX_CHARS: usize = 2_000;

// ---------------------------------------------------------------------------
// Types serialized to the frontend
// ---------------------------------------------------------------------------

#[derive(Serialize, Clone)]
struct ColumnInfo {
    name: String,
    #[serde(rename = "type")]
    type_name: String,
    numeric: bool,
}

#[derive(Serialize, Clone)]
struct FileMeta {
    path: String,
    file_name: String,
    file_size: u64,
    num_rows: i64,
    num_columns: usize,
    num_row_groups: usize,
    compression: String,
    created_by: Option<String>,
    version: i32,
    columns: Vec<ColumnInfo>,
}

#[derive(Deserialize, Clone, PartialEq)]
struct SortSpec {
    column: usize,
    ascending: bool,
}

/// One condition of an advanced filter, e.g. column 3 "gt" "500".
#[derive(Deserialize, Clone, PartialEq)]
struct Condition {
    column: usize,
    /// One of: contains, not_contains, equals, not_equals, starts_with,
    /// ends_with, regex, gt, gte, lt, lte, is_null, is_not_null.
    op: String,
    #[serde(default)]
    value: String,
    #[serde(default)]
    case_sensitive: bool,
}

#[derive(Deserialize, Clone, PartialEq)]
#[serde(tag = "mode", rename_all = "lowercase")]
enum FilterSpec {
    /// Plain substring search over one column (`Some`) or all columns (`None`).
    Simple {
        query: String,
        column: Option<usize>,
        #[serde(default)]
        case_sensitive: bool,
    },
    /// A set of conditions combined with AND (`combine = "and"`) or OR.
    Advanced {
        conditions: Vec<Condition>,
        #[serde(default = "default_combine")]
        combine: String,
    },
}

fn default_combine() -> String {
    "and".to_string()
}

impl FilterSpec {
    /// Whether this filter actually restricts anything.
    fn is_active(&self) -> bool {
        match self {
            FilterSpec::Simple { query, .. } => !query.is_empty(),
            FilterSpec::Advanced { conditions, .. } => !conditions.is_empty(),
        }
    }
}

#[derive(Serialize)]
struct RowsResponse {
    rows: Vec<Vec<Option<String>>>,
    /// Global (file) row index for each returned row, in display order. Lets the
    /// frontend pin per-cell edits to a stable row regardless of sort/filter.
    indices: Vec<u32>,
    total_rows: usize,
    offset: usize,
    /// True when an active filter hit the SEARCH_CAP and results are partial.
    truncated: bool,
}

// ---------------------------------------------------------------------------
// Backend state
// ---------------------------------------------------------------------------

struct FileCache {
    /// Parsed footer/schema, loaded once so paging never re-reads it.
    meta: ArrowReaderMetadata,
    schema: SchemaRef,
    num_rows: usize,
    num_columns: usize,
    /// Sorted permutation of global row indices for the active sort.
    sort_cache: Option<(SortSpec, Vec<u32>)>,
    /// Matching global row indices for the active filter (+ truncated flag).
    filter_cache: Option<(FilterSpec, Vec<u32>, bool)>,
    /// Fully materialized single columns, kept for sorting.
    column_cache: HashMap<usize, ArrayRef>,
}

#[derive(Default)]
struct AppState {
    files: Mutex<HashMap<String, FileCache>>,
    /// A file passed at launch (CLI arg or macOS "Open With") awaiting pickup.
    pending_open: Mutex<Option<String>>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn load_metadata(path: &str) -> Result<ArrowReaderMetadata, String> {
    let file = File::open(path).map_err(|e| format!("Cannot open file: {e}"))?;
    let opts = ArrowReaderOptions::new();
    ArrowReaderMetadata::load(&file, opts).map_err(|e| format!("Not a valid Parquet file: {e}"))
}

fn friendly_type(dt: &DataType) -> String {
    use DataType::*;
    match dt {
        Boolean => "bool".into(),
        Int8 => "int8".into(),
        Int16 => "int16".into(),
        Int32 => "int32".into(),
        Int64 => "int64".into(),
        UInt8 => "uint8".into(),
        UInt16 => "uint16".into(),
        UInt32 => "uint32".into(),
        UInt64 => "uint64".into(),
        Float16 | Float32 => "float".into(),
        Float64 => "double".into(),
        Utf8 | LargeUtf8 => "string".into(),
        Binary | LargeBinary | FixedSizeBinary(_) => "binary".into(),
        Date32 | Date64 => "date".into(),
        Time32(_) | Time64(_) => "time".into(),
        Timestamp(_, _) => "timestamp".into(),
        Decimal128(_, _) | Decimal256(_, _) => "decimal".into(),
        List(_) | LargeList(_) | FixedSizeList(_, _) => "list".into(),
        Struct(_) => "struct".into(),
        Map(_, _) => "map".into(),
        other => format!("{other:?}").to_lowercase(),
    }
}

fn is_numeric(dt: &DataType) -> bool {
    use DataType::*;
    matches!(
        dt,
        Int8 | Int16
            | Int32
            | Int64
            | UInt8
            | UInt16
            | UInt32
            | UInt64
            | Float16
            | Float32
            | Float64
            | Decimal128(_, _)
            | Decimal256(_, _)
    )
}

/// Turns a batch into rows of stringified cells, appending to `out`.
/// Nulls become `None` so the frontend can style them distinctly.
fn append_batch_rows(
    batch: &RecordBatch,
    out: &mut Vec<Vec<Option<String>>>,
) -> Result<(), String> {
    let opts = FormatOptions::default().with_null("");
    let ncols = batch.num_columns();
    let formatters: Vec<ArrayFormatter> = (0..ncols)
        .map(|c| ArrayFormatter::try_new(batch.column(c).as_ref(), &opts))
        .collect::<Result<_, _>>()
        .map_err(|e| format!("Formatting error: {e}"))?;

    for row in 0..batch.num_rows() {
        let mut record = Vec::with_capacity(ncols);
        for c in 0..ncols {
            if batch.column(c).is_null(row) {
                record.push(None);
            } else {
                let v = formatters[c].value(row).to_string();
                let v = if v.chars().count() > CELL_MAX_CHARS {
                    let mut t: String = v.chars().take(CELL_MAX_CHARS).collect();
                    t.push('…');
                    t
                } else {
                    v
                };
                record.push(Some(v));
            }
        }
        out.push(record);
    }
    Ok(())
}

/// Reads a contiguous window `[offset, offset+limit)` in file order. This skips
/// entire row groups that fall before `offset`, so it stays cheap deep into
/// huge files.
fn read_contiguous(
    meta: &ArrowReaderMetadata,
    path: &str,
    offset: usize,
    limit: usize,
) -> Result<Vec<Vec<Option<String>>>, String> {
    if limit == 0 {
        return Ok(vec![]);
    }
    let file = File::open(path).map_err(|e| format!("Cannot open file: {e}"))?;
    let reader = ParquetRecordBatchReaderBuilder::new_with_metadata(file, meta.clone())
        .with_offset(offset)
        .with_limit(limit)
        .with_batch_size(limit)
        .build()
        .map_err(|e| format!("Read error: {e}"))?;

    let mut out = Vec::with_capacity(limit);
    for batch in reader {
        let batch = batch.map_err(|e| format!("Read error: {e}"))?;
        append_batch_rows(&batch, &mut out)?;
    }
    Ok(out)
}

/// Builds a RowSelection selecting exactly the given ascending, unique row
/// indices (skip the gaps, select the hits).
fn selection_for(indices: &[u32]) -> RowSelection {
    let mut selectors = Vec::new();
    let mut cursor: u32 = 0;
    let mut run: u32 = 0;
    for &g in indices {
        if g > cursor {
            if run > 0 {
                selectors.push(RowSelector::select(run as usize));
                run = 0;
            }
            selectors.push(RowSelector::skip((g - cursor) as usize));
        }
        run += 1;
        cursor = g + 1;
    }
    if run > 0 {
        selectors.push(RowSelector::select(run as usize));
    }
    RowSelection::from(selectors)
}

/// Reads an arbitrary set of (possibly scattered) global row indices, returned
/// in the same order as `page`. Uses a RowSelection so only the pages holding
/// those rows are decoded.
fn read_scattered(
    meta: &ArrowReaderMetadata,
    path: &str,
    page: &[u32],
) -> Result<Vec<Vec<Option<String>>>, String> {
    if page.is_empty() {
        return Ok(vec![]);
    }
    // Sort by file position (required for RowSelection) while remembering each
    // row's display position so we can restore the requested order afterward.
    let mut ordered: Vec<(u32, usize)> =
        page.iter().enumerate().map(|(pos, &g)| (g, pos)).collect();
    ordered.sort_by_key(|(g, _)| *g);
    let sorted_global: Vec<u32> = ordered.iter().map(|(g, _)| *g).collect();

    let file = File::open(path).map_err(|e| format!("Cannot open file: {e}"))?;
    let reader = ParquetRecordBatchReaderBuilder::new_with_metadata(file, meta.clone())
        .with_row_selection(selection_for(&sorted_global))
        .with_batch_size(sorted_global.len())
        .build()
        .map_err(|e| format!("Read error: {e}"))?;

    let mut file_order = Vec::with_capacity(sorted_global.len());
    for batch in reader {
        let batch = batch.map_err(|e| format!("Read error: {e}"))?;
        append_batch_rows(&batch, &mut file_order)?;
    }
    if file_order.len() != ordered.len() {
        return Err("Row selection returned an unexpected count".into());
    }

    // Scatter back into requested (display) order.
    let mut result: Vec<Vec<Option<String>>> = vec![Vec::new(); page.len()];
    for (k, (_, disp_pos)) in ordered.into_iter().enumerate() {
        result[disp_pos] = std::mem::take(&mut file_order[k]);
    }
    Ok(result)
}

/// Loads one full column (all row groups, projected to just that column) as a
/// single contiguous array. Memory scales with one column, not the whole file.
fn load_full_column(
    cache: &mut FileCache,
    path: &str,
    col: usize,
) -> Result<ArrayRef, String> {
    if let Some(a) = cache.column_cache.get(&col) {
        return Ok(a.clone());
    }
    let descr = cache.meta.metadata().file_metadata().schema_descr();
    let mask = ProjectionMask::roots(descr, [col]);
    let file = File::open(path).map_err(|e| format!("Cannot open file: {e}"))?;
    let reader = ParquetRecordBatchReaderBuilder::new_with_metadata(file, cache.meta.clone())
        .with_projection(mask)
        .with_batch_size(16384)
        .build()
        .map_err(|e| format!("Read error: {e}"))?;

    let mut arrays: Vec<ArrayRef> = Vec::new();
    for batch in reader {
        let batch = batch.map_err(|e| format!("Read error: {e}"))?;
        arrays.push(batch.column(0).clone());
    }
    let full: ArrayRef = if arrays.is_empty() {
        new_empty_array(cache.schema.field(col).data_type())
    } else {
        let refs: Vec<&dyn Array> = arrays.iter().map(|a| a.as_ref()).collect();
        concat(&refs).map_err(|e| format!("Concat error: {e}"))?
    };
    cache.column_cache.insert(col, full.clone());
    Ok(full)
}

/// Ensures `cache.sort_cache` holds a full-file permutation for `spec`.
fn ensure_sort(cache: &mut FileCache, path: &str, spec: &SortSpec) -> Result<(), String> {
    if cache.sort_cache.as_ref().map(|(s, _)| s == spec).unwrap_or(false) {
        return Ok(());
    }
    let col = load_full_column(cache, path, spec.column)?;
    let opts = SortOptions {
        descending: !spec.ascending,
        nulls_first: false,
    };
    let idx = sort_to_indices(col.as_ref(), Some(opts), None)
        .map_err(|e| format!("Sort error: {e}"))?;
    let perm: Vec<u32> = idx.values().to_vec();
    cache.sort_cache = Some((spec.clone(), perm));
    Ok(())
}

/// Sorts the given filtered global indices by column `spec.column`.
fn sort_filtered(
    cache: &mut FileCache,
    path: &str,
    filtered: &[u32],
    spec: &SortSpec,
) -> Result<Vec<u32>, String> {
    let col = load_full_column(cache, path, spec.column)?;
    let idx_arr = UInt32Array::from(filtered.to_vec());
    let sub = take(col.as_ref(), &idx_arr, None).map_err(|e| format!("Take error: {e}"))?;
    let opts = SortOptions {
        descending: !spec.ascending,
        nulls_first: false,
    };
    let order = sort_to_indices(sub.as_ref(), Some(opts), None)
        .map_err(|e| format!("Sort error: {e}"))?;
    Ok(order.values().iter().map(|&p| filtered[p as usize]).collect())
}

/// Dispatches a filter to the simple or advanced scanner.
fn run_filter(
    meta: &ArrowReaderMetadata,
    path: &str,
    num_columns: usize,
    filter: &FilterSpec,
) -> Result<(Vec<u32>, bool), String> {
    match filter {
        FilterSpec::Simple {
            query,
            column,
            case_sensitive,
        } => run_simple(meta, path, num_columns, query, *column, *case_sensitive),
        FilterSpec::Advanced {
            conditions,
            combine,
        } => run_advanced(
            meta,
            path,
            num_columns,
            conditions,
            combine.eq_ignore_ascii_case("or"),
        ),
    }
}

/// Streams the file (projected to the searched column, or all columns) and
/// collects global indices of rows containing `query` as a substring.
fn run_simple(
    meta: &ArrowReaderMetadata,
    path: &str,
    num_columns: usize,
    query: &str,
    column: Option<usize>,
    case_sensitive: bool,
) -> Result<(Vec<u32>, bool), String> {
    if query.is_empty() {
        return Ok((Vec::new(), false));
    }
    let needle = if case_sensitive {
        query.to_string()
    } else {
        query.to_lowercase()
    };
    let descr = meta.metadata().file_metadata().schema_descr();
    let proj: Vec<usize> = match column {
        Some(c) => vec![c],
        None => (0..num_columns).collect(),
    };
    let mask = ProjectionMask::roots(descr, proj);
    let file = File::open(path).map_err(|e| format!("Cannot open file: {e}"))?;
    let reader = ParquetRecordBatchReaderBuilder::new_with_metadata(file, meta.clone())
        .with_projection(mask)
        .with_batch_size(8192)
        .build()
        .map_err(|e| format!("Read error: {e}"))?;

    let opts = FormatOptions::default().with_null("");
    let mut indices: Vec<u32> = Vec::new();
    let mut global: u32 = 0;
    let mut truncated = false;

    'outer: for batch in reader {
        let batch = batch.map_err(|e| format!("Read error: {e}"))?;
        let ncols = batch.num_columns();
        let formatters: Vec<ArrayFormatter> = (0..ncols)
            .map(|c| ArrayFormatter::try_new(batch.column(c).as_ref(), &opts))
            .collect::<Result<_, _>>()
            .map_err(|e| format!("Formatting error: {e}"))?;

        for row in 0..batch.num_rows() {
            let mut matched = false;
            for c in 0..ncols {
                if batch.column(c).is_null(row) {
                    continue;
                }
                let cell = formatters[c].value(row).to_string();
                let hay = if case_sensitive {
                    cell
                } else {
                    cell.to_lowercase()
                };
                if hay.contains(&needle) {
                    matched = true;
                    break;
                }
            }
            if matched {
                indices.push(global);
                if indices.len() >= SEARCH_CAP {
                    truncated = true;
                    break 'outer;
                }
            }
            global += 1;
        }
    }
    Ok((indices, truncated))
}

// A comparison/predicate operator for an advanced condition.
enum Op {
    Contains,
    NotContains,
    Equals,
    NotEquals,
    StartsWith,
    EndsWith,
    Regex,
    Gt,
    Gte,
    Lt,
    Lte,
    IsNull,
    IsNotNull,
}

/// A condition pre-processed for the row scan (regex compiled, needle cached).
struct Prepared {
    column: usize,
    op: Op,
    needle: String,
    regex: Option<regex::Regex>,
    case_sensitive: bool,
}

fn prepare(cond: &Condition) -> Result<Prepared, String> {
    let op = match cond.op.as_str() {
        "contains" => Op::Contains,
        "not_contains" => Op::NotContains,
        "equals" => Op::Equals,
        "not_equals" => Op::NotEquals,
        "starts_with" => Op::StartsWith,
        "ends_with" => Op::EndsWith,
        "regex" => Op::Regex,
        "gt" => Op::Gt,
        "gte" => Op::Gte,
        "lt" => Op::Lt,
        "lte" => Op::Lte,
        "is_null" | "is_empty" => Op::IsNull,
        "is_not_null" | "is_not_empty" => Op::IsNotNull,
        other => return Err(format!("Unknown operator: {other}")),
    };
    let regex = if matches!(op, Op::Regex) {
        Some(
            regex::RegexBuilder::new(&cond.value)
                .case_insensitive(!cond.case_sensitive)
                .build()
                .map_err(|e| format!("Invalid regex: {e}"))?,
        )
    } else {
        None
    };
    Ok(Prepared {
        column: cond.column,
        op,
        needle: cond.value.clone(),
        regex,
        case_sensitive: cond.case_sensitive,
    })
}

/// Compares two cell strings numerically when both parse as numbers, else
/// lexicographically.
fn compare_vals(a: &str, b: &str) -> Option<Ordering> {
    match (a.trim().parse::<f64>(), b.trim().parse::<f64>()) {
        (Ok(x), Ok(y)) => x.partial_cmp(&y),
        _ => Some(a.cmp(b)),
    }
}

/// Evaluates one prepared condition against a cell (`None` == null).
fn eval(p: &Prepared, cell: Option<&str>) -> bool {
    match p.op {
        Op::IsNull => return cell.is_none(),
        Op::IsNotNull => return cell.is_some(),
        _ => {}
    }
    let Some(s) = cell else { return false };
    match p.op {
        Op::Regex => p.regex.as_ref().map(|re| re.is_match(s)).unwrap_or(false),
        Op::Gt => matches!(compare_vals(s, &p.needle), Some(Ordering::Greater)),
        Op::Gte => matches!(
            compare_vals(s, &p.needle),
            Some(Ordering::Greater | Ordering::Equal)
        ),
        Op::Lt => matches!(compare_vals(s, &p.needle), Some(Ordering::Less)),
        Op::Lte => matches!(
            compare_vals(s, &p.needle),
            Some(Ordering::Less | Ordering::Equal)
        ),
        _ => {
            // String predicates, honoring case sensitivity.
            let (hay, needle) = if p.case_sensitive {
                (s.to_string(), p.needle.clone())
            } else {
                (s.to_lowercase(), p.needle.to_lowercase())
            };
            match p.op {
                Op::Contains => hay.contains(&needle),
                Op::NotContains => !hay.contains(&needle),
                Op::Equals => hay == needle,
                Op::NotEquals => hay != needle,
                Op::StartsWith => hay.starts_with(&needle),
                Op::EndsWith => hay.ends_with(&needle),
                _ => false,
            }
        }
    }
}

/// Streams the file and collects global indices of rows satisfying the given
/// conditions, combined with AND (default) or OR when `any` is true.
fn run_advanced(
    meta: &ArrowReaderMetadata,
    path: &str,
    num_columns: usize,
    conditions: &[Condition],
    any: bool,
) -> Result<(Vec<u32>, bool), String> {
    let prepared: Vec<Prepared> = conditions.iter().map(prepare).collect::<Result<_, _>>()?;
    for p in &prepared {
        if p.column >= num_columns {
            return Err("Condition references an invalid column".to_string());
        }
    }

    let file = File::open(path).map_err(|e| format!("Cannot open file: {e}"))?;
    let reader = ParquetRecordBatchReaderBuilder::new_with_metadata(file, meta.clone())
        .with_batch_size(8192)
        .build()
        .map_err(|e| format!("Read error: {e}"))?;

    let opts = FormatOptions::default().with_null("");
    let mut indices: Vec<u32> = Vec::new();
    let mut global: u32 = 0;
    let mut truncated = false;

    'outer: for batch in reader {
        let batch = batch.map_err(|e| format!("Read error: {e}"))?;
        let ncols = batch.num_columns();
        let formatters: Vec<ArrayFormatter> = (0..ncols)
            .map(|c| ArrayFormatter::try_new(batch.column(c).as_ref(), &opts))
            .collect::<Result<_, _>>()
            .map_err(|e| format!("Formatting error: {e}"))?;

        for row in 0..batch.num_rows() {
            let mut result = !any; // AND starts true, OR starts false
            for p in &prepared {
                let cell = if batch.column(p.column).is_null(row) {
                    None
                } else {
                    Some(formatters[p.column].value(row).to_string())
                };
                let m = eval(p, cell.as_deref());
                if any {
                    if m {
                        result = true;
                        break;
                    }
                } else if !m {
                    result = false;
                    break;
                }
            }
            if result {
                indices.push(global);
                if indices.len() >= SEARCH_CAP {
                    truncated = true;
                    break 'outer;
                }
            }
            global += 1;
        }
    }
    Ok((indices, truncated))
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

#[tauri::command]
async fn open_file(state: State<'_, AppState>, path: String) -> Result<FileMeta, String> {
    let meta = load_metadata(&path)?;
    let schema = meta.schema().clone();
    let pq = meta.metadata();
    let fmd = pq.file_metadata();

    let num_rows = fmd.num_rows();
    let num_row_groups = pq.num_row_groups();
    let num_columns = schema.fields().len();

    // Row indices are tracked as u32 throughout; refuse files that would overflow
    // it rather than silently wrapping.
    if num_rows > u32::MAX as i64 {
        return Err(format!(
            "File has {num_rows} rows, which exceeds the {} row limit.",
            u32::MAX
        ));
    }

    // Collect the distinct compression codecs used across row group 0.
    let compression = if num_row_groups > 0 {
        let rg = pq.row_group(0);
        let mut codecs: Vec<String> = Vec::new();
        for i in 0..rg.num_columns() {
            let c = format!("{:?}", rg.column(i).compression()).to_uppercase();
            if !codecs.contains(&c) {
                codecs.push(c);
            }
        }
        codecs.join(", ")
    } else {
        "—".into()
    };

    let file_size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let file_name = std::path::Path::new(&path)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| path.clone());

    let columns: Vec<ColumnInfo> = schema
        .fields()
        .iter()
        .map(|f| ColumnInfo {
            name: f.name().clone(),
            type_name: friendly_type(f.data_type()),
            numeric: is_numeric(f.data_type()),
        })
        .collect();

    let file_meta = FileMeta {
        path: path.clone(),
        file_name,
        file_size,
        num_rows,
        num_columns,
        num_row_groups,
        compression,
        created_by: fmd.created_by().map(|s| s.to_string()),
        version: fmd.version(),
        columns,
    };

    let cache = FileCache {
        meta,
        schema,
        num_rows: num_rows.max(0) as usize,
        num_columns,
        sort_cache: None,
        filter_cache: None,
        column_cache: HashMap::new(),
    };

    state.files.lock().unwrap().insert(path, cache);
    Ok(file_meta)
}

#[tauri::command]
async fn get_rows(
    state: State<'_, AppState>,
    path: String,
    offset: usize,
    limit: usize,
    sort: Option<SortSpec>,
    filter: Option<FilterSpec>,
) -> Result<RowsResponse, String> {
    let mut files = state.files.lock().unwrap();
    let cache = files.get_mut(&path).ok_or("File is not open")?;
    let num_rows = cache.num_rows;
    let num_columns = cache.num_columns;
    let meta = cache.meta.clone();

    // Validate/clamp caller-supplied parameters (defense in depth; the advanced
    // filter path validates its own columns in `run_advanced`).
    let limit = limit.min(MAX_PAGE);
    if let Some(s) = &sort {
        if s.column >= num_columns {
            return Err("Sort column is out of range".to_string());
        }
    }
    if let Some(FilterSpec::Simple {
        column: Some(c), ..
    }) = &filter
    {
        if *c >= num_columns {
            return Err("Filter column is out of range".to_string());
        }
    }

    // Resolve the active filter into a set of matching global indices.
    let filtered: Option<Vec<u32>> = match &filter {
        Some(f) if f.is_active() => {
            let hit = cache
                .filter_cache
                .as_ref()
                .map(|(cf, _, _)| cf == f)
                .unwrap_or(false);
            if !hit {
                let (idx, trunc) = run_filter(&meta, &path, num_columns, f)?;
                cache.filter_cache = Some((f.clone(), idx, trunc));
            }
            Some(cache.filter_cache.as_ref().unwrap().1.clone())
        }
        _ => None,
    };
    let truncated = filtered
        .as_ref()
        .and(cache.filter_cache.as_ref().map(|(_, _, t)| *t))
        .unwrap_or(false);

    // Compute this page's rows and their global indices, in display order.
    let (rows, total_rows, indices) = match (&filter_or_none(&filter), &sort) {
        // Fast path: no sort, no filter -> contiguous read.
        (None, None) => {
            let rows = read_contiguous(&meta, &path, offset, limit)?;
            let idx: Vec<u32> = (0..rows.len()).map(|k| (offset + k) as u32).collect();
            (rows, num_rows, idx)
        }
        // Sort only.
        (None, Some(spec)) => {
            ensure_sort(cache, &path, spec)?;
            let perm = &cache.sort_cache.as_ref().unwrap().1;
            let page = slice_page(perm, offset, limit);
            let rows = read_scattered(&meta, &path, &page)?;
            (rows, num_rows, page)
        }
        // Filter only.
        (Some(_), None) => {
            let fi = filtered.as_ref().unwrap();
            let page = slice_page(fi, offset, limit);
            let rows = read_scattered(&meta, &path, &page)?;
            (rows, fi.len(), page)
        }
        // Filter + sort.
        (Some(_), Some(spec)) => {
            let fi = filtered.as_ref().unwrap();
            let sorted = sort_filtered(cache, &path, fi, spec)?;
            let page = slice_page(&sorted, offset, limit);
            let rows = read_scattered(&meta, &path, &page)?;
            (rows, sorted.len(), page)
        }
    };

    Ok(RowsResponse {
        rows,
        indices,
        total_rows,
        offset,
        truncated,
    })
}

/// A file passed at launch, consumed once by the frontend on startup.
#[tauri::command]
fn take_startup_file(state: State<'_, AppState>) -> Option<String> {
    state.pending_open.lock().unwrap().take()
}

#[tauri::command]
async fn pick_parquet_file(app: tauri::AppHandle) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;
    let picked = app
        .dialog()
        .file()
        .add_filter("Parquet", &["parquet"])
        .blocking_pick_file();
    Ok(picked
        .and_then(|f| f.into_path().ok())
        .map(|p| p.to_string_lossy().to_string()))
}

// Treats an inactive (empty) filter as "no filter" for the match arm above.
fn filter_or_none(filter: &Option<FilterSpec>) -> Option<FilterSpec> {
    match filter {
        Some(f) if f.is_active() => Some(f.clone()),
        _ => None,
    }
}

/// Returns `order[offset..offset+limit]` (clamped) as an owned page.
fn slice_page(order: &[u32], offset: usize, limit: usize) -> Vec<u32> {
    if offset >= order.len() {
        return Vec::new();
    }
    let end = (offset + limit).min(order.len());
    order[offset..end].to_vec()
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            open_file,
            get_rows,
            take_startup_file,
            pick_parquet_file
        ])
        .setup(|app| {
            // A file path may arrive as a CLI arg when launched via `open -a`.
            if let Some(path) = std::env::args().skip(1).find(|a| a.ends_with(".parquet")) {
                *app.state::<AppState>().pending_open.lock().unwrap() = Some(path);
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building ParquetView")
        .run(|app_handle, event| {
            // macOS delivers "Open With" / dock-drop files through this event.
            if let tauri::RunEvent::Opened { urls } = event {
                for url in urls {
                    if let Ok(p) = url.to_file_path() {
                        let path = p.to_string_lossy().to_string();
                        if let Some(state) = app_handle.try_state::<AppState>() {
                            *state.pending_open.lock().unwrap() = Some(path.clone());
                        }
                        let _ = app_handle.emit("open-file", path);
                    }
                }
            }
        });
}

// ---------------------------------------------------------------------------
// Tests — exercise the read engine against a real file.
// Run with: PARQUETVIEW_TEST_FILE=/path/to/sample.parquet cargo test
// Tests are skipped when the env var is unset.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn open_cache(path: &str) -> FileCache {
        let meta = load_metadata(path).unwrap();
        let schema = meta.schema().clone();
        let num_rows = meta.metadata().file_metadata().num_rows().max(0) as usize;
        let num_columns = schema.fields().len();
        FileCache {
            meta,
            schema,
            num_rows,
            num_columns,
            sort_cache: None,
            filter_cache: None,
            column_cache: HashMap::new(),
        }
    }

    fn sample() -> Option<String> {
        std::env::var("PARQUETVIEW_TEST_FILE").ok()
    }

    #[test]
    fn contiguous_paging_skips_row_groups() {
        let Some(p) = sample() else { return };
        let c = open_cache(&p);
        let first = read_contiguous(&c.meta, &p, 0, 5).unwrap();
        assert_eq!(first.len(), 5);
        assert_eq!(first[0][0].as_deref(), Some("1")); // id == row index + 1
        let mid = read_contiguous(&c.meta, &p, 250_000, 3).unwrap();
        assert_eq!(mid[0][0].as_deref(), Some("250001"));
    }

    #[test]
    fn scattered_read_preserves_requested_order() {
        let Some(p) = sample() else { return };
        let c = open_cache(&p);
        let page = vec![250_001u32, 3u32, 499_999u32, 100u32];
        let got = read_scattered(&c.meta, &p, &page).unwrap();
        assert_eq!(got.len(), 4);
        assert_eq!(got[0][0].as_deref(), Some("250002"));
        assert_eq!(got[1][0].as_deref(), Some("4"));
        assert_eq!(got[2][0].as_deref(), Some("500000"));
        assert_eq!(got[3][0].as_deref(), Some("101"));
    }

    #[test]
    fn sort_produces_full_permutation() {
        let Some(p) = sample() else { return };
        let mut c = open_cache(&p);
        let spec = SortSpec { column: 4, ascending: true }; // score
        ensure_sort(&mut c, &p, &spec).unwrap();
        let perm = c.sort_cache.as_ref().unwrap().1.clone();
        assert_eq!(perm.len(), c.num_rows);
        let page: Vec<u32> = perm[0..200].to_vec();
        let rows = read_scattered(&c.meta, &p, &page).unwrap();
        let mut prev = i64::MIN;
        for r in &rows {
            let s: i64 = r[4].as_deref().unwrap().parse().unwrap();
            assert!(s >= prev, "sort not monotonic: {s} < {prev}");
            prev = s;
        }
        assert_eq!(rows[0][4].as_deref(), Some("0"));
    }

    #[test]
    fn search_matches_and_reads_back() {
        let Some(p) = sample() else { return };
        let c = open_cache(&p);
        let filter = FilterSpec::Simple {
            query: "Reno".into(),
            column: Some(1), // city
            case_sensitive: false,
        };
        let (idx, truncated) = run_filter(&c.meta, &p, c.num_columns, &filter).unwrap();
        assert!(!idx.is_empty());
        assert!(!truncated);
        let page: Vec<u32> = idx.iter().take(50).cloned().collect();
        let rows = read_scattered(&c.meta, &p, &page).unwrap();
        for r in &rows {
            assert_eq!(r[1].as_deref(), Some("Reno"));
        }
    }

    #[test]
    fn advanced_filter_and_or() {
        let Some(p) = sample() else { return };
        let c = open_cache(&p);
        // score > 90 AND city equals Reno
        let conds = vec![
            Condition {
                column: 4,
                op: "gt".into(),
                value: "90".into(),
                case_sensitive: false,
            },
            Condition {
                column: 1,
                op: "equals".into(),
                value: "reno".into(),
                case_sensitive: false, // case-insensitive should still match "Reno"
            },
        ];
        let (and_idx, _) = run_advanced(&c.meta, &p, c.num_columns, &conds, false).unwrap();
        assert!(!and_idx.is_empty());
        let page: Vec<u32> = and_idx.iter().take(50).cloned().collect();
        let rows = read_scattered(&c.meta, &p, &page).unwrap();
        for r in &rows {
            assert_eq!(r[1].as_deref(), Some("Reno"));
            let s: i64 = r[4].as_deref().unwrap().parse().unwrap();
            assert!(s > 90, "score {s} should be > 90");
        }
        // OR of the same conditions must match at least as many rows as AND.
        let (or_idx, _) = run_advanced(&c.meta, &p, c.num_columns, &conds, true).unwrap();
        assert!(or_idx.len() >= and_idx.len());
    }
}
