//! One-shot effects drawn over the rendered frame and dropped when they
//! finish.

use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Style};
use tachyonfx::{Effect, EffectTimer, Interpolation, RefCount, fx, ref_count};

use crate::server::config::AttachStyle;
use crate::server::layout::WindowId;
use crate::server::palette::{self, Palette, TermColors};

const DIM_FADE: (u32, Interpolation) = (300, Interpolation::QuadOut);
const ZOOM: (u32, Interpolation) = (200, Interpolation::QuadOut);
const MATERIALIZE: (u32, Interpolation) = (400, Interpolation::QuadOut);
const RAIN: (u32, Interpolation) = (700, Interpolation::Linear);
/// The share of the rain's run a column may wait before its drop starts.
const RAIN_STAGGER: f32 = 0.4;
/// The shares of the rain's run the fastest and slowest drops fall for.
const RAIN_FALL: (f32, f32) = (0.3, 0.6);
/// Rows dimmed behind each drop.
const RAIN_TRAIL: u16 = 4;

/// A buffer a transition draws a window from: live for a window growing,
/// a snapshot for one shrinking.
pub type Frame = RefCount<Buffer>;

pub struct Zoom {
    pub window: WindowId,
    from: Rect,
    to: Rect,
    frame: Frame,
    pub live: bool,
    effect: Effect,
}

impl Zoom {
    pub fn rect(&self) -> Rect {
        let alpha = self.effect.timer().map_or(1.0, |t| t.alpha());
        lerp(self.from, self.to, alpha)
    }
}

#[derive(Default)]
pub struct Transitions {
    dims: Vec<(WindowId, Effect)>,
    zoom: Option<Zoom>,
    materialize: Option<Effect>,
    last: Option<Instant>,
}

impl Transitions {
    pub fn running(&self) -> bool {
        !self.dims.is_empty() || self.zoom.is_some() || self.materialize.is_some()
    }

    pub fn tick(&mut self, now: Instant) {
        let delta = self
            .last
            .map_or(Duration::ZERO, |last| now.saturating_duration_since(last));
        self.last = Some(now);
        for effect in self.effects_mut() {
            if let Some(timer) = effect.timer_mut() {
                timer.process(delta);
            }
        }
    }

    pub fn prune(&mut self) -> bool {
        let before = self.count();
        self.dims.retain(|(_, e)| !e.done());
        if self.zoom.as_ref().is_some_and(|z| z.effect.done()) {
            self.zoom = None;
        }
        if self.materialize.as_ref().is_some_and(|e| e.done()) {
            self.materialize = None;
        }
        if !self.running() {
            self.last = None;
        }
        self.count() != before
    }

    fn count(&self) -> usize {
        self.dims.len() + self.zoom.iter().count() + self.materialize.iter().count()
    }

    fn effects_mut(&mut self) -> impl Iterator<Item = &mut Effect> {
        self.dims
            .iter_mut()
            .map(|(_, e)| e)
            .chain(self.zoom.iter_mut().map(|z| &mut z.effect))
            .chain(self.materialize.iter_mut())
    }

    /// The clock stops between transitions, so a new one never starts with
    /// a stale delta.
    fn start(&mut self) {
        if !self.running() {
            self.last = Some(Instant::now());
        }
    }

    pub fn dim(&mut self, window: WindowId, palette: Palette, colors: TermColors) {
        self.start();
        self.undim(window);
        let effect = fx::effect_fn_buf((), timer(DIM_FADE), move |_, ctx, buf| {
            let factor = 1.0 - (1.0 - palette::DIM) * ctx.alpha();
            palette::shade(buf, ctx.area, &palette, &colors, factor);
        });
        self.dims.push((window, effect));
    }

    pub fn undim(&mut self, window: WindowId) {
        self.dims.retain(|(id, _)| *id != window);
    }

    pub fn dim_mut(&mut self, window: WindowId) -> Option<&mut Effect> {
        self.dims
            .iter_mut()
            .find(|(id, _)| *id == window)
            .map(|(_, e)| e)
    }

    pub fn zoom(&mut self, window: WindowId, from: Rect, to: Rect, snapshot: Option<Buffer>) {
        self.start();
        let live = snapshot.is_none();
        let frame = ref_count(snapshot.unwrap_or_else(|| Buffer::empty(to)));
        let effect = {
            let frame = frame.clone();
            fx::effect_fn_buf((), timer(ZOOM), move |_, ctx, buf| {
                let frame = frame.borrow();
                let rect = lerp(from, to, ctx.alpha());
                let anchor = frame.area;
                blit(
                    &frame,
                    buf,
                    rect,
                    i32::from(rect.x) - i32::from(anchor.x),
                    i32::from(rect.y) - i32::from(anchor.y),
                );
            })
        };
        self.zoom = Some(Zoom {
            window,
            from,
            to,
            frame,
            live,
            effect,
        });
    }

