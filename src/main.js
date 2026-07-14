"use strict";

// ---- Tauri bridge (withGlobalTauri) ----------------------------------------
const invoke = window.__TAURI__.core.invoke;
const listen = window.__TAURI__.event.listen;

// ---- Layout constants (ROW_H mirrors the --row-h CSS var) -------------------
let ROW_H = 30; // updated by the density setting
const HEADER_H = 34;
const GUTTER_W = 66;
const PAGE = 200; // rows fetched per backend request
const BUFFER = 8; // extra rows rendered above/below the viewport
const SEARCH_CAP = 100000; // must match SEARCH_CAP in main.rs

// ---- DOM --------------------------------------------------------------------
const $ = (id) => document.getElementById(id);
const viewport = $("viewport");
const grid = $("grid");
const headerRow = $("headerRow");
const spacer = $("spacer");
const rows = $("rows");
const emptyEl = $("empty");
const tableWrap = $("tableWrap");
const statusBar = $("statusBar");
const statLeft = $("statLeft");
const statCenter = $("statCenter");
const statRight = $("statRight");
const fileNameEl = $("fileName");
const searchWrap = $("searchWrap");
const searchInput = $("searchInput");
const searchClear = $("searchClear");
const searchColumn = $("searchColumn");
const metaBtn = $("metaBtn");
const metaPanel = $("metaPanel");
const metaBackdropEl = $("metaBackdropEl");
const metaBody = $("metaBody");
const dropOverlay = $("dropOverlay");
const loading = $("loading");
const loadingText = $("loadingText");
const toast = $("toast");
// Advanced filter
const advBtn = $("advBtn");
const advBadge = $("advBadge");
const advPanel = $("advPanel");
const advBackdrop = $("advBackdrop");
const advConditions = $("advConditions");
const advAdd = $("advAdd");
const advClear = $("advClear");
const advApply = $("advApply");
const advCombine = $("advCombine");
// Settings
const settingsBtn = $("settingsBtn");
const settingsWin = $("settingsWin");
const settingsBackdrop = $("settingsBackdrop");
const settingsClose = $("settingsClose");
const setTheme = $("setTheme");
const setDensity = $("setDensity");
const setFont = $("setFont");
const setAutoFit = $("setAutoFit");
const setCase = $("setCase");

// ---- State ------------------------------------------------------------------
let currentPath = null;
let fileMeta = null;
let colWidths = [];
let gridWidth = 0;
let totalRows = 0;
let sortState = null; // { column, ascending }
let filterState = null; // { query, column }
let truncated = false;
let viewToken = 0; // bumped on every view change to discard stale fetches

const cache = new Map(); // pageIndex -> { rows, indices }
const pending = new Set(); // pageIndex currently fetching
const edits = new Map(); // "globalRow:col" -> edited string (session-local, not saved to file)
let editingEl = null;
let editingKey = null;
let editingFileOrig = "";
let searchTimer = null;
let rafPending = false;

// ---- Helpers ----------------------------------------------------------------
function escapeHtml(s) {
  return String(s).replace(/[&<>"]/g, (m) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[m]
  );
}

function humanSize(b) {
  if (b < 1024) return b + " B";
  const u = ["KB", "MB", "GB", "TB"];
  let i = -1;
  let n = b;
  do {
    n /= 1024;
    i++;
  } while (n >= 1024 && i < u.length - 1);
  return n.toFixed(n < 10 ? 2 : 1) + " " + u[i];
}

function setLoading(on, text) {
  if (text) loadingText.textContent = text;
  loading.classList.toggle("hidden", !on);
}

let toastTimer = null;
function showToast(msg) {
  toast.textContent = msg;
  toast.classList.remove("hidden");
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => toast.classList.add("hidden"), 4800);
}

