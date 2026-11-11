mod console;
mod diagram;
mod er_layout;
mod grid;
mod navigator;
mod table_view;
mod tree;
mod workspace;

use gpui_kit::component::*;
use gpui_kit::*;

actions!(cirrus, [RunQuery, CopySelection, Quit, CloseTab]);

fn main() {
    // Start the I/O runtime before any window exists.
    cirrus_core::runtime::runtime();

    gpui_kit::application()
        .with_assets(gpui_kit::assets::AllAssets)
        .with_quit_mode(QuitMode::LastWindowClosed)
        .run(|cx: &mut App| {
            gpui_kit::init(cx);
            Theme::sync_system_appearance(None, cx);

            cx.bind_keys([
                KeyBinding::new("cmd-enter", RunQuery, Some("Console > Input")),
                KeyBinding::new("cmd-enter", RunQuery, Some("Console")),
                KeyBinding::new("cmd-c", CopySelection, Some("DataTable")),
                KeyBinding::new("cmd-w", CloseTab, Some("Workspace")),
                KeyBinding::new("cmd-q", Quit, None),
            ]);
            cx.on_action(|_: &Quit, cx| cx.quit());
            cx.set_menus(vec![Menu { name: "Cirrus".into(), items: vec![MenuItem::action("Quit", Quit)], disabled: false }]);

            let bounds = Bounds::centered(None, size(px(1360.), px(860.)), cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(900.), px(560.))),
                ..TitleBar::window_options()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                window
                    .observe_window_appearance(|window, cx| Theme::sync_system_appearance(Some(window), cx))
                    .detach();
                window.set_window_title("Cirrus");
                cx.new(|cx| workspace::Workspace::new(window, cx))
            })
            .expect("failed to open window");
            cx.activate(true);
        });
}
