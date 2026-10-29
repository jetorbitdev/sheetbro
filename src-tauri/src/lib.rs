mod mcp;

use std::{
    collections::HashMap,
    fs,
    io::BufWriter,
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Mutex, MutexGuard, PoisonError,
    },
};

use ironcalc::{
    base::{
        cell::CellValue,
        expressions::{
            parser::{stringify::to_english_string, Node},
            token::Error,
            types::{Area, CellReferenceRC},
        },
        types::{CellType, Color, HorizontalAlignment, Style},
        ClipboardData, Model, UserModel,
    },
    export::save_xlsx_to_writer,
    import::load_from_xlsx,
};
use serde::{Deserialize, Serialize};
use tauri::{
    menu::{Menu, MenuItemBuilder, SubmenuBuilder},
    AppHandle, Emitter, Manager, Runtime, State, WebviewWindowBuilder, Window, WindowEvent,
};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

const MAX_ROW: i32 = 1_048_576;
const MAX_COL: i32 = 16_384;
const LOCALE: &str = "en";
const TZ: &str = "UTC";
const LANG: &str = "en";

struct Doc {
    model: UserModel<'static>,
    path: Option<PathBuf>,
    dirty: bool,
    /// IronCalc records one history entry per cell edit. Each item here is the
    /// number of entries one user action produced, so undo/redo can replay a
    /// whole paste or fill at once.
    undo_groups: Vec<usize>,
    redo_groups: Vec<usize>,
    /// Changes whenever the content may have changed, so caches built from it know to refresh
    generation: u64,
    /// The empty workbook a window starts with, not a file that was opened
    blank: bool,
    /// Formatting of the last copied range, for "paste formatting only"
    format_clip: Vec<Vec<Style>>,
}

static GENERATION: AtomicU64 = AtomicU64::new(1);

fn next_generation() -> u64 {
    GENERATION.fetch_add(1, Ordering::Relaxed)
}

impl Doc {
    fn new(model: UserModel<'static>, path: Option<PathBuf>) -> Self {
        Doc {
            model,
            path,
            dirty: false,
            undo_groups: Vec::new(),
            redo_groups: Vec::new(),
            generation: next_generation(),
            blank: false,
            format_clip: Vec::new(),
        }
    }

    fn empty() -> CmdResult<Self> {
        Ok(Doc { blank: true, ..Doc::new(new_model("Untitled")?, None) })
    }

    /// Still the untouched empty workbook, so a file can be opened in its place.
    fn is_blank(&self) -> bool {
        self.blank && !self.dirty && self.path.is_none()
    }

    /// Marks the last `entries` history entries as one undoable action.
    fn record(&mut self, entries: usize) {
        if entries == 0 {
            return;
        }
        self.undo_groups.push(entries);
        // A new action wipes IronCalc's redo stack, so ours goes too
        self.redo_groups.clear();
        self.dirty = true;
        self.generation = next_generation();
    }

    fn replay(&mut self, redo: bool) -> CmdResult<()> {
        let groups = if redo { &mut self.redo_groups } else { &mut self.undo_groups };
        let Some(n) = groups.pop() else { return Ok(()) };
        self.model.pause_evaluation();
        let mut result = Ok(());
        for _ in 0..n {
            result = if redo { self.model.redo() } else { self.model.undo() };
            if result.is_err() {
                break;
            }
        }
        self.model.resume_evaluation();
        self.model.evaluate();
        if redo { &mut self.undo_groups } else { &mut self.redo_groups }.push(n);
        self.dirty = true;
        self.generation = next_generation();
        result
    }
}

struct AppState {
    /// One workbook per window, keyed by window label
    docs: Mutex<HashMap<String, Doc>>,
    /// Label of the window that last had focus: menu commands and AI tools act on it
    active: Mutex<String>,
    /// Files waiting for a window to pick them up once its webview listens. A label
    /// stays here after its files were taken, so that window is not handed another one.
    pending_open: Mutex<HashMap<String, Vec<String>>>,
    /// Server that lets AI tools edit the open workbook
    mcp: Mutex<mcp::Runtime>,
    /// Scratch copy of the workbook for previewing a formula before it is entered
    preview: Mutex<Option<(u64, Model<'static>)>>,
}

/// The workbook of one window, held under the lock of all of them.
struct DocGuard<'a> {
    docs: MutexGuard<'a, HashMap<String, Doc>>,
    label: String,
}

impl Deref for DocGuard<'_> {
    type Target = Doc;
    fn deref(&self) -> &Doc {
        &self.docs[&self.label]
    }
}

impl DerefMut for DocGuard<'_> {
    fn deref_mut(&mut self) -> &mut Doc {
        self.docs.get_mut(&self.label).unwrap()
    }
}

impl AppState {
    /// The workbook of window `label`; a window starts with an empty one.
    fn doc(&self, label: &str) -> DocGuard<'_> {
        let mut docs = self.docs.lock_recovering();
        if !docs.contains_key(label) {
            docs.insert(label.to_string(), Doc::empty().expect("create empty workbook"));
        }
        DocGuard { docs, label: label.to_string() }
    }

    fn active_label(&self) -> String {
        self.active.lock_recovering().clone()
    }

    fn active_doc(&self) -> DocGuard<'_> {
        self.doc(&self.active_label())
    }

    /// A window with nothing in it yet and no file on its way.
    fn is_untouched(&self, label: &str) -> bool {
        !self.pending_open.lock_recovering().contains_key(label)
            && self.docs.lock_recovering().get(label).map_or(true, Doc::is_blank)
    }
}

type CmdResult<T> = Result<T, String>;

/// Locks a mutex even when a thread panicked while holding it. One tool call that panicked
/// must not make every later lock panic too, which would take the whole app down with the
/// unsaved work in it.
trait LockExt<T> {
    fn lock_recovering(&self) -> MutexGuard<'_, T>;
}

impl<T> LockExt<T> for Mutex<T> {
    fn lock_recovering(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Builds the workbook on a plain `Model` so nothing lands in the undo history
/// (`UserModel::set_name` and `set_user_input` both record undo entries).
fn new_base_model(name: &str) -> CmdResult<Model<'static>> {
    let mut m = Model::new_empty("Untitled", LOCALE, TZ, LANG)?;
    m.workbook.name = name.to_string();
    Ok(m)
}

fn new_model(name: &str) -> CmdResult<UserModel<'static>> {
    Ok(UserModel::from_model(new_base_model(name)?))
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "Untitled".into())
}

/// Picks the most likely delimiter by counting candidates in the first line.
fn sniff_delimiter(data: &[u8]) -> u8 {
    let first_line = data.split(|b| *b == b'\n').next().unwrap_or(&[]);
    [b',', b';', b'\t', b'|']
        .into_iter()
        .max_by_key(|d| first_line.iter().filter(|b| *b == d).count())
        .unwrap_or(b',')
}

fn load_csv(path: &Path) -> CmdResult<UserModel<'static>> {
    let mut data = fs::read(path).map_err(|e| e.to_string())?;
    if data.starts_with(&[0xEF, 0xBB, 0xBF]) {
        data.drain(..3);
    }
    let mut model = new_base_model(&file_stem(path))?;
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .delimiter(sniff_delimiter(&data))
        .from_reader(data.as_slice());
    let mut record = csv::ByteRecord::new();
    let mut row = 1;
    while reader.read_byte_record(&mut record).map_err(|e| e.to_string())? {
        for (i, field) in record.iter().enumerate() {
            if field.is_empty() {
                continue;
            }
            let text = String::from_utf8_lossy(field);
            model.set_user_input(0, row, i as i32 + 1, text.into_owned())?;
        }
        row += 1;
    }
    model.evaluate();
    Ok(UserModel::from_model(model))
}

fn load_xlsx(path: &Path) -> CmdResult<UserModel<'static>> {
    let m: Model<'static> = load_from_xlsx(&path.to_string_lossy(), LOCALE, TZ, LANG)
        .map_err(|e| e.to_string())?;
    Ok(UserModel::from_model(m))
}

fn csv_escape(s: &str, delim: char) -> String {
    if s.contains(delim) || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn save_csv(model: &UserModel, sheet: u32, path: &Path) -> CmdResult<()> {
    let dim = model
        .get_model()
        .workbook
        .worksheet(sheet)?
        .dimension();
    let mut out = String::new();
    for r in 1..=dim.max_row {
        let line: Vec<String> = (1..=dim.max_column)
            .map(|c| {
                let v = model.get_formatted_cell_value(sheet, r, c).unwrap_or_default();
                csv_escape(&v, ',')
            })
            .collect();
        out.push_str(&line.join(","));
        out.push('\n');
    }
    fs::write(path, out).map_err(|e| e.to_string())
}

fn save_xlsx(model: &UserModel, path: &Path) -> CmdResult<()> {
    // IronCalc refuses to overwrite, so write to a sibling temp file and rename.
    let tmp = path.with_extension("xlsx.tmp");
    let file = fs::File::create(&tmp).map_err(|e| e.to_string())?;
    save_xlsx_to_writer(model.get_model(), BufWriter::new(file)).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        e.to_string()
    })?;
    fs::rename(&tmp, path).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------- DTOs

#[derive(Serialize, Clone)]
struct SheetInfo {
    name: String,
    rows: i32,
    cols: i32,
}

#[derive(Serialize, Clone)]
struct WorkbookInfo {
    name: String,
    path: Option<String>,
    dirty: bool,
    sheets: Vec<SheetInfo>,
    can_undo: bool,
    can_redo: bool,
    /// untouched empty workbook: opening a file replaces it instead of using a new window
    blank: bool,
}

#[derive(Serialize)]
struct MergeDto {
    row: i32,
    col: i32,
    height: i32,
    width: i32,
}

#[derive(Serialize)]
struct Layout {
    merges: Vec<MergeDto>,
    col_widths: Vec<f64>,
    default_row_height: f64,
    /// (row, height) for rows whose height differs from the default
    row_heights: Vec<(i32, f64)>,
}

#[derive(Serialize)]
struct Cell {
    r: i32,
    c: i32,
    text: String,
    /// "num" | "text" | "bool" | "err"
    kind: &'static str,
    bold: bool,
    italic: bool,
    underline: bool,
    color: Option<String>,
    fill: Option<String>,
    /// "left" | "center" | "right" | null (= by type)
    align: Option<&'static str>,
}

fn info(doc: &Doc) -> WorkbookInfo {
    let model = doc.model.get_model();
    let props = doc.model.get_worksheets_properties();
    let sheets = props
        .into_iter()
        .enumerate()
        .map(|(i, p)| {
            let dim = model
                .workbook
                .worksheet(i as u32)
                .map(|w| w.dimension())
                .ok();
            SheetInfo {
                name: p.name,
                rows: dim.as_ref().map_or(1, |d| d.max_row),
                cols: dim.as_ref().map_or(1, |d| d.max_column),
            }
        })
        .collect();
    WorkbookInfo {
        name: doc
            .path
            .as_deref()
            .map(file_stem)
            .unwrap_or_else(|| doc.model.get_name()),
        path: doc.path.as_ref().map(|p| p.to_string_lossy().to_string()),
        dirty: doc.dirty,
        sheets,
        can_undo: !doc.undo_groups.is_empty(),
        can_redo: !doc.redo_groups.is_empty(),
        blank: doc.is_blank(),
    }
}