    pub fn zoom_state(&self) -> Option<&Zoom> {
        self.zoom.as_ref()
    }

    /// The buffer `window` renders into instead of the screen while a
    /// transition draws it from there.
    pub fn live(&self, window: WindowId) -> Option<Frame> {
        self.zoom
            .as_ref()
            .filter(|z| z.window == window && z.live)
            .map(|z| z.frame.clone())
    }

    pub fn forget(&mut self, window: WindowId) {
        self.undim(window);
        if self.zoom.as_ref().is_some_and(|z| z.window == window) {
            self.zoom = None;
        }
    }

    pub fn overlay(&mut self, buf: &mut Buffer) {
        if let Some(zoom) = &mut self.zoom {
            let area = buf.area;
            zoom.effect.process(Duration::ZERO, buf, area);
        }
    }

    pub fn materialize(&mut self, style: AttachStyle, palette: Palette, colors: TermColors) {
        self.start();
        self.materialize = Some(match style {
            AttachStyle::Coalesce => fx::coalesce_from(Style::reset(), timer(MATERIALIZE)),
            AttachStyle::Rain => rain(palette, colors),
        });
    }

    pub fn materializing(&self) -> bool {
        self.materialize.is_some()
    }

    /// Runs last, over the finished frame, chrome included.
    pub fn reveal(&mut self, buf: &mut Buffer) {
        if let Some(effect) = &mut self.materialize {
            let area = buf.area;
            effect.process(Duration::ZERO, buf, area);
        }
    }
}

fn timer((ms, interpolation): (u32, Interpolation)) -> EffectTimer {
    EffectTimer::from_ms(ms, interpolation)
}

/// Each column's drop starts after its own delay and falls at its own
/// speed, blanking the cells it hasn't reached yet and dimming a trail
/// behind it. The drop runs past the bottom so the trail leaves the frame.
fn rain(palette: Palette, colors: TermColors) -> Effect {
    fx::effect_fn_buf((), timer(RAIN), move |_, ctx, buf| {
        let area = ctx.area;
        let run = f32::from(area.height + RAIN_TRAIL);
        let (fastest, slowest) = RAIN_FALL;
        for x in area.left()..area.right() {
            let delay = RAIN_STAGGER * column_noise(x, 0);
            let fall = fastest + (slowest - fastest) * column_noise(x, 1);
            let progress = ((ctx.alpha() - delay) / fall).clamp(0.0, 1.0);
            let head = area.top() + (run * progress).round() as u16;
            for y in head..area.bottom() {
                if let Some(cell) = buf.cell_mut(Position::new(x, y)) {
                    cell.reset();
                }
            }
            let trail = head.saturating_sub(RAIN_TRAIL).max(area.top());
            for y in trail..head.min(area.bottom()) {
                let factor = f32::from(head - y) / f32::from(RAIN_TRAIL + 1);
                dim_cell(buf, Position::new(x, y), &palette, &colors, factor);
            }
        }
    })
}

/// A default background stays as it is, matching the blank cells below.
fn dim_cell(buf: &mut Buffer, pos: Position, palette: &Palette, colors: &TermColors, factor: f32) {
    let default_bg = buf.cell(pos).is_some_and(|cell| cell.bg == Color::Reset);
    palette::shade(buf, Rect::new(pos.x, pos.y, 1, 1), palette, colors, factor);
    if default_bg && let Some(cell) = buf.cell_mut(pos) {
        cell.bg = Color::Reset;
    }
}

/// In `0..1`, spread so neighboring columns land far apart, and unrelated
/// from one `salt` to the next.
fn column_noise(x: u16, salt: u32) -> f32 {
    let mut hash = (u32::from(x) ^ (salt << 16)).wrapping_mul(0x9E37_79B1);
    hash ^= hash >> 15;
    hash = hash.wrapping_mul(0x85EB_CA6B);
    hash ^= hash >> 13;
    (hash >> 8) as f32 / (1u32 << 24) as f32
}

fn blit(src: &Buffer, buf: &mut Buffer, within: Rect, dx: i32, dy: i32) {
    for pos in src.area.positions() {
        let (x, y) = (i32::from(pos.x) + dx, i32::from(pos.y) + dy);
        let Ok(to) = u16::try_from(x).and_then(|x| u16::try_from(y).map(|y| Position::new(x, y)))
        else {
            continue;
        };
        if !within.contains(to) {
            continue;
        }
        if let Some(cell) = buf.cell_mut(to) {
            *cell = src[pos].clone();
        }
    }
}

