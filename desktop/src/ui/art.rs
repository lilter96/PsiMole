use crate::runtime::Phase;
use eframe::egui::{self, Color32, Context, Pos2, Rect, Vec2, pos2, vec2};
use psimole::tunnel::REGIONS;
use std::time::Duration;

const MOLE: &[u8] = include_bytes!("../../assets/mole.png");
const FLAGS: &[u8] = include_bytes!("../../assets/flags.png");

pub(super) struct Art {
    mole: egui::TextureHandle,
    flags: egui::TextureHandle,
}

impl Art {
    pub(super) fn new(ctx: &Context) -> Self {
        Self {
            mole: texture(ctx, "Mole scene", MOLE),
            flags: texture(ctx, "Country flags", FLAGS),
        }
    }

    pub(super) fn hero(
        &self,
        ui: &egui::Ui,
        rect: Rect,
        phase: Phase,
        country: Option<&str>,
        animate: bool,
    ) {
        // Resting poses from the GNOME Cairo scene, with the digging ring in its own tile.
        let frame = match phase {
            Phase::Off => 0.0,
            Phase::Preparing | Phase::Connecting | Phase::Reconnecting => 1.0,
            Phase::Connected => 2.0,
            Phase::Failed => 3.0,
        };
        if frame == 1.0 {
            let mut ring = egui::Mesh::with_texture(self.mole.id());
            ring.add_rect_with_uv(rect, pose_uv(4.0), Color32::WHITE);
            if animate && ui.is_rect_visible(rect) {
                let time = ui.input(|input| input.time);
                let angle = (time * 4.4 % std::f64::consts::TAU) as f32;
                ring.rotate(egui::emath::Rot2::from_angle(angle), rect.center());
                ui.ctx().request_repaint_after(Duration::from_millis(33));
            }
            ui.painter().add(ring);
        }
        ui.painter()
            .image(self.mole.id(), rect, pose_uv(frame), Color32::WHITE);
        if let Some(flag) = country.and_then(flag_origin) {
            // Same coordinates as the flag layer in the GNOME hill, whose view is 248×248.
            let scale = rect.width() / 248.0;
            let flag_rect = Rect::from_min_size(
                rect.center() + vec2(56.0, -24.0) * scale,
                vec2(42.0, 62.0) * scale,
            );
            ui.painter().image(
                self.flags.id(),
                flag_rect,
                flag_uv(flag, vec2(84.0, 124.0)),
                Color32::WHITE,
            );
        }
    }

    pub(super) fn flag(&self, ui: &egui::Ui, center: Pos2, country: &str) {
        if let Some(flag) = flag_origin(country) {
            ui.painter().image(
                self.flags.id(),
                Rect::from_center_size(center, vec2(27.0, 13.0)),
                flag_uv(flag + vec2(0.0, 124.0), vec2(84.0, 40.0)),
                Color32::WHITE,
            );
        }
    }
}

fn texture(ctx: &Context, name: &'static str, bytes: &[u8]) -> egui::TextureHandle {
    let image = image::load_from_memory_with_format(bytes, image::ImageFormat::Png)
        .expect("embedded PNG asset")
        .into_rgba8();
    let (width, height) = image.dimensions();
    ctx.load_texture(
        name,
        egui::ColorImage::from_rgba_unmultiplied([width as usize, height as usize], image.as_raw()),
        egui::TextureOptions::LINEAR,
    )
}

fn pose_uv(frame: f32) -> Rect {
    Rect::from_min_max(pos2(frame / 5.0, 0.0), pos2((frame + 1.0) / 5.0, 1.0))
}

fn flag_origin(country: &str) -> Option<Pos2> {
    let index = REGIONS.iter().position(|&code| code == country)?;
    Some(pos2((index % 9) as f32 * 84.0, (index / 9) as f32 * 164.0))
}

fn flag_uv(origin: Pos2, size: Vec2) -> Rect {
    let uv = |point: Pos2| pos2(point.x / 756.0, point.y / 492.0);
    Rect::from_min_max(uv(origin), uv(origin + size))
}