fn rgb(color: &Color) -> Option<String> {
    match color {
        Color::Rgb(s) => {
            let s = s.trim_start_matches('#');
            // Excel stores ARGB; keep the last six digits
            let s = if s.len() == 8 { &s[2..] } else { s };
            Some(format!("#{s}"))
        }
        _ => None,
    }
}

// ------------------------------------------------------------ commands

fn load_doc(path: &str) -> CmdResult<Doc> {
    let p = PathBuf::from(path);
    let ext = p
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let (model, keep_path) = match ext.as_str() {
        "xlsx" | "xlsm" => (load_xlsx(&p)?, true),
        "csv" | "tsv" | "txt" => (load_csv(&p)?, false),
        _ => return Err(format!("Unsupported file type: .{ext}")),
    };
    // CSV is imported as a new workbook, so "Save" will ask for an .xlsx name
    Ok(Doc::new(model, keep_path.then_some(p)))
}

#[tauri::command]
fn open_file(path: String, window: Window, state: State<AppState>) -> CmdResult<WorkbookInfo> {
    let loaded = load_doc(&path)?;
    let mut doc = state.doc(window.label());
    *doc = loaded;
    Ok(info(&doc))
}

#[tauri::command]
fn workbook_info(window: Window, state: State<AppState>) -> WorkbookInfo {
    info(&state.doc(window.label()))
}

#[tauri::command]
fn get_layout(sheet: u32, window: Window, state: State<AppState>) -> CmdResult<Layout> {
    let doc = state.doc(window.label());
    let dim = doc.model.get_model().workbook.worksheet(sheet)?.dimension();
    let cols = (dim.max_column + 10).max(26);
    let col_widths = (1..=cols)
        .map(|c| doc.model.get_column_width(sheet, c).unwrap_or(90.0))
        .collect();
    let default_row_height = 25.0;
    let row_heights = (1..=dim.max_row)
        .filter_map(|r| {
            let h = doc.model.get_row_height(sheet, r).ok()?;
            ((h - default_row_height).abs() > 0.5).then_some((r, h))
        })
        .collect();
    let merges = doc
        .model
        .get_merged_cells(sheet)?
        .into_iter()
        .map(|m| MergeDto {
            row: m.row,
            col: m.column,
            height: m.height,
            width: m.width,
        })
        .collect();
    Ok(Layout {
        merges,
        col_widths,
        default_row_height,
        row_heights,
    })
}

#[tauri::command]
fn get_viewport(
    sheet: u32,
    row_start: i32,
    row_end: i32,
    col_start: i32,
    col_end: i32,
    window: Window,
    state: State<AppState>,
) -> CmdResult<Vec<Cell>> {
    let doc = state.doc(window.label());
    let m = &doc.model;
    let mut cells = Vec::new();
    for r in row_start.max(1)..=row_end {
        for c in col_start.max(1)..=col_end {
            let text = m.get_formatted_cell_value(sheet, r, c)?;
            let style = m.get_cell_style(sheet, r, c)?;
            let fill = rgb(&style.fill.color);
            if text.is_empty() && fill.is_none() {
                continue;
            }
            let kind = cell_kind(m.get_cell_type(sheet, r, c)?);
            let align = style.alignment.as_ref().and_then(|a| match a.horizontal {
                HorizontalAlignment::Left => Some("left"),
                HorizontalAlignment::Center | HorizontalAlignment::CenterContinuous => {
                    Some("center")
                }
                HorizontalAlignment::Right => Some("right"),
                _ => None,
            });
            cells.push(Cell {
                r,
                c,
                text,
                kind,
                bold: style.font.b,
                italic: style.font.i,
                underline: style.font.u,
                color: rgb(&style.font.color),
                fill,
                align,
            });
        }
    }
    Ok(cells)
}

fn cell_kind(t: CellType) -> &'static str {
    match t {
        CellType::Number => "num",
        CellType::LogicalValue => "bool",
        CellType::ErrorValue => "err",
        _ => "text",
    }
}

#[tauri::command]
fn get_cell_content(
    sheet: u32,
    row: i32,
    col: i32,
    window: Window,
    state: State<AppState>,
) -> CmdResult<String> {
    state.doc(window.label()).model.get_cell_content(sheet, row, col)
}

/// Writes `values` with their top-left corner at (row, col) as one undo step.
fn write_cells(
    doc: &mut Doc,
    sheet: u32,
    row: i32,
    col: i32,
    values: &[Vec<String>],
) -> CmdResult<()> {
    doc.model.pause_evaluation();
    let mut written = 0;
    let mut result = Ok(());
    'outer: for (i, line) in values.iter().enumerate() {
        for (j, v) in line.iter().enumerate() {
            result = doc
                .model
                .set_user_input(sheet, row + i as i32, col + j as i32, v);
            if result.is_err() {
                break 'outer;
            }
            written += 1;
        }
    }
    doc.model.resume_evaluation();
    doc.model.evaluate();
    // Even on a partial failure the cells already written stay one undo step
    doc.record(written);
    result
}

#[tauri::command]
fn set_cells(
    sheet: u32,
    row: i32,
    col: i32,
    values: Vec<Vec<String>>,
    window: Window,
    state: State<AppState>,
) -> CmdResult<WorkbookInfo> {
    let mut doc = state.doc(window.label());
    write_cells(&mut doc, sheet, row, col, &values)?;
    Ok(info(&doc))
}

#[tauri::command]
fn clear_range(
    sheet: u32,
    row: i32,
    col: i32,
    height: i32,
    width: i32,
    window: Window,
    state: State<AppState>,
) -> CmdResult<WorkbookInfo> {
    let mut doc = state.doc(window.label());
    let area = Area { sheet, row, column: col, width, height };
    doc.model.range_clear_contents(&area)?;
    doc.record(1);
    Ok(info(&doc))
}

#[tauri::command]
fn style_range(
    sheet: u32,
    row: i32,
    col: i32,
    height: i32,
    width: i32,
    path: String,
    value: String,
    window: Window,
    state: State<AppState>,
) -> CmdResult<WorkbookInfo> {
    let mut doc = state.doc(window.label());
    let area = Area { sheet, row, column: col, width, height };
    doc.model.update_range_style(&area, &path, &value)?;
    doc.record(1);
    Ok(info(&doc))
}

#[derive(Clone, Copy, PartialEq, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MergeMode {
    /// one merged cell
    Merge,
    /// one merged cell, centered horizontally
    Center,
    /// every row of the range merged separately
    Across,
    /// every column of the range merged separately
    Down,
}

/// Error text the frontend recognises to ask for confirmation.
const MERGE_CONFLICT: &str = "merge_conflict";

fn count_content(model: &UserModel, a: &Area) -> usize {
    (a.row..a.row + a.height)
        .flat_map(|r| (a.column..a.column + a.width).map(move |c| (r, c)))
        .filter(|&(r, c)| {
            model
                .get_cell_content(a.sheet, r, c)
                .is_ok_and(|s| !s.is_empty())
        })
        .count()
}

/// Merges `area`. IronCalc refuses when more than one cell has content; like
/// Excel we then keep only the upper-left value, but only once the caller
/// confirmed with `discard`. Clearing and merging are one undo step.
fn merge_in(doc: &mut Doc, area: &Area, mode: MergeMode, discard: bool) -> CmdResult<()> {
    let parts: Vec<Area> = match mode {
        MergeMode::Merge | MergeMode::Center => vec![Area { ..*area }],
        MergeMode::Across => (0..area.height)
            .map(|i| Area { row: area.row + i, height: 1, ..*area })
            .collect(),
        MergeMode::Down => (0..area.width)
            .map(|i| Area { column: area.column + i, width: 1, ..*area })
            .collect(),
    };
    let conflicting: Vec<&Area> = parts
        .iter()
        .filter(|a| count_content(&doc.model, a) > 1)
        .collect();
    if !conflicting.is_empty() && !discard {
        return Err(MERGE_CONFLICT.to_string());
    }
    let mut entries = 0;
    let mut result = Ok(());
    for a in conflicting {
        // everything except the anchor: rest of the first row, then the rows below
        if a.width > 1 {
            let rest = Area { column: a.column + 1, width: a.width - 1, height: 1, ..*a };
            if let Err(e) = doc.model.range_clear_contents(&rest) {
                result = Err(e);
                break;
            }
            entries += 1;
        }
        if a.height > 1 {
            let rest = Area { row: a.row + 1, height: a.height - 1, ..*a };
            if let Err(e) = doc.model.range_clear_contents(&rest) {
                result = Err(e);
                break;
            }
            entries += 1;
        }
    }
    if result.is_ok() {
        let merged = match mode {
            MergeMode::Merge => doc.model.merge_cells(area),
            MergeMode::Center => doc.model.merge_cells_center(area),
            MergeMode::Across => doc.model.merge_cells_across(area),
            MergeMode::Down => doc.model.merge_cells_down(area),
        };
        match merged {
            Ok(()) => entries += 1,
            Err(e) => result = Err(e),
        }
    }
    doc.record(entries);
    result
}

fn unmerge_in(doc: &mut Doc, area: &Area) -> CmdResult<()> {
    let before = doc.model.get_merged_cells(area.sheet)?;
    doc.model.unmerge_cells(area)?;
    // IronCalc records nothing when no merge intersects the range
    if doc.model.get_merged_cells(area.sheet)? != before {
        doc.record(1);
    }
    Ok(())
}

#[tauri::command]
fn merge_cells(
    sheet: u32,
    row: i32,
    col: i32,
    height: i32,
    width: i32,
    mode: MergeMode,
    discard: bool,
    window: Window,
    state: State<AppState>,
) -> CmdResult<WorkbookInfo> {
    let mut doc = state.doc(window.label());
    merge_in(&mut doc, &Area { sheet, row, column: col, width, height }, mode, discard)?;
    Ok(info(&doc))
}

