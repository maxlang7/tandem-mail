use std::collections::BTreeMap;

use eframe::egui::{
    self, Color32, Id, Pos2, Rect, Response, Sense, Stroke, Ui, WidgetInfo, WidgetType,
    collapsing_header::CollapsingState, vec2,
};

use super::{
    MailApp, Modal, Theme,
    theme::{Icon, icon},
    workspace::paint_line,
};
use crate::{model::Label, store::View};

#[derive(Default)]
struct LabelTree<'a> {
    label: Option<&'a Label>,
    children: BTreeMap<&'a str, LabelTree<'a>>,
}

impl<'a> LabelTree<'a> {
    fn insert(&mut self, label: &'a Label) {
        let name = label.name.as_str();
        if name.starts_with('/') || name.ends_with('/') || name.contains("//") {
            self.children.entry(name).or_default().label = Some(label);
            return;
        }

        let mut node = self;
        for part in name.split('/') {
            node = node.children.entry(part).or_default();
        }
        node.label = Some(label);
    }
}

impl MailApp {
    pub(super) fn label_sidebar(&mut self, ui: &mut Ui) {
        let t = self.theme;
        ui.add_space(7.);
        ui.separator();
        ui.add_space(3.);
        ui.label(t.muted("Labels").size(12.).strong());

        let mut selected = None;
        let mut create = None;
        for account in &self.accounts {
            let id = ui.make_persistent_id(("label-account", &account.id));
            let scoped = self.scope.as_ref() == Some(&account.id);
            let mut state = CollapsingState::load_with_default_open(ui.ctx(), id, scoped);
            let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 29.), Sense::hover());
            let add_rect = Rect::from_min_size(rect.right_top() - vec2(26., 0.), vec2(26., 29.));
            let header_rect = Rect::from_min_max(rect.left_top(), add_rect.left_bottom());
            let header = ui.interact(header_rect, id.with("header"), Sense::click());
            let add = ui.interact(add_rect, id.with("add"), Sense::click());

            if header.clicked() {
                state.set_open(!state.is_open());
            }
            if add.clicked() {
                state.set_open(true);
                create = Some(account.id.clone());
            }
            state.store(ui.ctx());

            row_background(ui, rect, &header, false, t);
            if add.hovered() || add.has_focus() {
                ui.painter().rect_filled(add_rect.shrink(2.), 5, t.raised);
            }
            disclosure(
                ui,
                rect.left_center() + vec2(10., 0.),
                state.is_open(),
                t.muted,
            );
            let (badge, color) = t.account(&account.id);
            let marker = rect.left_center() + vec2(25., 0.);
            ui.painter().circle_filled(marker, 6., badge);
            ui.painter().circle_filled(marker, 2.7, color);
            paint_line(
                ui,
                Rect::from_min_max(rect.left_top() + vec2(36., 0.), header_rect.right_bottom()),
                &account.name,
                13.,
                t.ink,
                true,
            );
            let plus = add_rect.center();
            for axis in [vec2(4., 0.), vec2(0., 4.)] {
                ui.painter()
                    .line_segment([plus - axis, plus + axis], Stroke::new(1.45, t.muted));
            }
            header.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, &account.name));
            header.on_hover_text(&account.email);
            let add_label = format!("Create label in {}", account.name);
            add.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, &add_label));
            add.on_hover_text(add_label);

            if state.is_open() {
                let mut root = LabelTree::default();
                for (_, label) in self
                    .labels
                    .iter()
                    .filter(|(id, label)| id == &account.id && label.kind == "user")
                {
                    root.insert(label);
                }

                if root.children.is_empty() {
                    ui.horizontal(|ui| {
                        ui.add_space(34.);
                        ui.label(t.muted("No labels").size(12.));
                    });
                }
                let active = match &self.view {
                    View::Tag(label) if scoped => Some(label.as_str()),
                    _ => None,
                };
                let mut list = LabelList {
                    theme: t,
                    color,
                    active,
                    selected: None,
                };
                list.draw(ui, id, &root, 0);
                if let Some(label) = list.selected {
                    selected = Some((account.id.clone(), label));
                }
            }
        }
        if let Some((account, label)) = selected {
            self.scope = Some(account);
            self.go(View::Tag(label));
        }
        if let Some(account) = create {
            self.set_modal(Modal::CreateLabel);
            self.modal_account = account;
        }
    }
}

struct LabelList<'a> {
    theme: Theme,
    color: Color32,
    active: Option<&'a str>,
    selected: Option<String>,
}

