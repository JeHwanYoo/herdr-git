use ratatui::layout::Rect;
use ratatui::style::Color;
use tiny_skia::{BlendMode, FillRule, LineCap, Paint, PathBuilder, Pixmap, Stroke, Transform};

use super::layout::{GraphLayout, GraphViewport, ROW_HEIGHT, lane_cell};
use crate::herdr::{GraphicsPlacement, GraphicsSurface};
use crate::ui::theme;

const MAX_PIXELS: u64 = 8_000_000;
const CURVES_LAYER: &str = "curves";
const RULES_LAYER: &str = "rules";

type Rgb = (u8, u8, u8);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ui) struct RuleViewport {
    pub(in crate::ui) area: Rect,
    pub(in crate::ui) rows: usize,
    pub(in crate::ui) hidden: Rect,
}

pub(in crate::ui) struct CurveLayer {
    enabled: bool,
    curves: GraphicsLayer<(u64, GraphViewport)>,
    rules: GraphicsLayer<RuleViewport>,
}

impl CurveLayer {
    pub(in crate::ui) fn disabled() -> Self {
        Self {
            enabled: true,
            curves: GraphicsLayer::disabled(),
            rules: GraphicsLayer::disabled(),
        }
    }

    pub(in crate::ui) fn connect(enabled: bool) -> Self {
        Self {
            enabled,
            curves: GraphicsLayer::connect(CURVES_LAYER),
            rules: GraphicsLayer::connect(RULES_LAYER),
        }
    }

    pub(in crate::ui) fn enabled(&self) -> bool {
        self.enabled
    }

    pub(in crate::ui) fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub(in crate::ui) fn connected(&self) -> bool {
        self.curves.available()
    }

    pub(in crate::ui) fn available(&self) -> bool {
        self.enabled && self.connected()
    }

    pub(in crate::ui) fn rules_available(&self) -> bool {
        self.available() && self.rules.available()
    }

    pub(in crate::ui) fn reconnect(&mut self) {
        self.curves.reconnect(CURVES_LAYER);
        self.rules.reconnect(RULES_LAYER);
    }

    pub(super) fn present(
        &mut self,
        layout: &GraphLayout,
        viewport: Option<GraphViewport>,
        rules: Option<RuleViewport>,
    ) -> bool {
        let curves_failed = self.curves.present(
            viewport.map(|viewport| (viewport.area, (layout.generation, viewport))),
            |(_, viewport), cell| rasterize(layout, viewport, cell),
        );
        let rules = rules.filter(|_| self.available());
        let rules_failed = self
            .rules
            .present(rules.map(|rules| (rules.area, rules)), |rules, cell| {
                rasterize_rules(rules, cell)
            });
        curves_failed || rules_failed
    }
}

struct GraphicsLayer<F> {
    surface: Option<GraphicsSurface>,
    painted: Option<(F, (u32, u32))>,
}

impl<F: PartialEq> GraphicsLayer<F> {
    fn disabled() -> Self {
        Self {
            surface: None,
            painted: None,
        }
    }

    fn connect(layer: &'static str) -> Self {
        Self {
            surface: GraphicsSurface::connect(layer).ok(),
            painted: None,
        }
    }

    fn available(&self) -> bool {
        self.surface.is_some()
    }

    fn reconnect(&mut self, layer: &'static str) {
        self.painted = None;
        let refreshed = self
            .surface
            .as_mut()
            .is_some_and(|surface| surface.refresh_cell_size().is_ok());
        if !refreshed {
            self.surface = GraphicsSurface::connect(layer).ok();
        }
    }

    fn present(
        &mut self,
        scene: Option<(Rect, F)>,
        draw: impl FnOnce(&F, (u32, u32)) -> Result<Pixmap, String>,
    ) -> bool {
        let Some(surface) = &mut self.surface else {
            return false;
        };
        let Some((area, scene)) = scene.filter(|(area, _)| !area.is_empty()) else {
            if self.painted.take().is_some() {
                surface.hide();
            }
            return false;
        };
        let cell = surface.cell_size();
        if self
            .painted
            .as_ref()
            .is_some_and(|painted| painted.0 == scene && painted.1 == cell)
        {
            return false;
        }
        let shown = draw(&scene, cell).and_then(|pixmap| {
            let png = pixmap.encode_png().map_err(|error| error.to_string())?;
            surface.show_png(
                &png,
                (pixmap.width(), pixmap.height()),
                GraphicsPlacement {
                    column: area.x,
                    row: area.y,
                    columns: area.width,
                    rows: area.height,
                },
            )
        });
        if shown.is_err() {
            self.surface = None;
            self.painted = None;
            return true;
        }
        self.painted = Some((scene, cell));
        false
    }
}

