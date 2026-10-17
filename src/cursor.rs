use std::cell::RefCell;
use std::collections::HashMap;
use std::env;
use std::fs::File;
use std::io::Read;
use std::rc::Rc;
use std::time::Duration;

use anyhow::{anyhow, Context};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::backend::renderer::utils::CommitCounter;
use smithay::input::pointer::{CursorIcon, CursorImageStatus, CursorImageSurfaceData};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{IsAlive, Logical, Physical, Point, Transform};
use smithay::wayland::compositor::with_states;
use xcursor::parser::{parse_xcursor, Image};
use xcursor::CursorTheme;

use crate::animation::{Animation, Clock, Curve};

/// Some default looking `left_ptr` icon.
static FALLBACK_CURSOR_DATA: &[u8] = include_bytes!("../resources/cursor.rgba");

type XCursorCache = HashMap<(CursorIcon, i32), Option<Rc<XCursor>>>;

pub struct CursorManager {
    theme: CursorTheme,
    size: u8,
    current_cursor: CursorImageStatus,
    named_cursor_cache: RefCell<XCursorCache>,
}

impl CursorManager {
    pub fn new(theme: &str, size: u8) -> Self {
        Self::ensure_env(theme, size);

        let theme = CursorTheme::load(theme);

        Self {
            theme,
            size,
            current_cursor: CursorImageStatus::default_named(),
            named_cursor_cache: Default::default(),
        }
    }

    /// Reload the cursor theme.
    pub fn reload(&mut self, theme: &str, size: u8) {
        Self::ensure_env(theme, size);
        self.theme = CursorTheme::load(theme);
        self.size = size;
        self.named_cursor_cache.get_mut().clear();
    }

    /// Checks if the cursor WlSurface is alive, and if not, cleans it up.
    pub fn check_cursor_image_surface_alive(&mut self) {
        if let CursorImageStatus::Surface(surface) = &self.current_cursor {
            if !surface.alive() {
                self.current_cursor = CursorImageStatus::default_named();
            }
        }
    }

    /// Get the current rendering cursor.
    pub fn get_render_cursor(&self, scale: i32) -> RenderCursor {
        match self.current_cursor.clone() {
            CursorImageStatus::Hidden => RenderCursor::Hidden,
            CursorImageStatus::Surface(surface) => {
                let hotspot = with_states(&surface, |states| {
                    states
                        .data_map
                        .get::<CursorImageSurfaceData>()
                        .unwrap()
                        .lock()
                        .unwrap()
                        .hotspot
                });

                RenderCursor::Surface { hotspot, surface }
            }
            CursorImageStatus::Named(icon) => self.get_render_cursor_named(icon, scale),
        }
    }

    fn get_render_cursor_named(&self, icon: CursorIcon, scale: i32) -> RenderCursor {
        self.get_cursor_with_name(icon, scale)
            .map(|cursor| RenderCursor::Named {
                icon,
                scale,
                cursor,
            })
            .unwrap_or_else(|| RenderCursor::Named {
                icon: Default::default(),
                scale,
                cursor: self.get_default_cursor(scale),
            })
    }

    pub fn is_current_cursor_animated(&self, scale: i32) -> bool {
        match &self.current_cursor {
            CursorImageStatus::Hidden => false,
            CursorImageStatus::Surface(_) => false,
            CursorImageStatus::Named(icon) => self
                .get_cursor_with_name(*icon, scale)
                .unwrap_or_else(|| self.get_default_cursor(scale))
                .is_animated_cursor(),
        }
    }

    /// Get named cursor for the given `icon` and `scale`.
    pub fn get_cursor_with_name(&self, icon: CursorIcon, scale: i32) -> Option<Rc<XCursor>> {
        self.named_cursor_cache
            .borrow_mut()
            .entry((icon, scale))
            .or_insert_with_key(|(icon, scale)| {
                let size = self.size as i32 * scale;
                let mut cursor = Self::load_xcursor(&self.theme, icon.name(), size);

                // Check alternative names to account for non-compliant themes.
                if cursor.is_err() {
                    for name in icon.alt_names() {
                        cursor = Self::load_xcursor(&self.theme, name, size);
                        if cursor.is_ok() {
                            break;
                        }
                    }
                }

                if let Err(err) = &cursor {
                    warn!("error loading xcursor {}@{size}: {err:?}", icon.name());
                }

                // The default cursor must always have a fallback.
                if *icon == CursorIcon::Default && cursor.is_err() {
                    cursor = Ok(Self::fallback_cursor());
                }

                cursor.ok().map(Rc::new)
            })
            .clone()
    }

