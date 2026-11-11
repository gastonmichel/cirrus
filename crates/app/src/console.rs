//! Read-only SQL console for one database.

use gpui_kit::assets::IconName as I;
use gpui_kit::component::button::*;
use gpui_kit::component::input::{Editor, EditorState};
use gpui_kit::component::*;
use gpui_kit::component::tag::Tag;
use gpui_kit::*;

use crate::RunQuery;
use crate::grid::ResultView;
use crate::tree::DbRef;

const MAX_ROWS: usize = 1000;

pub struct ConsoleView {
    db: DbRef,
    editor: Entity<EditorState>,
    results: Entity<ResultView>,
    running: Option<Task<()>>,
}

impl ConsoleView {
    pub fn new(db: DbRef, initial_sql: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let sql = initial_sql.unwrap_or_else(|| {
            "-- Read-only console: every statement runs in a READ ONLY transaction that is rolled back.\n\
             -- ⌘↵ runs the selection, or the whole editor if nothing is selected.\n\n\
             SELECT table_schema, table_name, table_type\n\
             FROM information_schema.tables\n\
             WHERE table_schema NOT IN ('pg_catalog', 'information_schema')\n\
             ORDER BY 1, 2;\n"
                .to_string()
        });
        let editor = cx.new(|cx| EditorState::new(window, cx).language("sql").default_value(sql));
        let results = cx.new(|cx| ResultView::new(format!("{}_query", db.database), window, cx));
        Self { db, editor, results, running: None }
    }

    pub fn title(&self) -> String {
        format!("SQL · {}", self.db.database)
    }

    fn run(&mut self, _: &RunQuery, _: &mut Window, cx: &mut Context<Self>) {
        let editor = self.editor.read(cx);
        let selected = editor.selected_value();
        let sql = if selected.trim().is_empty() { editor.value() } else { selected }.to_string();
        if sql.trim().is_empty() {
            return;
        }
        self.results.update(cx, |r, cx| r.set_loading(true, cx));
        let db = self.db.clone();
        // Replacing the task cancels the UI side of a previous run.
        self.running = Some(cx.spawn(async move |this, cx| {
            let result = cirrus_core::runtime::spawn(async move {
                let session = cirrus_core::pg::session(&db.project, &db.instance, &db.database).await?;
                session.run_readonly(&sql, MAX_ROWS).await
            })
            .await;
            this.update(cx, |this, cx| {
                this.running = None;
                this.results.update(cx, |r, cx| match result {
                    Ok(res) => {
                        r.status = res.has_more.then(|| format!("limited to {MAX_ROWS} rows"));
                        r.set_result(res, cx)
                    }
                    Err(e) => r.set_error(format!("{e:#}"), cx),
                });
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }
}

impl Render for ConsoleView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let running = self.running.is_some();
        v_flex()
            .size_full()
            .key_context("Console")
            .on_action(cx.listener(Self::run))
            .child(
                h_flex()
                    .px_3()
                    .py_1p5()
                    .gap_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(Icon::new(I::SquareTerminal).text_color(theme.muted_foreground))
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(format!("{} / {}", self.db.instance, self.db.database)),
                    )
                    .child(Tag::secondary().small().child("read-only"))
                    .child(div().flex_1())
                    .child(
                        Button::new("run")
                            .primary()
                            .small()
                            .icon(Icon::new(I::Play))
                            .label("Run")
                            .loading(running)
                            .tooltip("Run (⌘↵)")
                            .on_click(cx.listener(|this, _, window, cx| this.run(&RunQuery, window, cx))),
                    ),
            )
            .child(
                v_resizable("console-split")
                    .child(
                        resizable_panel()
                            .size(px(260.))
                            .size_range(px(80.)..px(2000.))
                            .child(div().size_full().child(Editor::new(&self.editor).h_full().bordered(false))),
                    )
                    .child(resizable_panel().child(self.results.clone())),
            )
    }
}
