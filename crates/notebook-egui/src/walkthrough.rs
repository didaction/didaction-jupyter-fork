//! Fixed microscope navigation above a bounded, composable teaching stage.
use super::*;
use notebook_protocol::microscope::{
    Annotation, AnnotationColor, AnnotationTarget, StageBackground, StageBounds, Walkthrough,
    WalkthroughFocus, validate_focus,
};

impl NotebookEguiApp {
    pub(super) fn microscope_shortcuts(&mut self, ctx: &egui::Context) {
        if self.microscope_target.is_none()
            || !ctx.input(|input| input.focused)
            || self.state.snapshot.notebook.workspace == "temporary"
            || ctx.wants_keyboard_input()
            || egui::Popup::is_any_open(ctx)
        {
            return;
        }
        let key = ctx.input_mut(|input| {
            [
                Key::Backspace,
                Key::ArrowLeft,
                Key::ArrowRight,
                Key::ArrowUp,
                Key::ArrowDown,
            ]
            .into_iter()
            .find(|key| input.consume_key(egui::Modifiers::NONE, *key))
        });
        if key == Some(Key::Backspace) {
            let _ = self.open_microscope(None);
            return;
        }
        let Some(walkthrough) = self
            .microscope_document
            .as_ref()
            .and_then(|doc| doc.walkthrough.as_ref())
        else {
            return;
        };
        let focus = self
            .microscope_target
            .as_ref()
            .and_then(|target| target.focus.clone())
            .unwrap_or_default();
        if let Some(next) = key.and_then(|key| navigation_focus(walkthrough, &focus, key)) {
            let _ = self.focus_walkthrough(next);
        }
    }

    pub fn focus_walkthrough(&mut self, focus: WalkthroughFocus) -> Result<(), String> {
        let walkthrough = self
            .microscope_document
            .as_ref()
            .and_then(|doc| doc.walkthrough.as_ref())
            .ok_or("Open a microscope with a walkthrough first")?;
        validate_focus(walkthrough, &focus).map_err(|error| error.to_string())?;
        let target = self
            .microscope_target
            .as_mut()
            .ok_or("No microscope open")?;
        self.walkthrough_scroll_to_focus = target.focus.as_ref() != Some(&focus);
        target.focus = Some(focus);
        Ok(())
    }

    pub fn walkthrough_context(&self) -> serde_json::Value {
        let Some(walkthrough) = self
            .microscope_document
            .as_ref()
            .and_then(|doc| doc.walkthrough.as_ref())
        else {
            return serde_json::Value::Null;
        };
        let focus = self
            .microscope_target
            .as_ref()
            .and_then(|target| target.focus.clone())
            .unwrap_or_default();
        serde_json::json!({
            "title": walkthrough.title, "step_index": focus.step_index,
            "step_count": walkthrough.steps.len(), "step_id": walkthrough.steps[focus.step_index].id,
            "annotation_id": focus.annotation_id, "graphics_regions": self.graphics_status(),
        })
    }