    /// Get default cursor.
    pub fn get_default_cursor(&self, scale: i32) -> Rc<XCursor> {
        // The default cursor always has a fallback.
        self.get_cursor_with_name(CursorIcon::Default, scale)
            .unwrap()
    }

    /// Currently used cursor_image as a cursor provider.
    pub fn cursor_image(&self) -> &CursorImageStatus {
        &self.current_cursor
    }

    /// Set new cursor image provider.
    pub fn set_cursor_image(&mut self, cursor: CursorImageStatus) {
        self.current_cursor = cursor;
    }

    /// Load the cursor with the given `name` from the file system picking the closest
    /// one to the given `size`.
    fn load_xcursor(theme: &CursorTheme, name: &str, size: i32) -> anyhow::Result<XCursor> {
        let _span = tracy_client::span!("load_xcursor");

        let path = theme
            .load_icon(name)
            .ok_or_else(|| anyhow!("no default icon"))?;

        let mut file = File::open(path).context("error opening cursor icon file")?;
        let mut buf = vec![];
        file.read_to_end(&mut buf)
            .context("error reading cursor icon file")?;

        let mut images = parse_xcursor(&buf).context("error parsing cursor icon file")?;

        let (width, height) = images
            .iter()
            .min_by_key(|image| (size - image.size as i32).abs())
            .map(|image| (image.width, image.height))
            .unwrap();

        images.retain(move |image| image.width == width && image.height == height);

        let animation_duration = images.iter().fold(0, |acc, image| acc + image.delay);

        Ok(XCursor {
            images,
            animation_duration,
        })
    }

    /// Set the common XCURSOR env variables.
    fn ensure_env(theme: &str, size: u8) {
        env::set_var("XCURSOR_THEME", theme);
        env::set_var("XCURSOR_SIZE", size.to_string());
    }

    fn fallback_cursor() -> XCursor {
        let images = vec![Image {
            size: 32,
            width: 64,
            height: 64,
            xhot: 1,
            yhot: 1,
            delay: 0,
            pixels_rgba: Vec::from(FALLBACK_CURSOR_DATA),
            pixels_argb: vec![],
        }];

        XCursor {
            images,
            animation_duration: 0,
        }
    }
}

/// The cursor prepared for renderer.
pub enum RenderCursor {
    Hidden,
    Surface {
        hotspot: Point<i32, Logical>,
        surface: WlSurface,
    },
    Named {
        icon: CursorIcon,
        scale: i32,
        cursor: Rc<XCursor>,
    },
}

type TextureCache = HashMap<(CursorIcon, i32), Vec<MemoryRenderBuffer>>;

#[derive(Default)]
pub struct CursorTextureCache {
    cache: RefCell<TextureCache>,
}

impl CursorTextureCache {
    pub fn clear(&mut self) {
        self.cache.get_mut().clear();
    }

    pub fn get(
        &self,
        icon: CursorIcon,
        scale: i32,
        cursor: &XCursor,
        idx: usize,
    ) -> MemoryRenderBuffer {
        self.cache
            .borrow_mut()
            .entry((icon, scale))
            .or_insert_with(|| {
                cursor
                    .frames()
                    .iter()
                    .map(|frame| {
                        MemoryRenderBuffer::from_slice(
                            &frame.pixels_rgba,
                            Fourcc::Argb8888,
                            (frame.width as i32, frame.height as i32),
                            scale,
                            Transform::Normal,
                            None,
                        )
                    })
                    .collect()
            })[idx]
            .clone()
    }
}

/// Tilt settling threshold, in degrees.
const TILT_SETTLE_DEG: f64 = 0.125;
/// Largest tilt, in degrees.
const TILT_MAX_DEG: f64 = 22.;
/// Horizontal speed, in logical px/s, at which the tilt reaches ~76% of the maximum.
const TILT_SPEED: f64 = 800.;
/// Time constant of the tilt smoothing, in seconds.
const TILT_TAU: f64 = 0.08;
/// Duration of a programmatic pointer glide, in milliseconds.
const GLIDE_DURATION_MS: u64 = 350;
/// Gaps between updates longer than this, in seconds, settle the simulation at rest.
const MAX_GAP: f64 = 0.25;

