use cairo::{Format, ImageSurface};
use eyre::{Context, Result};
use kurbo::{Arc, BezPath, CubicBez, Line, QuadBez, SvgArc, Vec2};
use piet::kurbo::{Point, Size};
use piet::{Color, FixedLinearGradient, FixedRadialGradient, GradientStop, RenderContext};
use piet_cairo::CairoRenderContext;

use crate::format::{Command, OutlineStyle, Segment, SegmentCommand, SegmentCommandKind, Style};

impl crate::format::File {
    pub fn render_png(&self, writer: &mut impl std::io::Write) -> Result<()> {
        let size = Size {
            width: self.header.width as f64,
            height: self.header.height as f64,
        };

        let surface = ImageSurface::create(Format::ARgb32, size.width as i32, size.height as i32)
            .wrap_err("failed to create cairo surface")?;
        let cr = cairo::Context::new(&surface).unwrap();

        let render_result = {
            let mut piet_context = CairoRenderContext::new(&cr);

            let result = self
                .draw(&mut piet_context)
                .wrap_err("failed to draw tinyvg file");

            piet_context
                .finish()
                .map_err(|e| eyre::eyre!("{}", e))
                .wrap_err("failed to finalize piet context")?;

            result
        };

        surface.flush();
        surface.write_to_png(writer)?;

        render_result?;

        Ok(())
    }

    fn outline_style<R>(&self, rc: &mut R, o: &Option<OutlineStyle>) -> Result<(f64, R::Brush)>
    where
        R: RenderContext,
    {
        match o {
            Some(style) => Ok((style.line_width, self.brush(rc, &style.line_style)?)),
            None => Ok((0.0, nil_brush(rc))),
        }
    }

    fn color(&self, index: usize) -> Result<Color> {
        self.color_table.get(index).cloned().ok_or_else(|| {
            eyre::eyre!(
                "file has {} colors but tried to get index {}",
                self.color_table.len(),
                index
            )
        })
    }

    fn brush<R>(&self, rc: &mut R, style: &Style) -> Result<R::Brush>
    where
        R: RenderContext,
    {
        let brush = match style {
            Style::FlatColor { color_index } => rc.solid_brush(self.color(*color_index)?),
            Style::LinearGradient {
                point_0,
                point_1,
                color_index_0,
                color_index_1,
            } => rc
                .gradient(FixedLinearGradient {
                    start: *point_0,
                    end: *point_1,
                    stops: vec![
                        GradientStop {
                            pos: 0.0,
                            color: self.color(*color_index_0)?,
                        },
                        GradientStop {
                            pos: 1.0,
                            color: self.color(*color_index_1)?,
                        },
                    ],
                })
                .map_err(|e| eyre::eyre!("{}", e))?,
            Style::RadialGradient {
                point_0,
                point_1,
                color_index_0,
                color_index_1,
            } => rc
                .gradient(FixedRadialGradient {
                    center: *point_0,
                    origin_offset: Vec2 { x: 0.0, y: 0.0 },
                    radius: point_0.distance(*point_1),
                    stops: vec![
                        GradientStop {
                            pos: 0.0,
                            color: self.color(*color_index_0)?,
                        },
                        GradientStop {
                            pos: 1.0,
                            color: self.color(*color_index_1)?,
                        },
                    ],
                })
                .map_err(|e| eyre::eyre!("{}", e))?,
        };

        Ok(brush)
    }

