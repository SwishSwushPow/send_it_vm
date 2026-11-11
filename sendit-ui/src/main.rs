//! sendit-ui: a desktop app for sendit's VMs, base images and
//! configuration. It reads them through sendit's library and runs the
//! sendit binary for everything that changes them.

mod app;
mod config_edit;
mod config_page;
mod images;
mod launch;
mod ops;
mod other_pages;
mod prefs;
mod runopts;
#[cfg(feature = "screenshot")]
mod screenshot;
mod snapshot;
mod ui;
mod vms;

use gpui_kit::*;

gpui_kit::actions!(sendit_ui, [Quit]);

fn main() {
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(|cx| {
            gpui_kit::init(cx);
            cx.on_action(|_: &Quit, cx| cx.quit());
            cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
            cx.set_menus([Menu::new("sendit").items([MenuItem::action("Quit", Quit)])]);
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1180.), px(780.)),
                    cx,
                ))),
                titlebar: Some(TitlebarOptions {
                    title: Some("Send it".into()),
                    appears_transparent: true,
                    traffic_light_position: Some(point(px(12.), px(12.))),
                }),
                window_min_size: Some(size(px(900.), px(560.))),
                ..WindowOptions::default()
            };
            let (_window, _view) = gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| app::SenditApp::new(window, cx))
            })
            .expect("failed to open the window");
            #[cfg(feature = "screenshot")]
            screenshot::schedule(_window, _view, cx);
            cx.activate(true);
        });
}
