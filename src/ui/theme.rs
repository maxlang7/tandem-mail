use eframe::egui::{
    self, Color32, FontId, Frame, Margin, RichText, Stroke, TextStyle, Ui, Visuals,
};

#[derive(Clone, Copy)]
pub struct Theme {
    pub dark: bool,
    pub canvas: Color32,
    pub surface: Color32,
    pub raised: Color32,
    pub line: Color32,
    pub ink: Color32,
    pub muted: Color32,
    pub accent: Color32,
    pub selected: Color32,
    pub read: Color32,
    pub read_ink: Color32,
    pub read_preview: Color32,
    pub unread: Color32,
    pub unread_ink: Color32,
    pub error: Color32,
    pub star: Color32,
    pub primary_fill: Color32,
    pub on_primary: Color32,
}

impl Theme {
    pub fn new(dark: bool) -> Self {
        let c = |hex: u32| Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8);
        let on_primary = c(0x282828);
        if dark {
            Self {
                dark,
                canvas: c(0x1d2021),
                surface: c(0x282828),
                raised: c(0x3c3836),
                line: c(0x3d3835),
                ink: c(0xebdbb2),
                muted: c(0xbdae93),
                accent: c(0xa9c765),
                selected: c(0x3b482d),
                read: c(0x201e1d),
                read_ink: c(0xbcb8ae),
                read_preview: c(0xb5b1a7),
                unread: c(0x3c3836),
                unread_ink: c(0xfbf1c7),
                error: c(0xfb826d),
                star: c(0xfabd2f),
                primary_fill: c(0xa9c765),
                on_primary,
            }
        } else {
            Self {
                dark,
                canvas: c(0xf9f5d7),
                surface: c(0xfbf1c7),
                raised: c(0xeddfb7),
                line: c(0xdfd0a8),
                ink: c(0x282828),
                muted: c(0x665c54),
                accent: c(0x415b24),
                selected: c(0xe4ebcd),
                read: c(0xeee5cd),
                read_ink: c(0x59564e),
                read_preview: c(0x6b6257),
                unread: c(0xfffae6),
                unread_ink: c(0x242019),
                error: c(0x9d2221),
                star: c(0x8f5b05),
                primary_fill: c(0xa7bc6b),
                on_primary,
            }
        }
    }

    pub fn mail_ink(self, unread: bool) -> Color32 {
        if unread {
            self.unread_ink
        } else {
            self.read_ink
        }
    }

    pub fn apply(self, ctx: &egui::Context) {
        let mut visuals = if self.dark {
            Visuals::dark()
        } else {
            Visuals::light()
        };
        visuals.panel_fill = self.canvas;
        visuals.window_fill = self.surface;
        visuals.window_stroke = Stroke::new(1., self.line);
        visuals.extreme_bg_color = self.canvas;
        visuals.faint_bg_color = if self.dark {
            Color32::from_rgb(50, 48, 47)
        } else {
            Color32::from_rgb(255, 247, 218)
        };
        visuals.code_bg_color = self.raised;
        visuals.override_text_color = Some(self.ink);
        visuals.weak_text_color = Some(self.muted);
        visuals.selection.bg_fill = self.selected;
        visuals.selection.stroke = Stroke::new(1., self.accent);
        visuals.hyperlink_color = self.accent;
        visuals.error_fg_color = self.error;
        for (widget, fill) in [
            (&mut visuals.widgets.noninteractive, self.surface),
            (&mut visuals.widgets.inactive, self.raised),
            (&mut visuals.widgets.hovered, self.selected),
            (&mut visuals.widgets.active, self.selected),
            (&mut visuals.widgets.open, self.raised),
        ] {
            widget.bg_fill = fill;
            widget.weak_bg_fill = fill;
            widget.bg_stroke = Stroke::new(1., self.line);
            widget.fg_stroke.color = self.ink;
            widget.corner_radius = egui::CornerRadius::same(6);
        }
        ctx.set_visuals(visuals);
        ctx.style_mut(|style| {
            style
                .text_styles
                .insert(TextStyle::Body, FontId::proportional(14.));
            style
                .text_styles
                .insert(TextStyle::Button, FontId::proportional(14.));
            style
                .text_styles
                .insert(TextStyle::Small, FontId::proportional(12.));
            style.spacing.item_spacing = egui::vec2(7., 7.);
            style.spacing.button_padding = egui::vec2(9., 5.);
            style.interaction.selectable_labels = false;
        });
    }

    pub fn card(self, padding: i8) -> Frame {
        Frame::new()
            .fill(self.surface)
            .stroke(Stroke::new(1., self.line))
            .corner_radius(10)
            .inner_margin(Margin::same(padding))
    }

    pub fn muted(self, text: impl Into<String>) -> RichText {
        RichText::new(text).color(self.muted)
    }

    pub fn primary(self, ui: &mut Ui, text: &str) -> egui::Response {
        ui.add(
            egui::Button::new(RichText::new(text).strong().color(self.on_primary))
                .fill(self.primary_fill)
                .corner_radius(8)
                .stroke(Stroke::NONE)
                .min_size(egui::vec2(72., 34.)),
        )
    }

    pub fn account(self, id: &str) -> (Color32, Color32) {
        let c = Color32::from_rgb;
        match (self.dark, id) {
            (false, "cmu") => (c(243, 219, 188), c(145, 38, 35)),
            (true, "cmu") => (c(73, 52, 46), c(255, 168, 138)),
            (false, _) => (c(226, 231, 193), c(46, 86, 62)),
            (true, _) => (c(52, 62, 51), c(157, 206, 143)),
        }
    }
}