    pub(super) fn walkthrough_ui(&mut self, ui: &mut egui::Ui, walkthrough: &Walkthrough) {
        let mut focus = self
            .microscope_target
            .as_ref()
            .and_then(|target| target.focus.clone())
            .unwrap_or_default();
        let mut index = focus.step_index;
        let step = &walkthrough.steps[index];
        ui.heading(&step.title);
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if toolbar_icon_button(
                ui,
                index > 0,
                ToolbarIcon::Left,
                "Previous step (Left arrow)",
            ) {
                index -= 1;
            }
            for (position, candidate) in walkthrough.steps.iter().enumerate() {
                if walkthrough_light(ui, position == index, position, &candidate.title) {
                    index = position;
                }
            }
            if toolbar_icon_button(
                ui,
                index + 1 < walkthrough.steps.len(),
                ToolbarIcon::Right,
                "Next step (Right arrow)",
            ) {
                index += 1;
            }
            ui.separator();
            let rendered = rendered_markdown_response(
                ui,
                &format!("walkthrough-description-{}", step.id),
                &step.description,
                &mut self.markdown_cache,
                &self.math_cache,
            );
            if let Some(url) = rendered.clicked_link {
                self.markdown_link = Some((
                    url,
                    ui.ctx()
                        .pointer_latest_pos()
                        .unwrap_or(rendered.response.rect.center()),
                ));
            }
            if focus.annotation_id.is_some() && ui.small_button("Clear focus").clicked() {
                focus.annotation_id = None;
                let _ = self.focus_walkthrough(focus.clone());
            }
        });
        if index != focus.step_index {
            focus = WalkthroughFocus {
                step_index: index,
                annotation_id: None,
            };
            let _ = self.focus_walkthrough(focus.clone());
        }
        ui.separator();
        let step = &walkthrough.steps[index];
        let target = self.microscope_target.as_ref().expect("mounted microscope");
        let scope = format!(
            "{}-{}-{}-{}",
            target.cell_id, target.microscope_id, target.revision, step.id
        );
        self.walkthrough_stage(ui, step, &scope, &focus);
    }

    fn walkthrough_stage(
        &mut self,
        ui: &mut egui::Ui,
        step: &notebook_protocol::microscope::WalkthroughStep,
        scope: &str,
        focus: &WalkthroughFocus,
    ) {
        let stage = ui.available_rect_before_wrap();
        ui.allocate_rect(stage, egui::Sense::hover());
        if let Some(frames) = self.microscope_capture_frames {
            if frames == 0 {
                let visible = stage.intersect(ui.clip_rect());
                self.capture_region =
                    Some((visible, ui.ctx().pixels_per_point(), visible != stage));
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::new(
                        "notebook-cell",
                    )));
                self.microscope_capture_frames = None;
            } else {
                self.microscope_capture_frames = Some(frames - 1);
                ui.ctx().request_repaint();
            }
        }
        ui.painter().rect_filled(
            stage,
            0.0,
            background_color(step.background.as_ref(), Color32::from_rgb(247, 249, 250)),
        );
        for region in &step.graphics_regions {
            let rect = stage_rect(stage, &region.bounds);
            // Regions are layers in one stage canvas, not visible cards. Paint
            // their optional base into the shared canvas, then alpha-composite
            // the rendered pixels directly over it.
            if region.background.is_some() {
                ui.painter().rect_filled(
                    rect,
                    0.0,
                    background_color(region.background.as_ref(), Color32::TRANSPARENT),
                );
            }
            self.graphics_region(ui, &region.id, rect);
            let response = ui.interact(
                rect,
                ui.id().with((&region.id, "graphics")),
                egui::Sense::hover(),
            );
            let error = self.graphics_error_for(&region.id).map(str::to_owned);
            response.on_hover_text(error.as_deref().unwrap_or(&region.description));
            if let Some(error) = error {
                ui.scope_builder(egui::UiBuilder::new().max_rect(rect.shrink(8.0)), |ui| {
                    ui.colored_label(Color32::from_rgb(198, 40, 40), error);
                    if ui.button("Retry graphics").clicked() {
                        self.retry_graphics();
                    }
                });
            }
        }
        let code_bounds = step.code_bounds.clone().unwrap_or(StageBounds {
            x: 20,
            y: 560,
            width: 620,
            height: 420,
        });
        self.walkthrough_code(ui, step, stage_rect(stage, &code_bounds), scope, focus);
        self.graphics_annotations(ui, step, stage, focus);
    }

    fn walkthrough_code(
        &mut self,
        ui: &mut egui::Ui,
        step: &notebook_protocol::microscope::WalkthroughStep,
        rect: egui::Rect,
        scope: &str,
        focus: &WalkthroughFocus,
    ) {
        ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
            ui.set_clip_rect(rect);
            ui.set_min_size(rect.size());
            ui.set_max_size(rect.size());
            egui::Frame::new()
                .fill(Color32::from_white_alpha(242))
                .stroke(Stroke::new(1.0, Color32::from_gray(205)))
                .inner_margin(Margin::same(8))
                .show(ui, |ui| {
                    let viewport_height = (rect.height() * 0.55).clamp(60.0, 280.0);
                    ui.set_max_width((rect.width() - 18.0).max(40.0));
                    ui.style_mut().spacing.scroll.floating = false;
                    ui.style_mut().spacing.scroll.bar_width = 10.0;
                    ui.horizontal(|ui| {
                        if step.playground_code.is_some()
                            && toolbar_icon_button(
                                ui,
                                !self.read_only,
                                ToolbarIcon::Run,
                                "Open playground window",
                            )
                        {
                            self.playground_requested = Some(focus.step_index);
                        }
                        ui.label(RichText::new("Code | read-only").strong());
                        if !step.graphics_regions.is_empty()
                            && !self.reduced_motion
                            && ui
                                .small_button(if self.graphics_paused() {
                                    "Resume"
                                } else {
                                    "Pause"
                                })
                                .clicked()
                        {
                            self.toggle_graphics_pause();
                        }
                    });
                    egui::ScrollArea::both()
                        .id_salt((scope, "code"))
                        .auto_shrink([false, false])
                        .scroll_bar_visibility(
                            egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded,
                        )
                        .max_height(viewport_height)
                        .max_width((rect.width() - 18.0).max(40.0))
                        .show(ui, |ui| {
                            let font = egui::TextStyle::Monospace.resolve(ui.style());
                            let content_width = step
                                .code
                                .split('\n')
                                .enumerate()
                                .map(|(line, source)| {
                                    ui.painter()
                                        .layout_no_wrap(
                                            format!("{:>4}  {source}", line + 1),
                                            font.clone(),
                                            Color32::BLACK,
                                        )
                                        .size()
                                        .x
                                        + 16.0
                                })
                                .fold(0.0_f32, f32::max);
                            ui.set_min_width(content_width);
                            for (line, source) in step.code.split('\n').enumerate() {
                                let code_annotations: Vec<_> = step
                                    .annotations
                                    .iter()
                                    .filter(|annotation| code_contains(annotation, line + 1))
                                    .collect();
                                let focused = code_annotations.iter().find(|annotation| {
                                    focus.annotation_id.as_ref() == Some(&annotation.id)
                                });
                                let accent = focused
                                    .copied()
                                    .or_else(|| code_annotations.first().copied())
                                    .map(|annotation| color(annotation.color));
                                egui::Frame::new()
                                    .fill(accent.map_or(Color32::TRANSPARENT, |value| {
                                        value.gamma_multiply(if focused.is_some() {
                                            0.18
                                        } else {
                                            0.07
                                        })
                                    }))
                                    .stroke(Stroke::new(
                                        2.0,
                                        focused.map_or(Color32::TRANSPARENT, |annotation| {
                                            color(annotation.color)
                                        }),
                                    ))
                                    .inner_margin(Margin::symmetric(6, 3))
                                    .show(ui, |ui| {
                                        ui.add(
                                            egui::Label::new(
                                                RichText::new(format!("{:>4}  {source}", line + 1))
                                                    .monospace(),
                                            )
                                            .wrap_mode(egui::TextWrapMode::Extend),
                                        );
                                    });
                            }
                        });
                    if !step.annotations.is_empty() {
                        ui.separator();
                        for annotation in &step.annotations {
                            let selected = focus.annotation_id.as_ref() == Some(&annotation.id);
                            let response = walkthrough_annotation_button(
                                ui,
                                &annotation_location(annotation),
                                &annotation.text,
                                color(annotation.color),
                                selected,
                            );
                            if response.clicked() {
                                let _ = self.focus_walkthrough(WalkthroughFocus {
                                    step_index: focus.step_index,
                                    annotation_id: Some(annotation.id.clone()),
                                });
                            }
                        }
                    }
                });
        });
    }

    fn graphics_annotations(
        &mut self,
        ui: &mut egui::Ui,
        step: &notebook_protocol::microscope::WalkthroughStep,
        stage: egui::Rect,
        focus: &WalkthroughFocus,
    ) {
        for annotation in &step.annotations {
            let AnnotationTarget::GraphicsPoint { region_id, x, y } = &annotation.target else {
                continue;
            };
            let Some(region) = step
                .graphics_regions
                .iter()
                .find(|region| &region.id == region_id)
            else {
                continue;
            };
            let region_rect = stage_rect(stage, &region.bounds);
            let point = region_rect.min
                + egui::vec2(
                    region_rect.width() * f32::from(*x) / 1000.0,
                    region_rect.height() * f32::from(*y) / 1000.0,
                );
            let selected = focus.annotation_id.as_ref() == Some(&annotation.id);
            let marker = egui::Rect::from_center_size(
                point,
                egui::vec2(
                    if selected { 24.0 } else { 18.0 },
                    if selected { 24.0 } else { 18.0 },
                ),
            );
            let response = ui.interact(
                marker,
                ui.id().with((&annotation.id, "point")),
                egui::Sense::click(),
            );
            ui.painter().circle_filled(
                point,
                if selected { 8.0 } else { 6.0 },
                color(annotation.color),
            );
            ui.painter().circle_stroke(
                point,
                if selected { 11.0 } else { 8.0 },
                Stroke::new(2.0, Color32::WHITE),
            );
            let response = response.on_hover_text(&annotation.text);
            if response.clicked() {
                let _ = self.focus_walkthrough(WalkthroughFocus {
                    step_index: focus.step_index,
                    annotation_id: Some(annotation.id.clone()),
                });
            }
            if selected {
                egui::Area::new(ui.id().with((&annotation.id, "callout")))
                    .fixed_pos(point + egui::vec2(12.0, 12.0))
                    .order(egui::Order::Tooltip)
                    .show(ui.ctx(), |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            ui.set_max_width(280.0);
                            ui.label(&annotation.text);
                        });
                    });
            }
        }
    }
}

