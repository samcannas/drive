//! Tray icons drawn in code: a monochrome cloud glyph with a small status badge.
//!
//! Drawing them here (instead of committing PNGs) keeps every state, animation frame and
//! light/dark variant consistent and lets them follow the taskbar colour at runtime.

use tauri::image::Image;

const SIZE: usize = 32;
const SAMPLES: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Glyph {
    /// Plain cloud: everything is up to date.
    UpToDate,
    /// Cloud with a sync badge; the frame (0..3) rotates the badge while transferring.
    Syncing(u8),
    Paused,
    /// Outlined cloud with a slash: no connection to uloz.to.
    Offline,
    /// Red badge with an exclamation mark.
    Error,
    /// Amber badge: the user has to sign in or confirm something.
    Attention,
    /// Outlined cloud: not set up yet, or the service isn't running.
    Inactive,
}

pub const SYNC_FRAMES: u8 = 3;

type Rgb = [f32; 3];

const RED: Rgb = [0.769, 0.169, 0.110]; // #C42B1C
const AMBER: Rgb = [1.0, 0.725, 0.0]; // #FFB900
const WHITE: Rgb = [1.0, 1.0, 1.0];
const INK: Rgb = [0.106, 0.106, 0.106]; // #1B1B1B

fn circle(p: (f32, f32), c: (f32, f32), r: f32) -> f32 {
    ((p.0 - c.0).powi(2) + (p.1 - c.1).powi(2)).sqrt() - r
}

fn rect(p: (f32, f32), c: (f32, f32), half: (f32, f32)) -> f32 {
    let dx = (p.0 - c.0).abs() - half.0;
    let dy = (p.1 - c.1).abs() - half.1;
    let outside = (dx.max(0.0).powi(2) + dy.max(0.0).powi(2)).sqrt();
    outside + dx.max(dy).min(0.0)
}

fn segment(p: (f32, f32), a: (f32, f32), b: (f32, f32), r: f32) -> f32 {
    let (pax, pay) = (p.0 - a.0, p.1 - a.1);
    let (bax, bay) = (b.0 - a.0, b.1 - a.1);
    let h = ((pax * bax + pay * bay) / (bax * bax + bay * bay)).clamp(0.0, 1.0);
    ((pax - bax * h).powi(2) + (pay - bay * h).powi(2)).sqrt() - r
}

fn smin(a: f32, b: f32, k: f32) -> f32 {
    let h = (0.5 + 0.5 * (b - a) / k).clamp(0.0, 1.0);
    b + (a - b) * h - k * h * (1.0 - h)
}

/// Signed distance to the cloud (negative inside), in pixels of the 32×32 canvas.
fn cloud(p: (f32, f32)) -> f32 {
    let top = circle(p, (17.0, 13.0), 7.0);
    let left = circle(p, (9.5, 18.0), 5.5);
    let right = circle(p, (23.5, 18.0), 5.0);
    let base = rect(p, (16.5, 20.75), (7.0, 2.75));
    smin(smin(smin(top, left, 1.5), right, 1.5), base, 1.0)
}

const BADGE: (f32, f32) = (24.5, 24.5);
const BADGE_R: f32 = 7.0;
const BADGE_GAP: f32 = 1.6;

/// Badge symbol distance (negative inside), or `None` for a badge-less glyph.
fn symbol(g: Glyph, p: (f32, f32)) -> Option<f32> {
    let (cx, cy) = BADGE;
    Some(match g {
        Glyph::Syncing(frame) => {
            // A ring broken by two gaps, rotated a third of a turn per frame.
            let ring = circle(p, BADGE, 3.4).abs() - 0.95;
            let rot = frame as f32 * std::f32::consts::TAU / 3.0;
            let a = ((p.1 - cy).atan2(p.0 - cx) - rot).rem_euclid(std::f32::consts::PI);
            if a < 0.75 { 1.0 } else { ring }
        }
        Glyph::Paused => {
            rect(p, (cx - 1.7, cy), (0.95, 3.2)).min(rect(p, (cx + 1.7, cy), (0.95, 3.2)))
        }
        Glyph::Error | Glyph::Attention => {
            segment(p, (cx, cy - 3.4), (cx, cy + 0.6), 1.0).min(circle(p, (cx, cy + 3.3), 1.15))
        }
        Glyph::UpToDate | Glyph::Offline | Glyph::Inactive => return None,
    })
}

