use crate::ui::RasterSurface;
use base64::Engine;
use crossterm::cursor::{MoveTo, RestorePosition, SavePosition};
use crossterm::{queue, terminal};
use ratatui::backend::CrosstermBackend;
use std::collections::HashSet;
use std::io::{self, Write};

const CANVAS: [u8; 3] = [5, 12, 24];

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

pub(crate) struct SurfaceRenderer {
    active: &'static str,
    cell_width: u16,
    cell_height: u16,
    transmitted: HashSet<([u8; 3], Corner)>,
    placements: Vec<(u32, u32)>,
}

impl SurfaceRenderer {
    pub(crate) fn new(requested: &str, theme: &str) -> Self {
        let window = terminal::window_size().ok();
        let (cell_width, cell_height) = window
            .filter(|window| window.columns > 0 && window.rows > 0)
            .map(|window| (window.width / window.columns, window.height / window.rows))
            .unwrap_or_default();
        let known_graphics_terminal = std::env::var("TERM_PROGRAM").is_ok_and(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "ghostty" | "kitty" | "wezterm"
            )
        }) || std::env::var_os("KITTY_WINDOW_ID").is_some();
        let light_auto = theme == "auto"
            && std::env::var("COLORFGBG")
                .ok()
                .and_then(|value| value.rsplit(';').next()?.parse::<u8>().ok())
                .is_some_and(|background| background >= 8);
        let concrete_dark_canvas = theme == "dark" || (theme == "auto" && !light_auto);
        let kitty = concrete_dark_canvas
            && (matches!(requested, "kitty") || (requested == "auto" && known_graphics_terminal));
        let active = if kitty && cell_width > 0 && cell_height > 0 {
            "kitty"
        } else if requested == "square"
            || theme == "no_color"
            || std::env::var("TERM").is_ok_and(|term| term == "dumb")
        {
            "square"
        } else {
            "quadrant"
        };
        Self {
            active,
            cell_width,
            cell_height,
            transmitted: HashSet::new(),
            placements: Vec::new(),
        }
    }

    pub(crate) const fn active_name(&self) -> &'static str {
        self.active
    }

    pub(crate) fn render<W: Write>(
        &mut self,
        backend: &mut CrosstermBackend<W>,
        surfaces: &[RasterSurface],
    ) -> io::Result<()> {
        if self.active != "kitty" {
            return Ok(());
        }
        // Ratatui positions the hardware cursor before this post-render graphics pass.
        // Every corner placement below uses an explicit MoveTo, so preserve the editor
        // cursor around the entire pass rather than leaving it at the final corner.
        queue!(backend, SavePosition)?;
        for (image_id, placement_id) in self.placements.drain(..) {
            write!(backend, "\x1b_Ga=d,d=p,i={image_id},p={placement_id},q=2;\x1b\\")?;
        }
        let corners = [
            Corner::TopLeft,
            Corner::TopRight,
            Corner::BottomLeft,
            Corner::BottomRight,
        ];
        let mut placement_id = 1_u32;
        for surface in surfaces {
            if surface.rect.width < 2 || surface.rect.height < 2 {
                continue;
            }
            let positions = [
                (surface.rect.x, surface.rect.y),
                (surface.rect.right().saturating_sub(1), surface.rect.y),
                (surface.rect.x, surface.rect.bottom().saturating_sub(1)),
                (
                    surface.rect.right().saturating_sub(1),
                    surface.rect.bottom().saturating_sub(1),
                ),
            ];
            for (corner, (x, y)) in corners.into_iter().zip(positions) {
                let image_id = image_id(surface.fill, corner);
                if self.transmitted.insert((surface.fill, corner)) {
                    let payload =
                        rounded_corner_rgba(self.cell_width, self.cell_height, CANVAS, surface.fill, corner);
                    let encoded = base64::engine::general_purpose::STANDARD.encode(payload);
                    write!(
                        backend,
                        "\x1b_Ga=t,f=32,s={},v={},i={image_id},q=2;{encoded}\x1b\\",
                        self.cell_width, self.cell_height
                    )?;
                }
                queue!(backend, MoveTo(x, y))?;
                write!(
                    backend,
                    "\x1b_Ga=p,i={image_id},p={placement_id},c=1,r=1,C=1,q=2;\x1b\\"
                )?;
                self.placements.push((image_id, placement_id));
                placement_id = placement_id.saturating_add(1);
            }
        }
        queue!(backend, RestorePosition)?;
        backend.flush()
    }
}

