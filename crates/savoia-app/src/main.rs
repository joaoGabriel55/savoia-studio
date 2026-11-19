mod assets;
#[cfg(feature = "bench")]
mod bench;
mod commands;
mod completion;
mod connection_form;
mod console;
mod crash;
mod data_grid;
mod data_pickers;
mod data_sources;
mod data_view;
mod diagram;
mod erd_layout;
mod explorer;
mod history;
mod memory;
mod relations;
mod results;
mod runtime;
mod session;
mod settings;
mod settings_view;
mod structure;
mod table_menu;
mod theme;
mod transfer;
#[cfg(test)]
mod ui_tests;
mod updates;
mod workspace;

use gpui_kit::component::TitleBar;
use gpui_kit::*;

/// The Wayland app id and X11 `WM_CLASS`. Desktops find the icon by matching
/// it to `savoia-studio.desktop`, which every Linux package installs.
const APP_ID: &str = "savoia-studio";

/// The window icon on X11, which also shows it for an AppImage that installed
/// no `.desktop` file. Wayland has no such property and goes by [`APP_ID`].
#[cfg(target_os = "linux")]
fn x11_icon() -> Option<std::sync::Arc<image::RgbaImage>> {
    let png = include_bytes!("../../../assets/icons/icon-128.png");
    image::load_from_memory_with_format(png, image::ImageFormat::Png)
        .map(|icon| std::sync::Arc::new(icon.into_rgba8()))
        .ok()
}

fn main() {
    crash::install();
    gpui_kit::application()
        .with_assets(assets::AppAssets)
        .run(|cx| {
            gpui_kit::init(cx);
            // The saved appearance replaces this once the store is open.
            theme::apply(theme::Appearance::System, cx);
            console::init(cx);
            commands::init(cx);

            let bounds = Bounds::centered(None, size(px(1360.), px(860.)), cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(900.), px(560.))),
                app_id: Some(APP_ID.into()),
                #[cfg(target_os = "linux")]
                icon: x11_icon(),
                ..TitleBar::window_options()
            };
            #[cfg(feature = "bench")]
            if let Some(bench) = bench::from_env() {
                return run_bench(bench, options, cx);
            }
            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| workspace::Workspace::new(window, cx))
            })
            .expect("failed to open the main window");
            cx.activate(true);
        });
}

#[cfg(feature = "bench")]
fn run_bench(bench: bench::Bench, options: WindowOptions, cx: &mut App) {
    match bench {
        // The real startup path, store and all.
        bench::Bench::Startup => {
            gpui_kit::open_window(options, cx, |window, cx| {
                bench::after_first_frame(window, bench::report_startup);
                cx.new(|cx| workspace::Workspace::new(window, cx))
            })
            .expect("window");
        }
        bench::Bench::Scroll => {
            let data_sources = bench::data_sources(cx);
            let ds = data_sources.clone();
            let (window, workspace) = gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| workspace::Workspace::with_data_sources(ds, window, cx))
            })
            .expect("window");
            window
                .update(cx, |_, window, cx| {
                    bench::scroll(workspace, data_sources, window, cx)
                })
                .expect("window");
        }
    }
    cx.activate(true);
}