impl LabelList<'_> {
    fn draw(&mut self, ui: &mut Ui, parent: Id, tree: &LabelTree<'_>, depth: usize) {
        let mut children: Vec<_> = tree.children.iter().collect();
        children.sort_by_cached_key(|(name, _)| name.to_lowercase());
        for (&name, node) in children {
            let id = parent.with(name);
            let active = node
                .label
                .is_some_and(|label| self.active == Some(label.id.as_str()));
            let has_children = !node.children.is_empty();
            let t = self.theme;
            let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 25.), Sense::hover());
            let offset = 13. + 10. * depth.min(6) as f32;
            let caret = rect.left_center() + vec2(offset, 0.);
            let caret_rect = Rect::from_center_size(caret, vec2(19., rect.height()));
            let label_rect = Rect::from_min_max(caret_rect.right_top(), rect.right_bottom());
            let response = ui.interact(
                if node.label.is_some() {
                    label_rect
                } else {
                    rect
                },
                id.with("label"),
                Sense::click(),
            );
            row_background(ui, rect, &response, active, t);
            let mut state = CollapsingState::load_with_default_open(ui.ctx(), id, active);
            if has_children {
                let toggle = if node.label.is_some() {
                    ui.interact(caret_rect, id.with("toggle"), Sense::click())
                } else {
                    response.clone()
                };
                if toggle.clicked() {
                    state.set_open(!state.is_open());
                }
                state.store(ui.ctx());
                disclosure(
                    ui,
                    caret,
                    state.is_open(),
                    if toggle.hovered() || toggle.has_focus() {
                        t.ink
                    } else {
                        t.muted
                    },
                );
                toggle.widget_info(|| {
                    WidgetInfo::labeled(WidgetType::Button, true, format!("Toggle {name} labels"))
                });
            }
            let tag = caret + vec2(17., 0.);
            icon(
                ui.painter(),
                Icon::Tag,
                tag,
                if active { t.accent } else { self.color },
            );
            paint_line(
                ui,
                Rect::from_min_max(
                    egui::pos2(tag.x + 13., rect.top()),
                    rect.right_bottom() - vec2(4., 0.),
                ),
                name,
                13.,
                if active { t.accent } else { t.ink },
                active,
            );
            if let Some(label) = node.label {
                response.widget_info(|| {
                    WidgetInfo::selected(WidgetType::SelectableLabel, true, active, &label.name)
                });
                if response.on_hover_text(&label.name).clicked() {
                    self.selected = Some(label.id.clone());
                }
            }
            if has_children && state.is_open() {
                self.draw(ui, id, node, depth + 1);
            }
        }
    }
}

fn row_background(ui: &Ui, rect: Rect, response: &Response, active: bool, theme: Theme) {
    if active || response.hovered() || response.has_focus() {
        ui.painter()
            .rect_filled(rect, 5, if active { theme.selected } else { theme.raised });
    }
    if active {
        let marker = Rect::from_center_size(rect.left_center() + vec2(2., 0.), vec2(2., 14.));
        ui.painter().rect_filled(marker, 1, theme.accent);
    }
}

fn disclosure(ui: &Ui, center: Pos2, open: bool, color: Color32) {
    let points = if open {
        [vec2(-3.5, -1.5), vec2(0., 2.), vec2(3.5, -1.5)]
    } else {
        [vec2(-1.5, -3.5), vec2(2., 0.), vec2(-1.5, 3.5)]
    };
    ui.painter().add(egui::Shape::line(
        points.into_iter().map(|point| center + point).collect(),
        Stroke::new(1.35, color),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{model, store::Store, ui::seed_demo};

    fn text_rect(frame: &egui::FullOutput, label: &str) -> Option<Rect> {
        frame.shapes.iter().find_map(|shape| match &shape.shape {
            egui::Shape::Text(text) if text.pos.x < 194. && text.galley.text() == label => {
                Some(text.visual_bounding_rect())
            }
            _ => None,
        })
    }

    #[test]
    fn account_dropdowns_expand_and_select_independently() -> anyhow::Result<()> {
        let path = std::path::PathBuf::from(":memory:");
        let accounts = model::accounts(std::path::Path::new("/tmp"), &path)?;
        let mut store = Store::open(&path)?;
        seed_demo(&mut store, &accounts)?;
        let ctx = egui::Context::default();
        let mut app = MailApp::new(
            &ctx,
            store,
            path,
            accounts,
            tokio::runtime::Runtime::new()?,
            true,
        );
        let run = |app: &mut MailApp, events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1320., 840.))),
                    events,
                    ..Default::default()
                },
                |ctx| app.frame(ctx),
            )
        };
        let click = |app: &mut MailApp, pos| {
            let button = |pressed| egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            let _ = run(app, vec![egui::Event::PointerMoved(pos), button(true)]);
            run(app, vec![button(false)])
        };

        let frame = run(&mut app, vec![]);
        assert!(text_rect(&frame, "Tutoring").is_none());
        assert!(text_rect(&frame, "Research").is_none());
        let frame = click(&mut app, text_rect(&frame, "Personal").unwrap().center());
        let tutoring = text_rect(&frame, "Tutoring").unwrap();
        assert!(text_rect(&frame, "Research").is_none());
        assert!(text_rect(&frame, "SAT").is_none());
        let frame = click(
            &mut app,
            egui::pos2(tutoring.left() - 30., tutoring.center().y),
        );
        assert!(text_rect(&frame, "SAT").is_some());

        let frame = click(&mut app, text_rect(&frame, "CMU").unwrap().center());
        assert!(text_rect(&frame, "SAT").is_some());
        let frame = click(&mut app, text_rect(&frame, "Research").unwrap().center());
        assert_eq!(app.scope.as_deref(), Some("cmu"));
        assert_eq!(app.view, View::Tag("Label_1".into()));

        let frame = click(&mut app, text_rect(&frame, "Tutoring").unwrap().center());
        assert_eq!(app.scope.as_deref(), Some("home"));
        assert_eq!(app.view, View::Tag("Label_1".into()));
        let cmu = text_rect(&frame, "CMU").unwrap().center();
        let _ = click(&mut app, egui::pos2(166., cmu.y));
        assert!(app.modal == Some(Modal::CreateLabel));
        assert_eq!(app.modal_account, "cmu");
        Ok(())
    }
}