// ---- Open a file ------------------------------------------------------------
async function openPath(path) {
  setLoading(true, "Reading file…");
  try {
    const meta = await invoke("open_file", { path });
    currentPath = path;
    fileMeta = meta;
    sortState = null;
    filterState = null;
    truncated = false;
    cache.clear();
    pending.clear();
    edits.clear();
    resetAdvanced();
    viewToken++;
    totalRows = meta.num_rows;

    populateSearchColumns();
    buildMetaPanel();

    emptyEl.classList.add("hidden");
    tableWrap.classList.remove("hidden");
    statusBar.classList.remove("hidden");
    searchWrap.classList.remove("hidden");
    metaBtn.classList.remove("hidden");
    searchInput.value = "";
    searchClear.classList.add("hidden");
    fileNameEl.textContent = meta.file_name;
    document.title = "ParquetView — " + meta.file_name;

    // The table is visible now, so viewport.clientWidth is valid for sizing.
    computeColWidths();
    renderHeader();
    updateSpacer();

    viewport.scrollTop = 0;
    await loadPage(0, true);
    renderRows();
    updateStatus();
  } catch (e) {
    showToast("Couldn’t open file: " + e);
  } finally {
    setLoading(false);
  }
}

async function pickFile() {
  try {
    const path = await invoke("pick_parquet_file");
    if (path) openPath(path);
  } catch (e) {
    showToast(String(e));
  }
}

// ---- Column sizing ----------------------------------------------------------
function computeColWidths() {
  // Natural (content-based) width per column.
  const natural = fileMeta.columns.map((c) => {
    const chars = Math.max(c.name.length, (c.type || "").length + 3);
    return Math.min(340, Math.max(120, chars * 8 + 40));
  });
  const naturalTotal = natural.reduce((a, b) => a + b, 0);
  const avail = Math.max(0, viewport.clientWidth - GUTTER_W);

  if (settings.autoFit && naturalTotal > 0 && naturalTotal < avail) {
    // Room to spare: stretch columns proportionally so the grid fills the
    // window — no blank gap on the right.
    const scale = avail / naturalTotal;
    colWidths = natural.map((w) => Math.floor(w * scale));
    const used = colWidths.reduce((a, b) => a + b, 0);
    colWidths[colWidths.length - 1] += avail - used; // absorb rounding
  } else {
    // Columns already exceed the window: keep natural widths, scroll sideways.
    colWidths = natural;
  }
  gridWidth = GUTTER_W + colWidths.reduce((a, b) => a + b, 0);
}

function populateSearchColumns() {
  let html = '<option value="">All columns</option>';
  fileMeta.columns.forEach((c, i) => {
    html += `<option value="${i}">${escapeHtml(c.name)}</option>`;
  });
  searchColumn.innerHTML = html;
}

// ---- Rendering --------------------------------------------------------------
function renderHeader() {
  let html = '<div class="h-cell gutter">#</div>';
  fileMeta.columns.forEach((c, i) => {
    let ind = "";
    if (sortState && sortState.column === i)
      ind = `<span class="sort-ind">${sortState.ascending ? "▲" : "▼"}</span>`;
    html += `<div class="h-cell" style="width:${colWidths[i]}px" data-col="${i}" title="${escapeHtml(c.name)} · ${escapeHtml(c.type)}">
      <div class="h-name">${escapeHtml(c.name)}${ind}</div>
      <div class="h-type">${escapeHtml(c.type)}</div>
    </div>`;
  });
  html += '<div class="h-cell filler"></div>';
  headerRow.innerHTML = html;
  headerRow.style.width = "100%";
  grid.style.width = gridWidth + "px";
  headerRow.querySelectorAll(".h-cell[data-col]").forEach((el) => {
    el.addEventListener("click", () => onHeaderClick(parseInt(el.dataset.col, 10)));
  });
}

function updateSpacer() {
  spacer.style.height = totalRows * ROW_H + "px";
}

function getRow(i) {
  const p = Math.floor(i / PAGE);
  const page = cache.get(p);
  if (!page) return null;
  return page.rows[i - p * PAGE] ?? null;
}

// Global (file) row index for display row i — stable across sort/filter.
function getGindex(i) {
  const p = Math.floor(i / PAGE);
  const page = cache.get(p);
  if (!page) return null;
  return page.indices[i - p * PAGE] ?? null;
}

