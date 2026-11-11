//! The configuration page: a form over sendit's config file.

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, Textarea};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::scroll::ScrollableElement;
use gpui_kit::component::{
    ActiveTheme, Disableable, IconName, Sizable, StyledExt, WindowExt, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use crate::app::SenditApp;
use crate::config_edit::{ConfigFile, Fields, Section};
use crate::ui::{self, card, choice, field, header};

impl SenditApp {
    /// Reads the config file again, dropping unsaved changes.
    pub fn load_config(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.config_path() else {
            return;
        };
        let config = ConfigFile::load(&path).map_err(|e| format!("{e:#}"));
        self.config_saved = config
            .as_ref()
            .map_or_else(|_| String::new(), ConfigFile::text);
        self.config = Some(config);
        self.config_error = None;
        let section = self.config_section.clone();
        let exists =
            matches!(&self.config, Some(Ok(config)) if config.sections().contains(&section));
        self.show_section(if exists { section } else { Section::Global }, window, cx);
    }

    /// Whether the config has changes that aren't saved, the form's
    /// included.
    pub fn config_dirty(&self, cx: &App) -> bool {
        let Some(Ok(config)) = &self.config else {
            return false;
        };
        let mut copy = match ConfigFile::parse(&config.path, &config.text()) {
            Ok(copy) => copy,
            Err(_) => return true,
        };
        copy.set_fields(&self.config_section, &self.form_fields(cx))
            .is_err()
            || copy.text() != self.config_saved
    }

    fn form_fields(&self, cx: &App) -> Fields {
        let form = &self.config_form;
        let text = |state: &Entity<gpui_kit::component::input::InputState>| {
            state.read(cx).value().to_string()
        };
        let area = |state: &Entity<gpui_kit::component::input::TextareaState>| {
            state.read(cx).value().to_string()
        };
        Fields {
            cpus: text(&form.cpus),
            memory: text(&form.memory),
            disk_size: text(&form.disk_size),
            image: text(&form.image),
            network: form.network.clone(),
            network_allow: area(&form.network_allow),
            mounts: area(&form.mounts),
            read_only: form.read_only,
            expose_git: form.expose_git,
            banner: form.banner,
        }
    }

    /// Takes the form's values into the config in memory.
    fn apply_form(&mut self, cx: &mut Context<Self>) -> bool {
        let fields = self.form_fields(cx);
        let section = self.config_section.clone();
        let Some(Ok(config)) = &mut self.config else {
            return false;
        };
        match config.set_fields(&section, &fields) {
            Ok(()) => true,
            Err(e) => {
                self.config_error = Some(format!("{e:#}"));
                cx.notify();
                false
            }
        }
    }

    fn show_section(&mut self, section: Section, window: &mut Window, cx: &mut Context<Self>) {
        let fields = match &self.config {
            Some(Ok(config)) => config.fields(&section),
            _ => Fields::default(),
        };
        let form = &mut self.config_form;
        for (state, value) in [
            (&form.cpus, fields.cpus),
            (&form.memory, fields.memory),
            (&form.disk_size, fields.disk_size),
            (&form.image, fields.image),
        ] {
            state.update(cx, |state, cx| state.set_value(value, window, cx));
        }
        for (state, value) in [
            (&form.network_allow, fields.network_allow),
            (&form.mounts, fields.mounts),
        ] {
            state.update(cx, |state, cx| state.set_value(value, window, cx));
        }
        form.network = fields.network;
        form.read_only = fields.read_only;
        form.expose_git = fields.expose_git;
        form.banner = fields.banner;
        self.config_section = section;
        cx.notify();
    }

    fn switch_section(&mut self, section: Section, window: &mut Window, cx: &mut Context<Self>) {
        if self.apply_form(cx) {
            self.config_error = None;
            self.show_section(section, window, cx);
        }
    }

    fn save_config(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.apply_form(cx) {
            return;
        }
        let Some(Ok(config)) = &mut self.config else {
            return;
        };
        match config.save() {
            Ok(()) => {
                self.config_saved = config.text();
                self.config_error = None;
                window.push_notification(
                    Notification::success(
                        "Saved. Running VMs take the changes when they next start.",
                    ),
                    cx,
                );
                self.refresh(cx);
            }
            Err(e) => self.config_error = Some(format!("{e:#}")),
        }
        cx.notify();
    }

    fn add_image_section(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.new_image_section.read(cx).value().trim().to_string();
        let section = Section::Image(name);
        if !self.apply_form(cx) {
            return;
        }
        let Some(Ok(config)) = &mut self.config else {
            return;
        };
        match config.add_section(&section) {
            Ok(()) => {
                self.new_image_section
                    .update(cx, |state, cx| state.set_value("", window, cx));
                self.config_error = None;
                self.show_section(section, window, cx);
            }
            Err(e) => {
                self.config_error = Some(format!("{e:#}"));
                cx.notify();
            }
        }
    }

    fn add_project_section(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose project".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(dir) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update_in(cx, |this, window, cx| {
                let Ok(sendit_paths) = &this.paths else {
                    return;
                };
                // As people write them, e.g. ~/Code/app.
                let dir = send_it_vm::canonical(&dir);
                // Also when written another way, e.g. without ~: sendit
                // refuses two tables for one project.
                let existing = match &this.config {
                    Some(Ok(config)) => {
                        config
                            .sections()
                            .into_iter()
                            .find_map(|section| match section {
                                Section::Project(key) => {
                                    let path =
                                        sendit_paths.expand_tilde(std::path::Path::new(&key));
                                    (send_it_vm::canonical(&path) == dir).then_some(key)
                                }
                                _ => None,
                            })
                    }
                    _ => None,
                };
                let key = sendit_paths.display(&dir);
                if let Some(existing) = existing {
                    if this.apply_form(cx) {
                        this.show_section(Section::Project(existing.clone()), window, cx);
                        this.config_error =
                            Some(format!("[projects.\"{existing}\"] configures it already"));
                    }
                    return;
                }
                let section = Section::Project(key);
                if !this.apply_form(cx) {
                    return;
                }
                let Some(Ok(config)) = &mut this.config else {
                    return;
                };
                match config.add_section(&section) {
                    Ok(()) => {
                        this.config_error = None;
                        this.show_section(section, window, cx);
                    }
                    Err(e) => {
                        this.config_error = Some(format!("{e:#}"));
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    pub fn render_config(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let dirty = self.config_dirty(cx);
        let path = self.config_path();
        let actions = h_flex()
            .gap_2()
            .child(
                Button::new("config-save")
                    .primary()
                    .small()
                    .label("Save")
                    .disabled(!dirty)
                    .on_click(cx.listener(|this, _, window, cx| this.save_config(window, cx))),
            )
            .child(
                Button::new("config-revert")
                    .outline()
                    .small()
                    .label("Reload")
                    .tooltip("Read the file again, without the changes made here")
                    .on_click(cx.listener(|this, _, window, cx| this.load_config(window, cx))),
            )
            .when_some(path.clone(), |this, path| {
                this.child(
                    Button::new("config-editor")
                        .ghost()
                        .small()
                        .icon(IconName::FileText)
                        .label("Open in editor")
                        .tooltip(
                            "Edit config.toml itself; read it here again afterwards with Reload",
                        )
                        .on_click(cx.listener(move |this, _, window, cx| {
                            if let Some(dir) = path.parent() {
                                let _ = std::fs::create_dir_all(dir);
                            }
                            if !path.exists() {
                                let _ = std::fs::write(&path, "");
                            }
                            this.open_in_editor(&path, window, cx)
                        })),
                )
            });
        let subtitle = path.map(|path| {
            let shown = self
                .paths
                .as_ref()
                .map_or_else(|_| path.display().to_string(), |p| p.display(&path));
            format!("{shown}{}", if dirty { " — unsaved changes" } else { "" })
        });
        let body = match &self.config {
            None => div().child("Loading…").into_any_element(),
            Some(Err(e)) => v_flex()
                .gap_2()
                .child(ui::banner(
                    format!("The config file can't be read: {e}. Fix it in an editor, then choose Reload."),
                    true,
                    cx,
                ))
                .into_any_element(),
            Some(Ok(config)) => {
                let sections = config.sections();
                h_flex()
                    .items_start()
                    .gap_4()
                    .child(self.render_section_list(sections, cx))
                    .child(self.render_section_form(cx))
                    .into_any_element()
            }
        };
        v_flex()
            .id("config-page")
            .size_full()
            .p_6()
            .gap_4()
            .overflow_y_scrollbar()
            .child(header("Configuration", subtitle, actions, cx))
            .when_some(self.config_error.clone(), |this, error| {
                this.child(ui::banner(error, true, cx))
            })
            .child(body)
    }

    fn render_section_list(
        &self,
        sections: Vec<Section>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        v_flex()
            .w(px(240.))
            .flex_shrink_0()
            .gap_1()
            .children(sections.into_iter().enumerate().map(|(ix, section)| {
                let selected = section == self.config_section;
                let kind = match &section {
                    Section::Global => "defaults",
                    Section::Image(_) => "image",
                    Section::Project(_) => "project",
                };
                let title = section.title();
                h_flex()
                    .id(("config-section", ix))
                    .gap_2()
                    .px_3()
                    .py_1p5()
                    .rounded(cx.theme().radius)
                    .text_sm()
                    .cursor_pointer()
                    .when(selected, |this| this.bg(cx.theme().accent).font_medium())
                    .hover(|this| this.bg(cx.theme().accent))
                    .child(div().flex_1().min_w_0().truncate().child(title))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(kind),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.switch_section(section.clone(), window, cx)
                    }))
            }))
            .child(
                v_flex()
                    .pt_3()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_1()
                            .child(div().flex_1().child(Input::new(&self.new_image_section)))
                            .child(
                                Button::new("add-image-section")
                                    .outline()
                                    .small()
                                    .icon(IconName::Plus)
                                    .tooltip("Add CPUs and memory for an image and its VMs")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.add_image_section(window, cx)
                                    })),
                            ),
                    )
                    .child(
                        Button::new("add-project-section")
                            .outline()
                            .small()
                            .icon(IconName::Plus)
                            .label("Add project…")
                            .tooltip("Add settings for one project folder")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.add_project_section(window, cx)
                            })),
                    ),
            )
    }

    fn render_section_form(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let section = self.config_section.clone();
        let form = &self.config_form;
        let image_only = section.is_image();
        let description = match &section {
            Section::Global => {
                "Settings for every project. Projects and images override them; mounts and network-allow entries from all of them are combined."
            }
            Section::Image(_) => {
                "CPUs and memory for building this image and for the VMs made from it, over the settings for all projects."
            }
            Section::Project(_) => {
                "Settings for this project folder and its git worktrees, over all others but a run's own options."
            }
        };
        let network = form.network.clone();
        card(cx)
            .flex_1()
            .min_w_0()
            .child(
                h_flex()
                    .justify_between()
                    .child(div().font_medium().child(section.title()))
                    .when(section != Section::Global, |this| {
                        let section = section.clone();
                        this.child(
                            Button::new("remove-section")
                                .ghost()
                                .small()
                                .icon(IconName::Delete)
                                .label("Remove")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    if let Some(Ok(config)) = &mut this.config {
                                        config.remove_section(&section);
                                    }
                                    this.show_section(Section::Global, window, cx);
                                })),
                        )
                    }),
            )
            .child(div().text_sm().text_color(cx.theme().muted_foreground).child(description))
            .child(
                h_flex()
                    .items_start()
                    .gap_3()
                    .child(div().flex_1().child(field("CPUs", None, Input::new(&form.cpus), cx)))
                    .child(div().flex_1().child(field("Memory", Some("e.g. 8G or 512M"), Input::new(&form.memory), cx)))
                    .when(!image_only, |this| {
                        this.child(div().flex_1().child(field(
                            "Disk size",
                            Some("The disk file is sparse and only grows"),
                            Input::new(&form.disk_size),
                            cx,
                        )))
                    }),
            )
            .when(!image_only, |this| {
                this.child(field(
                    "Image",
                    Some("The base image new VMs are made from"),
                    Input::new(&form.image),
                    cx,
                ))
                .child(field(
                    "Network",
                    Some("Internet only: not the local network, the Mac or other VMs. None: nothing. Open: everything."),
                    h_flex().gap_1().children(
                        [("", "Inherit"), ("internet", "Internet only"), ("none", "None"), ("open", "Open")]
                            .into_iter()
                            .map(|(value, label)| {
                                choice(SharedString::from(format!("cfg-net-{label}")), label, network == value).on_click(
                                    cx.listener(move |this, _, _, cx| {
                                        this.config_form.network = value.to_string();
                                        cx.notify();
                                    }),
                                )
                            }),
                    ),
                    cx,
                ))
                .child(field(
                    "Allow connections to",
                    Some("host[:PORT] for the Mac, ADDR[/LEN][:PORT], IPV6[/LEN] or [IPV6[/LEN]]:PORT. One per line."),
                    Textarea::new(&form.network_allow),
                    cx,
                ))
                .child(field(
                    "Shares",
                    Some("HOST[:GUEST][:ro|rw], host paths absolute or starting with ~, read-only unless :rw. One per line."),
                    Textarea::new(&form.mounts),
                    cx,
                ))
                .child(self.tri_field("read-only", "Share the project read-only", "Builds that write into it must write elsewhere.", form.read_only, |this, v| this.config_form.read_only = v, cx))
                .child(self.tri_field("expose-git", "Show .git to the VM", "Lets the VM change hooks and config that git on the Mac runs.", form.expose_git, |this, v| this.config_form.expose_git = v, cx))
                .child(self.tri_field("banner", "Login banner", "The \"Send It\" banner in the login message.", form.banner, |this, v| this.config_form.banner = v, cx))
            })
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("Save checks the whole file the way sendit reads it before writing. Comments and layout stay as they are."),
            )
    }

    /// A setting that is inherited, on or off.
    fn tri_field(
        &self,
        id: &'static str,
        label: &'static str,
        hint: &'static str,
        value: Option<bool>,
        set: fn(&mut SenditApp, Option<bool>),
        cx: &mut Context<Self>,
    ) -> Div {
        let buttons: Vec<_> = [(None, "Inherit"), (Some(true), "Yes"), (Some(false), "No")]
            .into_iter()
            .map(|(option, text)| {
                choice(
                    SharedString::from(format!("{id}-{text}")),
                    text,
                    value == option,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    set(this, option);
                    cx.notify();
                }))
            })
            .collect();
        field(label, Some(hint), h_flex().gap_1().children(buttons), cx)
    }
}