/// Smoothed tilt following horizontal velocity and eased glides across pointer warps.
#[derive(Default)]
pub struct CursorPhysics {
    /// Last update time and pointer position, moved along with warps since.
    last: Option<(Duration, Point<f64, Logical>)>,
    angle: f64,
    /// Drawn position relative to the pointer.
    offset: Point<f64, Logical>,
    /// Initial offset and remaining fraction of the current glide.
    glide: Option<(Point<f64, Logical>, Animation)>,
    /// Incremented whenever the angle changes.
    commit: CommitCounter,
}

impl CursorPhysics {
    /// Keeps the cursor drawn where it is while the pointer warps from `from` to `to`.
    pub fn warp(&mut self, time: Duration, from: Point<f64, Logical>, to: Point<f64, Logical>) {
        self.jump(time, from, to);
        if from == to {
            return;
        }
        self.offset += from - to;
        self.glide = Some((
            self.offset,
            Animation::ease(
                Clock::with_time(self.last.unwrap().0),
                1.,
                0.,
                0.,
                GLIDE_DURATION_MS,
                Curve::from(niri_config::animations::Curve::CubicBezier(
                    0.42, 0., 0.58, 1.,
                )),
            ),
        ));
    }

    /// Moves the drawn cursor along with a pointer jump from `from` to `to` that isn't motion.
    pub fn jump(&mut self, time: Duration, from: Point<f64, Logical>, to: Point<f64, Logical>) {
        self.update(time, from);
        if let Some((_, pos)) = &mut self.last {
            *pos += to - from;
        }
    }

    /// Advances the simulation to `time` with the pointer at `pos`.
    pub fn update(&mut self, time: Duration, pos: Point<f64, Logical>) {
        let Some((last_time, last_pos)) = self.last else {
            self.last = Some((time, pos));
            return;
        };
        if time <= last_time {
            return;
        }
        let dt = (time - last_time).as_secs_f64();
        if dt > MAX_GAP {
            self.settle(Some((time, pos)));
            return;
        }
        let last_drawn = last_pos + self.offset;
        if let Some((from, animation)) = &self.glide {
            let remaining = animation.value_at(time);
            self.offset = from.upscale(remaining);
            if remaining == 0. {
                self.glide = None;
            }
        }

        let drawn = pos + self.offset;
        self.last = Some((time, pos));
        let target = TILT_MAX_DEG * ((drawn - last_drawn).x / dt / TILT_SPEED).tanh();
        let angle = self.angle + (target - self.angle) * (1. - (-dt / TILT_TAU).exp());
        if angle != self.angle {
            self.angle = angle;
            self.commit.increment();
        }

        if !self.is_active() {
            self.settle(self.last);
        }
    }

    /// Drops any tilt and glide.
    pub fn reset(&mut self) {
        self.settle(None);
    }

    fn settle(&mut self, last: Option<(Duration, Point<f64, Logical>)>) {
        if self.angle != 0. {
            self.commit.increment();
        }
        *self = Self {
            last,
            commit: self.commit,
            ..Self::default()
        };
    }

    /// Current clockwise tilt in degrees.
    pub fn angle(&self) -> f64 {
        self.angle
    }

    /// Drawn cursor position relative to the pointer.
    pub fn offset(&self) -> Point<f64, Logical> {
        self.offset
    }

    /// Commit counter tracking angle changes for damage.
    pub fn commit(&self) -> CommitCounter {
        self.commit
    }

    pub fn is_active(&self) -> bool {
        self.angle.abs() > TILT_SETTLE_DEG || self.glide.is_some()
    }
}

// The XCursorBuffer implementation is inspired by `wayland-rs`, thus provided under MIT license.

/// The state of the `NamedCursor`.
pub struct XCursor {
    /// The image for the underlying named cursor.
    images: Vec<Image>,
    /// The total duration of the animation.
    animation_duration: u32,
}

impl XCursor {
    /// Given a time, calculate which frame to show, and how much time remains until the next frame.
    ///
    /// Time will wrap, so if for instance the cursor has an animation lasting 100ms,
    /// then calling this function with 5ms and 105ms as input gives the same output.
    pub fn frame(&self, mut millis: u32) -> (usize, &Image) {
        if self.animation_duration == 0 {
            return (0, &self.images[0]);
        }

        millis %= self.animation_duration;

        let mut res = 0;
        for (i, img) in self.images.iter().enumerate() {
            if millis < img.delay {
                res = i;
                break;
            }
            millis -= img.delay;
        }

        (res, &self.images[res])
    }