#[tauri::command]
fn unmerge_cells(
    sheet: u32,
    row: i32,
    col: i32,
    height: i32,
    width: i32,
    window: Window,
    state: State<AppState>,
) -> CmdResult<WorkbookInfo> {
    let mut doc = state.doc(window.label());
    unmerge_in(&mut doc, &Area { sheet, row, column: col, width, height })?;
    Ok(info(&doc))
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Axis {
    Rows,
    Columns,
}

/// Inserts `count` empty rows or columns before `at`, pushing the cells after them down or right.
#[tauri::command]
fn insert_lines(
    sheet: u32,
    axis: Axis,
    at: i32,
    count: i32,
    window: Window,
    state: State<AppState>,
) -> CmdResult<WorkbookInfo> {
    let mut doc = state.doc(window.label());
    match axis {
        Axis::Rows => doc.model.insert_rows(sheet, at, count)?,
        Axis::Columns => doc.model.insert_columns(sheet, at, count)?,
    }
    doc.record(1);
    Ok(info(&doc))
}

fn remove_lines(
    model: &mut UserModel,
    sheet: u32,
    axis: Axis,
    at: i32,
    count: i32,
) -> CmdResult<()> {
    match axis {
        Axis::Rows => model.delete_rows(sheet, at, count),
        Axis::Columns => model.delete_columns(sheet, at, count),
    }
}

/// Every formula of the workbook as (sheet, row, column, text).
fn all_formulas(model: &UserModel) -> CmdResult<Vec<(u32, i32, i32, String)>> {
    let mut out = Vec::new();
    for (i, sheet) in model.get_model().workbook.worksheets.iter().enumerate() {
        for row in sheet.sheet_data.rows() {
            for (col, cell) in sheet.sheet_data.cells_in_row(row) {
                if cell.has_formula() {
                    let text = model.get_cell_content(i as u32, row, col)?;
                    out.push((i as u32, row, col, text));
                }
            }
        }
    }
    Ok(out)
}

/// Deletes rows or columns as one undo step.
///
/// IronCalc rewrites the formulas that pointed into the deleted cells (to #REF!, or to a
/// shorter range) and its undo brings the cells back but not those formulas. So a delete is
/// tried on a copy first; every formula that the copy does not get back is entered again
/// unchanged before the real delete, and undoing that edit restores its text.
fn delete_lines_in(doc: &mut Doc, sheet: u32, axis: Axis, at: i32, count: i32) -> CmdResult<()> {
    let before = all_formulas(&doc.model)?;
    // no formula can lose its references, so there is nothing to put back on undo
    if before.is_empty() {
        remove_lines(&mut doc.model, sheet, axis, at, count)?;
        doc.record(1);
        return Ok(());
    }
    let mut copy = UserModel::from_bytes(&doc.model.to_bytes(), LANG)?;
    remove_lines(&mut copy, sheet, axis, at, count)?;
    copy.undo()?;
    let mut entries = 0;
    let result = (|| {
        doc.model.pause_evaluation();
        for (s, r, c, text) in &before {
            if copy.get_cell_content(*s, *r, *c)? != *text {
                if let Err(e) = doc.model.set_user_input(*s, *r, *c, text) {
                    doc.model.resume_evaluation();
                    return Err(e);
                }
                entries += 1;
            }
        }
        doc.model.resume_evaluation();
        remove_lines(&mut doc.model, sheet, axis, at, count)?;
        entries += 1;
        Ok(())
    })();
    doc.record(entries);
    result
}

/// Deletes `count` rows or columns starting at `at`, pulling the cells after them up or left.
#[tauri::command]
fn delete_lines(
    sheet: u32,
    axis: Axis,
    at: i32,
    count: i32,
    window: Window,
    state: State<AppState>,
) -> CmdResult<WorkbookInfo> {
    let mut doc = state.doc(window.label());
    delete_lines_in(&mut doc, sheet, axis, at, count)?;
    Ok(info(&doc))
}

/// Which way inserted cells push the cells after them: down a column, or right along a row.
#[derive(Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Shift {
    Down,
    Right,
}

/// Which way the cells after deleted ones are pulled: up a column, or left along a row.
#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Pull {
    Up,
    Left,
}

/// Calls `f` on every node of a formula, parents first.
fn visit(node: &mut Node, f: &mut impl FnMut(&mut Node)) {
    f(node);
    match node {
        Node::ImplicitIntersection { child, .. } | Node::SpillRangeOperator { child } => visit(child, f),
        Node::UnaryKind { right, .. } => visit(right, f),
        Node::OpRangeKind { left, right }
        | Node::OpConcatenateKind { left, right }
        | Node::OpSumKind { left, right, .. }
        | Node::OpProductKind { left, right, .. }
        | Node::OpPowerKind { left, right }
        | Node::CompareKind { left, right, .. } => {
            visit(left, f);
            visit(right, f);
        }
        Node::FunctionKind { args, .. } | Node::NamedFunctionKind { args, .. } => {
            args.iter_mut().for_each(|a| visit(a, f));
        }
        Node::LambdaDefKind { body, .. } => visit(body, f),
        Node::LambdaCallKind { lambda, args } => {
            visit(lambda, f);
            args.iter_mut().for_each(|a| visit(a, f));
        }
        _ => {}
    }
}

/// Rewrites every formula of the workbook for which `change` alters a node of the formula in
/// cell (row, column). Returns how many were typed in again.
fn rewrite_formulas(
    doc: &mut Doc,
    mut change: impl FnMut(&mut Node, (i32, i32)) -> bool,
) -> CmdResult<usize> {
    let model = doc.model.get_model();
    let mut updates = Vec::new();
    for (i, sheet) in model.workbook.worksheets.iter().enumerate() {
        for row in sheet.sheet_data.rows() {
            for (col, cell) in sheet.sheet_data.cells_in_row(row) {
                let Some(f) = cell.get_formula() else { continue };
                let Some((node, _)) = model.parsed_formulas.get(i).and_then(|p| p.get(f as usize))
                else {
                    continue;
                };
                let mut node = Node::clone(node);
                let mut changed = false;
                visit(&mut node, &mut |n| changed |= change(n, (row, col)));
                if changed {
                    let at = CellReferenceRC { sheet: sheet.get_name(), row, column: col };
                    updates.push((i as u32, row, col, format!("={}", to_english_string(&node, &at))));
                }
            }
        }
    }
    for (sheet, row, col, text) in &updates {
        doc.model.set_user_input(*sheet, *row, *col, text)?;
    }
    Ok(updates.len())
}

/// The cells a reference or range node points at, as absolute (row1, col1, row2, col2).
/// References are stored relative to the cell of the formula unless they are absolute.
fn reference_cells(node: &Node, cell: (i32, i32)) -> Option<(u32, [i32; 4])> {
    let at = |absolute: bool, v: i32, base: i32| if absolute { v } else { v + base };
    match node {
        Node::ReferenceKind { sheet_index, absolute_row, absolute_column, row, column, .. } => {
            let (r, c) = (at(*absolute_row, *row, cell.0), at(*absolute_column, *column, cell.1));
            Some((*sheet_index, [r, c, r, c]))
        }
        Node::RangeKind {
            sheet_index,
            absolute_row1,
            absolute_column1,
            row1,
            column1,
            absolute_row2,
            absolute_column2,
            row2,
            column2,
            ..
        } => Some((
            *sheet_index,
            [
                at(*absolute_row1, *row1, cell.0),
                at(*absolute_column1, *column1, cell.1),
                at(*absolute_row2, *row2, cell.0),
                at(*absolute_column2, *column2, cell.1),
            ],
        )),
        _ => None,
    }
}

/// Moves one end of a range node. `end` is 1 or 2; `by` applies to the row or the column.
/// An absolute reference holds the cell itself and a relative one the distance to the formula's
/// cell, so the same amount is added either way.
fn nudge_end(node: &mut Node, end: u8, vertical: bool, by: i32) {
    if let Node::RangeKind { row1, column1, row2, column2, .. } = node {
        match (end, vertical) {
            (1, true) => *row1 += by,
            (1, false) => *column1 += by,
            (_, true) => *row2 += by,
            (_, false) => *column2 += by,
        }
    }
}

/// Where cells were pushed aside: ranges that start before it and end at or after it must grow.
struct Cut {
    sheet: u32,
    /// the first row (shifting down) or column (shifting right) that moved
    at: i32,
    /// the columns (shifting down) or rows (shifting right) that shifted
    span: (i32, i32),
    shift: Shift,
    by: i32,
}

/// Grows a range the insertion cut through. The cut and paste IronCalc does moves only the
/// references that sit wholly inside the moved cells, so `SUM(A1:A4)` stays as it is when a
/// cell is inserted in the middle of A1:A4.
fn grow_cut_range(node: &mut Node, cell: (i32, i32), cut: &Cut) -> bool {
    let Some((sheet, [r1, c1, r2, c2])) = reference_cells(node, cell) else { return false };
    if sheet != cut.sheet || !matches!(node, Node::RangeKind { .. }) {
        return false;
    }
    let vertical = cut.shift == Shift::Down;
    // along: the axis that moved; across: the one that did not
    let (along, across) = if vertical { ((r1, r2), (c1, c2)) } else { ((c1, c2), (r1, r2)) };
    let cuts = along.0.min(along.1) < cut.at
        && along.0.max(along.1) >= cut.at
        && across.0.min(across.1) >= cut.span.0
        && across.0.max(across.1) <= cut.span.1;
    if cuts {
        // the far end moves, whichever way the range was written
        nudge_end(node, if along.0 >= along.1 { 1 } else { 2 }, vertical, cut.by);
    }
    cuts
}

/// Cells that are about to be deleted.
struct Gone {
    sheet: u32,
    /// the first deleted row (pulling up) or column (pulling left)
    from: i32,
    count: i32,
    /// the columns (pulling up) or rows (pulling left) that are deleted from
    span: (i32, i32),
    pull: Pull,
}

/// What deleting cells does to a reference: one that points at them turns into #REF!, and a
/// range that reaches into them is cut short. The cells after them are moved by the cut and
/// paste, which also moves the references that point at those.
fn drop_gone_refs(node: &mut Node, cell: (i32, i32), gone: &Gone) -> bool {
    let Some((sheet, [r1, c1, r2, c2])) = reference_cells(node, cell) else { return false };
    if sheet != gone.sheet {
        return false;
    }
    let vertical = matches!(gone.pull, Pull::Up);
    let (along, across) = if vertical { ((r1, r2), (c1, c2)) } else { ((c1, c2), (r1, r2)) };
    let (lo, hi) = (along.0.min(along.1), along.0.max(along.1));
    let (first, last) = (gone.from, gone.from + gone.count - 1);
    if across.0.min(across.1) < gone.span.0 || across.0.max(across.1) > gone.span.1 || hi < first || lo > last {
        return false;
    }
    if lo >= first && hi <= last {
        *node = Node::ErrorKind(Error::REF);
        return true;
    }
    // the near end falls on the first cell after the deleted ones, the far end follows its cell
    let new_lo = if lo >= first { first } else { lo };
    let new_hi = if hi <= last { first - 1 } else { hi - gone.count };
    let (near, far) = if along.0 <= along.1 { (1, 2) } else { (2, 1) };
    nudge_end(node, near, vertical, new_lo - lo);
    nudge_end(node, far, vertical, new_hi - hi);
    true
}

/// Cuts the cells of `from` (first row, first column, last row, last column) and pastes them
/// with their top-left corner at `to`. IronCalc moves their content, formatting, links and merges,
/// and rewrites the formulas that point at them. One history entry.
fn cut_and_paste(
    doc: &mut Doc,
    sheet: u32,
    from: (i32, i32, i32, i32),
    to: (i32, i32),
) -> CmdResult<()> {
    doc.model.set_selected_sheet(sheet)?;
    doc.model.set_selected_cell(from.0, from.1)?;
    doc.model.set_selected_range(from.0, from.1, from.2, from.3)?;
    let clip = doc.model.copy_to_clipboard()?;
    // the clipboard type is not exported, but its cells are
    let data: ClipboardData = serde_json::from_value(
        serde_json::to_value(&clip).map_err(|e| e.to_string())?["data"].take(),
    )
    .map_err(|e| e.to_string())?;
    doc.model.set_selected_cell(to.0, to.1)?;
    doc.model.paste_from_clipboard(sheet, from, &data, true)
}