fn rasterize_rules(rules: &RuleViewport, cell: (u32, u32)) -> Result<Pixmap, String> {
    let width = u32::from(rules.area.width) * cell.0;
    let height = u32::from(rules.area.height) * cell.1;
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err("Row rule image is too large".to_owned());
    }
    let mut pixmap = Pixmap::new(width, height).ok_or("Row rule image is empty")?;
    let paint = solid(rgb(theme::RULE).unwrap_or((74, 78, 86)), 255);
    for row in 1..=rules.rows {
        let bottom = (row * ROW_HEIGHT) as u32 * cell.1;
        if bottom > height {
            break;
        }
        if let Some(rect) = tiny_skia::Rect::from_xywh(0.0, (bottom - 1) as f32, width as f32, 1.0)
        {
            pixmap.fill_rect(rect, &paint, Transform::identity(), None);
        }
    }
    clear_hidden(&mut pixmap, rules.area, rules.hidden, cell);
    Ok(pixmap)
}

fn clear_hidden(pixmap: &mut Pixmap, area: Rect, hidden: Rect, cell: (u32, u32)) {
    let hidden = hidden.intersection(area);
    if hidden.is_empty() {
        return;
    }
    if let Some(rect) = tiny_skia::Rect::from_xywh(
        (u32::from(hidden.x - area.x) * cell.0) as f32,
        (u32::from(hidden.y - area.y) * cell.1) as f32,
        (u32::from(hidden.width) * cell.0) as f32,
        (u32::from(hidden.height) * cell.1) as f32,
    ) {
        let paint = Paint {
            blend_mode: BlendMode::Clear,
            ..Paint::default()
        };
        pixmap.fill_rect(rect, &paint, Transform::identity(), None);
    }
}

pub(super) fn rasterize(
    layout: &GraphLayout,
    viewport: &GraphViewport,
    cell: (u32, u32),
) -> Result<Pixmap, String> {
    let width = u32::from(viewport.area.width) * cell.0;
    let height = u32::from(viewport.area.height) * cell.1;
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err("Graph image is too large".to_owned());
    }
    let mut pixmap = Pixmap::new(width, height).ok_or("Graph image is empty")?;
    let (cw, ch) = (cell.0 as f32, cell.1 as f32);
    let x = |column: usize| (lane_cell(column) as f32 + 0.5 - viewport.scroll as f32) * cw;
    let stroke_width = (cw * 0.18).max(1.4);
    let radius = (cw * 0.5).max(3.5);
    let mut nodes = Vec::new();
    for (offset, (index, row)) in layout
        .rows
        .iter()
        .enumerate()
        .skip(viewport.first)
        .take(viewport.rows)
        .enumerate()
    {
        let top = (offset * ROW_HEIGHT) as f32 * ch;
        let center = top + 0.5 * ch;
        let bottom = top + ROW_HEIGHT as f32 * ch;
        let background = row_background(viewport, index);
        if let Some(rgb) = background
            && let Some(rect) = tiny_skia::Rect::from_xywh(0.0, top, width as f32, bottom - top)
        {
            pixmap.fill_rect(rect, &solid(rgb, 255), Transform::identity(), None);
        }
        for (column, color) in row.above.iter().enumerate() {
            if let Some(color) = color {
                let mut path = PathBuilder::new();
                path.move_to(x(column), top);
                path.line_to(x(column), center);
                stroke(&mut pixmap, path, lane_rgb(*color), stroke_width);
            }
        }
        for edge in &row.edges {
            let mut path = PathBuilder::new();
            path.move_to(x(edge.from), center);
            if edge.from == edge.to {
                path.line_to(x(edge.to), bottom);
            } else {
                path.cubic_to(
                    x(edge.from),
                    top + 1.5 * ch,
                    x(edge.to),
                    top + ch,
                    x(edge.to),
                    bottom,
                );
            }
            stroke(&mut pixmap, path, lane_rgb(edge.color), stroke_width);
        }
        nodes.push((x(row.column), center, row, index, background));
    }
    for (x, y, row, index, background) in nodes {
        let rgb = lane_rgb(row.color);
        if viewport.selected == Some(index) {
            fill_circle(&mut pixmap, x, y, radius * 1.6, solid(rgb, 70));
        }
        fill_circle(&mut pixmap, x, y, radius, solid(rgb, 255));
        if row.uncommitted {
            let mut paint = match background {
                Some(background) => solid(background, 255),
                None => solid((0, 0, 0), 255),
            };
            if background.is_none() {
                paint.blend_mode = BlendMode::Clear;
            }
            fill_circle(&mut pixmap, x, y, radius - stroke_width, paint);
        }
    }
    clear_hidden(&mut pixmap, viewport.area, viewport.hidden, cell);
    Ok(pixmap)
}

fn row_background(viewport: &GraphViewport, index: usize) -> Option<Rgb> {
    if viewport.selected == Some(index) {
        rgb(theme::SURFACE_SELECTION)
    } else if viewport.hovered == Some(index) {
        rgb(theme::SURFACE_HOVER)
    } else {
        None
    }
}