fn background_color(background: Option<&StageBackground>, fallback: Color32) -> Color32 {
    let Some(background) = background else {
        return fallback;
    };
    let parse = |range| u8::from_str_radix(&background.color[range], 16).unwrap_or(0);
    Color32::from_rgba_unmultiplied(parse(1..3), parse(3..5), parse(5..7), background.opacity)
}
fn stage_rect(stage: egui::Rect, bounds: &StageBounds) -> egui::Rect {
    let scale = |value: u16| f32::from(value) / 1000.0;
    egui::Rect::from_min_size(
        stage.min
            + egui::vec2(
                stage.width() * scale(bounds.x),
                stage.height() * scale(bounds.y),
            ),
        egui::vec2(
            stage.width() * scale(bounds.width),
            stage.height() * scale(bounds.height),
        ),
    )
}
fn code_range(annotation: &Annotation) -> Option<(usize, usize, Option<usize>, Option<usize>)> {
    match annotation.target {
        AnnotationTarget::CodeRange {
            start_line,
            end_line,
            start_column,
            end_column,
        } => Some((start_line, end_line, start_column, end_column)),
        AnnotationTarget::GraphicsPoint { .. } => None,
    }
}
fn code_contains(annotation: &Annotation, line: usize) -> bool {
    code_range(annotation).is_some_and(|(start, end, _, _)| start <= line && end >= line)
}
fn annotation_location(annotation: &Annotation) -> String {
    match &annotation.target {
        AnnotationTarget::CodeRange {
            start_line,
            end_line,
            start_column,
            end_column,
        } => match (start_column, end_column) {
            (Some(start), Some(end)) => format!("L{start_line}:{start}-{end}"),
            _ if start_line == end_line => format!("L{start_line}"),
            _ => format!("L{start_line}-{end_line}"),
        },
        AnnotationTarget::GraphicsPoint { region_id, .. } => format!("@{region_id}"),
    }
}
fn walkthrough_annotation_button(
    ui: &mut egui::Ui,
    location: &str,
    text: &str,
    accent: Color32,
    selected: bool,
) -> egui::Response {
    egui::Frame::new()
        .fill(if selected {
            accent.gamma_multiply(0.14)
        } else {
            Color32::TRANSPARENT
        })
        .stroke(Stroke::new(
            2.0,
            if selected {
                accent
            } else {
                Color32::from_rgb(215, 220, 223)
            },
        ))
        .inner_margin(Margin::symmetric(8, 6))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(location).monospace().strong());
                ui.label(text);
            });
        })
        .response
        .interact(egui::Sense::click())
}
fn walkthrough_light(ui: &mut egui::Ui, active: bool, position: usize, title: &str) -> bool {
    let (_, response) = ui.allocate_exact_size(egui::vec2(18.0, 28.0), egui::Sense::click());
    let response = response.on_hover_text(format!("{}: {title}", position + 1));
    let center = response.rect.center();
    let color = if active || response.hovered() {
        Color32::from_rgb(45, 105, 143)
    } else {
        Color32::from_rgb(215, 220, 223)
    };
    ui.painter()
        .circle_filled(center, if active { 6.0 } else { 4.0 }, color);
    if active {
        ui.painter().circle_stroke(
            center,
            8.0,
            Stroke::new(1.0, Color32::from_rgb(45, 105, 143)),
        );
    }
    response.clicked()
}
fn navigation_focus(
    walkthrough: &Walkthrough,
    focus: &WalkthroughFocus,
    key: Key,
) -> Option<WalkthroughFocus> {
    let step = walkthrough.steps.get(focus.step_index)?;
    match key {
        Key::ArrowLeft | Key::ArrowRight => {
            let index = if key == Key::ArrowLeft {
                focus.step_index.checked_sub(1)?
            } else {
                focus.step_index + 1
            };
            walkthrough.steps.get(index)?;
            Some(WalkthroughFocus {
                step_index: index,
                annotation_id: None,
            })
        }
        Key::ArrowUp | Key::ArrowDown if !step.annotations.is_empty() => {
            let count = step.annotations.len();
            let current = step
                .annotations
                .iter()
                .position(|annotation| Some(&annotation.id) == focus.annotation_id.as_ref());
            let index = match (key, current) {
                (Key::ArrowUp, Some(index)) => (index + count - 1) % count,
                (Key::ArrowUp, None) => count - 1,
                (_, Some(index)) => (index + 1) % count,
                _ => 0,
            };
            Some(WalkthroughFocus {
                step_index: focus.step_index,
                annotation_id: Some(step.annotations[index].id.clone()),
            })
        }
        _ => None,
    }
}
fn color(color: AnnotationColor) -> Color32 {
    match color {
        AnnotationColor::Blue => Color32::from_rgb(45, 105, 143),
        AnnotationColor::BlueLight => Color32::from_rgb(78, 137, 175),
        AnnotationColor::BlueDeep => Color32::from_rgb(30, 76, 110),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn arrows_cycle_all_annotation_targets() {
        let walkthrough: Walkthrough = serde_json::from_value(serde_json::json!({"title":"Walk","steps":[{
            "id":"one","title":"One","description":"Use $x$.","code":"x","annotations":[
                {"id":"a","text":"A","target":{"kind":"code_range","start_line":1,"end_line":1}},
                {"id":"b","text":"B","target":{"kind":"graphics_point","region_id":"g","x":500,"y":500}}
            ],"graphics_regions":[{"id":"g","bounds":{"x":0,"y":0,"width":500,"height":500},
                "language":"assemblyscript-rgba-1","source":"source","description":"Diagram"}]}]})).unwrap();
        let start = WalkthroughFocus::default();
        assert_eq!(
            navigation_focus(&walkthrough, &start, Key::ArrowDown)
                .unwrap()
                .annotation_id
                .as_deref(),
            Some("a")
        );
        assert_eq!(
            navigation_focus(
                &walkthrough,
                &WalkthroughFocus {
                    step_index: 0,
                    annotation_id: Some("a".into())
                },
                Key::ArrowDown
            )
            .unwrap()
            .annotation_id
            .as_deref(),
            Some("b")
        );
    }
    #[test]
    fn focusing_annotation_preserves_layout_size() {
        let measure = |selected| {
            let context = egui::Context::default();
            let mut size = egui::Vec2::ZERO;
            let _ = context.run(egui::RawInput::default(), |context| {
                egui::CentralPanel::default().show(context, |ui| {
                    ui.set_width(320.0);
                    size =
                        walkthrough_annotation_button(ui, "L3", "Explain", Color32::BLUE, selected)
                            .rect
                            .size();
                });
            });
            size
        };
        assert_eq!(measure(false), measure(true));
    }
}