    /// Get the frames for the given `XCursor`.
    pub fn frames(&self) -> &[Image] {
        &self.images
    }

    /// Check whether the cursor is animated.
    pub fn is_animated_cursor(&self) -> bool {
        self.images.len() > 1
    }

    /// Get hotspot for the given `image`.
    pub fn hotspot(image: &Image) -> Point<i32, Physical> {
        (image.xhot as i32, image.yhot as i32).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilt_follows_motion_and_settles() {
        let mut physics = CursorPhysics::default();
        let mut time = Duration::ZERO;
        let mut pos = Point::from((0., 0.));
        physics.update(time, pos);
        for _ in 0..30 {
            time += Duration::from_millis(16);
            pos.x += 20.;
            physics.update(time, pos);
        }
        assert!(physics.angle() > 5.);
        for _ in 0..200 {
            time += Duration::from_millis(16);
            physics.update(time, pos);
        }
        assert!(!physics.is_active());
        assert_eq!(physics.angle(), 0.);
    }

    #[test]
    fn warp_glides_to_pointer() {
        let mut physics = CursorPhysics::default();
        let mut time = Duration::ZERO;
        let (from, to) = (Point::from((0., 0.)), Point::from((1000., 0.)));
        physics.update(time, from);
        physics.warp(time, from, to);
        assert_eq!(physics.offset(), Point::from((-1000., 0.)));
        let mut last = from.x;
        let mut speed = 0.;
        for i in 0..20 {
            time = Duration::from_millis(GLIDE_DURATION_MS) * (i + 1) / 20;
            physics.update(time, to);
            let drawn = (to + physics.offset()).x;
            assert!(drawn > last && drawn <= to.x);
            // Eases in rather than starting at full speed.
            if i < 3 {
                assert!(drawn - last > speed);
            }
            speed = drawn - last;
            last = drawn;
        }
        assert_eq!(to + physics.offset(), to);
        assert!(physics.angle() > 0.);
    }

    #[test]
    fn interrupted_warp_keeps_drawn_position() {
        let mut physics = CursorPhysics::default();
        let (from, first, second) = (
            Point::from((0., 0.)),
            Point::from((1000., 0.)),
            Point::from((-500., 300.)),
        );
        physics.update(Duration::ZERO, from);
        physics.warp(Duration::ZERO, from, first);
        let mut time = Duration::from_millis(80);
        physics.update(time, first);
        let drawn = first + physics.offset();
        physics.warp(time, first, second);
        assert_eq!(second + physics.offset(), drawn);
        for _ in 0..50 {
            time += Duration::from_millis(16);
            physics.update(time, second);
        }
        assert_eq!(physics.offset(), Point::default());
        assert!(!physics.is_active());
    }

    #[test]
    fn warp_timing_preserves_drawn_position() {
        // Idle warp and warp timestamp older than the last update.
        for (last, warp, next) in [(0, 10_000, 10_016), (16, 10, 32)] {
            let mut physics = CursorPhysics::default();
            let (from, to) = (Point::from((0., 0.)), Point::from((1000., 0.)));
            physics.update(Duration::ZERO, from);
            physics.update(Duration::from_millis(last), from);
            physics.warp(Duration::from_millis(warp), from, to);
            assert_eq!(to + physics.offset(), from);
            physics.update(Duration::from_millis(next), to);
            let drawn = to + physics.offset();
            assert!(drawn.x > from.x && drawn.x < 50., "warp at {warp}ms");
        }
    }

    #[test]
    fn jump_is_not_motion() {
        let mut physics = CursorPhysics::default();
        let (from, to) = (Point::from((0., 0.)), Point::from((300., 0.)));
        physics.update(Duration::ZERO, from);
        physics.jump(Duration::from_millis(8), from, to);
        physics.update(Duration::from_millis(16), to);
        assert!(!physics.is_active());
        assert_eq!(physics.offset(), Point::from((0., 0.)));
    }

    #[test]
    fn long_gap_settles_glide() {
        let mut physics = CursorPhysics::default();
        let (from, to) = (Point::from((0., 0.)), Point::from((1000., 0.)));
        physics.update(Duration::ZERO, from);
        physics.warp(Duration::ZERO, from, to);
        physics.update(Duration::from_secs(1), to);
        assert!(!physics.is_active());
        assert_eq!(physics.offset(), Point::from((0., 0.)));
    }
}
