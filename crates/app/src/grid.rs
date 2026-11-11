//! Reusable, virtualized result grid with copy and CSV export.
//!
//! Sorting and infinite scrolling are delegated to the owner through callbacks, so
//! the same grid serves server-side paging (table data) and in-memory results (console).

use std::path::PathBuf;
use std::rc::Rc;

use gpui_kit::assets::IconName as I;
use gpui_kit::component::button::*;
use gpui_kit::component::table::{Column, ColumnSort, DataTable, TableDelegate, TableSelection, TableState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::component::notification::Notification;
use gpui_kit::*;
use cirrus_core::QueryResult;

use crate::CopySelection;

pub type SortCallback = Rc<dyn Fn(String, ColumnSort, &mut Window, &mut App)>;
/// Receives the offset (number of rows already loaded).
pub type LoadMoreCallback = Rc<dyn Fn(usize, &mut Window, &mut App)>;

const MAX_CELL_CHARS: usize = 300;

pub struct ResultDelegate {
    pub result: QueryResult,
    pub loading: bool,
    pub loading_more: bool,
    /// Server-side sort; when `None`, sorting happens in memory.
    on_sort: Option<SortCallback>,
    on_load_more: Option<LoadMoreCallback>,
    widths: Vec<Pixels>,
}

impl ResultDelegate {
    fn new() -> Self {
        Self { result: QueryResult::default(), loading: false, loading_more: false, on_sort: None, on_load_more: None, widths: vec![] }
    }

    fn compute_widths(&mut self) {
        self.widths = self
            .result
            .columns
            .iter()
            .enumerate()
            .map(|(ix, col)| {
                let header = col.name.chars().count().max(col.type_name.chars().count() + 2);
                let sample = self
                    .result
                    .rows
                    .iter()
                    .take(100)
                    .map(|r| r.get(ix).and_then(|v| v.as_deref()).map_or(4, |v| v.chars().count()))
                    .max()
                    .unwrap_or(0);
                px((header.max(sample) as f32 * 7.5 + 28.).clamp(70., 420.))
            })
            .collect();
    }

    fn value(&self, row: usize, col: usize) -> Option<&str> {
        self.result.rows.get(row)?.get(col)?.as_deref()
    }
}

impl TableDelegate for ResultDelegate {
    fn columns_count(&self, _: &App) -> usize {
        self.result.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.result.rows.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        let col = &self.result.columns[col_ix];
        Column::new(SharedString::from(format!("c{col_ix}")), SharedString::from(col.name.clone()))
            .width(self.widths.get(col_ix).copied().unwrap_or(px(120.)))
            .sortable()
    }

    fn render_th(&mut self, col_ix: usize, _: &mut Window, cx: &mut Context<TableState<Self>>) -> impl IntoElement {
        let col = &self.result.columns[col_ix];
        v_flex()
            .overflow_hidden()
            .child(
                div()
                    .text_sm()
                    .line_height(px(18.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .truncate()
                    .child(col.name.clone()),
            )
            .when(!col.type_name.is_empty(), |el| {
                el.child(
                    div()
                        .text_xs()
                        .line_height(px(15.))
                        .text_color(cx.theme().muted_foreground)
                        .truncate()
                        .child(col.type_name.clone()),
                )
            })
    }

    fn render_td(&mut self, row: usize, col: usize, _: &mut Window, cx: &mut Context<TableState<Self>>) -> impl IntoElement {
        let el = div().text_sm().truncate();
        match self.value(row, col) {
            None => el.italic().text_color(cx.theme().muted_foreground.opacity(0.7)).child("NULL"),
            Some(v) => {
                let mut text: String = v.chars().take(MAX_CELL_CHARS).collect();
                if text.contains(['\n', '\r']) {
                    text = text.replace(['\n', '\r'], " ");
                }
                el.child(text)
            }
        }
    }

    fn cell_text(&self, row: usize, col: usize, _: &App) -> String {
        self.value(row, col).unwrap_or("").to_string()
    }

    fn loading(&self, _: &App) -> bool {
        self.loading
    }

    fn has_more(&self, _: &App) -> bool {
        self.on_load_more.is_some() && self.result.has_more && !self.loading_more
    }

    fn load_more(&mut self, window: &mut Window, cx: &mut Context<TableState<Self>>) {
        if self.loading_more || !self.result.has_more {
            return;
        }
        if let Some(cb) = self.on_load_more.clone() {
            self.loading_more = true;
            cb(self.result.rows.len(), window, cx);
        }
    }

    fn perform_sort(&mut self, col_ix: usize, sort: ColumnSort, window: &mut Window, cx: &mut Context<TableState<Self>>) {
        match self.on_sort.clone() {
            Some(cb) => {
                let name = self.result.columns[col_ix].name.clone();
                cb(name, sort, window, cx);
            }
            None => {
                // In-memory sort (console results). Default order is not restorable; keep current order.
                let desc = matches!(sort, ColumnSort::Descending);
                if matches!(sort, ColumnSort::Ascending | ColumnSort::Descending) {
                    self.result.rows.sort_by(|a, b| {
                        let (a, b) = (a.get(col_ix).cloned().flatten(), b.get(col_ix).cloned().flatten());
                        let ord = compare_values(a.as_deref(), b.as_deref());
                        if desc { ord.reverse() } else { ord }
                    });
                }
                cx.notify();
            }
        }
    }
}

/// Numeric-aware comparison; NULLs sort last.
fn compare_values(a: Option<&str>, b: Option<&str>) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    match (a, b) {
        (None, None) => Equal,
        (None, _) => Greater,
        (_, None) => Less,
        (Some(a), Some(b)) => match (a.parse::<f64>(), b.parse::<f64>()) {
            (Ok(x), Ok(y)) => x.total_cmp(&y),
            _ => a.cmp(b),
        },
    }
}

/// A result grid plus a small toolbar (row count, copy, export).
pub struct ResultView {
    table: Entity<TableState<ResultDelegate>>,
    error: Option<String>,
    /// Suggested file name (without extension) for CSV export.
    export_name: String,
    /// Extra status text supplied by the owner (e.g. "1,234 rows total").
    pub status: Option<String>,
}

impl ResultView {
    pub fn new(export_name: impl Into<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let table = cx.new(|cx| {
            TableState::new(ResultDelegate::new(), window, cx)
                .cell_selectable(true)
                .col_resizable(true)
                .col_movable(false)
                .sortable(true)
        });
        Self { table, error: None, export_name: export_name.into(), status: None }
    }

    pub fn on_sort(&mut self, cb: SortCallback, cx: &mut Context<Self>) {
        self.table.update(cx, |t, _| t.delegate_mut().on_sort = Some(cb));
    }

    pub fn on_load_more(&mut self, cb: LoadMoreCallback, cx: &mut Context<Self>) {
        self.table.update(cx, |t, _| t.delegate_mut().on_load_more = Some(cb));
    }

    pub fn set_loading(&mut self, loading: bool, cx: &mut Context<Self>) {
        self.table.update(cx, |t, cx| {
            t.delegate_mut().loading = loading;
            cx.notify();
        });
        cx.notify();
    }

    /// Replaces the whole result (resets columns, widths and sort indicators).
    pub fn set_result(&mut self, result: QueryResult, cx: &mut Context<Self>) {
        self.error = None;
        self.table.update(cx, |t, cx| {
            let d = t.delegate_mut();
            d.result = result;
            d.loading = false;
            d.loading_more = false;
            d.compute_widths();
            t.refresh(cx);
            t.clear_selection(cx);
            t.scroll_to_row(0, cx);
            cx.notify();
        });
        cx.notify();
    }

    /// Replaces rows but keeps columns, widths and the sort indicator (server-side sort).
    pub fn replace_rows(&mut self, result: QueryResult, cx: &mut Context<Self>) {
        self.error = None;
        self.table.update(cx, |t, cx| {
            let d = t.delegate_mut();
            d.result = result;
            d.loading = false;
            d.loading_more = false;
            t.clear_selection(cx);
            t.scroll_to_row(0, cx);
            cx.notify();
        });
        cx.notify();
    }

    pub fn append_rows(&mut self, page: QueryResult, cx: &mut Context<Self>) {
        self.table.update(cx, |t, cx| {
            let d = t.delegate_mut();
            d.result.rows.extend(page.rows);
            d.result.has_more = page.has_more;
            d.loading_more = false;
            cx.notify();
        });
        cx.notify();
    }

    pub fn set_error(&mut self, error: impl Into<String>, cx: &mut Context<Self>) {
        self.error = Some(error.into());
        self.table.update(cx, |t, cx| {
            let d = t.delegate_mut();
            d.loading = false;
            d.loading_more = false;
            cx.notify();
        });
        cx.notify();
    }

    pub fn result<'a>(&self, cx: &'a App) -> &'a QueryResult {
        &self.table.read(cx).delegate().result
    }

    fn copy_selection(&mut self, _: &CopySelection, window: &mut Window, cx: &mut Context<Self>) {
        let state = self.table.read(cx);
        let d = state.delegate();
        let text = match state.selection() {
            TableSelection::Cell(r, c) => d.value(r, c).unwrap_or("").to_string(),
            TableSelection::Row(r) => cirrus_core::export::to_tsv([row_values(&d.result, r)]),
            TableSelection::Column(c) => {
                cirrus_core::export::to_tsv(d.result.rows.iter().map(|row| vec![row.get(c).and_then(|v| v.as_deref())]))
            }
            _ => return,
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        window.push_notification("Copied to clipboard", cx);
    }

    fn copy_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let result = self.result(cx);
        let header = result.columns.iter().map(|c| Some(c.name.as_str())).collect::<Vec<_>>();
        let rows = (0..result.rows.len()).map(|r| row_values(result, r));
        let text = cirrus_core::export::to_tsv(std::iter::once(header).chain(rows));
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        window.push_notification("Copied all loaded rows to clipboard", cx);
    }

    fn export_csv(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let csv = match cirrus_core::export::to_csv(self.result(cx)) {
            Ok(csv) => csv,
            Err(e) => return window.push_notification(Notification::error(e.to_string()), cx),
        };
        let dir = std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Downloads")).unwrap_or_else(|| ".".into());
        let rx = cx.prompt_for_new_path(&dir, Some(&format!("{}.csv", self.export_name)));
        cx.spawn_in(window, async move |_, cx| {
            let Ok(Ok(Some(path))) = rx.await else { return };
            let outcome = std::fs::write(&path, csv);
            cx.update(|window, cx| match outcome {
                Ok(()) => window.push_notification(
                    Notification::success(format!("Exported to {}", path.display())),
                    cx,
                ),
                Err(e) => window.push_notification(Notification::error(format!("Export failed: {e}")), cx),
            })
            .ok();
        })
        .detach();
    }
}

