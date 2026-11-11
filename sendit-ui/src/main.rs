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
            let (window, view) = gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| app::SenditApp::new(window, cx))
            })
            .expect("failed to open the window");
            let quit_view = view.clone();
            cx.on_action(move |_: &Quit, cx| {
                let view = quit_view.clone();
                let asked = window.update(cx, |_, window, cx| {
                    view.update(cx, |app, cx| app.request_quit(window, cx))
                });
                if asked.is_err() {
                    cx.quit();
                }
            });
            #[cfg(feature = "screenshot")]
            screenshot::schedule(window, view, cx);
            cx.activate(true);
        });
}