/// Colour of one sample: (rgb, alpha).
fn sample(g: Glyph, ink: Rgb, p: (f32, f32)) -> (Rgb, f32) {
    let clear = ([0.0; 3], 0.0);
    if let Some(sym) = symbol(g, p) {
        let d = circle(p, BADGE, BADGE_R);
        if d <= 0.0 {
            return match g {
                Glyph::Error => (if sym <= 0.0 { WHITE } else { RED }, 1.0),
                Glyph::Attention => (if sym <= 0.0 { INK } else { AMBER }, 1.0),
                // Monochrome badge with the symbol cut out.
                _ => {
                    if sym <= 0.0 {
                        clear
                    } else {
                        (ink, 1.0)
                    }
                }
            };
        }
        if d <= BADGE_GAP {
            return clear;
        }
    }
    let d = cloud(p);
    let inside = match g {
        Glyph::Inactive => d.abs() <= 1.05,
        Glyph::Offline => {
            let slash = segment(p, (5.0, 5.5), (27.0, 27.5), 1.05);
            let gap = segment(p, (5.0, 5.5), (27.0, 27.5), 2.6);
            slash <= 0.0 || (d.abs() <= 1.05 && gap > 0.0)
        }
        _ => d <= 0.0,
    };
    if inside { (ink, 1.0) } else { clear }
}

/// Renders a 32×32 RGBA tray icon. `light_background` picks a dark glyph for light taskbars.
pub fn render(g: Glyph, light_background: bool) -> Image<'static> {
    let ink = if light_background { INK } else { WHITE };
    let mut rgba = vec![0u8; SIZE * SIZE * 4];
    let step = 1.0 / SAMPLES as f32;
    for y in 0..SIZE {
        for x in 0..SIZE {
            let mut acc = [0.0f32; 3];
            let mut alpha = 0.0f32;
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let p = (
                        x as f32 + (sx as f32 + 0.5) * step,
                        y as f32 + (sy as f32 + 0.5) * step,
                    );
                    let (c, a) = sample(g, ink, p);
                    for i in 0..3 {
                        acc[i] += c[i] * a;
                    }
                    alpha += a;
                }
            }
            let n = (SAMPLES * SAMPLES) as f32;
            let o = (y * SIZE + x) * 4;
            if alpha > 0.0 {
                for i in 0..3 {
                    rgba[o + i] = ((acc[i] / alpha) * 255.0).round() as u8;
                }
            }
            rgba[o + 3] = ((alpha / n) * 255.0).round() as u8;
        }
    }
    Image::new_owned(rgba, SIZE as u32, SIZE as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_glyph_draws_something() {
        for g in [
            Glyph::UpToDate,
            Glyph::Syncing(0),
            Glyph::Syncing(1),
            Glyph::Syncing(2),
            Glyph::Paused,
            Glyph::Offline,
            Glyph::Error,
            Glyph::Attention,
            Glyph::Inactive,
        ] {
            let img = render(g, false);
            let opaque = img.rgba().chunks(4).filter(|px| px[3] > 128).count();
            assert!(opaque > 40, "{g:?} drew only {opaque} pixels");
        }
    }

    /// `DRIVE_DUMP_TRAY_ICONS=<dir> cargo test -p drive-app dump` writes raw RGBA files for review.
    #[test]
    fn dump() {
        let Some(dir) = std::env::var_os("DRIVE_DUMP_TRAY_ICONS") else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        for (name, g) in [
            ("up_to_date", Glyph::UpToDate),
            ("syncing0", Glyph::Syncing(0)),
            ("syncing1", Glyph::Syncing(1)),
            ("syncing2", Glyph::Syncing(2)),
            ("paused", Glyph::Paused),
            ("offline", Glyph::Offline),
            ("error", Glyph::Error),
            ("attention", Glyph::Attention),
            ("inactive", Glyph::Inactive),
        ] {
            for light in [false, true] {
                let file = dir.join(format!(
                    "{name}-{}.rgba",
                    if light { "light" } else { "dark" }
                ));
                std::fs::write(file, render(g, light).rgba()).unwrap();
            }
        }
    }

    #[test]
    fn frames_differ() {
        assert_ne!(
            render(Glyph::Syncing(0), true).rgba(),
            render(Glyph::Syncing(1), true).rgba()
        );
    }
}