function renderRows() {
  if (!fileMeta) return;
  if (editingEl) return; // don't rebuild while a cell editor is open
  const vpH = viewport.clientHeight;
  const first = Math.max(0, Math.floor(viewport.scrollTop / ROW_H) - BUFFER);
  const visCount = Math.ceil(vpH / ROW_H) + BUFFER * 2;
  const last = Math.min(totalRows, first + visCount);
  const ncols = fileMeta.columns.length;

  let html = "";
  for (let i = first; i < last; i++) {
    const rec = getRow(i);
    const g = getGindex(i);
    const alt = i % 2 ? " alt" : "";
    html += `<div class="data-row${alt}" style="top:${i * ROW_H}px">`;
    html += `<div class="gutter">${(i + 1).toLocaleString()}</div>`;
    if (rec) {
      for (let c = 0; c < ncols; c++) {
        const w = colWidths[c];
        const key = g + ":" + c;
        const hasEdit = g !== null && edits.has(key);
        const val = hasEdit ? edits.get(key) : rec[c];
        const attrs = `data-r="${i}" data-c="${c}"`;
        if (!hasEdit && (val === null || val === undefined)) {
          html += `<div class="cell null" style="width:${w}px" ${attrs}>null</div>`;
        } else {
          let cls = fileMeta.columns[c].numeric ? "cell num" : "cell";
          if (hasEdit) cls += " edited";
          const esc = escapeHtml(val);
          html += `<div class="${cls}" style="width:${w}px" title="${esc}" ${attrs}>${esc}</div>`;
        }
      }
    } else {
      for (let c = 0; c < ncols; c++) {
        html += `<div class="cell null" style="width:${colWidths[c]}px">…</div>`;
      }
    }
    html += '<div class="cell filler"></div>';
    html += "</div>";
  }
  rows.innerHTML = html;
  ensureVisibleLoaded(first, last);
}

function scheduleRender() {
  if (rafPending) return;
  rafPending = true;
  requestAnimationFrame(() => {
    rafPending = false;
    renderRows();
  });
}

// ---- Paging -----------------------------------------------------------------
function ensureVisibleLoaded(first, last) {
  if (last <= first) return;
  const startPage = Math.floor(first / PAGE);
  const endPage = Math.floor((last - 1) / PAGE);
  for (let p = startPage; p <= endPage; p++) {
    if (!cache.has(p) && !pending.has(p)) {
      loadPage(p).then((changed) => {
        if (changed) scheduleRender();
      });
    }
  }
}

async function loadPage(pageIndex, force) {
  if (!force && (cache.has(pageIndex) || pending.has(pageIndex))) return false;
  if (pending.has(pageIndex)) return false;
  pending.add(pageIndex);
  const token = viewToken;
  try {
    const resp = await invoke("get_rows", {
      path: currentPath,
      offset: pageIndex * PAGE,
      limit: PAGE,
      sort: sortState,
      filter: filterState,
    });
    if (token !== viewToken) return false; // view changed underneath us
    cache.set(pageIndex, { rows: resp.rows, indices: resp.indices });
    totalRows = resp.total_rows;
    truncated = resp.truncated;
    return true;
  } catch (e) {
    if (token === viewToken) showToast(String(e));
    return false;
  } finally {
    pending.delete(pageIndex);
  }
}

// Re-fetch everything after a sort/filter change.
async function applyView() {
  viewToken++;
  cache.clear();
  pending.clear();
  const heavy = !!filterState || !!sortState;
  if (heavy) setLoading(true, filterState ? "Searching…" : "Sorting…");
  if (!filterState) totalRows = fileMeta.num_rows;
  await loadPage(0, true);
  if (heavy) setLoading(false);
  viewport.scrollTop = 0;
  updateSpacer();
  renderRows();
  updateStatus();
}

// ---- Sorting ----------------------------------------------------------------
function onHeaderClick(colIndex) {
  if (Number.isNaN(colIndex)) return;
  if (sortState && sortState.column === colIndex) {
    sortState = sortState.ascending ? { column: colIndex, ascending: false } : null;
  } else {
    sortState = { column: colIndex, ascending: true };
  }
  renderHeader();
  applyView();
}

// ---- Search -----------------------------------------------------------------
function currentSearchColumn() {
  const v = searchColumn.value;
  return v === "" ? null : parseInt(v, 10);
}