fn lane_rgb(index: usize) -> Rgb {
    rgb(theme::graph_lane(index)).unwrap_or((200, 200, 200))
}

fn rgb(color: Color) -> Option<Rgb> {
    match color {
        Color::Rgb(red, green, blue) => Some((red, green, blue)),
        _ => None,
    }
}

fn solid(rgb: Rgb, alpha: u8) -> Paint<'static> {
    let mut paint = Paint::default();
    paint.set_color_rgba8(rgb.0, rgb.1, rgb.2, alpha);
    paint.anti_alias = true;
    paint
}

fn stroke(pixmap: &mut Pixmap, path: PathBuilder, rgb: Rgb, width: f32) {
    if let Some(path) = path.finish() {
        pixmap.stroke_path(
            &path,
            &solid(rgb, 255),
            &Stroke {
                width,
                line_cap: LineCap::Round,
                ..Stroke::default()
            },
            Transform::identity(),
            None,
        );
    }
}

fn fill_circle(pixmap: &mut Pixmap, x: f32, y: f32, radius: f32, paint: Paint<'_>) {
    if let Some(path) = PathBuilder::from_circle(x, y, radius) {
        pixmap.fill_path(
            &path,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
}

#[cfg(test)]
mod tests {
    use ratatui::layout::Rect;

    use super::*;
    use crate::ui::graph::layout::LayoutNode;

    fn diamond() -> GraphLayout {
        let parents = [
            vec!["a".to_owned(), "b".to_owned()],
            vec!["r".to_owned()],
            vec!["r".to_owned()],
            Vec::new(),
        ];
        GraphLayout::build(
            ["", "b", "a", "r"]
                .into_iter()
                .zip(&parents)
                .map(|(sha, parents)| LayoutNode { sha, parents }),
            1,
        )
    }

    #[test]
    fn rules_mark_the_bottom_pixel_row_of_each_commit() {
        let mut rules = RuleViewport {
            area: Rect::new(4, 1, 6, 7),
            rows: 4,
            hidden: Rect::default(),
        };
        let image = rasterize_rules(&rules, (10, 20)).unwrap();
        assert_eq!((image.width(), image.height()), (60, 140));
        let rule = rgb(theme::RULE).unwrap();
        let ruled_rows: Vec<u32> = (0..image.height())
            .filter(|&y| image.pixel(30, y).unwrap().alpha() > 0)
            .collect();
        assert_eq!(ruled_rows, [39, 79, 119]);
        let pixel = image.pixel(0, 39).unwrap();
        assert_eq!(
            (pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()),
            (rule.0, rule.1, rule.2, 255)
        );
        rules.hidden = Rect::new(6, 2, 20, 3);
        let image = rasterize_rules(&rules, (10, 20)).unwrap();
        assert_eq!(image.pixel(19, 39).unwrap().alpha(), 255);
        assert_eq!(image.pixel(20, 39).unwrap().alpha(), 0, "dialog hole");
        assert_eq!(image.pixel(59, 79).unwrap().alpha(), 0, "dialog hole");
        assert_eq!(image.pixel(20, 119).unwrap().alpha(), 255);
        let huge = RuleViewport {
            area: Rect::new(0, 0, 400, 200),
            rows: 100,
            hidden: Rect::default(),
        };
        assert!(rasterize_rules(&huge, (128, 256)).is_err());
    }

    #[test]
    fn curves_are_antialiased_over_a_transparent_background() {
        let layout = diamond();
        let viewport = GraphViewport {
            selected: Some(1),
            ..GraphViewport::new(Rect::new(0, 0, 5, 8), 0, 0, 4)
        };
        let image = rasterize(&layout, &viewport, (10, 20)).unwrap();
        assert_eq!((image.width(), image.height()), (50, 160));
        assert_eq!(image.pixel(49, 0).unwrap().alpha(), 0);
        assert_eq!(
            image.pixel(15, 10).unwrap().alpha(),
            0,
            "hollow uncommitted node"
        );
        let selection = rgb(theme::SURFACE_SELECTION).unwrap();
        let pixel = image.pixel(49, 50).unwrap();
        assert_eq!(
            (pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()),
            (selection.0, selection.1, selection.2, 255)
        );
        let partial = image
            .pixels()
            .iter()
            .filter(|pixel| (1..255).contains(&pixel.alpha()))
            .count();
        assert!(partial > 20, "curves should have antialiased edges");
        assert!(rasterize(&layout, &viewport, (128, 256)).is_ok());
        let huge = GraphViewport::new(Rect::new(0, 0, 200, 200), 0, 0, 4);
        assert!(rasterize(&layout, &huge, (128, 256)).is_err());
    }
}
