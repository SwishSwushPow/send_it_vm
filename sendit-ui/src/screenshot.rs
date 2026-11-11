//! For development: saves the window as an image and quits, to look at the
//! app without a screen to capture. See the `screenshot` feature.

use std::time::Duration;

use gpui_kit::*;

use crate::app::{Page, Panel, Selection, SenditApp};

pub fn schedule(window: AnyWindowHandle, view: Entity<SenditApp>, cx: &mut App) {
    let Some(file) = std::env::var_os("SENDIT_UI_SCREENSHOT") else {
        return;
    };
    cx.spawn(async move |cx| {
        cx.background_executor().timer(Duration::from_secs(2)).await;
        let _ = cx.update_window(window, |_, window, cx| {
            view.update(cx, |app, cx| {
                let page = match std::env::var("SENDIT_UI_PAGE").as_deref() {
                    Ok("images") => Page::Images,
                    Ok("config") => Page::Config,
                    Ok("maintenance") => Page::Maintenance,
                    Ok("activity") => Page::Activity,
                    Ok("preferences") => Page::Preferences,
                    _ => Page::Vms,
                };
                app.set_page(page, window, cx);
                if let Some(name) = std::env::var("SENDIT_UI_SELECT")
                    .ok()
                    .and_then(|n| n.parse().ok())
                {
                    app.select(Selection::Vm(name), cx);
                }
                // An action, as its button would do it.
                if let Ok(action) = std::env::var("SENDIT_UI_ACTION") {
                    let (verb, arg) = action.split_once(':').unwrap_or((&action, ""));
                    match verb {
                        "build" => app.run_op(
                            format!("Build the {arg} image"),
                            vec!["provision".into(), arg.into()],
                            crate::ops::FollowUp::None,
                            Some("provision".into()),
                            window,
                            cx,
                        ),
                        "start-dir" => {
                            let dir = std::path::PathBuf::from(arg);
                            app.select(Selection::New(dir.clone()), cx);
                            app.start_vm(
                                format!("Start {arg}"),
                                &crate::runopts::Target::Dir(dir),
                                &crate::runopts::RunOptions {
                                    mounts: std::env::var("SENDIT_UI_MOUNTS").unwrap_or_default(),
                                    ..Default::default()
                                },
                                window,
                                cx,
                            );
                        }
                        "stop" => app.run_op(
                            format!("Stop {arg}"),
                            vec!["-n".into(), arg.into(), "stop".into()],
                            crate::ops::FollowUp::None,
                            Some(format!("vm:{arg}")),
                            window,
                            cx,
                        ),
                        _ => eprintln!("unknown action {action}"),
                    }
                }
                app.panel = match std::env::var("SENDIT_UI_PANEL").as_deref() {
                    Ok("start") => Panel::StartOptions,
                    Ok("worktree") => Panel::Worktree,
                    Ok("command") => Panel::RunCommand,
                    Ok("rename") => Panel::Rename,
                    _ => Panel::None,
                };
            })
        });
        let wait = std::env::var("SENDIT_UI_WAIT")
            .ok()
            .and_then(|secs| secs.parse().ok())
            .unwrap_or(3);
        cx.background_executor()
            .timer(Duration::from_secs(wait))
            .await;
        // Drawn here: a window that isn't on screen may not draw itself.
        let _ = cx.update_window(window, |_, window, cx| {
            window.draw(cx).clear(cx);
            match window.render_to_image() {
                Ok(image) => {
                    if let Err(e) = image.save(&file) {
                        eprintln!("saving the screenshot: {e}");
                    }
                }
                Err(e) => eprintln!("rendering the screenshot: {e:#}"),
            }
        });
        cx.update(|cx| cx.quit());
    })
    .detach();
}