function triggerSearch() {
  const v = searchInput.value.trim();
  searchClear.classList.toggle("hidden", v.length === 0);
  filterState = v
    ? {
        mode: "simple",
        query: v,
        column: currentSearchColumn(),
        case_sensitive: settings.caseSensitive,
      }
    : null;
  applyView();
}

searchInput.addEventListener("input", () => {
  clearTimeout(searchTimer);
  searchClear.classList.toggle("hidden", searchInput.value.length === 0);
  searchTimer = setTimeout(triggerSearch, 320);
});
searchInput.addEventListener("keydown", (e) => {
  if (e.key === "Enter") {
    clearTimeout(searchTimer);
    triggerSearch();
  }
});
searchColumn.addEventListener("change", () => {
  if (filterState) triggerSearch();
});
searchClear.addEventListener("click", () => {
  searchInput.value = "";
  searchClear.classList.add("hidden");
  clearTimeout(searchTimer);
  if (filterState && filterState.mode === "simple") {
    filterState = null;
    applyView();
  }
});

// ---- Status bar -------------------------------------------------------------
function updateStatus() {
  if (!fileMeta) return;
  const totalStr = fileMeta.num_rows.toLocaleString();
  if (filterState) {
    statLeft.textContent = `${totalRows.toLocaleString()} match${totalRows === 1 ? "" : "es"} of ${totalStr} rows`;
    statCenter.innerHTML = truncated
      ? `<span class="warn">Showing first ${SEARCH_CAP.toLocaleString()} matches</span>`
      : "";
  } else {
    statLeft.textContent = `${totalStr} rows × ${fileMeta.num_columns} columns`;
    statCenter.textContent = sortState
      ? `Sorted by “${fileMeta.columns[sortState.column].name}” ${sortState.ascending ? "↑" : "↓"}`
      : "";
  }
  statRight.textContent = `${humanSize(fileMeta.file_size)} · ${fileMeta.num_row_groups.toLocaleString()} row group${fileMeta.num_row_groups === 1 ? "" : "s"}`;
}

// ---- Metadata panel ---------------------------------------------------------
function buildMetaPanel() {
  const m = fileMeta;
  const row = (k, v) =>
    `<div class="meta-row"><span class="k">${k}</span><span class="v">${escapeHtml(v)}</span></div>`;
  let html = "";
  html += row("File", m.file_name);
  html += row("Size", humanSize(m.file_size));
  html += row("Rows", m.num_rows.toLocaleString());
  html += row("Columns", m.num_columns.toLocaleString());
  html += row("Row groups", m.num_row_groups.toLocaleString());
  html += row("Compression", m.compression);
  html += row("Format version", "v" + m.version);
  if (m.created_by) html += row("Created by", m.created_by);
  html += '<div class="meta-section-title">Schema</div>';
  m.columns.forEach((c) => {
    html += `<div class="schema-item"><span class="sname">${escapeHtml(c.name)}</span><span class="stype">${escapeHtml(c.type)}</span></div>`;
  });
  html += '<div class="meta-section-title">Path</div>';
  html += `<div class="meta-row"><span class="v" style="text-align:left;font-family:var(--mono);font-size:11px">${escapeHtml(m.path)}</span></div>`;
  metaBody.innerHTML = html;
}

function openMeta() {
  metaPanel.classList.add("open");
  metaBackdropEl.classList.add("open");
}
function closeMeta() {
  metaPanel.classList.remove("open");
  metaBackdropEl.classList.remove("open");
}