    fn draw(&self, rc: &mut impl RenderContext) -> Result<()> {
        // rc.clear(None, Color::WHITE);
        for cmd in &self.commands {
            match cmd {
                Command::FillPath {
                    fill_style,
                    path,
                    outline,
                } => {
                    let fill = self.brush(rc, fill_style)?;
                    let (line_width, line_brush) = self.outline_style(rc, outline)?;

                    draw_path(rc, fill, line_brush, line_width, path)?;
                }
                Command::FillRectangles {
                    fill_style,
                    rectangles,
                    outline,
                } => {
                    let brush = self.brush(rc, fill_style)?;
                    let (line_width, line_brush) = self.outline_style(rc, outline)?;

                    for rect in rectangles {
                        rc.fill(rect, &brush);
                        rc.stroke(rect, &line_brush, line_width);
                    }
                }
                Command::FillPolygon {
                    fill_style,
                    polygon,
                    outline,
                } => {
                    let brush = self.brush(rc, fill_style)?;
                    let (line_width, line_brush) = self.outline_style(rc, outline)?;

                    let mut bez = BezPath::new();
                    bez.move_to(polygon[0]);

                    for point in polygon {
                        bez.line_to(*point);
                    }

                    rc.fill(&bez, &brush);
                    rc.stroke(&bez, &line_brush, line_width);
                }
                Command::DrawLines {
                    line_style,
                    line_width,
                    lines,
                } => {
                    let brush = self.brush(rc, line_style)?;

                    for line in lines {
                        rc.stroke(line, &brush, *line_width);
                    }
                }
                Command::DrawLineLoop {
                    line_style,
                    line_width,
                    close_path,
                    points,
                } => {
                    let line = self.brush(rc, line_style)?;

                    let mut bez = BezPath::new();
                    let start = points[0];
                    bez.move_to(start);

                    for p in points {
                        bez.line_to(*p);
                    }

                    if *close_path {
                        bez.line_to(start);
                    }

                    rc.stroke(bez, &line, *line_width);
                }
                Command::DrawLinePath {
                    line_style,
                    line_width,
                    path,
                } => {
                    let line = self.brush(rc, line_style)?;
                    let fill = nil_brush(rc);

                    draw_path(rc, fill, line, *line_width, path)?;
                }
            }
        }

        Ok(())
    }
}

fn draw_path<R>(
    rc: &mut R,
    fill: R::Brush,
    line: R::Brush,
    mut line_width: f64,
    path: &[Segment],
) -> Result<()>
where
    R: RenderContext,
{
    let mut bezier = BezPath::new();

    for Segment { start, commands } in path {
        let mut pen = *start;

        bezier.move_to(pen);

        for SegmentCommand {
            kind,
            line_width: next_line_width,
        } in commands
        {
            if let Some(lw) = next_line_width {
                line_width = *lw;
            }

            match kind {
                SegmentCommandKind::Line { end } => {
                    bezier.line_to(*end);
                    rc.stroke(Line { p0: pen, p1: *end }, &line, line_width);

                    pen = *end;
                }
                SegmentCommandKind::VerticalLine { y } => {
                    let end = Point { x: pen.x, y: *y };

                    bezier.line_to(end);
                    rc.stroke(Line { p0: pen, p1: end }, &line, line_width);

                    pen = end;
                }
                SegmentCommandKind::CubicBezier {
                    control_0,
                    control_1,
                    point_1,
                } => {
                    bezier.curve_to(*control_0, *control_1, *point_1);
                    rc.stroke(
                        CubicBez::new(pen, *control_0, *control_1, *point_1),
                        &line,
                        line_width,
                    );

                    pen = *point_1;
                }
                SegmentCommandKind::HorizontalLine { x } => {
                    let end = Point { x: *x, y: pen.y };

                    bezier.line_to(end);
                    rc.stroke(Line { p0: pen, p1: end }, &line, line_width);

                    pen = end;
                }
                SegmentCommandKind::ArcEllipse {
                    large,
                    sweep,
                    radius_x,
                    radius_y,
                    rotation,
                    target,
                } => {
                    let svg_arc = SvgArc {
                        from: pen,
                        to: *target,
                        radii: Vec2 {
                            x: *radius_x,
                            y: *radius_y,
                        },
                        x_rotation: *rotation,
                        large_arc: *large,
                        sweep: *sweep,
                    };
                    let arc = Arc::from_svg_arc(&svg_arc).ok_or_else(|| {
                        eyre::eyre!("failed to create arc from svg arc {:?}", svg_arc)
                    })?;

                    for segment in arc.append_iter(0.2) {
                        bezier.push(segment);
                    }
                    rc.stroke(&arc, &line, line_width);

                    pen = *target;
                }
                SegmentCommandKind::ClosePath => {
                    bezier.line_to(*start);
                    rc.stroke(
                        Line {
                            p0: pen,
                            p1: *start,
                        },
                        &line,
                        line_width,
                    );

                    pen = *start;
                }
                SegmentCommandKind::QuadraticBezier { control, point_1 } => {
                    bezier.quad_to(*control, *point_1);
                    rc.stroke(QuadBez::new(pen, *control, *point_1), &line, line_width);

                    pen = *point_1;
                }
            }
        }
    }

    rc.fill(&bezier, &fill);

    Ok(())
}

fn nil_brush<R>(rc: &mut R) -> R::Brush
where
    R: RenderContext,
{
    rc.solid_brush(Color::rgba(0.0, 0.0, 0.0, 0.0))
}
