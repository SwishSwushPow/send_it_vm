//! The images page: the base images, building and deleting them, and their
//! image files.

use std::fs;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::Input;
use gpui_kit::component::scroll::ScrollableElement;
use gpui_kit::component::{ActiveTheme, Disableable, IconName, Sizable, StyledExt, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use send_it_vm::BaseState;
use send_it_vm::ByteSize;
use send_it_vm::ImageName;
use send_it_vm::ImageRow;

use crate::app::SenditApp;
use crate::ops::FollowUp;
use crate::ui::{self, card, field, header, section};

/// What a new image file starts with.
const IMAGE_FILE_TEMPLATE: &str = "\
# Runs at the end of provisioning this image, as the VM's user with sudo.
# Call shared scripts from ~/.config/sendit/provision-scripts/ with
# sendit-provision, e.g.:
#
# sendit-provision rust.sh
# sudo apt-get install -y postgresql-client
";

/// The key of image builds: sendit builds one at a time.
const PROVISION_KEY: &str = "provision";

impl SenditApp {
    pub fn render_images(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let building = self.busy(PROVISION_KEY);
        let actions = h_flex()
            .gap_2()
            .child(
                Button::new("provision-all")
                    .outline()
                    .small()
                    .icon(IconName::HardDrive)
                    .label("Build all")
                    .tooltip("Build every image: default, those in the config file or with an image file, and those built before. Existing ones only with \"Rebuild\".")
                    .disabled(building)
                    .on_click(cx.listener(|this, _, window, cx| {
                        let mut args = vec!["provision".to_string(), "--all".to_string()];
                        if let Some(extra) = this.provision_args(window, cx) {
                            args.extend(extra);
                            this.run_op("Build all images", args, FollowUp::None, Some(PROVISION_KEY.into()), window, cx);
                        }
                    })),
            )
            .when_some(self.paths.as_ref().ok().map(|p| p.provision_scripts_dir()), |this, dir| {
                this.child(
                    Button::new("scripts-dir")
                        .ghost()
                        .small()
                        .icon(IconName::FolderOpen)
                        .label("Provisioning scripts")
                        .on_click(move |_, _, cx| {
                            let _ = fs::create_dir_all(&dir);
                            cx.reveal_path(&dir)
                        }),
                )
            });
        let rows: Vec<AnyElement> = self
            .snapshot
            .images
            .clone()
            .into_iter()
            .map(|row| self.render_image(row, building, cx).into_any_element())
            .collect();
        v_flex()
            .id("images-page")
            .size_full()
            .p_6()
            .gap_4()
            .overflow_y_scrollbar()
            .child(header(
                "Base images",
                Some("Each project's VM starts as a copy of one; rebuilding one doesn't change existing VMs.".into()),
                actions,
                cx,
            ))
            .when_some(self.snapshot.images_error.clone(), |this, error| {
                this.child(ui::banner(error, true, cx))
            })
            .child(
                section("Build settings", cx).child(
                    card(cx)
                        .child(
                            h_flex()
                                .gap_3()
                                .items_end()
                                .child(div().w(px(160.)).child(field("CPUs", None, Input::new(&self.provision_cpus), cx)))
                                .child(div().w(px(160.)).child(field("Memory", None, Input::new(&self.provision_memory), cx)))
                                .child(
                                    Checkbox::new("force")
                                        .label("Rebuild images that exist (--force)")
                                        .checked(self.provision_force)
                                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                            this.provision_force = *checked;
                                            cx.notify();
                                        })),
                                ),
                        )
                        .child(div().text_xs().text_color(cx.theme().muted_foreground).child(
                            "Empty fields use the config file's CPUs and memory for the image. Builds \
                             download Debian once and take a few minutes; their output is in Activity.",
                        )),
                ),
            )
            .child(
                section("Images", cx).child(
                    v_flex()
                        .gap_2()
                        .when(rows.is_empty() && self.loaded, |this| {
                            this.child(div().text_sm().text_color(cx.theme().muted_foreground).child("No images."))
                        })
                        .children(rows),
                ),
            )
            .child(
                section("New image", cx).child(
                    card(cx)
                        .child(div().text_sm().text_color(cx.theme().muted_foreground).child(
                            "An image is named by its image file, ~/.config/sendit/images/NAME.sh, which \
                             runs at the end of its build. Pick it for a project with `image` in the \
                             configuration, or when starting a new VM.",
                        ))
                        .child(
                            h_flex()
                                .gap_2()
                                .child(div().w(px(240.)).child(Input::new(&self.new_image)))
                                .child(
                                    Button::new("new-image")
                                        .primary()
                                        .small()
                                        .icon(IconName::Plus)
                                        .label("Create image file")
                                        .on_click(cx.listener(|this, _, window, cx| this.create_image_file(window, cx))),
                                ),
                        ),
                ),
            )
    }

    fn render_image(
        &self,
        row: ImageRow,
        building: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let image = row.image.clone();
        let file = self
            .paths
            .as_ref()
            .ok()
            .map(|paths| paths.image_file(&image));
        let has_file = file.as_ref().is_some_and(|file| file.exists());
        let (build_label, force) = match row.state {
            BaseState::Missing | BaseState::Outdated => ("Build", false),
            BaseState::ScriptsChanged | BaseState::Current => ("Rebuild", true),
        };
        let id = image.to_string();
        h_flex()
            .gap_4()
            .p_3()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .child(
                v_flex()
                    .flex_1()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().font_medium().child(id.clone()))
                            .child(ui::base_state_tag(row.state)),
                    )
                    .child(
                        div().text_xs().text_color(cx.theme().muted_foreground).child(format!(
                            "{} · {} · {}",
                            row.disk.map_or("not built".to_string(), |disk: ByteSize| format!("{} on disk", disk.approx())),
                            match row.vms {
                                1 => "1 VM".to_string(),
                                n => format!("{n} VMs"),
                            },
                            if has_file { "has an image file" } else { "no image file: the plain base image" },
                        )),
                    ),
            )
            .child({
                let image = image.clone();
                Button::new(SharedString::from(format!("build-{id}")))
                    .small()
                    .when(!force, |b| b.primary())
                    .when(force, |b| b.outline())
                    .icon(IconName::HardDrive)
                    .label(build_label)
                    .loading(building)
                    .disabled(building)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let mut args = vec!["provision".to_string(), image.to_string()];
                        if force {
                            args.push("--force".into());
                        }
                        if let Some(extra) = this.provision_args(window, cx) {
                            // --force may be there already, from the button.
                            args.extend(
                                extra
                                    .into_iter()
                                    .filter(|arg| !(force && arg == "--force")),
                            );
                            this.run_op(
                                format!("Build the {image} image"),
                                args,
                                FollowUp::None,
                                Some(PROVISION_KEY.into()),
                                window,
                                cx,
                            );
                        }
                    }))
            })
            .when_some(file, |this, file| {
                this.child(
                    Button::new(SharedString::from(format!("edit-{id}")))
                        .ghost()
                        .small()
                        .icon(IconName::FileText)
                        .label(if has_file { "Edit image file" } else { "Add image file" })
                        .on_click(cx.listener(move |this, _, window, cx| {
                            if !file.exists()
                                && let Err(e) = write_new_image_file(&file)
                            {
                                this.notify_error(format!("{e:#}"), window, cx);
                                return;
                            }
                            this.open_in_editor(&file, window, cx);
                            this.refresh(cx);
                        })),
                )
            })
            .child({
                let image = image.clone();
                Button::new(SharedString::from(format!("delete-{id}")))
                    .ghost()
                    .small()
                    .icon(IconName::Delete)
                    .tooltip("Delete the built image; its VMs keep working")
                    .disabled(row.state == BaseState::Missing || building)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let image = image.clone();
                        this.confirm(
                            format!("Delete the {image} image?"),
                            format!(
                                "The VMs made from it keep working, but once one is reset, it can't be \
                                 made again until the {image} image is built again. Its image file stays."
                            ),
                            "Delete the image",
                            window,
                            cx,
                            move |this, window, cx| {
                                this.run_op(
                                    format!("Delete the {image} image"),
                                    vec!["images".into(), "--delete".into(), image.to_string(), "--yes".into()],
                                    FollowUp::None,
                                    Some(PROVISION_KEY.into()),
                                    window,
                                    cx,
                                )
                            },
                        )
                    }))
            })
    }

    /// `--cpus` and `--memory` for builds, as typed, or `None` after saying
    /// what is wrong with them.
    fn provision_args(&self, window: &mut Window, cx: &mut Context<Self>) -> Option<Vec<String>> {
        let mut args = Vec::new();
        if self.provision_force {
            args.push("--force".to_string());
        }
        let cpus = self.provision_cpus.read(cx).value().trim().to_string();
        if !cpus.is_empty() {
            match cpus.parse::<u32>() {
                Ok(n) if n > 0 => args.extend(["--cpus".to_string(), n.to_string()]),
                _ => {
                    self.notify_error(
                        format!("CPUs: {cpus:?} is not a number above 0"),
                        window,
                        cx,
                    );
                    return None;
                }
            }
        }
        let memory = self.provision_memory.read(cx).value().trim().to_string();
        if !memory.is_empty() {
            if let Err(e) = memory.parse::<ByteSize>() {
                self.notify_error(format!("Memory: {e:#}"), window, cx);
                return None;
            }
            args.extend(["--memory".to_string(), memory]);
        }
        Some(args)
    }

    fn create_image_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Ok(paths) = &self.paths else { return };
        let text = self.new_image.read(cx).value().trim().to_string();
        let image: ImageName = match text.parse() {
            Ok(image) => image,
            Err(e) => {
                self.notify_error(format!("{e:#}"), window, cx);
                return;
            }
        };
        let file = paths.image_file(&image);
        if file.exists() {
            self.notify_error(format!("{} exists already", file.display()), window, cx);
            return;
        }
        if let Err(e) = write_new_image_file(&file) {
            self.notify_error(format!("{e:#}"), window, cx);
            return;
        }
        self.new_image
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.open_in_editor(&file, window, cx);
        self.refresh(cx);
    }
}

fn write_new_image_file(file: &std::path::Path) -> anyhow::Result<()> {
    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(file)
        .and_then(|mut f| std::io::Write::write_all(&mut f, IMAGE_FILE_TEMPLATE.as_bytes()))
        .map_err(|e| anyhow::anyhow!("creating {}: {e}", file.display()))
}