// ---- Cell select / inline edit ---------------------------------------------
// Double-click a cell to select its whole value (copy with ⌘C) or edit it.
// Edits are session-local overrides keyed by global row index; they are shown
// with an accent marker and are NOT written back to the .parquet file.
function startEdit(cellEl, r, c) {
  if (editingEl) commitEdit();
  const g = getGindex(r);
  if (g === null) return; // row not loaded yet
  const rec = getRow(r);
  editingFileOrig = rec && rec[c] != null ? String(rec[c]) : "";
  const key = g + ":" + c;
  const current = edits.has(key) ? edits.get(key) : editingFileOrig;

  const input = document.createElement("input");
  input.className = "cell-editor";
  input.value = current;
  cellEl.textContent = "";
  cellEl.classList.add("editing");
  cellEl.appendChild(input);
  input.focus();
  input.select(); // whole value selected → ready to copy or overtype

  editingEl = cellEl;
  editingKey = key;

  input.addEventListener("keydown", (ev) => {
    if (ev.key === "Enter") {
      ev.preventDefault();
      commitEdit();
    } else if (ev.key === "Escape") {
      ev.preventDefault();
      cancelEdit();
    }
    ev.stopPropagation(); // keep global shortcuts out of the editor
  });
  input.addEventListener("blur", commitEdit);
  viewport.addEventListener("scroll", commitEdit, { once: true });
}

function commitEdit() {
  if (!editingEl) return;
  const input = editingEl.querySelector("input");
  const val = input ? input.value : editingFileOrig;
  const key = editingKey;
  editingEl = null;
  editingKey = null;
  viewport.removeEventListener("scroll", commitEdit);
  if (val === editingFileOrig) edits.delete(key);
  else edits.set(key, val);
  scheduleRender();
}

function cancelEdit() {
  if (!editingEl) return;
  editingEl = null;
  editingKey = null;
  viewport.removeEventListener("scroll", commitEdit);
  scheduleRender();
}

rows.addEventListener("dblclick", (e) => {
  const cellEl = e.target.closest(".cell");
  if (!cellEl || cellEl.classList.contains("filler")) return;
  const r = parseInt(cellEl.dataset.r, 10);
  const c = parseInt(cellEl.dataset.c, 10);
  if (Number.isNaN(r) || Number.isNaN(c)) return;
  e.preventDefault();
  startEdit(cellEl, r, c);
});

// ---- Settings ---------------------------------------------------------------
const DEFAULT_SETTINGS = {
  theme: "auto",
  density: "default",
  font: "default",
  autoFit: true,
  caseSensitive: false,
};
let settings = { ...DEFAULT_SETTINGS };
const DENSITY_PX = { compact: 24, default: 30, comfortable: 38 };
const FONT_PX = { small: 11, default: 12, large: 13 };

function loadSettings() {
  try {
    const raw = localStorage.getItem("parquetview.settings");
    if (raw) settings = { ...DEFAULT_SETTINGS, ...JSON.parse(raw) };
  } catch (_) {
    /* ignore corrupt settings */
  }
}
function saveSettings() {
  try {
    localStorage.setItem("parquetview.settings", JSON.stringify(settings));
  } catch (_) {
    /* ignore */
  }
}
function applySettings(rerender) {
  const root = document.documentElement;
  if (settings.theme === "auto") root.removeAttribute("data-theme");
  else root.setAttribute("data-theme", settings.theme);
  ROW_H = DENSITY_PX[settings.density] || 30;
  root.style.setProperty("--row-h", ROW_H + "px");
  root.style.setProperty("--cell-font", (FONT_PX[settings.font] || 12) + "px");
  if (rerender && fileMeta) {
    computeColWidths();
    renderHeader();
    updateSpacer();
    scheduleRender();
  }
}
function initSettingsControls() {
  setTheme.value = settings.theme;
  setDensity.value = settings.density;
  setFont.value = settings.font;
  setAutoFit.checked = settings.autoFit;
  setCase.checked = settings.caseSensitive;
}
function openSettings() {
  settingsWin.classList.add("open");
  settingsBackdrop.classList.add("open");
}
function closeSettings() {
  settingsWin.classList.remove("open");
  settingsBackdrop.classList.remove("open");
}

setTheme.addEventListener("change", () => {
  settings.theme = setTheme.value;
  applySettings(false);
  saveSettings();
});
setDensity.addEventListener("change", () => {
  settings.density = setDensity.value;
  applySettings(true);
  saveSettings();
});
setFont.addEventListener("change", () => {
  settings.font = setFont.value;
  applySettings(true);
  saveSettings();
});
setAutoFit.addEventListener("change", () => {
  settings.autoFit = setAutoFit.checked;
  applySettings(true);
  saveSettings();
});
setCase.addEventListener("change", () => {
  settings.caseSensitive = setCase.checked;
  saveSettings();
  if (filterState && filterState.mode === "simple") triggerSearch();
});
settingsBtn.addEventListener("click", openSettings);
settingsClose.addEventListener("click", closeSettings);
settingsBackdrop.addEventListener("click", closeSettings);