/// Refuses to touch cells where merged cells would be cut in two.
fn check_no_merges(doc: &Doc, sheet: u32, rows: (i32, i32), cols: (i32, i32)) -> CmdResult<()> {
    let merged = doc.model.get_merged_cells(sheet)?;
    let hit = merged.iter().any(|m| {
        m.row <= rows.1 && m.row + m.height > rows.0 && m.column <= cols.1 && m.column + m.width > cols.0
    });
    if hit {
        return Err("Cannot shift cells where cells are merged".to_string());
    }
    Ok(())
}

/// Inserts empty cells at `area`, shifting the cells below it (or to its right) out of the way.
///
/// IronCalc has no such operation, but a shift is a cut and paste of the cells from the
/// insertion point to the edge of the data, one step over. Ranges the insertion cuts through
/// are grown afterwards. All of it is one undo step.
fn insert_cells_in(doc: &mut Doc, area: &Area, shift: Shift) -> CmdResult<()> {
    let Area { sheet, row, column: col, width, height } = *area;
    let dim = doc.model.get_model().workbook.worksheet(sheet)?.dimension();
    let (last_row, last_col, to_row, to_col) = match shift {
        Shift::Down => (dim.max_row, col + width - 1, row + height, col),
        Shift::Right => (row + height - 1, dim.max_column, row, col + width),
    };
    // nothing at or past the insertion point: there is nothing to push away
    if last_row < row || last_col < col {
        return Ok(());
    }
    let (end_row, end_col) = match shift {
        Shift::Down => (last_row + height, last_col),
        Shift::Right => (last_row, last_col + width),
    };
    if end_row > MAX_ROW || end_col > MAX_COL {
        return Err("Cannot shift the cells past the end of the sheet".to_string());
    }
    cut_and_paste(doc, sheet, (row, col, last_row, last_col), (to_row, to_col))?;
    let cut = match shift {
        Shift::Down => Cut { sheet, at: row, span: (col, col + width - 1), shift, by: height },
        Shift::Right => Cut { sheet, at: col, span: (row, row + height - 1), shift, by: width },
    };
    let grown = rewrite_formulas(doc, |n, cell| grow_cut_range(n, cell, &cut));
    // the paste is done even if a formula could not be rewritten
    doc.record(1 + grown.as_ref().copied().unwrap_or(0));
    grown.map(|_| ())
}

/// Deletes the cells of `area`, pulling the cells below it (or to its right) over them.
///
/// The formulas are rewritten first (see `drop_gone_refs`), then the cells after the deleted ones
/// are cut and pasted over them. All of it is one undo step.
fn delete_cells_in(doc: &mut Doc, area: &Area, pull: Pull) -> CmdResult<()> {
    let Area { sheet, row, column: col, width, height } = *area;
    let dim = doc.model.get_model().workbook.worksheet(sheet)?.dimension();
    // (the deleted cells, the cells that come after them, as first/last row and column)
    let (gone, tail, to, gone_span) = match pull {
        Pull::Up => (
            Gone { sheet, from: row, count: height, span: (col, col + width - 1), pull },
            (row + height, col, dim.max_row, col + width - 1),
            (row, col),
            (row, row + height - 1, col, col + width - 1),
        ),
        Pull::Left => (
            Gone { sheet, from: col, count: width, span: (row, row + height - 1), pull },
            (row, col + width, row + height - 1, dim.max_column),
            (row, col),
            (row, row + height - 1, col, col + width - 1),
        ),
    };
    // all the cells in the way are empty
    if row > dim.max_row || col > dim.max_column {
        return Ok(());
    }
    let reach_rows = (row, tail.2.max(gone_span.1));
    let reach_cols = (col, tail.3.max(gone_span.3));
    check_no_merges(doc, sheet, reach_rows, reach_cols)?;

    let mut entries = 0;
    let result = (|| {
        entries += rewrite_formulas(doc, |n, cell| drop_gone_refs(n, cell, &gone))?;
        let has_tail = tail.2 >= tail.0 && tail.3 >= tail.1;
        let covered = if has_tail {
            cut_and_paste(doc, sheet, tail, to)?;
            entries += 1;
            // the pulled cells cover this many of the deleted ones
            match pull {
                Pull::Up => tail.2 - tail.0 + 1,
                Pull::Left => tail.3 - tail.1 + 1,
            }
        } else {
            0
        };
        // what the pulled cells did not cover is emptied, formatting and all
        let rest = match pull {
            Pull::Up => Area { row: row + covered, height: height - covered, ..*area },
            Pull::Left => Area { column: col + covered, width: width - covered, ..*area },
        };
        if rest.height > 0 && rest.width > 0 {
            doc.model.range_clear_all(&rest)?;
            entries += 1;
        }
        Ok(())
    })();
    doc.record(entries);
    result
}

/// Moves the cells of `area` so that their top-left corner lands on (to_row, to_col), over
/// whatever is there. Blank space past the data is left where it is.
fn move_cells_in(doc: &mut Doc, area: &Area, to_row: i32, to_col: i32) -> CmdResult<()> {
    let Area { sheet, row, column: col, width, height } = *area;
    if (to_row, to_col) == (row, col) {
        return Ok(());
    }
    if to_row < 1 || to_col < 1 || to_row + height - 1 > MAX_ROW || to_col + width - 1 > MAX_COL {
        return Err("Cannot move the cells off the sheet".to_string());
    }
    let dim = doc.model.get_model().workbook.worksheet(sheet)?.dimension();
    let last_row = (row + height - 1).min(dim.max_row);
    let last_col = (col + width - 1).min(dim.max_column);
    if last_row < row || last_col < col {
        return Ok(());
    }
    cut_and_paste(doc, sheet, (row, col, last_row, last_col), (to_row, to_col))?;
    doc.record(1);
    Ok(())
}

/// Inserts empty cells at the range, shifting the cells below it or to its right.
#[tauri::command]
fn insert_cells(
    sheet: u32,
    row: i32,
    col: i32,
    height: i32,
    width: i32,
    shift: Shift,
    window: Window,
    state: State<AppState>,
) -> CmdResult<WorkbookInfo> {
    let mut doc = state.doc(window.label());
    insert_cells_in(&mut doc, &Area { sheet, row, column: col, width, height }, shift)?;
    Ok(info(&doc))
}

/// Deletes the cells of the range, pulling the cells below it or to its right over them.
#[tauri::command]
fn delete_cells(
    sheet: u32,
    row: i32,
    col: i32,
    height: i32,
    width: i32,
    pull: Pull,
    window: Window,
    state: State<AppState>,
) -> CmdResult<WorkbookInfo> {
    let mut doc = state.doc(window.label());
    delete_cells_in(&mut doc, &Area { sheet, row, column: col, width, height }, pull)?;
    Ok(info(&doc))
}

/// Moves the cells of the range, with their formatting, to a new top-left corner.
#[tauri::command]
fn move_cells(
    sheet: u32,
    row: i32,
    col: i32,
    height: i32,
    width: i32,
    to_row: i32,
    to_col: i32,
    window: Window,
    state: State<AppState>,
) -> CmdResult<WorkbookInfo> {
    let mut doc = state.doc(window.label());
    move_cells_in(&mut doc, &Area { sheet, row, column: col, width, height }, to_row, to_col)?;
    Ok(info(&doc))
}

/// Remembers the formatting of a range for `paste_formats`.
#[tauri::command]
fn copy_formats(
    sheet: u32,
    row: i32,
    col: i32,
    height: i32,
    width: i32,
    window: Window,
    state: State<AppState>,
) -> CmdResult<()> {
    let mut doc = state.doc(window.label());
    let styles = read_range(&doc, sheet, row, col, height, width, |r, c| {
        doc.model.get_cell_style(sheet, r, c)
    })?;
    doc.format_clip = styles;
    Ok(())
}

/// Largest selection that is filled cell by cell, as in a plain paste.
const MAX_FILL_CELLS: i64 = 10_000;

/// Applies the formatting remembered by `copy_formats` to the range, repeating it to fill the range.
#[tauri::command]
fn paste_formats(
    sheet: u32,
    row: i32,
    col: i32,
    height: i32,
    width: i32,
    window: Window,
    state: State<AppState>,
) -> CmdResult<WorkbookInfo> {
    let mut doc = state.doc(window.label());
    let styles = doc.format_clip.clone();
    let Some(first) = styles.first() else {
        return Err("Copy some cells first".to_string());
    };
    let (clip_h, clip_w) = (styles.len() as i32, first.len() as i32);
    // a huge selection (a whole column, say) only gets one copy of the formatting
    let (h, w) = if i64::from(height) * i64::from(width) <= MAX_FILL_CELLS {
        (height.max(clip_h), width.max(clip_w))
    } else {
        (clip_h, clip_w)
    };
    doc.model.set_selected_sheet(sheet)?;
    doc.model.set_selected_cell(row, col)?;
    doc.model.set_selected_range(row, col, row + h - 1, col + w - 1)?;
    doc.model.on_paste_styles(&styles)?;
    doc.record(1);
    Ok(info(&doc))
}

/// Sets several column widths (pixels) as a single undo step.
#[tauri::command]
fn set_column_widths(
    sheet: u32,
    widths: Vec<(i32, f64)>,
    window: Window,
    state: State<AppState>,
) -> CmdResult<WorkbookInfo> {
    let mut doc = state.doc(window.label());
    let mut written = 0;
    let mut result = Ok(());
    for (col, width) in widths {
        result = doc.model.set_columns_width(sheet, col, col, width);
        if result.is_err() {
            break;
        }
        written += 1;
    }
    doc.record(written);
    result?;
    Ok(info(&doc))
}

#[derive(Serialize)]
struct ColumnText {
    text: String,
    bold: bool,
    num: bool,
}

/// Displayed texts of one column, used by the frontend to measure an autofit width.
#[tauri::command]
fn column_texts(
    sheet: u32,
    col: i32,
    window: Window,
    state: State<AppState>,
) -> CmdResult<Vec<ColumnText>> {
    let doc = state.doc(window.label());
    let m = &doc.model;
    let dim = m.get_model().workbook.worksheet(sheet)?.dimension();
    let mut out = Vec::new();
    for r in 1..=dim.max_row {
        let text = m.get_formatted_cell_value(sheet, r, col)?;
        if text.is_empty() {
            continue;
        }
        out.push(ColumnText {
            bold: m.get_cell_style(sheet, r, col)?.font.b,
            num: matches!(m.get_cell_type(sheet, r, col)?, CellType::Number),
            text,
        });
    }
    Ok(out)
}

#[tauri::command]
fn undo(window: Window, state: State<AppState>) -> CmdResult<WorkbookInfo> {
    let mut doc = state.doc(window.label());
    doc.replay(false)?;
    Ok(info(&doc))
}

#[tauri::command]
fn redo(window: Window, state: State<AppState>) -> CmdResult<WorkbookInfo> {
    let mut doc = state.doc(window.label());
    doc.replay(true)?;
    Ok(info(&doc))
}

#[tauri::command]
fn add_sheet(window: Window, state: State<AppState>) -> CmdResult<WorkbookInfo> {
    let mut doc = state.doc(window.label());
    doc.model.new_sheet()?;
    doc.record(1);
    Ok(info(&doc))
}

