use std::io::Read;

use byteorder::{LittleEndian, ReadBytesExt};
use eyre::{bail, ensure, eyre, Context, Result};
use packed_struct::prelude::*;

use crate::format::{
    Color, ColorEncoding, Command, CoordinateRange, File, Header, Line, Point, Rectangle, Segment,
    SegmentCommand, SegmentCommandKind, Style, Sweep,
};

struct ByteCountReader<R> {
    inner: R,
    bytes_read: usize,
}

impl<R> ByteCountReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            bytes_read: 0,
        }
    }
}

impl<R> Read for ByteCountReader<R>
where
    R: Read,
{
    fn read(&mut self, buffer: &mut [u8]) -> Result<usize, std::io::Error> {
        let bytes_read = self.inner.read(buffer)?;

        self.bytes_read += bytes_read;

        Ok(bytes_read)
    }
}

pub struct Parser<R> {
    reader: ByteCountReader<R>,
    coordinate_range: CoordinateRange,
    color_count: u32,
    color_encoding: ColorEncoding,
    scale: u32,
}

enum StyleVariant {
    FlatColor,
    LinearGradient,
    RadialGradient,
}

impl TryFrom<u8> for StyleVariant {
    type Error = eyre::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Ok(match value {
            0 => StyleVariant::FlatColor,
            1 => StyleVariant::LinearGradient,
            2 => StyleVariant::RadialGradient,
            x => bail!("unsupported primary style: {}", x),
        })
    }
}

struct SegmentCommandTag {
    instruction: SegmentCommandVariant,
    line_width: Option<f32>,
}

enum SegmentCommandVariant {
    Line,
    HorizontalLine,
    VerticalLine,
    CubicBezier,
    ArcCircle,
    ArcEllipse,
    ClosePath,
    QuadraticBezier,
}