fn row_values(result: &QueryResult, row: usize) -> Vec<Option<&str>> {
    result.rows[row].iter().map(|v| v.as_deref()).collect()
}

fn summarize(result: &QueryResult) -> String {
    let mut s = format!("{} row{}", result.rows.len(), if result.rows.len() == 1 { "" } else { "s" });
    if result.has_more {
        s.push_str(" loaded (more available)");
    }
    if !result.elapsed.is_zero() {
        s.push_str(&format!(" · {} ms", result.elapsed.as_millis()));
    }
    s
}

impl Render for ResultView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let result = self.result(cx);
        let has_rows = !result.rows.is_empty();
        let summary = match (&result.message, result.columns.is_empty()) {
            (Some(msg), true) => msg.clone(),
            _ => summarize(result),
        };
        let loading = self.table.read(cx).delegate().loading;
        let two_line_header = result.columns.iter().any(|c| !c.type_name.is_empty());

        let toolbar = h_flex()
            .h(px(34.))
            .px_2()
            .gap_1()
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.title_bar)
            .child(
                div()
                    .flex_1()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(match (&self.status, loading) {
                        (_, true) => "Loading…".to_string(),
                        (Some(status), _) => format!("{summary} · {status}"),
                        (None, _) => summary,
                    }),
            )
            .child(
                Button::new("copy-all")
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(I::Copy))
                    .label("Copy")
                    .tooltip("Copy all loaded rows as TSV (⌘C copies the selected cell/row)")
                    .disabled(!has_rows)
                    .on_click(cx.listener(|this, _, window, cx| this.copy_all(window, cx))),
            )
            .child(
                Button::new("export-csv")
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(I::FileDown))
                    .label("Export CSV")
                    .tooltip("Export loaded rows to a CSV file")
                    .disabled(!has_rows)
                    .on_click(cx.listener(|this, _, window, cx| this.export_csv(window, cx))),
            );

        v_flex()
            .size_full()
            .min_h_0()
            .on_action(cx.listener(Self::copy_selection))
            .child(match &self.error {
                Some(error) => div()
                    .id("result-error")
                    .flex_1()
                    .p_4()
                    .overflow_y_scroll()
                    .child(
                        div()
                            .p_3()
                            .rounded(theme.radius)
                            .border_1()
                            .border_color(theme.danger.opacity(0.4))
                            .bg(theme.danger.opacity(0.08))
                            .text_sm()
                            .text_color(theme.danger)
                            .font_family(theme.mono_font_family.clone())
                            .whitespace_normal()
                            .child(error.clone()),
                    )
                    .into_any_element(),
                None => div()
                    .flex_1()
                    .min_h_0()
                    .child(
                        DataTable::new(&self.table)
                            .stripe(true)
                            .bordered(false)
                            .small()
                            // Headers show the column name plus its type on a second line.
                            .header_row_height(px(if two_line_header { 44. } else { 30. })),
                    )
                    .into_any_element(),
            })
            .child(toolbar)
    }
}