#[derive(Clone, Copy)]
pub enum Icon {
    Mail,
    Search,
    Inbox,
    Star,
    Pencil,
    Send,
    Clock,
    All,
    Trash,
    Tag,
    Tune,
    Archive,
    Read,
    Clip,
    Settings,
    Back,
    Next,
}

pub fn icon(p: &egui::Painter, kind: Icon, center: egui::Pos2, color: Color32) {
    use egui::{Rect, vec2};
    let s = Stroke::new(1.45, color);
    let point = |x: f32, y: f32| center + vec2(x, y);
    let path = |coords: &[(f32, f32)]| {
        p.add(egui::Shape::line(
            coords.iter().map(|&(x, y)| point(x, y)).collect(),
            s,
        ))
    };
    let outline = |w, h, radius| {
        p.rect_stroke(
            Rect::from_center_size(center, vec2(w, h)),
            radius,
            s,
            egui::StrokeKind::Inside,
        )
    };
    match kind {
        Icon::Mail => {
            outline(15., 11., 2.);
            path(&[(-6., -4.), (0., 1.), (6., -4.)]);
        }
        Icon::Search => {
            p.circle_stroke(point(-1.5, -1.5), 4.7, s);
            path(&[(2., 2.), (6.8, 6.8)]);
        }
        Icon::Inbox => {
            path(&[
                (-7., 1.),
                (-5., -6.),
                (5., -6.),
                (7., 1.),
                (7., 6.),
                (-7., 6.),
                (-7., 1.),
                (-3., 1.),
                (-1.5, 3.),
                (1.5, 3.),
                (3., 1.),
                (7., 1.),
            ]);
        }
        Icon::Star => {
            let pts = (0..=10)
                .map(|i| {
                    let theta = i as f32 * std::f32::consts::PI / 5. - std::f32::consts::FRAC_PI_2;
                    let r = if i % 2 == 0 { 7.5 } else { 3.3 };
                    (r * theta.cos(), r * theta.sin())
                })
                .collect::<Vec<_>>();
            path(&pts);
        }
        Icon::Pencil => {
            path(&[
                (-6., 6.),
                (-5., 1.),
                (3., -7.),
                (7., -3.),
                (-1., 5.),
                (-6., 6.),
            ]);
            path(&[(1., -5.), (5., -1.)]);
        }
        Icon::Send => {
            path(&[(-7., -5.), (7., 0.), (-7., 6.), (-4., 0.), (-7., -5.)]);
            path(&[(-4., 0.), (3., 0.)]);
        }
        Icon::Clock => {
            p.circle_stroke(center, 6.8, s);
            path(&[(0., -4.), (0., 0.), (3., 2.)]);
        }
        Icon::All => {
            outline(14., 11., 2.);
            path(&[(-3., -2.), (3., -2.)]);
        }
        Icon::Trash => {
            path(&[(-7., -4.), (7., -4.)]);
            path(&[(-3., -4.), (-3., -6.), (3., -6.), (3., -4.)]);
            path(&[(-5., -3.), (-4., 7.), (4., 7.), (5., -3.)]);
        }
        Icon::Tag => {
            path(&[
                (-7., -6.),
                (-1., -6.),
                (7., 2.),
                (2., 7.),
                (-7., -2.),
                (-7., -6.),
            ]);
            p.circle_filled(point(-4., -3.), 1., color);
        }
        Icon::Tune => {
            for (y, x) in [(-5., -2.), (0., 3.), (5., -3.)] {
                path(&[(-7., y), (7., y)]);
                p.circle_filled(point(x, y), 2.2, color);
            }
        }
        Icon::Archive => {
            path(&[(-7., -2.), (7., -2.), (7., -6.), (-7., -6.), (-7., -2.)]);
            path(&[(-6., -2.), (-6., 6.), (6., 6.), (6., -2.)]);
            path(&[(-2., 1.), (0., 3.), (2., 1.)]);
        }
        Icon::Read => {
            path(&[
                (-7., 0.),
                (0., -6.),
                (7., 0.),
                (7., 6.),
                (-7., 6.),
                (-7., 0.),
                (0., 3.),
                (7., 0.),
            ]);
        }
        Icon::Clip => {
            path(&[
                (-1., -4.),
                (3., -7.),
                (6., -5.),
                (6., -2.),
                (-1., 6.),
                (-5., 6.),
                (-7., 3.),
                (-6., 0.),
                (0., -6.),
            ]);
            path(&[(-4., 3.), (1., -2.), (3., -2.), (3., 0.), (-1., 4.)]);
        }
        Icon::Settings => {
            p.circle_stroke(center, 2.5, s);
            p.circle_stroke(center, 5.6, s);
            for i in 0..8 {
                let angle = i as f32 * std::f32::consts::FRAC_PI_4;
                p.line_segment(
                    [
                        point(angle.cos() * 5.5, angle.sin() * 5.5),
                        point(angle.cos() * 7.8, angle.sin() * 7.8),
                    ],
                    s,
                );
            }
        }
        Icon::Back => {
            path(&[(3., -5.), (-2., 0.), (3., 5.)]);
        }
        Icon::Next => {
            path(&[(-3., -5.), (2., 0.), (-3., 5.)]);
        }
    }
}

pub fn icon_button(ui: &mut Ui, kind: Icon, tooltip: &str, theme: Theme) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(30., 30.), egui::Sense::click());
    paint_icon_button(ui, rect, &response, kind, tooltip, theme);
    response.on_hover_text(tooltip)
}

pub fn paint_icon_button(
    ui: &Ui,
    rect: egui::Rect,
    response: &egui::Response,
    kind: Icon,
    tooltip: &str,
    theme: Theme,
) {
    let active = response.hovered() || response.has_focus();
    if active && response.enabled() {
        ui.painter().rect_filled(rect, 6, theme.selected);
    }
    let color = match (response.enabled(), active) {
        (true, true) => theme.ink,
        (true, false) => theme.muted,
        (false, _) => theme.muted.gamma_multiply(0.53),
    };
    icon(ui.painter(), kind, rect.center(), color);
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, response.enabled(), tooltip)
    });
}