impl<R> Parser<R>
where
    R: Read,
{
    pub fn new(reader: R) -> Self {
        Self {
            reader: ByteCountReader::new(reader),
            coordinate_range: CoordinateRange::Default,
            color_count: 0,
            color_encoding: ColorEncoding::Rgb565,
            scale: 0,
        }
    }

    fn magic_number(&mut self) -> Result<()> {
        let b0 = self.reader.read_u8()?;
        let b1 = self.reader.read_u8()?;

        ensure!(
            b0 == 0x72 && b1 == 0x56,
            "tinyvg file must begin with magic number 0x72 0x56, found {:x} {:x}",
            b0,
            b1
        );

        Ok(())
    }

    fn version(&mut self) -> Result<u8> {
        let version = self.reader.read_u8()?;

        Ok(version)
    }

    fn scale_properties(&mut self) -> Result<ScaleProperties> {
        #[derive(PackedStruct, Debug)]
        #[packed_struct(bit_numbering = "msb0")]
        pub struct ScaleAndFlags {
            #[packed_field(bits = "4..8")]
            scale: Integer<u8, packed_bits::Bits<4>>,

            #[packed_field(bits = "2..4")]
            color_encoding: Integer<u8, packed_bits::Bits<2>>,

            #[packed_field(bits = "0..2")]
            coordinate_range: Integer<u8, packed_bits::Bits<2>>,
        }

        let x = self.reader.read_u8()?;

        let scale_and_flags = ScaleAndFlags::unpack(&[x])?;

        let coordinate_range = match *scale_and_flags.coordinate_range {
            0 => CoordinateRange::Default,
            1 => CoordinateRange::Reduced,
            2 => CoordinateRange::Enhanced,
            x => {
                bail!("unrecognized coordinate type {}", x);
            }
        };

        let color_encoding = match *scale_and_flags.color_encoding {
            0 => ColorEncoding::Rgba8888,
            1 => ColorEncoding::Rgb565,
            2 => ColorEncoding::RgbaF32,
            3 => {
                bail!("custom color encodings are not supported");
            }
            x => {
                bail!("unrecognized color encoding {}", x);
            }
        };

        Ok(ScaleProperties {
            scale: *scale_and_flags.scale,
            color_encoding,
            coordinate_range,
        })
    }

    fn read_with_coordinate_range(&mut self) -> Result<u32> {
        match self.coordinate_range {
            CoordinateRange::Reduced => {
                let x = self.reader.read_u8()?;
                Ok(x as u32)
            }
            CoordinateRange::Default => {
                let x = self.reader.read_u16::<LittleEndian>()?;
                Ok(x as u32)
            }
            CoordinateRange::Enhanced => {
                let x = self.reader.read_u32::<LittleEndian>()?;
                Ok(x as u32)
            }
        }
    }

    fn read_var_uint(&mut self) -> Result<u32> {
        let mut result = 0u32;
        let mut count = 0;

        loop {
            let b = self.reader.read_u8()? as u32;

            result |= (b & 0x7F) << (7 * count);

            if (b & 0x80) == 0 {
                break;
            }

            count += 1;
        }

        Ok(result)
    }

    fn parse_color_table(&mut self) -> Result<Vec<Color>> {
        let mut colors = Vec::new();

        for _ in 0..self.color_count {
            colors.push(match self.color_encoding {
                ColorEncoding::Rgba8888 => self.color_8888()?,
                ColorEncoding::RgbaF32 => self.color_f32()?,
                ColorEncoding::Rgb565 => self.color_565()?,
            })
        }

        Ok(colors)
    }

    fn color_8888(&mut self) -> Result<Color> {
        let red = self.reader.read_u8()?;
        let green = self.reader.read_u8()?;
        let blue = self.reader.read_u8()?;
        let alpha = self.reader.read_u8()?;

        Ok(Color {
            red: red as f32 / 255.0,
            green: green as f32 / 255.0,
            blue: blue as f32 / 255.0,
            alpha: alpha as f32 / 255.0,
        })
    }

    fn color_f32(&mut self) -> Result<Color> {
        let red = self.reader.read_f32::<LittleEndian>()?;
        let green = self.reader.read_f32::<LittleEndian>()?;
        let blue = self.reader.read_f32::<LittleEndian>()?;
        let alpha = self.reader.read_f32::<LittleEndian>()?;

        Ok(Color {
            red,
            green,
            blue,
            alpha,
        })
    }

    fn color_565(&mut self) -> Result<Color> {
        let rgb = self.reader.read_u16::<LittleEndian>()?;

        Ok(Color {
            red: (((rgb & 0x001F) >> 0) as f32) / 31.0,
            green: (((rgb & 0x07E0) >> 5) as f32) / 63.0,
            blue: (((rgb & 0xF800) >> 11) as f32) / 31.0,
            alpha: 1.0,
        })
    }

    fn header(&mut self) -> Result<Header> {
        self.magic_number()?;
        let version = self.version()?;
        let scale_properties = self.scale_properties()?;

        self.coordinate_range = scale_properties.coordinate_range;
        let width = self.read_with_coordinate_range()?;
        let height = self.read_with_coordinate_range()?;

        let color_count = self.read_var_uint()?;

        self.color_count = color_count;
        self.color_encoding = scale_properties.color_encoding;
        self.scale = scale_properties.scale as u32;

        Ok(Header {
            version,
            scale: scale_properties.scale,
            color_encoding: scale_properties.color_encoding,
            coordinate_range: scale_properties.coordinate_range,
            width,
            height,
            color_count,
        })
    }

    fn fill_polygon(&mut self, style_variant: StyleVariant) -> Result<Command> {
        let (fill_style, polygon) = self.count_and_style_command(style_variant, Self::point)?;

        Ok(Command::FillPolygon {
            fill_style,
            polygon,
        })
    }

    fn read_unit(&mut self) -> Result<f32> {
        let raw = self.read_with_coordinate_range()?;

        let scale_factor = 1u32 << self.scale;
        let result = (raw as f32) / (scale_factor as f32);

        Ok(result)
    }

    fn rectangle(&mut self) -> Result<Rectangle> {
        let x = self.read_unit()?;
        let y = self.read_unit()?;
        let width = self.read_unit()?;
        let height = self.read_unit()?;

        Ok(Rectangle {
            x,
            y,
            width,
            height,
        })
    }

    fn fill_rectangles(&mut self, style_variant: StyleVariant) -> Result<Command> {
        let (fill_style, rectangles) =
            self.count_and_style_command(style_variant, Self::rectangle)?;

        Ok(Command::FillRectangles {
            fill_style,
            rectangles,
        })
    }

    fn style(&mut self, variant: StyleVariant) -> Result<Style> {
        let style = match variant {
            StyleVariant::FlatColor => {
                let color_index = self.read_var_uint()?.try_into()?;

                Style::FlatColor { color_index }
            }
            StyleVariant::LinearGradient => {
                let point_0 = self.point()?;
                let point_1 = self.point()?;

                let color_index_0 = self.read_var_uint()?.try_into()?;
                let color_index_1 = self.read_var_uint()?.try_into()?;

                Style::LinearGradient {
                    point_0,
                    point_1,
                    color_index_0,
                    color_index_1,
                }
            }
            StyleVariant::RadialGradient => {
                let point_0 = self.point()?;
                let point_1 = self.point()?;

                let color_index_0 = self.read_var_uint()?.try_into()?;
                let color_index_1 = self.read_var_uint()?.try_into()?;

                Style::RadialGradient {
                    point_0,
                    point_1,
                    color_index_0,
                    color_index_1,
                }
            }
        };

        Ok(style)
    }

    fn point(&mut self) -> Result<Point> {
        let x = self.read_unit()?;
        let y = self.read_unit()?;

        Ok(Point { x, y })
    }

    fn segment_command_line(&mut self) -> Result<SegmentCommandKind> {
        let end = self.point()?;

        Ok(SegmentCommandKind::Line { end })
    }

    fn segment_command_horizontal_line(&mut self) -> Result<SegmentCommandKind> {
        let x = self.read_unit()?;

        Ok(SegmentCommandKind::HorizontalLine { x })
    }

    fn segment_command_vertical_line(&mut self) -> Result<SegmentCommandKind> {
        let y = self.read_unit()?;

        Ok(SegmentCommandKind::VerticalLine { y })
    }

    fn segment_command_cubic_bezier(&mut self) -> Result<SegmentCommandKind> {
        let control_0 = self.point()?;
        let control_1 = self.point()?;
        let point_1 = self.point()?;

        Ok(SegmentCommandKind::CubicBezier {
            control_0,
            control_1,
            point_1,
        })
    }

    fn segment_command_arc_circle(&mut self) -> Result<SegmentCommandKind> {
        let (large, sweep) = self.arc_header()?;
        let radius = self.read_unit()?;
        let target = self.point()?;

        Ok(SegmentCommandKind::ArcCircle {
            large,
            sweep,
            radius,
            target,
        })
    }

    fn segment_command_arc_ellipse(&mut self) -> Result<SegmentCommandKind> {
        let (large, sweep) = self.arc_header()?;
        let radius_x = self.read_unit()?;
        let radius_y = self.read_unit()?;
        let rotation = self.read_unit()?;
        let target = self.point()?;

        Ok(SegmentCommandKind::ArcEllipse {
            large,
            sweep,
            radius_x,
            radius_y,
            rotation,
            target,
        })
    }

    fn arc_header(&mut self) -> Result<(bool, Sweep)> {
        let raw = self.reader.read_u8()?;
        let is_large = (raw & 0b1000_0000) > 0;
        let sweep = if (raw & 0b0100_0000) > 0 {
            Sweep::Left
        } else {
            Sweep::Right
        };

        Ok((is_large, sweep))
    }

    fn segment_command_quadratic_bezier(&mut self) -> Result<SegmentCommandKind> {
        let control = self.point()?;
        let point_1 = self.point()?;

        Ok(SegmentCommandKind::QuadraticBezier { control, point_1 })
    }

    fn segment(&mut self) -> Result<Segment> {
        let count = self.read_var_uint()?;
        let start = self.point()?;

        dbg!(count, &start);

        let mut commands = Vec::new();
        for _ in 0..count {
            let tag = self.segment_command_tag()?;
            let kind = match tag.instruction {
                SegmentCommandVariant::Line => self.segment_command_line()?,
                SegmentCommandVariant::HorizontalLine => self.segment_command_horizontal_line()?,
                SegmentCommandVariant::VerticalLine => self.segment_command_vertical_line()?,
                SegmentCommandVariant::CubicBezier => self.segment_command_cubic_bezier()?,
                SegmentCommandVariant::ArcCircle => self.segment_command_arc_circle()?,
                SegmentCommandVariant::ArcEllipse => self.segment_command_arc_ellipse()?,
                SegmentCommandVariant::ClosePath => SegmentCommandKind::ClosePath,
                SegmentCommandVariant::QuadraticBezier => {
                    self.segment_command_quadratic_bezier()?
                }
            };

            commands.push(SegmentCommand {
                kind,
                line_width: tag.line_width,
            });
        }

        dbg!(&commands);

        Ok(Segment { start, commands })
    }

    fn segment_command_tag(&mut self) -> Result<SegmentCommandTag> {
        let raw = self.reader.read_u8()?;

        let instruction = (raw & 0b1110_0000) >> 5;

        let has_line_width = (raw & 0b000_1000) > 0;
        let line_width = if has_line_width {
            Some(self.read_unit()?)
        } else {
            None
        };

        let instruction = match instruction {
            0 => SegmentCommandVariant::Line,
            1 => SegmentCommandVariant::HorizontalLine,
            2 => SegmentCommandVariant::VerticalLine,
            3 => SegmentCommandVariant::CubicBezier,
            4 => SegmentCommandVariant::ArcCircle,
            5 => SegmentCommandVariant::ArcEllipse,
            6 => SegmentCommandVariant::ClosePath,
            7 => SegmentCommandVariant::QuadraticBezier,
            x => bail!("illegal path segment instruction: {}", x),
        };

        Ok(SegmentCommandTag {
            line_width,
            instruction,
        })
    }

    fn count_and_style_command<T>(
        &mut self,
        variant: StyleVariant,
        f: impl Fn(&mut Self) -> Result<T>,
    ) -> Result<(Style, Vec<T>)> {
        let count = self.read_var_uint()? + 1;
        let style = self.style(variant)?;

        dbg!(count);

        let mut items = Vec::new();
        for _ in 0..count {
            items.push(f(self)?);
        }

        Ok((style, items))
    }

    fn fill_path(&mut self, style_variant: StyleVariant) -> Result<Command> {
        let (fill_style, path) = self.count_and_style_command(style_variant, Self::segment)?;

        Ok(Command::FillPath { fill_style, path })
    }

    fn u6_u2(&mut self) -> Result<(u8, u8)> {
        let byte = self.reader.read_u8()?;

        let u6 = byte & 0b0011_1111;
        let u2 = (byte & 0b1100_0000) >> 6;

        Ok((u6, u2))
    }

    fn line(&mut self) -> Result<Line> {
        let start = self.point()?;
        let end = self.point()?;

        Ok(Line { start, end })
    }

    fn draw_lines(&mut self, style_variant: StyleVariant) -> Result<Command> {
        let count = self.read_var_uint()? + 1;
        let line_style = self.style(style_variant)?;
        let line_width = self.reader.read_f32::<LittleEndian>()?;

        let mut lines = Vec::new();
        for _ in 0..count {
            lines.push(self.line()?);
        }

        Ok(Command::DrawLines {
            line_style,
            line_width,
            lines,
        })
    }

    fn draw_line_loop(&mut self, style_variant: StyleVariant) -> Result<Command> {
        let count = self.read_var_uint()? + 1;
        let line_style = self.style(style_variant)?;
        let line_width = self.reader.read_f32::<LittleEndian>()?;

        let mut points = Vec::new();
        for _ in 0..count {
            points.push(self.point()?);
        }

        Ok(Command::DrawLineLoop {
            line_style,
            line_width,
            points,
        })
    }

    fn draw_line_strip(&mut self, style_variant: StyleVariant) -> Result<Command> {
        let count = self.read_var_uint()? + 1;
        let line_style = self.style(style_variant)?;
        let line_width = self.reader.read_f32::<LittleEndian>()?;

        let mut points = Vec::new();
        for _ in 0..count {
            points.push(self.point()?);
        }

        Ok(Command::DrawLineStrip {
            line_style,
            line_width,
            points,
        })
    }

    fn draw_line_path(&mut self, style_variant: StyleVariant) -> Result<Command> {
        let count = self.read_var_uint()? + 1;
        let line_style = self.style(style_variant)?;
        let line_width = self.reader.read_f32::<LittleEndian>()?;

        let mut path = Vec::new();
        for _ in 0..count {
            path.push(self.segment()?);
        }

        Ok(Command::DrawLinePath {
            line_style,
            line_width,
            path,
        })
    }

    fn outline_fill_cmd<T>(
        &mut self,
        primary_style: StyleVariant,
        f: impl Fn(&mut Self) -> Result<T>,
    ) -> Result<OutlineFill<T>> {
        let (segment_count, secondary_style) = self.u6_u2()?;
        let secondary_style = StyleVariant::try_from(secondary_style)?;

        let fill_style = self.style(primary_style)?;
        let line_style = self.style(secondary_style)?;

        let line_width = self.reader.read_f32::<LittleEndian>()?;

        let mut items = Vec::new();
        for _ in 0..(segment_count + 1) {
            items.push(f(self)?);
        }

        Ok(OutlineFill {
            fill_style,
            line_style,
            line_width,
            items,
        })
    }

    fn outline_fill_polygon(&mut self, primary_style: StyleVariant) -> Result<Command> {
        let outline_fill = self.outline_fill_cmd(primary_style, Self::point)?;

        Ok(Command::OutlineFillPolygon {
            fill_style: outline_fill.fill_style,
            line_style: outline_fill.line_style,
            line_width: outline_fill.line_width,
            points: outline_fill.items,
        })
    }

    fn outline_fill_rectangles(&mut self, primary_style: StyleVariant) -> Result<Command> {
        let outline_fill = self.outline_fill_cmd(primary_style, Self::rectangle)?;

        Ok(Command::OutlineFillRectangle {
            fill_style: outline_fill.fill_style,
            line_style: outline_fill.line_style,
            line_width: outline_fill.line_width,
            rectangles: outline_fill.items,
        })
    }

    fn outline_fill_path(&mut self, primary_style: StyleVariant) -> Result<Command> {
        let outline_fill = self.outline_fill_cmd(primary_style, Self::segment)?;

        Ok(Command::OutlineFillPath {
            fill_style: outline_fill.fill_style,
            line_style: outline_fill.line_style,
            line_width: outline_fill.line_width,
            path: outline_fill.items,
        })
    }

    fn command(&mut self) -> Result<Option<Command>> {
        let (command_index, primary_style) = self.u6_u2()?;

        dbg!(command_index);

        let primary_style = primary_style.try_into()?;

        let command = match command_index {
            0 => return Ok(None),
            1 => self.fill_polygon(primary_style)?,
            2 => self.fill_rectangles(primary_style)?,
            3 => self.fill_path(primary_style)?,
            4 => self.draw_lines(primary_style)?,
            5 => self.draw_line_loop(primary_style)?,
            6 => self.draw_line_strip(primary_style)?,
            7 => self.draw_line_path(primary_style)?,
            8 => self.outline_fill_polygon(primary_style)?,
            9 => self.outline_fill_rectangles(primary_style)?,
            10 => self.outline_fill_path(primary_style)?,
            x => bail!("unsupported command type: {}", x),
        };

        dbg!(&command);

        Ok(Some(command))
    }

    pub fn parse(mut self) -> Result<File> {
        self.parse_inner().wrap_err_with(|| {
            eyre!(
                "parsing failed after reading {} bytes",
                self.reader.bytes_read
            )
        })
    }

    fn parse_inner(&mut self) -> Result<File> {
        let header = self.header().wrap_err("error parsing header")?;
        let color_table = self
            .parse_color_table()
            .wrap_err("error parsing color table")?;

        let mut commands = Vec::new();
        while let Some(command) = self.command().wrap_err("error parsing command")? {
            commands.push(command);
        }

        let mut trailer = Vec::new();
        self.reader
            .read_to_end(&mut trailer)
            .wrap_err("error reading trailing bytes")?;

        Ok(File {
            header,
            color_table,
            commands,
            trailer,
        })
    }
}

struct OutlineFill<T> {
    fill_style: Style,
    line_style: Style,
    line_width: f32,
    items: Vec<T>,
}

#[derive(Debug)]
struct ScaleProperties {
    scale: u8,
    color_encoding: ColorEncoding,
    coordinate_range: CoordinateRange,
}

#[cfg(test)]
mod tests {
    use super::Parser;
    use eyre::Result;
    use std::fs::File;

    #[test]
    fn test_parser() -> Result<()> {
        let file = File::open("data/everything.tvg")?;
        let p = Parser::new(file);

        let result = p.parse()?;

        insta::assert_debug_snapshot!(result);

        Ok(())
    }
}