fn lerp(from: Rect, to: Rect, t: f32) -> Rect {
    let mix = |a: u16, b: u16| (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as u16;
    Rect::new(
        mix(from.x, to.x),
        mix(from.y, to.y),
        mix(from.width, to.width),
        mix(from.height, to.height),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filled(area: Rect, ch: char) -> Buffer {
        let mut buf = Buffer::empty(area);
        for pos in area.positions() {
            buf[pos].set_char(ch);
        }
        buf
    }

    fn row(buf: &Buffer, y: u16) -> String {
        (buf.area.left()..buf.area.right())
            .map(|x| buf[Position::new(x, y)].symbol().chars().next().unwrap())
            .collect()
    }

    fn materialize(t: &mut Transitions, style: AttachStyle) {
        t.materialize(style, Palette::DEFAULT, TermColors::default());
    }

    /// Rows shown from the top of each column, asserting nothing shows
    /// below them.
    fn heads(buf: &Buffer) -> Vec<u16> {
        let area = buf.area;
        (area.left()..area.right())
            .map(|x| {
                let shown = |y: u16| buf[Position::new(x, y)].symbol() == "x";
                let head = (area.top()..area.bottom())
                    .take_while(|&y| shown(y))
                    .count() as u16;
                assert!(
                    (head..area.bottom()).all(|y| !shown(y)),
                    "column {x} fills from the top down"
                );
                head
            })
            .collect()
    }

    fn advance(t: &mut Transitions, ms: u64) {
        let last = t.last.expect("running");
        t.tick(last + Duration::from_millis(ms));
    }

    #[test]
    fn a_zoom_grows_from_its_place_anchored_at_its_corner() {
        let screen = Rect::new(0, 0, 6, 4);
        let mut t = Transitions::default();
        t.zoom(1, Rect::new(3, 2, 3, 2), screen, None);
        let live = t.live(1).unwrap();
        let mut frame = Buffer::empty(screen);
        for pos in screen.positions() {
            frame[pos].set_char(char::from(b'0' + pos.y as u8));
        }
        *live.borrow_mut() = frame;
        let mut buf = filled(screen, '.');
        t.overlay(&mut buf);
        assert_eq!(row(&buf, 2), "...000", "the top row moves with the rect");
        assert_eq!(row(&buf, 3), "...111");
        assert_eq!(row(&buf, 0), "......");
        advance(&mut t, 500);
        let mut buf = filled(screen, '.');
        t.overlay(&mut buf);
        assert_eq!(row(&buf, 0), "000000");
        assert_eq!(row(&buf, 3), "333333");
    }

    #[test]
    fn a_zoom_shrinks_showing_its_snapshot() {
        let screen = Rect::new(0, 0, 6, 4);
        let mut t = Transitions::default();
        t.zoom(1, screen, Rect::new(3, 2, 3, 2), Some(filled(screen, 's')));
        assert!(t.live(1).is_none());
        advance(&mut t, 500);
        let mut buf = filled(screen, '.');
        t.overlay(&mut buf);
        assert_eq!(row(&buf, 0), "......");
        assert_eq!(row(&buf, 2), "...sss");
        assert_eq!(row(&buf, 3), "...sss");
    }

    #[test]
    fn the_dim_fade_ends_at_the_steady_shade() {
        let rect = Rect::new(0, 0, 2, 1);
        let mut t = Transitions::default();
        let colors = TermColors::default();
        t.dim(1, Palette::DEFAULT, colors);
        let mut buf = Buffer::empty(rect);
        buf[Position::new(0, 0)].fg = Color::Rgb(100, 100, 100);
        t.dim_mut(1)
            .unwrap()
            .process(Duration::ZERO, &mut buf, rect);
        assert_eq!(buf[Position::new(0, 0)].fg, Color::Rgb(100, 100, 100));
        advance(&mut t, 1000);
        let mut buf = Buffer::empty(rect);
        buf[Position::new(0, 0)].fg = Color::Rgb(100, 100, 100);
        t.dim_mut(1)
            .unwrap()
            .process(Duration::ZERO, &mut buf, rect);
        let mut steady = Buffer::empty(rect);
        steady[Position::new(0, 0)].fg = Color::Rgb(100, 100, 100);
        palette::shade(&mut steady, rect, &Palette::DEFAULT, &colors, palette::DIM);
        assert_eq!(buf, steady);
        t.undim(1);
        assert!(!t.running());
    }

    #[test]
    fn an_attaching_frame_materializes_cell_by_cell() {
        let screen = Rect::new(0, 0, 8, 4);
        let mut t = Transitions::default();
        materialize(&mut t, AttachStyle::Coalesce);
        let mut buf = filled(screen, 'x');
        buf[Position::new(0, 0)].bg = Color::Red;
        t.reveal(&mut buf);
        assert!(
            screen
                .positions()
                .all(|p| buf[p].symbol() == " " && buf[p].bg == Color::Reset),
            "starts blank, backgrounds included"
        );
        advance(&mut t, 200);
        let mut buf = filled(screen, 'x');
        t.reveal(&mut buf);
        let shown = screen
            .positions()
            .filter(|&p| buf[p].symbol() == "x")
            .count();
        assert!(shown > 0 && shown < 32, "part way through: {shown} shown");
        advance(&mut t, 300);
        let mut buf = filled(screen, 'x');
        t.reveal(&mut buf);
        assert!(screen.positions().all(|p| buf[p].symbol() == "x"));
        assert!(t.prune());
        assert!(!t.running());
    }

    #[test]
    fn an_attaching_frame_rains_in_column_by_column() {
        let screen = Rect::new(0, 0, 8, 20);
        let mut t = Transitions::default();
        materialize(&mut t, AttachStyle::Rain);
        let bg = |x: u16| {
            if x.is_multiple_of(2) {
                Color::Red
            } else {
                Color::Reset
            }
        };
        let painted = || {
            let mut buf = filled(screen, 'x');
            for pos in screen.positions() {
                buf[pos].fg = Color::Green;
                buf[pos].bg = bg(pos.x);
            }
            buf
        };
        let mut buf = painted();
        t.reveal(&mut buf);
        assert!(
            screen
                .positions()
                .all(|p| buf[p].symbol() == " " && buf[p].bg == Color::Reset),
            "starts blank, backgrounds included"
        );
        advance(&mut t, 350);
        let mut buf = painted();
        t.reveal(&mut buf);
        let heads = heads(&buf);
        assert!(
            heads.iter().any(|&h| h > 0) && heads.iter().any(|&h| h < screen.height),
            "part way through: {heads:?}"
        );
        assert!(heads.windows(2).any(|w| w[0] != w[1]), "columns stagger");
        let falling = (screen.left()..screen.right())
            .zip(&heads)
            .filter(|&(_, &head)| head > RAIN_TRAIL && head < screen.height);
        let none = TermColors::default();
        for (x, &head) in falling.clone() {
            for y in screen.top()..head {
                let cell = &buf[Position::new(x, y)];
                let behind = head - y;
                if behind > RAIN_TRAIL {
                    assert_eq!(
                        (cell.fg, cell.bg),
                        (Color::Green, bg(x)),
                        "landed at {x},{y}"
                    );
                    continue;
                }
                let factor = f32::from(behind) / f32::from(RAIN_TRAIL + 1);
                let dimmed = |color| palette::darken(color, Color::Reset, &none, factor);
                assert_eq!(cell.fg, dimmed(Color::Green), "trail at {x},{y}");
                let trail_bg = if bg(x) == Color::Reset {
                    Color::Reset
                } else {
                    dimmed(bg(x))
                };
                assert_eq!(cell.bg, trail_bg, "trail background at {x},{y}");
            }
        }
        assert!(falling.count() > 1, "drops mid-fall: {heads:?}");
        advance(&mut t, 350);
        let mut buf = painted();
        t.reveal(&mut buf);
        assert_eq!(buf, painted());
        assert!(t.prune());
        assert!(!t.running());
    }

    #[test]
    fn rain_columns_fall_at_different_speeds() {
        let screen = Rect::new(0, 0, 40, 40);
        let mut t = Transitions::default();
        materialize(&mut t, AttachStyle::Rain);
        let mut at = |ms: u64| {
            advance(&mut t, ms);
            let mut buf = filled(screen, 'x');
            t.reveal(&mut buf);
            heads(&buf)
        };
        let (before, after) = (at(300), at(100));
        let fallen: Vec<u16> = before
            .iter()
            .zip(&after)
            .filter(|&(&a, &b)| a > 0 && b < screen.height)
            .map(|(&a, &b)| b - a)
            .collect();
        assert!(fallen.len() > 1, "drops mid-fall: {fallen:?}");
        assert!(
            fallen.iter().max() > fallen.iter().min(),
            "rows fallen in the same time: {fallen:?}"
        );
    }

    #[test]
    fn forgetting_a_window_drops_everything_pinned_to_it() {
        let mut t = Transitions::default();
        t.dim(1, Palette::DEFAULT, TermColors::default());
        t.zoom(1, Rect::new(0, 0, 1, 1), Rect::new(0, 0, 2, 2), None);
        t.forget(1);
        assert!(t.dim_mut(1).is_none() && t.live(1).is_none() && t.zoom_state().is_none());
        assert!(!t.running());
    }
}