/// Saves to `path` (chosen by the caller) or to the current path when `None`.
/// The extension decides the format: `.csv` exports `sheet` only.
fn save_doc(doc: &mut Doc, path: Option<PathBuf>, sheet: u32) -> CmdResult<()> {
    let target = path.or_else(|| doc.path.clone()).ok_or("No file path")?;
    let is_csv = target
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("csv"));
    if is_csv {
        save_csv(&doc.model, sheet, &target)?;
    } else {
        save_xlsx(&doc.model, &target)?;
        doc.path = Some(target);
        doc.dirty = false;
    }
    Ok(())
}

#[tauri::command]
fn save_file(
    path: Option<String>,
    sheet: u32,
    window: Window,
    state: State<AppState>,
) -> CmdResult<WorkbookInfo> {
    let mut doc = state.doc(window.label());
    save_doc(&mut doc, path.map(PathBuf::from), sheet)?;
    Ok(info(&doc))
}

#[derive(Serialize)]
struct ActiveStyle {
    bold: bool,
    italic: bool,
    underline: bool,
    num_fmt: String,
}

#[tauri::command]
fn get_active_style(
    sheet: u32,
    row: i32,
    col: i32,
    window: Window,
    state: State<AppState>,
) -> CmdResult<ActiveStyle> {
    let s = state.doc(window.label()).model.get_cell_style(sheet, row, col)?;
    Ok(ActiveStyle {
        bold: s.font.b,
        italic: s.font.i,
        underline: s.font.u,
        num_fmt: s.num_fmt,
    })
}

/// Text of a rectangular range; `raw` returns formulas instead of displayed values.
/// Reads every cell of the range with `read`, leaving out the empty space past the data.
fn read_range<T>(
    doc: &Doc,
    sheet: u32,
    row: i32,
    col: i32,
    height: i32,
    width: i32,
    read: impl Fn(i32, i32) -> CmdResult<T>,
) -> CmdResult<Vec<Vec<T>>> {
    let dim = doc.model.get_model().workbook.worksheet(sheet)?.dimension();
    let last_row = (row + height - 1).min(dim.max_row).max(row);
    let last_col = (col + width - 1).min(dim.max_column).max(col);
    (row..=last_row)
        .map(|r| (col..=last_col).map(|c| read(r, c)).collect())
        .collect()
}

#[tauri::command]
fn get_range_text(
    sheet: u32,
    row: i32,
    col: i32,
    height: i32,
    width: i32,
    raw: bool,
    window: Window,
    state: State<AppState>,
) -> CmdResult<Vec<Vec<String>>> {
    let doc = state.doc(window.label());
    read_range(&doc, sheet, row, col, height, width, |r, c| {
        if raw {
            doc.model.get_cell_content(sheet, r, c)
        } else {
            doc.model.get_formatted_cell_value(sheet, r, c)
        }
    })
}

/// The values of a range, without formulas or number formats, as text a cell accepts.
#[tauri::command]
fn get_range_values(
    sheet: u32,
    row: i32,
    col: i32,
    height: i32,
    width: i32,
    window: Window,
    state: State<AppState>,
) -> CmdResult<Vec<Vec<String>>> {
    let doc = state.doc(window.label());
    let model = doc.model.get_model();
    read_range(&doc, sheet, row, col, height, width, |r, c| {
        Ok(match model.get_cell_value_by_index(sheet, r, c)? {
            CellValue::None => String::new(),
            CellValue::String(s) => s,
            CellValue::Number(n) => n.to_string(),
            CellValue::Boolean(b) => b.to_string().to_uppercase(),
        })
    })
}

#[derive(Serialize, Default)]
struct RangeStats {
    count: u32,
    numeric: u32,
    sum: f64,
}

#[tauri::command]
fn range_stats(
    sheet: u32,
    row: i32,
    col: i32,
    height: i32,
    width: i32,
    window: Window,
    state: State<AppState>,
) -> CmdResult<RangeStats> {
    let doc = state.doc(window.label());
    let model = doc.model.get_model();
    let dim = model.workbook.worksheet(sheet)?.dimension();
    let mut stats = RangeStats::default();
    for r in row..=(row + height - 1).min(dim.max_row) {
        for c in col..=(col + width - 1).min(dim.max_column) {
            match model.get_cell_value_by_index(sheet, r, c)? {
                CellValue::None => {}
                CellValue::Number(n) => {
                    stats.count += 1;
                    stats.numeric += 1;
                    stats.sum += n;
                }
                _ => stats.count += 1,
            }
        }
    }
    Ok(stats)
}

#[derive(Serialize)]
struct RectDto {
    row: i32,
    col: i32,
    height: i32,
    width: i32,
}

/// AutoSum heuristic: the run of numbers directly above (else directly left of)
/// the cell, or `None` when there is nothing to total.
#[tauri::command]
fn autosum_range(
    sheet: u32,
    row: i32,
    col: i32,
    window: Window,
    state: State<AppState>,
) -> Option<RectDto> {
    autosum_in(state.doc(window.label()).model.get_model(), sheet, row, col)
}

#[derive(Serialize)]
struct FormulaPreview {
    text: String,
    kind: &'static str,
}

/// What `formula` would show in the cell, computed on a scratch copy so the
/// workbook and its undo history stay untouched.
#[tauri::command]
fn preview_formula(
    sheet: u32,
    row: i32,
    col: i32,
    formula: String,
    window: Window,
    state: State<AppState>,
) -> CmdResult<FormulaPreview> {
    let mut cache = state.preview.lock_recovering();
    let original = {
        let doc = state.doc(window.label());
        if cache.as_ref().map_or(true, |(generation, _)| *generation != doc.generation) {
            let copy = Model::from_bytes(&doc.model.get_model().to_bytes(), LANG)?;
            *cache = Some((doc.generation, copy));
        }
        doc.model.get_cell_content(sheet, row, col)?
    };
    let model = &mut cache.as_mut().unwrap().1;
    preview_in(model, sheet, row, col, formula, &original)
}

fn preview_in(
    model: &mut Model,
    sheet: u32,
    row: i32,
    col: i32,
    formula: String,
    original: &str,
) -> CmdResult<FormulaPreview> {
    model.set_user_input(sheet, row, col, formula)?;
    model.evaluate();
    let text = model.get_formatted_cell_value(sheet, row, col)?;
    let kind = cell_kind(model.get_cell_type(sheet, row, col)?);
    // put the cell back, so a later preview elsewhere sees the real content
    if original.is_empty() {
        model.range_clear_contents(&Area { sheet, row, column: col, width: 1, height: 1 })?;
    } else {
        model.set_user_input(sheet, row, col, original.to_string())?;
    }
    Ok(FormulaPreview { text, kind })
}

fn autosum_in(model: &Model, sheet: u32, row: i32, col: i32) -> Option<RectDto> {
    let is_number = |r: i32, c: i32| {
        r >= 1
            && c >= 1
            && matches!(model.get_cell_value_by_index(sheet, r, c), Ok(CellValue::Number(_)))
    };
    let above = (1..row).take_while(|i| is_number(row - i, col)).count() as i32;
    if above > 0 {
        return Some(RectDto { row: row - above, col, height: above, width: 1 });
    }
    let left = (1..col).take_while(|i| is_number(row, col - i)).count() as i32;
    (left > 0).then_some(RectDto { row, col: col - left, height: 1, width: left })
}

#[tauri::command]
fn take_pending_open(window: Window, state: State<AppState>) -> Vec<String> {
    state
        .pending_open
        .lock()
        .unwrap()
        .get_mut(window.label())
        .map(std::mem::take)
        .unwrap_or_default()
}

static NEXT_WINDOW: AtomicU64 = AtomicU64::new(1);

/// Opens another window like the main one, which loads `path` once its webview is ready.
fn open_window(app: &AppHandle, path: Option<String>) -> CmdResult<()> {
    let label = format!("doc-{}", NEXT_WINDOW.fetch_add(1, Ordering::Relaxed));
    build_window(app, label, path)
}

/// Window `label` shows the workbook stored under that label, empty when there is none yet.
fn build_window(app: &AppHandle, label: String, path: Option<String>) -> CmdResult<()> {
    let state = app.state::<AppState>();
    let mut config = app
        .config()
        .app
        .windows
        .first()
        .cloned()
        .ok_or("No window configuration")?;
    config.label = label.clone();
    let mut builder = WebviewWindowBuilder::from_config(app, &config).map_err(|e| e.to_string())?;
    // cascade from the window in front, like other document apps
    if let Some(front) = app.get_webview_window(&state.active_label()) {
        if let (Ok(pos), Ok(scale)) = (front.outer_position(), front.scale_factor()) {
            let pos = pos.to_logical::<f64>(scale);
            builder = builder.position(pos.x + 24.0, pos.y + 24.0);
        }
    }
    if let Some(path) = path {
        state.pending_open.lock_recovering().insert(label.clone(), vec![path]);
    }
    builder.build().map_err(|e| {
        state.pending_open.lock_recovering().remove(&label);
        e.to_string()
    })?;
    Ok(())
}

/// Brings forward the window that has `path` open, if any. A file lives in one window
/// only, so two windows never save over each other.
fn focus_window_with(app: &AppHandle, path: &str) -> bool {
    let has_it = app
        .state::<AppState>()
        .docs
        .lock()
        .unwrap()
        .iter()
        .find(|(_, d)| d.path.as_deref() == Some(Path::new(path)))
        .map(|(label, _)| label.clone());
    let Some(window) = has_it.and_then(|label| app.get_webview_window(&label)) else {
        return false;
    };
    let _ = window.unminimize();
    let _ = window.set_focus();
    true
}

/// Shows `path` from Finder in the window that already has it open, else in an untouched
/// window, else in a new window.
fn route_open(app: &AppHandle, path: String) -> CmdResult<()> {
    if focus_window_with(app, &path) {
        return Ok(());
    }
    let state = app.state::<AppState>();
    let active = state.active_label();
    let mut labels: Vec<String> = app.webview_windows().into_keys().collect();
    // the window in front first
    labels.sort_by_key(|l| *l != active);
    if let Some(label) = labels.into_iter().find(|l| state.is_untouched(l)) {
        state.pending_open.lock_recovering().insert(label.clone(), vec![path.clone()]);
        let _ = app.emit_to(label.as_str(), "open-file", path);
        return Ok(());
    }
    open_window(app, Some(path))
}

/// Opens a new window, empty or showing `path`, or focuses the window that has `path` open
/// already. With `here` the calling window takes the file instead: then this returns true
/// and the caller loads it.
#[tauri::command]
async fn open_in_window(path: Option<String>, here: bool, app: AppHandle) -> CmdResult<bool> {
    let Some(path) = path else {
        open_window(&app, None)?;
        return Ok(false);
    };
    if focus_window_with(&app, &path) {
        return Ok(false);
    }
    if here {
        return Ok(true);
    }
    open_window(&app, Some(path))?;
    Ok(false)
}