// ---- Advanced filter --------------------------------------------------------
const OPERATORS = [
  { v: "contains", label: "contains" },
  { v: "not_contains", label: "does not contain" },
  { v: "equals", label: "equals" },
  { v: "not_equals", label: "not equals" },
  { v: "starts_with", label: "starts with" },
  { v: "ends_with", label: "ends with" },
  { v: "regex", label: "matches regex" },
  { v: "gt", label: "greater than (>)" },
  { v: "gte", label: "≥" },
  { v: "lt", label: "less than (<)" },
  { v: "lte", label: "≤" },
  { v: "is_null", label: "is empty" },
  { v: "is_not_null", label: "is not empty" },
];
const NO_VALUE_OPS = new Set(["is_null", "is_not_null"]);

function addConditionRow(preset) {
  const row = document.createElement("div");
  row.className = "adv-cond";
  const colOpts = fileMeta.columns
    .map((c, i) => `<option value="${i}">${escapeHtml(c.name)}</option>`)
    .join("");
  const opOpts = OPERATORS.map(
    (o) => `<option value="${o.v}">${o.label}</option>`
  ).join("");
  row.innerHTML =
    `<select class="select adv-col">${colOpts}</select>` +
    `<select class="select adv-op">${opOpts}</select>` +
    `<input class="adv-val" type="text" placeholder="value" spellcheck="false" />` +
    `<button class="adv-cs" type="button" title="Case sensitive">Aa</button>` +
    `<button class="adv-rm" type="button" title="Remove condition">✕</button>`;

  const colSel = row.querySelector(".adv-col");
  const opSel = row.querySelector(".adv-op");
  const valInp = row.querySelector(".adv-val");
  const csBtn = row.querySelector(".adv-cs");

  if (preset) {
    if (preset.column != null) colSel.value = String(preset.column);
    if (preset.op) opSel.value = preset.op;
    if (preset.value != null) valInp.value = preset.value;
    csBtn.classList.toggle("on", !!preset.case_sensitive);
  } else {
    csBtn.classList.toggle("on", settings.caseSensitive);
  }

  const syncDisabled = () => {
    valInp.disabled = NO_VALUE_OPS.has(opSel.value);
  };
  syncDisabled();
  opSel.addEventListener("change", syncDisabled);
  csBtn.addEventListener("click", () => csBtn.classList.toggle("on"));
  row.querySelector(".adv-rm").addEventListener("click", () => {
    row.remove();
    if (!advConditions.children.length) addConditionRow();
  });
  valInp.addEventListener("keydown", (e) => {
    if (e.key === "Enter") applyAdvanced();
  });
  advConditions.appendChild(row);
}

function gatherConditions() {
  const conds = [];
  advConditions.querySelectorAll(".adv-cond").forEach((row) => {
    const column = parseInt(row.querySelector(".adv-col").value, 10);
    const op = row.querySelector(".adv-op").value;
    const value = row.querySelector(".adv-val").value;
    const case_sensitive = row.querySelector(".adv-cs").classList.contains("on");
    if (Number.isNaN(column)) return;
    if (!NO_VALUE_OPS.has(op) && value === "") return; // skip incomplete rows
    conds.push({ column, op, value, case_sensitive });
  });
  return conds;
}

function openAdvanced() {
  if (!fileMeta) return;
  if (!advConditions.children.length) addConditionRow();
  advPanel.classList.remove("hidden");
  advBackdrop.classList.add("open");
}
function closeAdvanced() {
  advPanel.classList.add("hidden");
  advBackdrop.classList.remove("open");
}
function resetAdvanced() {
  advConditions.innerHTML = "";
  advCombine.value = "and";
  advBtn.classList.remove("active");
  advBadge.classList.add("hidden");
  searchInput.disabled = false;
  searchInput.placeholder = "Search…";
  closeAdvanced();
}

