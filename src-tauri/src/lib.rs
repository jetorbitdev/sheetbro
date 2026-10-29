mod mcp;

use std::{
    collections::HashMap,
    fs,
    io::BufWriter,
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex, MutexGuard,
    },
};

use ironcalc::{
    base::{
        cell::CellValue,
        expressions::types::Area,
        types::{CellType, Color, HorizontalAlignment},
        Model, UserModel,
    },
    export::save_xlsx_to_writer,
    import::load_from_xlsx,
};
use serde::{Deserialize, Serialize};
use tauri::{
    menu::{Menu, MenuItemBuilder, SubmenuBuilder},
    AppHandle, Emitter, Manager, Runtime, State, WebviewWindowBuilder, Window, WindowEvent,
};
use tauri_plugin_dialog::DialogExt;

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
        let mut docs = self.docs.lock().unwrap();
        if !docs.contains_key(label) {
            docs.insert(label.to_string(), Doc::empty().expect("create empty workbook"));
        }
        DocGuard { docs, label: label.to_string() }
    }

    fn active_label(&self) -> String {
        self.active.lock().unwrap().clone()
    }

    fn active_doc(&self) -> DocGuard<'_> {
        self.doc(&self.active_label())
    }

    /// A window with nothing in it yet and no file on its way.
    fn is_untouched(&self, label: &str) -> bool {
        !self.pending_open.lock().unwrap().contains_key(label)
            && self.docs.lock().unwrap().get(label).map_or(true, Doc::is_blank)
    }
}

type CmdResult<T> = Result<T, String>;

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
    let dim = doc.model.get_model().workbook.worksheet(sheet)?.dimension();
    // No point in copying the empty space past the data
    let last_row = (row + height - 1).min(dim.max_row).max(row);
    let last_col = (col + width - 1).min(dim.max_column).max(col);
    (row..=last_row)
        .map(|r| {
            (col..=last_col)
                .map(|c| {
                    if raw {
                        doc.model.get_cell_content(sheet, r, c)
                    } else {
                        doc.model.get_formatted_cell_value(sheet, r, c)
                    }
                })
                .collect()
        })
        .collect()
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
    let mut cache = state.preview.lock().unwrap();
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
        state.pending_open.lock().unwrap().insert(label.clone(), vec![path]);
    }
    builder.build().map_err(|e| {
        state.pending_open.lock().unwrap().remove(&label);
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
        state.pending_open.lock().unwrap().insert(label.clone(), vec![path.clone()]);
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
                WindowEvent::Focused(true) => *state.active.lock().unwrap() = label.to_string(),
                WindowEvent::Destroyed => {
                    state.docs.lock().unwrap().remove(label);
                    state.pending_open.lock().unwrap().remove(label);
                    let mut active = state.active.lock().unwrap();
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
            get_active_style,
            get_range_text,
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
}