/// Open… with no window to ask from: every picked file gets a window of its own.
fn pick_and_open(app: &AppHandle) {
    let handle = app.clone();
    app.dialog()
        .file()
        .add_filter("Spreadsheets", &["xlsx", "xlsm", "csv", "tsv", "txt"])
        .pick_files(move |picked| {
            for path in picked.into_iter().flatten().filter_map(|p| p.into_path().ok()) {
                let _ = route_open(&handle, path.to_string_lossy().to_string());
            }
        });
}

/// Set once the user agreed to quit with unsaved changes, so the exit that follows goes through.
static QUIT_CONFIRMED: AtomicBool = AtomicBool::new(false);
/// A "quit anyway?" question is on screen, so a second quit request does not stack another.
static QUIT_ASKING: AtomicBool = AtomicBool::new(false);

/// Quitting the app (Cmd+Q) closes every window without the per-window "unsaved changes" question,
/// and takes the MCP server down with them. Returns true when the quit has to wait for an answer.
fn hold_quit_for_unsaved_changes(app: &AppHandle) -> bool {
    if QUIT_CONFIRMED.load(Ordering::SeqCst) {
        return false;
    }
    let unsaved = app.state::<AppState>().docs.lock_recovering().values().filter(|d| d.dirty).count();
    if unsaved == 0 {
        return false;
    }
    if !QUIT_ASKING.swap(true, Ordering::SeqCst) {
        let text = match unsaved {
            1 => "1 workbook has unsaved changes. Quit without saving it?".to_string(),
            n => format!("{n} workbooks have unsaved changes. Quit without saving them?"),
        };
        let handle = app.clone();
        app.dialog()
            .message(text)
            .title("Unsaved changes")
            .kind(MessageDialogKind::Warning)
            .buttons(MessageDialogButtons::OkCancelCustom("Quit".into(), "Cancel".into()))
            .show(move |quit| {
                QUIT_ASKING.store(false, Ordering::SeqCst);
                if quit {
                    QUIT_CONFIRMED.store(true, Ordering::SeqCst);
                    handle.exit(0);
                }
            });
    }
    true
}

fn build_menu<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<Menu<R>> {
    let item = |id: &str, text: &str, accel: &str| {
        MenuItemBuilder::with_id(id, text).accelerator(accel).build(app)
    };
    let app_menu = SubmenuBuilder::new(app, "SheetBro")
        .about(None)
        .separator()
        .services()
        .separator()
        .hide()
        .hide_others()
        .show_all()
        .separator()
        .quit()
        .build()?;
    let file = SubmenuBuilder::new(app, "File")
        .item(&item("new", "New", "CmdOrCtrl+N")?)
        .item(&item("open", "Open…", "CmdOrCtrl+O")?)
        .separator()
        .item(&item("save", "Save", "CmdOrCtrl+S")?)
        .item(&item("save_as", "Save As…", "CmdOrCtrl+Shift+S")?)
        .separator()
        .close_window()
        .build()?;
    // Edit items are custom (not predefined) so the shortcuts reach the grid
    // instead of being swallowed by the native responder chain.
    let edit = SubmenuBuilder::new(app, "Edit")
        .item(&item("undo", "Undo", "CmdOrCtrl+Z")?)
        .item(&item("redo", "Redo", "CmdOrCtrl+Shift+Z")?)
        .separator()
        .item(&item("cut", "Cut", "CmdOrCtrl+X")?)
        .item(&item("copy", "Copy", "CmdOrCtrl+C")?)
        .item(&item("paste", "Paste", "CmdOrCtrl+V")?)
        .item(&item("select_all", "Select All", "CmdOrCtrl+A")?)
        .build()?;
    let format = SubmenuBuilder::new(app, "Format")
        .item(&item("bold", "Bold", "CmdOrCtrl+B")?)
        .item(&item("italic", "Italic", "CmdOrCtrl+I")?)
        .item(&item("underline", "Underline", "CmdOrCtrl+U")?)
        .separator()
        .item(&item("merge_center", "Merge & Center", "CmdOrCtrl+Alt+M")?)
        .item(&item("merge", "Merge Cells", "CmdOrCtrl+Alt+Shift+M")?)
        .item(&MenuItemBuilder::with_id("merge_across", "Merge Across").build(app)?)
        .item(&MenuItemBuilder::with_id("merge_down", "Merge Down").build(app)?)
        .item(&MenuItemBuilder::with_id("unmerge", "Unmerge Cells").build(app)?)
        .build()?;
    let window = SubmenuBuilder::new(app, "Window")
        .minimize()
        .maximize()
        .build()?;
    Menu::with_items(app, &[&app_menu, &file, &edit, &format, &window])
}

/// Entry point of `--mcp-stdio`: relays MCP messages between stdin/stdout and the running app.
pub fn mcp_stdio_bridge() -> i32 {
    mcp::stdio_bridge()
}