impl Drop for SurfaceRenderer {
    fn drop(&mut self) {
        if self.active == "kitty" {
            let _ = io::stdout().write_all(b"\x1b_Ga=d,d=a,q=2;\x1b\\");
        }
    }
}

fn image_id(fill: [u8; 3], corner: Corner) -> u32 {
    let corner = match corner {
        Corner::TopLeft => 1,
        Corner::TopRight => 2,
        Corner::BottomLeft => 3,
        Corner::BottomRight => 4,
    };
    ((0x40 + corner) << 24) | (u32::from(fill[0]) << 16) | (u32::from(fill[1]) << 8) | u32::from(fill[2])
}

fn rounded_corner_rgba(width: u16, height: u16, canvas: [u8; 3], fill: [u8; 3], corner: Corner) -> Vec<u8> {
    let width = usize::from(width.max(1));
    let height = usize::from(height.max(1));
    let radius = (width.min(height) as f32 * 0.9).max(1.0);
    let mut pixels = Vec::with_capacity(width * height * 4);
    for y in 0..height {
        for x in 0..width {
            let px = match corner {
                Corner::TopLeft | Corner::BottomLeft => x as f32,
                Corner::TopRight | Corner::BottomRight => (width - 1 - x) as f32,
            };
            let py = match corner {
                Corner::TopLeft | Corner::TopRight => y as f32,
                Corner::BottomLeft | Corner::BottomRight => (height - 1 - y) as f32,
            };
            let dx = radius - px;
            let dy = radius - py;
            let inside = px >= radius || py >= radius || dx * dx + dy * dy <= radius * radius;
            pixels.extend_from_slice(if inside { &fill } else { &canvas });
            pixels.push(255);
        }
    }
    pixels
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .map_err(|_| io::Error::other("test writer poisoned"))?
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn rounded_mask_keeps_outside_canvas_and_inside_fill() {
        let mask = rounded_corner_rgba(10, 20, CANVAS, [15, 34, 57], Corner::TopLeft);
        assert_eq!(&mask[..3], &CANVAS);
        let bottom_right = (10 * 20 - 1) * 4;
        assert_eq!(&mask[bottom_right..bottom_right + 3], &[15, 34, 57]);
    }

    #[test]
    fn kitty_render_restores_the_hardware_cursor_after_corner_placement() {
        let mut renderer = SurfaceRenderer {
            active: "kitty",
            cell_width: 8,
            cell_height: 16,
            transmitted: HashSet::new(),
            placements: Vec::new(),
        };
        let writer = SharedWriter::default();
        let output = Arc::clone(&writer.0);
        let mut backend = CrosstermBackend::new(writer);
        renderer
            .render(
                &mut backend,
                &[RasterSurface {
                    rect: ratatui::layout::Rect::new(2, 3, 12, 4),
                    fill: [15, 34, 57],
                }],
            )
            .expect("test renderer should succeed");
        let bytes = output.lock().expect("test writer should remain available");
        let save = bytes
            .windows(2)
            .position(|window| window == b"\x1b7")
            .expect("cursor save sequence");
        let restore = bytes
            .windows(2)
            .rposition(|window| window == b"\x1b8")
            .expect("cursor restore sequence");
        assert!(restore > save);
        assert!(
            bytes[save + 2..restore]
                .windows(2)
                .any(|window| window == b"\x1b[")
        );
        drop(bytes);
        renderer.active = "square";
    }
}
