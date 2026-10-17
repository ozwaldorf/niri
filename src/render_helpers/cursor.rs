use glam::{Mat3, Vec2};
use smithay::backend::renderer::element::memory::MemoryRenderBufferRenderElement;
use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::gles::{GlesError, GlesFrame, GlesRenderer};
use smithay::backend::renderer::utils::CommitCounter;
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Physical, Rectangle, Scale};

use super::renderer::AsGlesRenderer;
use super::shaders::mat3_uniform;
use crate::backend::tty::{TtyFrame, TtyRenderer, TtyRendererError};
use crate::render_helpers::renderer::AsGlesFrame as _;
use crate::render_helpers::shaders::Shaders;

#[derive(Debug)]
pub struct CursorRenderElement {
    inner: MemoryRenderBufferRenderElement<GlesRenderer>,
    matrix: Mat3,
    commit: CommitCounter,
}

impl CursorRenderElement {
    pub fn new(
        inner: MemoryRenderBufferRenderElement<GlesRenderer>,
        hotspot: (f32, f32),
        angle: f64,
        commit: CommitCounter,
    ) -> Self {
        let size = inner.src().size;
        let size = Vec2::new(size.w as f32, size.h as f32);
        let hotspot = Vec2::from(hotspot);
        let matrix = Mat3::from_scale(size.recip())
            * Mat3::from_translation(hotspot)
            * Mat3::from_angle(-angle.to_radians() as f32)
            * Mat3::from_translation(-hotspot)
            * Mat3::from_scale(size);
        Self {
            inner,
            matrix,
            commit,
        }
    }
}

impl Element for CursorRenderElement {
    fn id(&self) -> &Id {
        self.inner.id()
    }

    fn current_commit(&self) -> CommitCounter {
        self.commit
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.inner.geometry(scale)
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        self.inner.src()
    }

    fn kind(&self) -> Kind {
        self.inner.kind()
    }
}

impl RenderElement<GlesRenderer> for CursorRenderElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        let program = Shaders::get_from_frame(frame).cursor.clone();
        if let Some(program) = &program {
            frame.override_default_tex_program(
                program.clone(),
                vec![mat3_uniform("rotation", self.matrix)],
            );
        }
        let result = RenderElement::<GlesRenderer>::draw(
            &self.inner,
            frame,
            src,
            dst,
            damage,
            opaque_regions,
            cache,
        );
        if program.is_some() {
            frame.clear_tex_program_override();
        }
        result
    }

    fn underlying_storage(&self, renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        // Hardware rotation is not implemented; scan out the unrotated image.
        self.inner.underlying_storage(renderer)
    }
}

impl<'render> RenderElement<TtyRenderer<'render>> for CursorRenderElement {
    fn draw(
        &self,
        frame: &mut TtyFrame<'render, '_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), TtyRendererError<'render>> {
        let gles_frame = frame.as_gles_frame();
        RenderElement::<GlesRenderer>::draw(
            self,
            gles_frame,
            src,
            dst,
            damage,
            opaque_regions,
            cache,
        )?;
        Ok(())
    }

    fn underlying_storage(
        &self,
        renderer: &mut TtyRenderer<'render>,
    ) -> Option<UnderlyingStorage<'_>> {
        // Hardware rotation is not implemented; scan out the unrotated image.
        self.inner.underlying_storage(renderer.as_gles_renderer())
    }
}