// ----------------------------------------------------------------- run

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .manage(AppState {
            docs: Mutex::new(HashMap::new()),
            active: Mutex::new("main".into()),
            pending_open: Mutex::new(HashMap::new()),
            mcp: Mutex::new(mcp::Runtime::load()),
            preview: Mutex::new(None),
        })
        .setup(|app| {
            app.set_menu(build_menu(app.handle())?)?;
            mcp::autostart(app.handle());
            Ok(())
        })
        .on_menu_event(|app, event| {
            let id = event.id().0.as_str();
            if id == "new" {
                let _ = open_window(app, None);
                return;
            }
            // on macOS the app stays running with every window closed
            if app.webview_windows().is_empty() {
                if id == "open" {
                    pick_and_open(app);
                }
                return;
            }
            // only the window in front acts on a menu command
            let label = app.state::<AppState>().active_label();
            let _ = app.emit_to(label.as_str(), "menu", id);
        })
        .on_window_event(|window, event| {
            let state = window.state::<AppState>();
            let label = window.label();
            match event {
                WindowEvent::Focused(true) => *state.active.lock_recovering() = label.to_string(),
                WindowEvent::Destroyed => {
                    state.docs.lock_recovering().remove(label);
                    state.pending_open.lock_recovering().remove(label);
                    let mut active = state.active.lock_recovering();
                    if *active == label {
                        let others = window.webview_windows().into_keys().find(|l| l != label);
                        if let Some(next) = others {
                            *active = next;
                        }
                    }
                }
                _ => {}
            }
        })
        .invoke_handler(tauri::generate_handler![
            open_file,
            workbook_info,
            get_layout,
            get_viewport,
            get_cell_content,
            set_cells,
            clear_range,
            style_range,
            set_column_widths,
            column_texts,
            merge_cells,
            unmerge_cells,
            insert_lines,
            delete_lines,
            insert_cells,
            delete_cells,
            move_cells,
            copy_formats,
            paste_formats,
            get_active_style,
            get_range_text,
            get_range_values,
            range_stats,
            undo,
            redo,
            add_sheet,
            save_file,
            autosum_range,
            preview_formula,
            take_pending_open,
            open_in_window,
            mcp::mcp_status,
            mcp::mcp_set_enabled,
            mcp::mcp_regenerate_token,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(|handle, event| {
        if let tauri::RunEvent::ExitRequested { api, .. } = &event {
            if hold_quit_for_unsaved_changes(handle) {
                api.prevent_exit();
                return;
            }
        }
        #[cfg(target_os = "macos")]
        match event {
            // Finder "Open With" / dropping a file on the Dock icon
            tauri::RunEvent::Opened { urls } => {
                for path in urls.iter().filter_map(|u| u.to_file_path().ok()) {
                    let _ = route_open(handle, path.to_string_lossy().to_string());
                }
            }
            // the last window closed (Quit is `Some` or skips this): stay in the Dock like
            // other Mac apps
            tauri::RunEvent::ExitRequested { code: None, api, .. } => api.prevent_exit(),
            // clicking the Dock icon with no window open
            tauri::RunEvent::Reopen { .. } if handle.webview_windows().is_empty() => {
                let _ = open_window(handle, None);
            }
            _ => {}
        }
        #[cfg(not(target_os = "macos"))]
        let _ = (handle, event);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("spritsit-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn csv_to_xlsx_round_trip() {
        let csv = tmp("in.csv");
        fs::write(
            &csv,
            "\u{feff}name;qty;price\nwidget;2;1.5\n\"semi;colon\";3;=B3*2\n",
        )
        .unwrap();
        let model = load_csv(&csv).unwrap();
        // delimiter sniffed (;), BOM stripped, quoted field kept intact
        assert_eq!(model.get_formatted_cell_value(0, 1, 1).unwrap(), "name");
        assert_eq!(model.get_formatted_cell_value(0, 3, 1).unwrap(), "semi;colon");
        assert_eq!(model.get_formatted_cell_value(0, 3, 3).unwrap(), "6");

        let xlsx = tmp("out.xlsx");
        save_xlsx(&model, &xlsx).unwrap();
        // saving over an existing file must work (IronCalc itself refuses)
        save_xlsx(&model, &xlsx).unwrap();

        let back = load_xlsx(&xlsx).unwrap();
        assert_eq!(back.get_formatted_cell_value(0, 2, 2).unwrap(), "2");
        assert_eq!(back.get_cell_content(0, 3, 3).unwrap(), "=B3*2");
        assert_eq!(back.get_formatted_cell_value(0, 3, 3).unwrap(), "6");

        let out_csv = tmp("out.csv");
        save_csv(&back, 0, &out_csv).unwrap();
        let text = fs::read_to_string(out_csv).unwrap();
        assert_eq!(text, "name,qty,price\nwidget,2,1.5\nsemi;colon,3,6\n");
    }

    #[test]
    fn opening_a_file_leaves_nothing_to_undo() {
        let csv = tmp("open.csv");
        fs::write(&csv, "a,b\n1,2\n").unwrap();
        let mut doc = Doc::new(load_csv(&csv).unwrap(), None);
        assert!(!doc.model.can_undo());
        doc.replay(false).unwrap();
        assert_eq!(doc.model.get_formatted_cell_value(0, 2, 2).unwrap(), "2");
    }

    #[test]
    fn only_an_untouched_empty_workbook_makes_way_for_a_file() {
        assert!(Doc::empty().unwrap().is_blank());
        let mut edited = Doc::empty().unwrap();
        write_cells(&mut edited, 0, 1, 1, &[vec!["x".into()]]).unwrap();
        edited.replay(false).unwrap();
        // undone back to empty, but still the user's work
        assert!(!edited.is_blank());

        let csv = tmp("blank.csv");
        fs::write(&csv, "a\n").unwrap();
        // an imported CSV has no path, yet it is not blank
        assert!(!Doc::new(load_csv(&csv).unwrap(), None).is_blank());
    }

    #[test]
    fn paste_undoes_and_redoes_as_one_step() {
        let mut doc = Doc::new(new_model("t").unwrap(), None);
        doc.model.set_user_input(0, 1, 1, "keep").unwrap();
        doc.record(1);

        // a 3x2 paste is six history entries in IronCalc
        for r in 1..=3 {
            for c in 1..=2 {
                doc.model.set_user_input(0, r + 1, c, &format!("{r}{c}")).unwrap();
            }
        }
        doc.record(6);

        doc.replay(false).unwrap();
        for r in 2..=4 {
            for c in 1..=2 {
                assert_eq!(doc.model.get_formatted_cell_value(0, r, c).unwrap(), "");
            }
        }
        // the earlier edit is untouched
        assert_eq!(doc.model.get_formatted_cell_value(0, 1, 1).unwrap(), "keep");

        doc.replay(true).unwrap();
        assert_eq!(doc.model.get_formatted_cell_value(0, 4, 2).unwrap(), "32");
        assert_eq!(doc.undo_groups, vec![1, 6]);
        assert!(doc.redo_groups.is_empty());
    }

    #[test]
    fn column_widths_undo_as_one_step() {
        let mut doc = Doc::new(new_model("t").unwrap(), None);
        let before = doc.model.get_column_width(0, 2).unwrap();
        for col in 2..=4 {
            doc.model.set_columns_width(0, col, col, 150.0).unwrap();
        }
        doc.record(3);
        doc.replay(false).unwrap();
        for col in 2..=4 {
            assert_eq!(doc.model.get_column_width(0, col).unwrap(), before);
        }
        doc.replay(true).unwrap();
        assert_eq!(doc.model.get_column_width(0, 4).unwrap(), 150.0);
    }

    fn area(row: i32, column: i32, height: i32, width: i32) -> Area {
        Area { sheet: 0, row, column, width, height }
    }

    #[test]
    fn merge_conflict_needs_confirmation_and_undoes_in_one_step() {
        let mut doc = Doc::new(new_model("t").unwrap(), None);
        for (r, c, v) in [(1, 1, "a"), (1, 2, "b"), (2, 1, "c")] {
            doc.model.set_user_input(0, r, c, v).unwrap();
        }
        doc.record(3);
        let target = area(1, 1, 2, 2);

        // without confirmation nothing changes
        assert_eq!(merge_in(&mut doc, &target, MergeMode::Center, false), Err(MERGE_CONFLICT.into()));
        assert!(doc.model.get_merged_cells(0).unwrap().is_empty());
        assert_eq!(doc.undo_groups, vec![3]);

        merge_in(&mut doc, &target, MergeMode::Center, true).unwrap();
        let merged = doc.model.get_merged_cells(0).unwrap();
        assert_eq!((merged[0].row, merged[0].column, merged[0].width, merged[0].height), (1, 1, 2, 2));
        assert_eq!(doc.model.get_formatted_cell_value(0, 1, 1).unwrap(), "a");
        assert_eq!(doc.model.get_formatted_cell_value(0, 2, 1).unwrap(), "");
        // two clears + the merge are a single step
        assert_eq!(doc.undo_groups, vec![3, 3]);

        doc.replay(false).unwrap();
        assert!(doc.model.get_merged_cells(0).unwrap().is_empty());
        assert_eq!(doc.model.get_formatted_cell_value(0, 1, 2).unwrap(), "b");
        assert_eq!(doc.model.get_formatted_cell_value(0, 2, 1).unwrap(), "c");
    }

    #[test]
    fn merge_across_and_unmerge() {
        let mut doc = Doc::new(new_model("t").unwrap(), None);
        merge_in(&mut doc, &area(1, 1, 3, 2), MergeMode::Across, false).unwrap();
        assert_eq!(doc.model.get_merged_cells(0).unwrap().len(), 3);
        assert_eq!(doc.undo_groups, vec![1]);

        unmerge_in(&mut doc, &area(2, 1, 1, 1)).unwrap();
        assert_eq!(doc.model.get_merged_cells(0).unwrap().len(), 2);
        // unmerging where nothing is merged records nothing
        unmerge_in(&mut doc, &area(10, 10, 2, 2)).unwrap();
        assert_eq!(doc.undo_groups, vec![1, 1]);
    }

    #[test]
    fn formulas_evaluate_and_follow_edits() {
        let mut doc = Doc::new(new_model("t").unwrap(), None);
        let set = |doc: &mut Doc, r: i32, c: i32, v: &str| {
            doc.model.pause_evaluation();
            doc.model.set_user_input(0, r, c, v).unwrap();
            doc.model.resume_evaluation();
            doc.model.evaluate();
            doc.record(1);
        };
        set(&mut doc, 1, 1, "1");
        set(&mut doc, 2, 1, "2");
        set(&mut doc, 3, 1, "=SUM(A1:A2)");
        assert_eq!(doc.model.get_formatted_cell_value(0, 3, 1).unwrap(), "3");
        set(&mut doc, 1, 1, "5");
        assert_eq!(doc.model.get_formatted_cell_value(0, 3, 1).unwrap(), "7");
        set(&mut doc, 4, 1, "=AVERAGE(A1:A2)");
        set(&mut doc, 5, 1, "=IF(A1>2,\"big\",\"small\")");
        set(&mut doc, 6, 1, "=VLOOKUP(2,A1:A2,1,FALSE)");
        set(&mut doc, 7, 1, "=NOPE(1)");
        for (r, want) in [(4, "3.5"), (5, "big"), (6, "2")] {
            assert_eq!(doc.model.get_formatted_cell_value(0, r, 1).unwrap(), want, "row {r}");
        }
        println!("unknown fn -> {:?}", doc.model.get_formatted_cell_value(0, 7, 1));
        doc.replay(false).unwrap();
        doc.replay(false).unwrap();
        doc.replay(false).unwrap();
        doc.replay(false).unwrap();
        doc.replay(false).unwrap();
        // back to A1 = 1: SUM follows the undo
        assert_eq!(doc.model.get_formatted_cell_value(0, 3, 1).unwrap(), "3");
    }

    #[test]
    fn formula_preview_leaves_the_workbook_alone() {
        let mut doc = Doc::new(new_model("t").unwrap(), None);
        write_cells(&mut doc, 0, 1, 1, &[vec!["1200000".into()], vec!["300".into()]]).unwrap();
        let mut copy = Model::from_bytes(&doc.model.get_model().to_bytes(), LANG).unwrap();
        let p = preview_in(&mut copy, 0, 3, 1, "=A1+A2".into(), "").unwrap();
        assert_eq!((p.text.as_str(), p.kind), ("1200300", "num"));
        // the cell is restored, so another preview reads it as empty
        let p = preview_in(&mut copy, 0, 4, 1, "=A3".into(), "").unwrap();
        assert_eq!(p.text, "0");
        let p = preview_in(&mut copy, 0, 1, 2, "=SUM(A1:A2".into(), "").unwrap();
        println!("unfinished formula -> {:?} {}", p.text, p.kind);
        let p = preview_in(&mut copy, 0, 1, 2, "=A1/0".into(), "").unwrap();
        assert_eq!(p.kind, "err");
        // the real workbook never saw any of it
        assert_eq!(doc.model.get_cell_content(0, 3, 1).unwrap(), "");
        assert_eq!(doc.undo_groups.len(), 1);
    }

    #[test]
    fn autosum_finds_numbers_above_then_left() {
        let mut doc = Doc::new(new_model("t").unwrap(), None);
        for (r, c, v) in [(1, 1, "Qty"), (2, 1, "4"), (3, 1, "5"), (4, 1, "6"), (6, 2, "7"), (6, 3, "8")] {
            doc.model.set_user_input(0, r, c, v).unwrap();
        }
        let m = doc.model.get_model();
        // A5: stops at the header text above the run
        let r = autosum_in(m, 0, 5, 1).unwrap();
        assert_eq!((r.row, r.col, r.height, r.width), (2, 1, 3, 1));
        // D6: nothing above, numbers to the left
        let r = autosum_in(m, 0, 6, 4).unwrap();
        assert_eq!((r.row, r.col, r.height, r.width), (6, 2, 1, 2));
        // nothing numeric next to Z9
        assert!(autosum_in(m, 0, 9, 26).is_none());
    }
    /// Fills A1:A3 with 1, 2, 3 and A4:A6 with formulas that read them.
    fn doc_with_formulas() -> Doc {
        let mut doc = Doc::new(new_model("t").unwrap(), None);
        for (r, v) in [(1, "1"), (2, "2"), (3, "3"), (4, "=A1+A2"), (5, "=SUM(A1:A3)"), (6, "=A4*2")] {
            doc.model.set_user_input(0, r, 1, v).unwrap();
        }
        doc
    }

    #[test]
    fn deleting_rows_undoes_with_the_formulas_that_pointed_into_them() {
        let mut doc = doc_with_formulas();
        let before = all_formulas(&doc.model).unwrap();

        delete_lines_in(&mut doc, 0, Axis::Rows, 2, 2).unwrap();
        assert_eq!(doc.model.get_cell_content(0, 2, 1).unwrap(), "=A1+#REF!");
        assert_eq!(doc.model.get_cell_content(0, 4, 1).unwrap(), "=A2*2");
        // however many formulas had to be put back, it is still one step
        assert_eq!(doc.undo_groups.len(), 1);

        doc.replay(false).unwrap();
        assert_eq!(all_formulas(&doc.model).unwrap(), before);
        assert_eq!(doc.model.get_cell_content(0, 3, 1).unwrap(), "3");

        doc.replay(true).unwrap();
        assert_eq!(doc.model.get_cell_content(0, 2, 1).unwrap(), "=A1+#REF!");
        assert_eq!(doc.model.get_cell_content(0, 4, 1).unwrap(), "=A2*2");
    }

    #[test]
    fn deleting_columns_undoes_with_the_formulas_that_pointed_into_them() {
        let mut doc = Doc::new(new_model("t").unwrap(), None);
        for (c, v) in [(1, "1"), (2, "2"), (3, "=A1+B1"), (4, "=SUM(A1:B1)")] {
            doc.model.set_user_input(0, 1, c, v).unwrap();
        }
        let before = all_formulas(&doc.model).unwrap();
        delete_lines_in(&mut doc, 0, Axis::Columns, 2, 1).unwrap();
        assert_eq!(doc.model.get_cell_content(0, 1, 2).unwrap(), "=A1+#REF!");
        doc.replay(false).unwrap();
        assert_eq!(all_formulas(&doc.model).unwrap(), before);
    }

    #[test]
    fn deleting_lines_without_formulas_is_one_undo_step() {
        let mut doc = Doc::new(new_model("t").unwrap(), None);
        for (r, v) in [(1, "a"), (2, "b"), (3, "c")] {
            doc.model.set_user_input(0, r, 1, v).unwrap();
        }
        delete_lines_in(&mut doc, 0, Axis::Rows, 2, 1).unwrap();
        assert_eq!(doc.model.get_cell_content(0, 2, 1).unwrap(), "c");
        assert_eq!(doc.undo_groups, vec![1]);
        doc.replay(false).unwrap();
        assert_eq!(doc.model.get_cell_content(0, 2, 1).unwrap(), "b");
        assert_eq!(doc.model.get_cell_content(0, 3, 1).unwrap(), "c");
    }

    #[test]
    fn inserting_lines_is_one_undo_step() {
        let mut doc = doc_with_formulas();
        doc.model.insert_rows(0, 2, 3).unwrap();
        doc.record(1);
        // the formulas follow their cells
        assert_eq!(doc.model.get_cell_content(0, 7, 1).unwrap(), "=A1+A5");
        doc.replay(false).unwrap();
        assert_eq!(doc.model.get_cell_content(0, 4, 1).unwrap(), "=A1+A2");
    }
    fn insert_area(row: i32, column: i32, height: i32, width: i32) -> Area {
        Area { sheet: 0, row, column, width, height }
    }

    #[test]
    fn inserting_cells_down_moves_the_data_and_grows_the_totals() {
        let mut doc = Doc::new(new_model("t").unwrap(), None);
        for r in 1..=4 {
            doc.model.set_user_input(0, r, 1, &r.to_string()).unwrap();
        }
        doc.model.set_user_input(0, 5, 1, "=SUM(A1:A4)").unwrap(); // total below the data
        doc.model.set_user_input(0, 1, 3, "=SUM(A1:A5)").unwrap(); // total in another column
        doc.model.set_user_input(0, 2, 2, "=A2*10").unwrap(); // beside the data, not shifted
        doc.model.evaluate();

        insert_cells_in(&mut doc, &insert_area(3, 1, 1, 1), Shift::Down).unwrap();
        let cell = |r, c| doc.model.get_cell_content(0, r, c).unwrap();
        assert_eq!([cell(1, 1), cell(2, 1), cell(3, 1), cell(4, 1), cell(5, 1)], ["1", "2", "", "3", "4"]);
        assert_eq!(cell(6, 1), "=SUM(A1:A5)");
        assert_eq!(cell(1, 3), "=SUM(A1:A6)");
        assert_eq!(cell(2, 2), "=A2*10", "a cell above the insertion does not move");
        assert_eq!(doc.model.get_formatted_cell_value(0, 6, 1).unwrap(), "10");
        // the other total adds up the data and the first total
        assert_eq!(doc.model.get_formatted_cell_value(0, 1, 3).unwrap(), "20");

        // all of it is one step
        assert_eq!(doc.undo_groups.len(), 1);
        doc.replay(false).unwrap();
        let cell = |r, c| doc.model.get_cell_content(0, r, c).unwrap();
        assert_eq!([cell(3, 1), cell(4, 1), cell(5, 1)], ["3", "4", "=SUM(A1:A4)"]);
        assert_eq!(cell(1, 3), "=SUM(A1:A5)");
        doc.replay(true).unwrap();
        assert_eq!(doc.model.get_cell_content(0, 6, 1).unwrap(), "=SUM(A1:A5)");
        assert_eq!(doc.model.get_cell_content(0, 1, 3).unwrap(), "=SUM(A1:A6)");
    }

    #[test]
    fn inserting_cells_right_moves_the_row_and_follows_references() {
        let mut doc = Doc::new(new_model("t").unwrap(), None);
        for c in 1..=4 {
            doc.model.set_user_input(0, 1, c, &c.to_string()).unwrap();
        }
        doc.model.set_user_input(0, 1, 5, "=SUM(A1:D1)").unwrap();
        doc.model.set_user_input(0, 2, 1, "=A1+D1").unwrap();
        doc.model.evaluate();

        insert_cells_in(&mut doc, &insert_area(1, 2, 1, 2), Shift::Right).unwrap();
        let cell = |r, c| doc.model.get_cell_content(0, r, c).unwrap();
        assert_eq!([cell(1, 1), cell(1, 2), cell(1, 3), cell(1, 4), cell(1, 5), cell(1, 6)], ["1", "", "", "2", "3", "4"]);
        assert_eq!(cell(1, 7), "=SUM(A1:F1)");
        assert_eq!(cell(2, 1), "=A1+F1", "a single reference follows its cell");
        assert_eq!(doc.undo_groups.len(), 1);
    }

    #[test]
    fn inserting_cells_keeps_the_formatting_of_the_moved_cells() {
        let mut doc = Doc::new(new_model("t").unwrap(), None);
        doc.model.set_user_input(0, 1, 1, "x").unwrap();
        doc.model.set_user_input(0, 2, 1, "y").unwrap();
        doc.model.update_range_style(&insert_area(2, 1, 1, 1), "font.b", "true").unwrap();
        insert_cells_in(&mut doc, &insert_area(2, 1, 1, 1), Shift::Down).unwrap();
        assert!(!doc.model.get_cell_style(0, 2, 1).unwrap().font.b, "the new cell is plain");
        assert!(doc.model.get_cell_style(0, 3, 1).unwrap().font.b);
        assert_eq!(doc.model.get_cell_content(0, 3, 1).unwrap(), "y");
    }

    #[test]
    fn inserting_cells_past_the_data_changes_nothing() {
        let mut doc = Doc::new(new_model("t").unwrap(), None);
        doc.model.set_user_input(0, 1, 1, "x").unwrap();
        insert_cells_in(&mut doc, &insert_area(50, 1, 2, 1), Shift::Down).unwrap();
        insert_cells_in(&mut doc, &insert_area(1, 9, 1, 2), Shift::Right).unwrap();
        assert!(doc.undo_groups.is_empty());
    }
    #[test]
    fn deleting_cells_pulls_the_rest_up_and_fixes_the_references() {
        let mut doc = Doc::new(new_model("t").unwrap(), None);
        for r in 1..=5 {
            doc.model.set_user_input(0, r, 1, &r.to_string()).unwrap();
        }
        doc.model.set_user_input(0, 6, 1, "=SUM(A1:A5)").unwrap(); // total below the data
        doc.model.set_user_input(0, 1, 2, "=A3+1").unwrap(); // points at the deleted cell
        doc.model.set_user_input(0, 2, 2, "=A5*2").unwrap(); // points at a cell that moves
        doc.model.set_user_input(0, 1, 3, "=SUM(A1:A6)").unwrap(); // range that reaches into it
        doc.model.evaluate();

        delete_cells_in(&mut doc, &insert_area(3, 1, 1, 1), Pull::Up).unwrap();
        let cell = |r, c| doc.model.get_cell_content(0, r, c).unwrap();
        assert_eq!([cell(1, 1), cell(2, 1), cell(3, 1), cell(4, 1), cell(5, 1), cell(6, 1)], ["1", "2", "4", "5", "=SUM(A1:A4)", ""]);
        assert_eq!(cell(1, 2), "=#REF!+1");
        assert_eq!(cell(2, 2), "=A4*2");
        assert_eq!(cell(1, 3), "=SUM(A1:A5)");
        assert_eq!(doc.model.get_formatted_cell_value(0, 5, 1).unwrap(), "12");
        assert_eq!(doc.model.get_formatted_cell_value(0, 1, 3).unwrap(), "24");

        assert_eq!(doc.undo_groups.len(), 1);
        doc.replay(false).unwrap();
        let cell = |r, c| doc.model.get_cell_content(0, r, c).unwrap();
        assert_eq!([cell(3, 1), cell(5, 1), cell(6, 1)], ["3", "5", "=SUM(A1:A5)"]);
        assert_eq!([cell(1, 2), cell(2, 2), cell(1, 3)], ["=A3+1", "=A5*2", "=SUM(A1:A6)"]);
    }

    #[test]
    fn deleting_cells_pulls_the_rest_left() {
        let mut doc = Doc::new(new_model("t").unwrap(), None);
        for c in 1..=5 {
            doc.model.set_user_input(0, 1, c, &c.to_string()).unwrap();
        }
        doc.model.set_user_input(0, 1, 6, "=SUM(A1:E1)").unwrap();
        doc.model.set_user_input(0, 2, 1, "=B1+E1").unwrap();
        doc.model.evaluate();

        delete_cells_in(&mut doc, &insert_area(1, 2, 1, 2), Pull::Left).unwrap();
        let cell = |r, c| doc.model.get_cell_content(0, r, c).unwrap();
        assert_eq!([cell(1, 1), cell(1, 2), cell(1, 3), cell(1, 4)], ["1", "4", "5", "=SUM(A1:C1)"]);
        assert_eq!(cell(1, 5), "");
        assert_eq!(cell(2, 1), "=#REF!+C1");
        assert_eq!(doc.undo_groups.len(), 1);
    }

    #[test]
    fn deleting_cells_past_the_data_or_with_nothing_after_them() {
        let mut doc = Doc::new(new_model("t").unwrap(), None);
        for r in 1..=3 {
            doc.model.set_user_input(0, r, 1, &r.to_string()).unwrap();
            doc.model.update_range_style(&insert_area(r, 1, 1, 1), "font.b", "true").unwrap();
        }
        // beyond the data: nothing happens
        delete_cells_in(&mut doc, &insert_area(50, 1, 2, 1), Pull::Up).unwrap();
        assert!(doc.undo_groups.is_empty());
        // the last two cells: nothing comes up to replace them, formatting goes too
        delete_cells_in(&mut doc, &insert_area(2, 1, 5, 1), Pull::Up).unwrap();
        assert_eq!(doc.model.get_cell_content(0, 1, 1).unwrap(), "1");
        assert_eq!(doc.model.get_cell_content(0, 2, 1).unwrap(), "");
        assert!(!doc.model.get_cell_style(0, 3, 1).unwrap().font.b);
        assert_eq!(doc.undo_groups.len(), 1);
    }

    #[test]
    fn moving_cells_takes_their_formatting_and_the_references_along() {
        let mut doc = Doc::new(new_model("t").unwrap(), None);
        doc.model.set_user_input(0, 1, 1, "10").unwrap();
        doc.model.set_user_input(0, 2, 1, "20").unwrap();
        doc.model.update_range_style(&insert_area(1, 1, 1, 1), "font.b", "true").unwrap();
        doc.model.set_user_input(0, 1, 4, "=A1+A2").unwrap();
        doc.model.set_user_input(0, 3, 3, "gone").unwrap(); // overwritten by the move
        doc.model.evaluate();

        move_cells_in(&mut doc, &insert_area(1, 1, 3, 1), 1, 3).unwrap();
        let cell = |r, c| doc.model.get_cell_content(0, r, c).unwrap();
        assert_eq!([cell(1, 1), cell(2, 1), cell(1, 3), cell(2, 3), cell(3, 3)], ["", "", "10", "20", ""]);
        assert!(doc.model.get_cell_style(0, 1, 3).unwrap().font.b);
        assert!(!doc.model.get_cell_style(0, 1, 1).unwrap().font.b);
        assert_eq!(cell(1, 4), "=C1+C2");
        assert_eq!(doc.model.get_formatted_cell_value(0, 1, 4).unwrap(), "30");

        assert_eq!(doc.undo_groups.len(), 1);
        doc.replay(false).unwrap();
        assert_eq!(doc.model.get_cell_content(0, 1, 1).unwrap(), "10");
        assert_eq!(doc.model.get_cell_content(0, 3, 3).unwrap(), "gone");
        assert_eq!(doc.model.get_cell_content(0, 1, 4).unwrap(), "=A1+A2");
    }

    #[test]
    fn moving_cells_off_the_sheet_or_nowhere() {
        let mut doc = Doc::new(new_model("t").unwrap(), None);
        doc.model.set_user_input(0, 1, 1, "x").unwrap();
        assert!(move_cells_in(&mut doc, &insert_area(1, 1, 2, 1), 0, 1).is_err());
        assert!(move_cells_in(&mut doc, &insert_area(1, 1, 2, 1), MAX_ROW, 1).is_err());
        move_cells_in(&mut doc, &insert_area(1, 1, 1, 1), 1, 1).unwrap();
        assert!(doc.undo_groups.is_empty());
    }
}
