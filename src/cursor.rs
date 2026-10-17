use std::cell::RefCell;
use std::collections::HashMap;
use std::env;
use std::fs::File;
use std::io::Read;
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::input::pointer::{CursorIcon, CursorImageStatus, CursorImageSurfaceData};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{IsAlive, Logical, Physical, Point, Transform};
use smithay::wayland::compositor::with_states;
use xcursor::parser::{parse_xcursor, Image};
use xcursor::CursorTheme;

/// Some default looking `left_ptr` icon.
static FALLBACK_CURSOR_DATA: &[u8] = include_bytes!("../resources/cursor.rgba");

type XCursorCache = HashMap<(CursorIcon, i32), Option<Rc<XCursor>>>;

type TransitionCache = HashMap<(CursorIcon, CursorIcon, i32), Option<TransitionAsset>>;

type TransitionAsset = (String, bool, Rc<XCursor>);

#[derive(Clone, Copy)]
struct CursorTransition {
    from: CursorIcon,
    to: CursorIcon,
    started: Instant,
    position: f64,
    reverse: bool,
    duration_ms: f64,
    /// Latest requested cursor, played once this transition finishes.
    queued: Option<CursorIcon>,
}

impl CursorTransition {
    fn position_at(&self, now: Instant) -> f64 {
        let elapsed = now.duration_since(self.started).as_secs_f64() * 1000. / self.duration_ms;
        (self.position + if self.reverse { -elapsed } else { elapsed }).clamp(0., 1.)
    }

    fn active_at(&self, now: Instant) -> bool {
        let position = self.position_at(now);
        if self.reverse {
            position > 0.
        } else {
            position < 1.
        }
    }

    fn target(&self) -> CursorIcon {
        if self.reverse {
            self.from
        } else {
            self.to
        }
    }

    fn finished_at(&self) -> Instant {
        let distance = if self.reverse {
            self.position
        } else {
            1. - self.position
        };
        self.started + Duration::from_secs_f64(distance * self.duration_ms / 1000.)
    }
}