function applyAdvanced() {
  const conditions = gatherConditions();
  if (!conditions.length) {
    // Nothing usable → behave like clearing.
    if (filterState && filterState.mode === "advanced") {
      filterState = null;
      applyView();
    }
    resetActiveAdvancedChrome(false);
    closeAdvanced();
    return;
  }
  filterState = { mode: "advanced", conditions, combine: advCombine.value };
  resetActiveAdvancedChrome(true, conditions.length);
  closeAdvanced();
  applyView();
}

// Toggles the toolbar chrome (badge, disabled search box) for the advanced state.
function resetActiveAdvancedChrome(active, count) {
  advBtn.classList.toggle("active", active);
  advBadge.classList.toggle("hidden", !active);
  if (active) advBadge.textContent = String(count);
  searchInput.disabled = active;
  searchInput.placeholder = active ? "Advanced filter active" : "Search…";
  if (active) {
    searchInput.value = "";
    searchClear.classList.add("hidden");
  }
}

advBtn.addEventListener("click", () => {
  if (advPanel.classList.contains("hidden")) openAdvanced();
  else closeAdvanced();
});
advBackdrop.addEventListener("click", closeAdvanced);
advAdd.addEventListener("click", () => addConditionRow());
advApply.addEventListener("click", applyAdvanced);
advClear.addEventListener("click", () => {
  advConditions.innerHTML = "";
  advCombine.value = "and";
  addConditionRow();
  const wasActive = filterState && filterState.mode === "advanced";
  resetActiveAdvancedChrome(false);
  if (wasActive) {
    filterState = null;
    applyView();
  }
});

// ---- Wiring -----------------------------------------------------------------
$("openBtn").addEventListener("click", pickFile);
$("openBtn2").addEventListener("click", pickFile);
metaBtn.addEventListener("click", openMeta);
$("metaClose").addEventListener("click", closeMeta);
metaBackdropEl.addEventListener("click", closeMeta);

viewport.addEventListener("scroll", scheduleRender, { passive: true });
window.addEventListener("resize", () => {
  if (fileMeta) {
    computeColWidths();
    renderHeader();
  }
  scheduleRender();
});

window.addEventListener("keydown", (e) => {
  const mod = e.metaKey || e.ctrlKey;
  if (mod && e.key.toLowerCase() === "o") {
    e.preventDefault();
    pickFile();
  } else if (mod && e.key === ",") {
    e.preventDefault();
    openSettings();
  } else if (e.key === "Escape") {
    closeSettings();
    closeAdvanced();
    closeMeta();
  } else if (mod && e.key.toLowerCase() === "f" && fileMeta) {
    e.preventDefault();
    if (searchInput.disabled) openAdvanced();
    else {
      searchInput.focus();
      searchInput.select();
    }
  }
});

// ---- Native file open (drag-drop, "Open With", CLI) -------------------------
listen("tauri://drag-enter", () => dropOverlay.classList.add("show"));
listen("tauri://drag-over", () => dropOverlay.classList.add("show"));
listen("tauri://drag-leave", () => dropOverlay.classList.remove("show"));
listen("tauri://drag-drop", (e) => {
  dropOverlay.classList.remove("show");
  const paths = (e.payload && e.payload.paths) || [];
  const pq = paths.find((p) => p.toLowerCase().endsWith(".parquet")) || paths[0];
  if (pq) openPath(pq);
});

listen("open-file", (e) => {
  if (e.payload) openPath(e.payload);
});

// ---- Startup ----------------------------------------------------------------
loadSettings();
applySettings(false);
initSettingsControls();

// A file may have been passed at launch (Finder "Open With" / `open -a`).
// Retry a few times: on a cold launch the OS "Opened" event can land just
// after the first poll, so one check isn't always enough.
(async () => {
  for (const delay of [0, 400, 1200]) {
    if (delay) await new Promise((r) => setTimeout(r, delay));
    if (fileMeta) return; // a file already opened (event or earlier poll)
    try {
      const startup = await invoke("take_startup_file");
      if (startup) {
        openPath(startup);
        return;
      }
    } catch (_) {
      /* ignore */
    }
  }
})();