pub struct CursorManager {
    theme: CursorTheme,
    size: u8,
    current_cursor: CursorImageStatus,
    named_cursor_cache: RefCell<XCursorCache>,
    transition_cache: RefCell<TransitionCache>,
    transition: Option<CursorTransition>,
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
            transition_cache: Default::default(),
            transition: None,
        }
    }

    /// Reload the cursor theme.
    pub fn reload(&mut self, theme: &str, size: u8) {
        Self::ensure_env(theme, size);
        self.theme = CursorTheme::load(theme);
        self.size = size;
        self.named_cursor_cache.get_mut().clear();
        self.transition_cache.get_mut().clear();
        self.transition = None;
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
        let now = Instant::now();
        let transition = self.current_transition(now);
        if let Some(transition) = &transition {
            if transition.active_at(now) {
                if let Some((name, reverse, cursor)) =
                    self.get_transition(transition.from, transition.to, scale)
                {
                    let position = transition.position_at(now);
                    let position = if reverse { 1. - position } else { position };
                    let millis = ((position * cursor.animation_duration as f64) as u32)
                        .min(cursor.animation_duration - 1);
                    let frame = Some(cursor.frame(millis).0);
                    return RenderCursor::Named {
                        name,
                        scale,
                        cursor,
                        frame,
                    };
                }
            }
        }
        self.get_cursor_with_name(icon, scale)
            .map(|cursor| RenderCursor::Named {
                name: icon.name().to_owned(),
                scale,
                frame: transition
                    .as_ref()
                    .filter(|_| cursor.images.len() > 1)
                    .map(|transition| {
                        let distance = if transition.reverse {
                            transition.position
                        } else {
                            1. - transition.position
                        };
                        let elapsed = now
                            .saturating_duration_since(transition.started)
                            .as_millis() as f64;
                        cursor
                            .frame((elapsed - distance * transition.duration_ms).max(0.) as u32)
                            .0
                    }),
                cursor,
            })
            .unwrap_or_else(|| RenderCursor::Named {
                name: CursorIcon::Default.name().to_owned(),
                scale,
                cursor: self.get_default_cursor(scale),
                frame: None,
            })
    }

    pub fn is_current_cursor_animated(&self, scale: i32) -> bool {
        let now = Instant::now();
        if self
            .current_transition(now)
            .is_some_and(|t| t.active_at(now))
        {
            return true;
        }
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
        let now = Instant::now();
        if let (CursorImageStatus::Named(from), CursorImageStatus::Named(to)) =
            (&self.current_cursor, &cursor)
        {
            if from == to {
                return;
            }
            let current = self.current_transition(now);
            if let Some(mut transition) = current.filter(|t| t.active_at(now)) {
                // Reverse from the currently displayed position, or play the latest other
                // cursor once this transition finishes.
                if *to == transition.from || *to == transition.to {
                    transition.position = transition.position_at(now);
                    transition.started = now;
                    transition.reverse = *to == transition.from;
                    transition.queued = None;
                } else {
                    transition.queued = Some(*to);
                }
                self.transition = Some(transition);
            } else if let Some((_, _, asset)) = self.get_transition(*from, *to, 1) {
                self.transition = Some(CursorTransition {
                    from: *from,
                    to: *to,
                    started: now,
                    position: 0.,
                    reverse: false,
                    duration_ms: asset.animation_duration as f64,
                    queued: None,
                });
            } else {
                self.transition = None;
            }
        } else {
            self.transition = None;
        }
        self.current_cursor = cursor;
    }

    /// The transition at `now`, with a finished one replaced by its queued successor.
    fn current_transition(&self, now: Instant) -> Option<CursorTransition> {
        let transition = self.transition?;
        let Some(queued) = transition.queued.filter(|_| !transition.active_at(now)) else {
            return Some(transition);
        };
        let from = transition.target();
        let Some((_, _, asset)) = self.get_transition(from, queued, 1) else {
            return Some(CursorTransition {
                queued: None,
                ..transition
            });
        };
        Some(CursorTransition {
            from,
            to: queued,
            started: transition.finished_at(),
            position: 0.,
            reverse: false,
            duration_ms: asset.animation_duration as f64,
            queued: None,
        })
    }

    fn get_transition(
        &self,
        from: CursorIcon,
        to: CursorIcon,
        scale: i32,
    ) -> Option<TransitionAsset> {
        self.transition_cache
            .borrow_mut()
            .entry((from, to, scale))
            .or_insert_with(|| {
                // Choose the same asset in both directions, even if both files exist.
                let (a, b) = if from.name() < to.name() {
                    (from, to)
                } else {
                    (to, from)
                };
                for (source, target) in [(a, b), (b, a)] {
                    let name = format!("{}-to-{}", source.name(), target.name());
                    if let Ok(cursor) =
                        Self::load_xcursor(&self.theme, &name, self.size as i32 * scale)
                    {
                        if cursor.images.len() > 1
                            && cursor.animation_duration > 0
                            && cursor.images.iter().all(|frame| frame.delay > 0)
                        {
                            return Some((name, source != from, Rc::new(cursor)));
                        }
                    }
                }
                None
            })
            .clone()
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

        let size = images
            .iter()
            .min_by_key(|image| (size - image.size as i32).abs())
            .map(|image| image.size)
            .ok_or_else(|| anyhow!("empty cursor"))?;

        images.retain(|image| image.size == size);

        let animation_duration = images.iter().try_fold(0u32, |acc, image| {
            acc.checked_add(image.delay)
                .ok_or_else(|| anyhow!("cursor duration overflow"))
        })?;

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
        name: String,
        scale: i32,
        cursor: Rc<XCursor>,
        frame: Option<usize>,
    },
}

type TextureCache = HashMap<(String, i32), Vec<MemoryRenderBuffer>>;

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
        name: String,
        scale: i32,
        cursor: &XCursor,
        idx: usize,
    ) -> MemoryRenderBuffer {
        self.cache
            .borrow_mut()
            .entry((name, scale))
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
    pub fn hotspot(image: &Image) -> Point<f64, Physical> {
        (image.xhot as f64, image.yhot as f64).into()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn padded_frames_preserve_hotspot_at_fractional_output_scale() {
        let mut ordinary = CursorManager::fallback_cursor().images.remove(0);
        ordinary.xhot = 23;
        ordinary.yhot = 23;
        let mut padded = ordinary.clone();
        padded.xhot = 48;
        padded.yhot = 48;
        let pointer: Point<f64, Logical> = (100., 100.).into();
        let ordinary_origin = pointer - XCursor::hotspot(&ordinary).to_logical(2.);
        let padded_origin = pointer - XCursor::hotspot(&padded).to_logical(2.);
        let padding: Point<f64, Logical> = (12.5, 12.5).into();
        assert_eq!(ordinary_origin, padded_origin + padding);
        assert_eq!(
            ordinary_origin.to_physical_precise_round::<f64, i32>(1.5),
            (padded_origin + padding).to_physical_precise_round::<f64, i32>(1.5),
        );
        assert_eq!(ordinary_origin, Point::from((88.5, 88.5)));
    }

    fn asset() -> Rc<XCursor> {
        let mut cursor = CursorManager::fallback_cursor();
        cursor.images[0].delay = 20;
        cursor.images.push(cursor.images[0].clone());
        cursor.images[1].delay = 80;
        cursor.animation_duration = 100;
        Rc::new(cursor)
    }

    fn manager() -> CursorManager {
        let manager = CursorManager {
            theme: CursorTheme::load("nonexistent-transition-test-theme"),
            size: 24,
            current_cursor: CursorImageStatus::Named(CursorIcon::Default),
            named_cursor_cache: Default::default(),
            transition_cache: Default::default(),
            transition: None,
        };
        manager.transition_cache.borrow_mut().insert(
            (CursorIcon::Default, CursorIcon::Text, 1),
            Some(("default-to-text".into(), false, asset())),
        );
        manager
    }

    #[test]
    fn transition_reverses_without_restarting() {
        let mut manager = manager();
        manager.set_cursor_image(CursorImageStatus::Named(CursorIcon::Text));
        let transition = manager.transition.as_mut().unwrap();
        transition.duration_ms = 100_000.;
        transition.started -= Duration::from_secs(40);
        manager.set_cursor_image(CursorImageStatus::Named(CursorIcon::Default));
        let transition = manager.transition.as_ref().unwrap();
        assert!(transition.reverse);
        assert!((transition.position - 0.4).abs() < 0.01);
        assert!(!transition.active_at(transition.started + Duration::from_secs(41)));
    }

    #[test]
    fn repeated_requests_keep_start_time_and_third_shape_queues() {
        let mut manager = manager();
        manager.set_cursor_image(CursorImageStatus::Named(CursorIcon::Text));
        manager.transition.as_mut().unwrap().duration_ms = 100_000.;
        let started = manager.transition.as_ref().unwrap().started;
        manager.set_cursor_image(CursorImageStatus::Named(CursorIcon::Text));
        assert_eq!(manager.transition.as_ref().unwrap().started, started);
        manager.set_cursor_image(CursorImageStatus::Named(CursorIcon::Pointer));
        manager.set_cursor_image(CursorImageStatus::Named(CursorIcon::Wait));
        let transition = manager.transition.as_ref().unwrap();
        assert_eq!(transition.started, started);
        assert_eq!(transition.to, CursorIcon::Text);
        assert_eq!(transition.queued, Some(CursorIcon::Wait));
        manager.set_cursor_image(CursorImageStatus::Named(CursorIcon::Default));
        let transition = manager.transition.as_ref().unwrap();
        assert!(transition.reverse);
        assert_eq!(transition.queued, None);
    }

    #[test]
    fn queued_shape_plays_when_transition_finishes() {
        let mut manager = manager();
        manager.transition_cache.borrow_mut().insert(
            (CursorIcon::Text, CursorIcon::Pointer, 1),
            Some(("pointer-to-text".into(), true, asset())),
        );
        manager.set_cursor_image(CursorImageStatus::Named(CursorIcon::Text));
        manager.set_cursor_image(CursorImageStatus::Named(CursorIcon::Pointer));
        let started = manager.transition.as_ref().unwrap().started;
        manager.transition.as_mut().unwrap().started -= Duration::from_millis(150);
        let now = Instant::now();
        let next = manager.current_transition(now).unwrap();
        assert_eq!(
            (next.from, next.to, next.queued),
            (CursorIcon::Text, CursorIcon::Pointer, None)
        );
        assert_eq!(next.started, started - Duration::from_millis(50));
        assert!(next.active_at(now));
        assert!(manager.is_current_cursor_animated(1));
        match manager.get_render_cursor(1) {
            RenderCursor::Named { name, .. } => assert_eq!(name, "pointer-to-text"),
            _ => panic!("expected queued transition"),
        }
        manager.set_cursor_image(CursorImageStatus::Named(CursorIcon::Text));
        let transition = manager.transition.as_ref().unwrap();
        assert_eq!(
            (transition.from, transition.to),
            (CursorIcon::Text, CursorIcon::Pointer)
        );
        assert!(transition.reverse);
    }

    #[test]
    fn completion_returns_to_destination_and_stops_redrawing() {
        let mut manager = manager();
        manager.named_cursor_cache.borrow_mut().insert(
            (CursorIcon::Text, 1),
            Some(Rc::new(CursorManager::fallback_cursor())),
        );
        manager.set_cursor_image(CursorImageStatus::Named(CursorIcon::Text));
        manager.transition.as_mut().unwrap().started -= Duration::from_secs(1);
        assert!(!manager.is_current_cursor_animated(1));
        match manager.get_render_cursor(1) {
            RenderCursor::Named { name, frame, .. } => {
                assert_eq!(name, "text");
                assert_eq!(frame, None);
            }
            _ => panic!("expected destination cursor"),
        }
    }

    #[test]
    fn busy_animation_starts_after_transition_completion() {
        let mut manager = manager();
        manager
            .named_cursor_cache
            .borrow_mut()
            .insert((CursorIcon::Text, 1), Some(asset()));
        manager.set_cursor_image(CursorImageStatus::Named(CursorIcon::Text));
        manager.transition.as_mut().unwrap().started -= Duration::from_millis(130);
        match manager.get_render_cursor(1) {
            RenderCursor::Named { name, frame, .. } => {
                assert_eq!(name, "text");
                assert_eq!(frame, Some(1));
            }
            _ => panic!("expected animated destination"),
        }
    }

    #[test]
    fn hidden_cursor_cancels_immediately() {
        let mut manager = manager();
        manager.set_cursor_image(CursorImageStatus::Named(CursorIcon::Text));
        manager.set_cursor_image(CursorImageStatus::Hidden);
        assert!(manager.transition.is_none());
        assert!(matches!(manager.get_render_cursor(1), RenderCursor::Hidden));
        assert!(!manager.is_current_cursor_animated(1));
    }

    #[test]
    fn reverse_asset_starts_at_last_frame() {
        let mut manager = manager();
        manager.transition_cache.borrow_mut().insert(
            (CursorIcon::Default, CursorIcon::Text, 1),
            Some(("text-to-default".into(), true, asset())),
        );
        manager.set_cursor_image(CursorImageStatus::Named(CursorIcon::Text));
        manager.transition.as_mut().unwrap().duration_ms = 100_000.;
        match manager.get_render_cursor(1) {
            RenderCursor::Named { name, frame, .. } => {
                assert_eq!(name, "text-to-default");
                assert_eq!(frame, Some(1));
            }
            _ => panic!("expected reversed transition"),
        }
    }

    #[test]
    fn filenames_resolve_both_directions_and_missing_pairs_fall_back() {
        let directory = std::env::temp_dir().join(format!(
            "niri-cursor-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        std::fs::create_dir(directory.join("cursors")).unwrap();
        // Self-inheritance prevents fallback to installed themes.
        std::fs::write(
            directory.join("index.theme"),
            format!("[Icon Theme]\nInherits={}\n", directory.display()),
        )
        .unwrap();
        // Two one-pixel frames with different delays.
        let words = [
            0x72756358u32,
            16,
            0x10000,
            2,
            0xfffd0002,
            24,
            40,
            0xfffd0002,
            24,
            80,
            36,
            0xfffd0002,
            24,
            1,
            1,
            1,
            0,
            0,
            20,
            0xffffffff,
            36,
            0xfffd0002,
            24,
            1,
            1,
            1,
            0,
            0,
            80,
            0xff000000,
        ];
        let bytes: Vec<u8> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
        std::fs::write(directory.join("cursors/default-to-text"), bytes).unwrap();
        let mut manager = manager();
        manager.theme = CursorTheme::load(directory.to_str().unwrap());
        manager.transition_cache.get_mut().clear();
        let (name, reverse, cursor) = manager
            .get_transition(CursorIcon::Default, CursorIcon::Text, 1)
            .unwrap();
        assert_eq!(name, "default-to-text");
        assert!(!reverse);
        assert_eq!(cursor.animation_duration, 100);
        assert_eq!(cursor.frame(19).0, 0);
        assert_eq!(cursor.frame(20).0, 1);
        assert_eq!(cursor.frame(99).0, 1);
        let (name, reverse, _) = manager
            .get_transition(CursorIcon::Text, CursorIcon::Default, 1)
            .unwrap();
        assert_eq!(name, "default-to-text");
        assert!(reverse);
        assert!(manager
            .get_transition(CursorIcon::Default, CursorIcon::Pointer, 1)
            .is_none());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
