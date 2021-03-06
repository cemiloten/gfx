use std::borrow::Borrow;
use std::cell::Cell;
use std::iter::repeat;
use std::ops::Range;
use std::sync::{Arc, Mutex, RwLock};
use std::{mem, ptr, slice};

use crate::gl::types::{GLenum, GLfloat, GLint};
use crate::{gl, GlContainer};

use crate::hal::backend::FastHashMap;
use crate::hal::format::{Format, Swizzle};
use crate::hal::pool::CommandPoolCreateFlags;
use crate::hal::queue::QueueFamilyId;
use crate::hal::range::RangeArg;
use crate::hal::{self as c, buffer, device as d, error, image as i, mapping, memory, pass, pso, query};

use spirv_cross::{glsl, spirv, ErrorCode as SpirvErrorCode};

use crate::info::LegacyFeatures;
use crate::pool::{BufferMemory, OwnedBuffer, RawCommandPool};
use crate::{conv, native as n, state};
use crate::{Backend as B, Share, Starc, Surface, Swapchain};

/// Emit error during shader module creation. Used if we don't expect an error
/// but might panic due to an exception in SPIRV-Cross.
fn gen_unexpected_error(err: SpirvErrorCode) -> d::ShaderError {
    let msg = match err {
        SpirvErrorCode::CompilationError(msg) => msg,
        SpirvErrorCode::Unhandled => "Unexpected error".into(),
    };
    d::ShaderError::CompilationFailed(msg)
}

fn get_shader_iv(gl: &GlContainer, name: n::Shader, query: GLenum) -> gl::types::GLint {
    let mut iv = 0;
    unsafe { gl.GetShaderiv(name, query, &mut iv) };
    iv
}

fn get_program_iv(gl: &GlContainer, name: n::Program, query: GLenum) -> gl::types::GLint {
    let mut iv = 0;
    unsafe { gl.GetProgramiv(name, query, &mut iv) };
    iv
}

fn get_shader_log(gl: &GlContainer, name: n::Shader) -> String {
    let mut length = get_shader_iv(gl, name, gl::INFO_LOG_LENGTH);
    if length > 0 {
        let mut log = String::with_capacity(length as usize);
        log.extend(repeat('\0').take(length as usize));
        unsafe {
            gl.GetShaderInfoLog(
                name,
                length,
                &mut length,
                (&log[..]).as_ptr() as *mut gl::types::GLchar,
            );
        }
        log.truncate(length as usize);
        log
    } else {
        String::new()
    }
}

pub(crate) fn get_program_log(gl: &GlContainer, name: n::Program) -> String {
    let mut length = get_program_iv(gl, name, gl::INFO_LOG_LENGTH);
    if length > 0 {
        let mut log = String::with_capacity(length as usize);
        log.extend(repeat('\0').take(length as usize));
        unsafe {
            gl.GetProgramInfoLog(
                name,
                length,
                &mut length,
                (&log[..]).as_ptr() as *mut gl::types::GLchar,
            );
        }
        log.truncate(length as usize);
        log
    } else {
        String::new()
    }
}

fn create_fbo_internal(share: &Starc<Share>) -> Option<gl::types::GLuint> {
    if share.private_caps.framebuffer {
        let gl = &share.context;
        let mut name = 0 as n::FrameBuffer;
        unsafe {
            gl.GenFramebuffers(1, &mut name);
        }
        info!("\tCreated frame buffer {}", name);
        Some(name)
    } else {
        None
    }
}

/// GL device.
#[derive(Debug)]
pub struct Device {
    share: Starc<Share>,
}

impl Drop for Device {
    fn drop(&mut self) {
        self.share.open.set(false);
    }
}

impl Device {
    /// Create a new `Device`.
    pub(crate) fn new(share: Starc<Share>) -> Self {
        Device { share: share }
    }

    pub fn create_shader_module_from_source(
        &self,
        data: &[u8],
        stage: pso::Stage,
    ) -> Result<n::ShaderModule, d::ShaderError> {
        let gl = &self.share.context;

        let can_compute = self.share.limits.max_compute_work_group_count[0] != 0;
        let can_tessellate = self.share.limits.max_patch_size != 0;
        let target = match stage {
            pso::Stage::Vertex => gl::VERTEX_SHADER,
            pso::Stage::Hull if can_tessellate => gl::TESS_CONTROL_SHADER,
            pso::Stage::Domain if can_tessellate => gl::TESS_EVALUATION_SHADER,
            pso::Stage::Geometry => gl::GEOMETRY_SHADER,
            pso::Stage::Fragment => gl::FRAGMENT_SHADER,
            pso::Stage::Compute if can_compute => gl::COMPUTE_SHADER,
            _ => return Err(d::ShaderError::UnsupportedStage(stage)),
        };

        let name = unsafe { gl.CreateShader(target) };
        unsafe {
            gl.ShaderSource(
                name,
                1,
                &(data.as_ptr() as *const gl::types::GLchar),
                &(data.len() as gl::types::GLint),
            );
            gl.CompileShader(name);
        }
        info!("\tCompiled shader {}", name);
        if let Err(err) = self.share.check() {
            panic!("Error compiling shader: {:?}", err);
        }

        let status = get_shader_iv(gl, name, gl::COMPILE_STATUS);
        let log = get_shader_log(gl, name);
        if status != 0 {
            if !log.is_empty() {
                warn!("\tLog: {}", log);
            }
            Ok(n::ShaderModule::Raw(name))
        } else {
            Err(d::ShaderError::CompilationFailed(log))
        }
    }

    fn bind_target_compat(
        gl: &GlContainer,
        point: GLenum,
        attachment: GLenum,
        view: &n::ImageView,
    ) {
        match *view {
            n::ImageView::Surface(surface) => unsafe {
                gl.FramebufferRenderbuffer(point, attachment, gl::RENDERBUFFER, surface);
            },
            n::ImageView::Texture(texture, level) => unsafe {
                gl.BindTexture(gl::TEXTURE_2D, texture);
                gl.FramebufferTexture2D(point, attachment, gl::TEXTURE_2D, texture, level as _);
            },
            n::ImageView::TextureLayer(texture, level, layer) => unsafe {
                gl.BindTexture(gl::TEXTURE_2D, texture);
                gl.FramebufferTexture3D(
                    point,
                    attachment,
                    gl::TEXTURE_2D,
                    texture,
                    level as _,
                    layer as _,
                );
            },
        }
    }

    fn bind_target(gl: &GlContainer, point: GLenum, attachment: GLenum, view: &n::ImageView) {
        match *view {
            n::ImageView::Surface(surface) => unsafe {
                gl.FramebufferRenderbuffer(point, attachment, gl::RENDERBUFFER, surface);
            },
            n::ImageView::Texture(texture, level) => unsafe {
                gl.FramebufferTexture(point, attachment, texture, level as _);
            },
            n::ImageView::TextureLayer(texture, level, layer) => unsafe {
                gl.FramebufferTextureLayer(point, attachment, texture, level as _, layer as _);
            },
        }
    }

    fn parse_spirv(&self, raw_data: &[u8]) -> Result<spirv::Ast<glsl::Target>, d::ShaderError> {
        // spec requires "codeSize must be a multiple of 4"
        assert_eq!(raw_data.len() & 3, 0);

        let module = spirv::Module::from_words(unsafe {
            slice::from_raw_parts(
                raw_data.as_ptr() as *const u32,
                raw_data.len() / mem::size_of::<u32>(),
            )
        });

        spirv::Ast::parse(&module).map_err(|err| {
            let msg = match err {
                SpirvErrorCode::CompilationError(msg) => msg,
                SpirvErrorCode::Unhandled => "Unknown parsing error".into(),
            };
            d::ShaderError::CompilationFailed(msg)
        })
    }

    fn specialize_ast(
        &self,
        ast: &mut spirv::Ast<glsl::Target>,
        specialization: pso::Specialization,
    ) -> Result<(), d::ShaderError> {
        let spec_constants = ast
            .get_specialization_constants()
            .map_err(gen_unexpected_error)?;

        for spec_constant in spec_constants {
            if let Some(constant) = specialization
                .constants
                .iter()
                .find(|c| c.id == spec_constant.constant_id)
            {
                // Override specialization constant values
                let value = specialization.data
                    [constant.range.start as usize..constant.range.end as usize]
                    .iter()
                    .rev()
                    .fold(0u64, |u, &b| (u << 8) + b as u64);

                ast.set_scalar_constant(spec_constant.id, value)
                    .map_err(gen_unexpected_error)?;
            }
        }

        Ok(())
    }

    fn translate_spirv(
        &self,
        ast: &mut spirv::Ast<glsl::Target>,
    ) -> Result<String, d::ShaderError> {
        let mut compile_options = glsl::CompilerOptions::default();
        // see version table at https://en.wikipedia.org/wiki/OpenGL_Shading_Language
        compile_options.version = match self.share.info.shading_language.tuple() {
            (4, 60) => glsl::Version::V4_60,
            (4, 50) => glsl::Version::V4_50,
            (4, 40) => glsl::Version::V4_40,
            (4, 30) => glsl::Version::V4_30,
            (4, 20) => glsl::Version::V4_20,
            (4, 10) => glsl::Version::V4_10,
            (4, 00) => glsl::Version::V4_00,
            (3, 30) => glsl::Version::V3_30,
            (1, 50) => glsl::Version::V1_50,
            (1, 40) => glsl::Version::V1_40,
            (1, 30) => glsl::Version::V1_30,
            (1, 20) => glsl::Version::V1_20,
            (1, 10) => glsl::Version::V1_10,
            other if other > (4, 60) => glsl::Version::V4_60,
            other => panic!("GLSL version is not recognized: {:?}", other),
        };
        compile_options.vertex.invert_y = true;
        debug!("SPIR-V options {:?}", compile_options);

        ast.set_compiler_options(&compile_options)
            .map_err(gen_unexpected_error)?;
        ast.compile().map_err(|err| {
            let msg = match err {
                SpirvErrorCode::CompilationError(msg) => msg,
                SpirvErrorCode::Unhandled => "Unknown compile error".into(),
            };
            d::ShaderError::CompilationFailed(msg)
        })
    }

    fn remap_bindings(
        &self,
        ast: &mut spirv::Ast<glsl::Target>,
        desc_remap_data: &mut n::DescRemapData,
        nb_map: &mut FastHashMap<String, pso::DescriptorBinding>,
    ) {
        let res = ast.get_shader_resources().unwrap();
        self.remap_binding(
            ast,
            desc_remap_data,
            nb_map,
            &res.sampled_images,
            n::BindingTypes::Images,
        );
        self.remap_binding(
            ast,
            desc_remap_data,
            nb_map,
            &res.uniform_buffers,
            n::BindingTypes::UniformBuffers,
        );
    }

    fn remap_binding(
        &self,
        ast: &mut spirv::Ast<glsl::Target>,
        desc_remap_data: &mut n::DescRemapData,
        nb_map: &mut FastHashMap<String, pso::DescriptorBinding>,
        all_res: &[spirv::Resource],
        btype: n::BindingTypes,
    ) {
        for res in all_res {
            let set = ast
                .get_decoration(res.id, spirv::Decoration::DescriptorSet)
                .unwrap();
            let binding = ast
                .get_decoration(res.id, spirv::Decoration::Binding)
                .unwrap();
            let nbs = desc_remap_data
                .get_binding(btype, set as _, binding)
                .unwrap();

            for nb in nbs {
                if self
                    .share
                    .legacy_features
                    .contains(LegacyFeatures::EXPLICIT_LAYOUTS_IN_SHADER)
                {
                    ast.set_decoration(res.id, spirv::Decoration::Binding, *nb)
                        .unwrap()
                } else {
                    ast.unset_decoration(res.id, spirv::Decoration::Binding)
                        .unwrap();
                    assert!(nb_map.insert(res.name.clone(), *nb).is_none());
                }
                ast.unset_decoration(res.id, spirv::Decoration::DescriptorSet)
                    .unwrap();
            }
        }
    }

    fn combine_separate_images_and_samplers(
        &self,
        ast: &mut spirv::Ast<glsl::Target>,
        desc_remap_data: &mut n::DescRemapData,
        nb_map: &mut FastHashMap<String, pso::DescriptorBinding>,
    ) {
        let mut id_map =
            FastHashMap::<u32, (pso::DescriptorSetIndex, pso::DescriptorBinding)>::default();
        let res = ast.get_shader_resources().unwrap();
        self.populate_id_map(ast, &mut id_map, &res.separate_images);
        self.populate_id_map(ast, &mut id_map, &res.separate_samplers);

        for cis in ast.get_combined_image_samplers().unwrap() {
            let (set, binding) = id_map.get(&cis.image_id).unwrap();
            let nb = desc_remap_data.reserve_binding(n::BindingTypes::Images);
            desc_remap_data.insert_missing_binding(nb, n::BindingTypes::Images, *set, *binding);
            let (set, binding) = id_map.get(&cis.sampler_id).unwrap();
            desc_remap_data.insert_missing_binding(nb, n::BindingTypes::Images, *set, *binding);

            let new_name = "GFX_HAL_COMBINED_SAMPLER".to_owned()
                + "_"
                + &cis.sampler_id.to_string()
                + "_"
                + &cis.image_id.to_string()
                + "_"
                + &cis.combined_id.to_string();
            ast.set_name(cis.combined_id, &new_name).unwrap();
            if self
                .share
                .legacy_features
                .contains(LegacyFeatures::EXPLICIT_LAYOUTS_IN_SHADER)
            {
                ast.set_decoration(cis.combined_id, spirv::Decoration::Binding, nb)
                    .unwrap()
            } else {
                ast.unset_decoration(cis.combined_id, spirv::Decoration::Binding)
                    .unwrap();
                assert!(nb_map.insert(new_name, nb).is_none())
            }
            ast.unset_decoration(cis.combined_id, spirv::Decoration::DescriptorSet)
                .unwrap();
        }
    }

    fn populate_id_map(
        &self,
        ast: &mut spirv::Ast<glsl::Target>,
        id_map: &mut FastHashMap<u32, (pso::DescriptorSetIndex, pso::DescriptorBinding)>,
        all_res: &[spirv::Resource],
    ) {
        for res in all_res {
            let set = ast
                .get_decoration(res.id, spirv::Decoration::DescriptorSet)
                .unwrap();
            let binding = ast
                .get_decoration(res.id, spirv::Decoration::Binding)
                .unwrap();
            assert!(id_map.insert(res.id, (set as _, binding)).is_none())
        }
    }

    fn compile_shader(
        &self,
        point: &pso::EntryPoint<B>,
        stage: pso::Stage,
        desc_remap_data: &mut n::DescRemapData,
        name_binding_map: &mut FastHashMap<String, pso::DescriptorBinding>,
    ) -> n::Shader {
        assert_eq!(point.entry, "main");
        match *point.module {
            n::ShaderModule::Raw(raw) => {
                debug!("Can't remap bindings for raw shaders. Assuming they are already rebound.");
                raw
            }
            n::ShaderModule::Spirv(ref spirv) => {
                let mut ast = self.parse_spirv(spirv).unwrap();

                self.specialize_ast(&mut ast, point.specialization).unwrap();
                self.remap_bindings(&mut ast, desc_remap_data, name_binding_map);
                self.combine_separate_images_and_samplers(
                    &mut ast,
                    desc_remap_data,
                    name_binding_map,
                );

                let glsl = self.translate_spirv(&mut ast).unwrap();
                info!("Generated:\n{:?}", glsl);
                let shader = match self
                    .create_shader_module_from_source(glsl.as_bytes(), stage)
                    .unwrap()
                {
                    n::ShaderModule::Raw(raw) => raw,
                    _ => panic!("Unhandled"),
                };

                shader
            }
        }
    }
}

pub(crate) unsafe fn set_sampler_info<SetParamFloat, SetParamFloatVec, SetParamInt>(
    share: &Starc<Share>,
    info: &i::SamplerInfo,
    mut set_param_float: SetParamFloat,
    mut set_param_float_vec: SetParamFloatVec,
    mut set_param_int: SetParamInt,
) where
    SetParamFloat: FnMut(GLenum, GLfloat),
    SetParamFloatVec: FnMut(GLenum, &[GLfloat]),
    SetParamInt: FnMut(GLenum, GLint),
{
    let (min, mag) = conv::filter_to_gl(info.mag_filter, info.min_filter, info.mip_filter);
    match info.anisotropic {
        i::Anisotropic::On(fac) if fac > 1 => {
            if share.private_caps.sampler_anisotropy_ext {
                set_param_float(gl::TEXTURE_MAX_ANISOTROPY_EXT, fac as GLfloat);
            } else if share.features.contains(c::Features::SAMPLER_ANISOTROPY) {
                set_param_float(gl::TEXTURE_MAX_ANISOTROPY_EXT, fac as GLfloat);
            }
        }
        _ => (),
    }

    set_param_int(gl::TEXTURE_MIN_FILTER, min as GLint);
    set_param_int(gl::TEXTURE_MAG_FILTER, mag as GLint);

    let (s, t, r) = info.wrap_mode;
    set_param_int(gl::TEXTURE_WRAP_S, conv::wrap_to_gl(s) as GLint);
    set_param_int(gl::TEXTURE_WRAP_T, conv::wrap_to_gl(t) as GLint);
    set_param_int(gl::TEXTURE_WRAP_R, conv::wrap_to_gl(r) as GLint);

    if share
        .features
        .contains(hal::Features::SAMPLER_MIP_LOD_BIAS)
    {
        set_param_float(gl::TEXTURE_LOD_BIAS, info.lod_bias.into());
    }
    if share
        .legacy_features
        .contains(LegacyFeatures::SAMPLER_BORDER_COLOR)
    {
        let border: [f32; 4] = info.border.into();
        set_param_float_vec(gl::TEXTURE_BORDER_COLOR, &border);
    }

    set_param_float(gl::TEXTURE_MIN_LOD, info.lod_range.start.into());
    set_param_float(gl::TEXTURE_MAX_LOD, info.lod_range.end.into());

    match info.comparison {
        None => set_param_int(gl::TEXTURE_COMPARE_MODE, gl::NONE as GLint),
        Some(cmp) => {
            set_param_int(
                gl::TEXTURE_COMPARE_MODE,
                gl::COMPARE_REF_TO_TEXTURE as GLint,
            );
            set_param_int(
                gl::TEXTURE_COMPARE_FUNC,
                state::map_comparison(cmp) as GLint,
            );
        }
    }
}

impl d::Device<B> for Device {
    unsafe fn allocate_memory(
        &self,
        _mem_type: c::MemoryTypeId,
        size: u64,
    ) -> Result<n::Memory, d::AllocationError> {
        // TODO
        Ok(n::Memory {
            properties: memory::Properties::CPU_VISIBLE | memory::Properties::CPU_CACHED,
            first_bound_buffer: Cell::new(0),
            size,
        })
    }

    unsafe fn create_command_pool(
        &self,
        _family: QueueFamilyId,
        flags: CommandPoolCreateFlags,
    ) -> Result<RawCommandPool, d::OutOfMemory> {
        let fbo = create_fbo_internal(&self.share);
        let limits = self.share.limits.into();
        let memory = if flags.contains(CommandPoolCreateFlags::RESET_INDIVIDUAL) {
            BufferMemory::Individual {
                storage: FastHashMap::default(),
                next_buffer_id: 0,
            }
        } else {
            BufferMemory::Linear(OwnedBuffer::new())
        };

        // Ignoring `TRANSIENT` hint, unsure how to make use of this.

        Ok(RawCommandPool {
            fbo,
            limits,
            memory: Arc::new(Mutex::new(memory)),
        })
    }

    unsafe fn destroy_command_pool(&self, pool: RawCommandPool) {
        if let Some(fbo) = pool.fbo {
            let gl = &self.share.context;
            gl.DeleteFramebuffers(1, &fbo);
        }
    }

    unsafe fn create_render_pass<'a, IA, IS, ID>(
        &self,
        attachments: IA,
        subpasses: IS,
        _dependencies: ID,
    ) -> Result<n::RenderPass, d::OutOfMemory>
    where
        IA: IntoIterator,
        IA::Item: Borrow<pass::Attachment>,
        IS: IntoIterator,
        IS::Item: Borrow<pass::SubpassDesc<'a>>,
        ID: IntoIterator,
        ID::Item: Borrow<pass::SubpassDependency>,
    {
        let subpasses = subpasses
            .into_iter()
            .map(|subpass| {
                let color_attachments = subpass
                    .borrow()
                    .colors
                    .iter()
                    .map(|&(index, _)| index)
                    .collect();

                n::SubpassDesc { color_attachments }
            })
            .collect();

        Ok(n::RenderPass {
            attachments: attachments
                .into_iter()
                .map(|attachment| attachment.borrow().clone())
                .collect::<Vec<_>>(),
            subpasses,
        })
    }

    unsafe fn create_pipeline_layout<IS, IR>(
        &self,
        layouts: IS,
        _: IR,
    ) -> Result<n::PipelineLayout, d::OutOfMemory>
    where
        IS: IntoIterator,
        IS::Item: Borrow<n::DescriptorSetLayout>,
        IR: IntoIterator,
        IR::Item: Borrow<(pso::ShaderStageFlags, Range<u32>)>,
    {
        let mut drd = n::DescRemapData::new();

        layouts.into_iter().enumerate().for_each(|(set, layout)| {
            layout.borrow().iter().for_each(|binding| {
                // DescriptorType -> Descriptor
                //
                // Sampler -> Sampler
                // Image -> SampledImage, StorageImage, InputAttachment
                // CombinedImageSampler -> CombinedImageSampler
                // Buffer -> UniformBuffer, StorageBuffer
                // UniformTexel -> UniformTexel
                // StorageTexel -> StorageTexel

                assert!(!binding.immutable_samplers); //TODO: Implement immutable_samplers
                use crate::pso::DescriptorType::*;
                match binding.ty {
                    CombinedImageSampler => {
                        drd.insert_missing_binding_into_spare(
                            n::BindingTypes::Images,
                            set as _,
                            binding.binding,
                        );
                    }
                    Sampler | SampledImage => {
                        // We need to figure out combos once we get the shaders, until then we
                        // do nothing
                    }
                    UniformBuffer => {
                        drd.insert_missing_binding_into_spare(
                            n::BindingTypes::UniformBuffers,
                            set as _,
                            binding.binding,
                        );
                    }
                    StorageImage | UniformTexelBuffer | UniformBufferDynamic
                    | StorageTexelBuffer | StorageBufferDynamic | StorageBuffer
                    | InputAttachment => unimplemented!(), // 6
                }
            })
        });

        Ok(n::PipelineLayout {
            desc_remap_data: Arc::new(RwLock::new(drd)),
        })
    }

    unsafe fn create_pipeline_cache(&self, _data: Option<&[u8]>) -> Result<(), d::OutOfMemory> {
        Ok(())
    }

    unsafe fn get_pipeline_cache_data(&self, _cache: &()) -> Result<Vec<u8>, d::OutOfMemory> {
        //empty
        Ok(Vec::new())
    }

    unsafe fn destroy_pipeline_cache(&self, _: ()) {
        //empty
    }

    unsafe fn merge_pipeline_caches<I>(&self, _: &(), _: I) -> Result<(), d::OutOfMemory>
    where
        I: IntoIterator,
        I::Item: Borrow<()>,
    {
        //empty
        Ok(())
    }

    unsafe fn create_graphics_pipeline<'a>(
        &self,
        desc: &pso::GraphicsPipelineDesc<'a, B>,
        _cache: Option<&()>,
    ) -> Result<n::GraphicsPipeline, pso::CreationError> {
        let gl = &self.share.context;
        let share = &self.share;
        let desc = desc.borrow();
        let subpass = {
            let subpass = desc.subpass;
            match subpass.main_pass.subpasses.get(subpass.index) {
                Some(sp) => sp,
                None => return Err(pso::CreationError::InvalidSubpass(subpass.index)),
            }
        };

        let program = {
            let name = gl.CreateProgram();

            // Attach shaders to program
            let shaders = [
                (pso::Stage::Vertex, Some(&desc.shaders.vertex)),
                (pso::Stage::Hull, desc.shaders.hull.as_ref()),
                (pso::Stage::Domain, desc.shaders.domain.as_ref()),
                (pso::Stage::Geometry, desc.shaders.geometry.as_ref()),
                (pso::Stage::Fragment, desc.shaders.fragment.as_ref()),
            ];

            let mut name_binding_map = FastHashMap::<String, pso::DescriptorBinding>::default();
            let shader_names = &shaders
                .iter()
                .filter_map(|&(stage, point_maybe)| {
                    point_maybe.map(|point| {
                        let shader_name = self.compile_shader(
                            point,
                            stage,
                            &mut desc.layout.desc_remap_data.write().unwrap(),
                            &mut name_binding_map,
                        );

                        gl.AttachShader(name, shader_name);

                        shader_name
                    })
                })
                .collect::<Vec<_>>();

            if !share.private_caps.program_interface && share.private_caps.frag_data_location {
                for i in 0..subpass.color_attachments.len() {
                    let color_name = format!("Target{}\0", i);
                    gl.BindFragDataLocation(
                        name,
                        i as u32,
                        (&color_name[..]).as_ptr() as *mut gl::types::GLchar,
                    );
                }
            }

            gl.LinkProgram(name);
            info!("\tLinked program {}", name);
            if let Err(err) = share.check() {
                panic!("Error linking program: {:?}", err);
            }

            for shader_name in shader_names {
                gl.DetachShader(name, *shader_name);
                gl.DeleteShader(*shader_name);
            }

            if !self
                .share
                .legacy_features
                .contains(LegacyFeatures::EXPLICIT_LAYOUTS_IN_SHADER)
            {
                let gl = &self.share.context;
                gl.UseProgram(name);
                for (bname, binding) in name_binding_map.iter() {
                    let loc = gl.GetUniformLocation(name, bname.as_ptr() as _);
                    gl.Uniform1i(loc, *binding as _);
                }
            }

            let status = get_program_iv(gl, name, gl::LINK_STATUS);
            let log = get_program_log(gl, name);
            if status != 0 {
                if !log.is_empty() {
                    warn!("\tLog: {}", log);
                }
            } else {
                return Err(pso::CreationError::Shader(
                    d::ShaderError::CompilationFailed(log),
                ));
            }

            name
        };

        let patch_size = match desc.input_assembler.primitive {
            c::Primitive::PatchList(size) => Some(size as _),
            _ => None,
        };

        let mut vertex_buffers = Vec::new();
        for vb in &desc.vertex_buffers {
            while vertex_buffers.len() <= vb.binding as usize {
                vertex_buffers.push(None);
            }
            vertex_buffers[vb.binding as usize] = Some(*vb);
        }

        Ok(n::GraphicsPipeline {
            program,
            primitive: conv::primitive_to_gl_primitive(desc.input_assembler.primitive),
            patch_size,
            blend_targets: desc.blender.targets.clone(),
            vertex_buffers,
            attributes: desc
                .attributes
                .iter()
                .map(|&a| {
                    let (size, format, vertex_attrib_fn) =
                        conv::format_to_gl_format(a.element.format).unwrap();
                    n::AttributeDesc {
                        location: a.location,
                        offset: a.element.offset,
                        binding: a.binding,
                        size,
                        format,
                        vertex_attrib_fn,
                    }
                })
                .collect(),
        })
    }

    unsafe fn create_compute_pipeline<'a>(
        &self,
        desc: &pso::ComputePipelineDesc<'a, B>,
        _cache: Option<&()>,
    ) -> Result<n::ComputePipeline, pso::CreationError> {
        let gl = &self.share.context;
        let share = &self.share;

        let program = {
            let name = gl.CreateProgram();

            let mut name_binding_map = FastHashMap::<String, pso::DescriptorBinding>::default();
            let shader = self.compile_shader(
                &desc.shader,
                pso::Stage::Compute,
                &mut desc.layout.desc_remap_data.write().unwrap(),
                &mut name_binding_map,
            );
            gl.AttachShader(name, shader);

            gl.LinkProgram(name);
            info!("\tLinked program {}", name);
            if let Err(err) = share.check() {
                panic!("Error linking program: {:?}", err);
            }

            gl.DetachShader(name, shader);
            gl.DeleteShader(shader);

            if !self
                .share
                .legacy_features
                .contains(LegacyFeatures::EXPLICIT_LAYOUTS_IN_SHADER)
            {
                let gl = &self.share.context;
                gl.UseProgram(name);
                for (bname, binding) in name_binding_map.iter() {
                    let loc = gl.GetUniformLocation(name, bname.as_ptr() as _);
                    gl.Uniform1i(loc, *binding as _);
                }
            }

            let status = get_program_iv(gl, name, gl::LINK_STATUS);
            let log = get_program_log(gl, name);
            if status != 0 {
                if !log.is_empty() {
                    warn!("\tLog: {}", log);
                }
            } else {
                return Err(pso::CreationError::Other);
            }

            name
        };

        Ok(n::ComputePipeline { program })
    }

    unsafe fn create_framebuffer<I>(
        &self,
        pass: &n::RenderPass,
        attachments: I,
        _extent: i::Extent,
    ) -> Result<n::FrameBuffer, d::OutOfMemory>
    where
        I: IntoIterator,
        I::Item: Borrow<n::ImageView>,
    {
        if !self.share.private_caps.framebuffer {
            return Err(d::OutOfMemory::OutOfHostMemory);
        }

        let gl = &self.share.context;
        let target = gl::DRAW_FRAMEBUFFER;
        let mut name = 0;
        gl.GenFramebuffers(1, &mut name);
        gl.BindFramebuffer(target, name);

        let att_points = [
            gl::COLOR_ATTACHMENT0,
            gl::COLOR_ATTACHMENT1,
            gl::COLOR_ATTACHMENT2,
            gl::COLOR_ATTACHMENT3,
        ];

        let mut attachments_len = 0;
        //TODO: exclude depth/stencil attachments from here
        for (&att_point, view) in att_points.iter().zip(attachments.into_iter()) {
            attachments_len += 1;
            if self.share.private_caps.framebuffer_texture {
                Self::bind_target(gl, target, att_point, view.borrow());
            } else {
                Self::bind_target_compat(gl, target, att_point, view.borrow());
            }
        }
        assert_eq!(attachments_len, pass.attachments.len());
        // attachments_len actually equals min(attachments.len(), att_points.len()) until the next assert

        assert!(pass.attachments.len() <= att_points.len());
        gl.DrawBuffers(attachments_len as _, att_points.as_ptr());
        let _status = gl.CheckFramebufferStatus(target); //TODO: check status
        gl.BindFramebuffer(target, 0);

        if let Err(err) = self.share.check() {
            //TODO: attachments have been consumed
            panic!(
                "Error creating FBO: {:?} for {:?}", /* with attachments {:?}"*/
                err, pass /*, attachments*/
            );
        }

        Ok(name)
    }

    unsafe fn create_shader_module(
        &self,
        raw_data: &[u8],
    ) -> Result<n::ShaderModule, d::ShaderError> {
        Ok(n::ShaderModule::Spirv(raw_data.into()))
    }

    unsafe fn create_sampler(
        &self,
        info: i::SamplerInfo,
    ) -> Result<n::FatSampler, d::AllocationError> {
        if !self
            .share
            .legacy_features
            .contains(LegacyFeatures::SAMPLER_OBJECTS)
        {
            return Ok(n::FatSampler::Info(info));
        }

        let gl = &self.share.context;
        let mut name = 0 as n::Sampler;

        gl.GenSamplers(1, &mut name);
        set_sampler_info(
            &self.share,
            &info,
            |a, b| gl.SamplerParameterf(name, a, b),
            |a, b| gl.SamplerParameterfv(name, a, &b[0]),
            |a, b| gl.SamplerParameteri(name, a, b),
        );

        if let Err(_) = self.share.check() {
            Err(d::AllocationError::OutOfMemory(
                d::OutOfMemory::OutOfHostMemory,
            ))
        } else {
            Ok(n::FatSampler::Sampler(name))
        }
    }

    unsafe fn create_buffer(
        &self,
        size: u64,
        usage: buffer::Usage,
    ) -> Result<n::Buffer, buffer::CreationError> {
        if !self
            .share
            .legacy_features
            .contains(LegacyFeatures::CONSTANT_BUFFER)
            && usage.contains(buffer::Usage::UNIFORM)
        {
            return Err(buffer::CreationError::UnsupportedUsage { usage });
        }

        let target = if self.share.private_caps.buffer_role_change {
            gl::ARRAY_BUFFER
        } else {
            match conv::buffer_usage_to_gl_target(usage) {
                Some(target) => target,
                None => return Err(buffer::CreationError::UnsupportedUsage { usage }),
            }
        };

        let gl = &self.share.context;
        let mut name = 0;
        gl.GenBuffers(1, &mut name);

        Ok(n::Buffer {
            raw: name,
            target,
            requirements: memory::Requirements {
                size,
                alignment: 1, // TODO: do we need specific alignment for any use-case?
                type_mask: 0x7,
            },
        })
    }

    unsafe fn get_buffer_requirements(&self, buffer: &n::Buffer) -> memory::Requirements {
        buffer.requirements
    }

    unsafe fn bind_buffer_memory(
        &self,
        memory: &n::Memory,
        offset: u64,
        buffer: &mut n::Buffer,
    ) -> Result<(), d::BindError> {
        let gl = &self.share.context;
        let target = buffer.target;

        if offset == 0 {
            memory.first_bound_buffer.set(buffer.raw);
        } else {
            assert_ne!(0, memory.first_bound_buffer.get());
        }

        let cpu_can_read = memory.can_download();
        let cpu_can_write = memory.can_upload();

        if self.share.private_caps.buffer_storage {
            //TODO: gl::DYNAMIC_STORAGE_BIT | gl::MAP_PERSISTENT_BIT
            let flags = memory.map_flags();
            //TODO: use *Named calls to avoid binding
            gl.BindBuffer(target, buffer.raw);
            gl.BufferStorage(target, buffer.requirements.size as _, ptr::null(), flags);
            gl.BindBuffer(target, 0);
        } else {
            let flags = if cpu_can_read && cpu_can_write {
                gl::DYNAMIC_DRAW
            } else if cpu_can_write {
                gl::STREAM_DRAW
            } else if cpu_can_read {
                gl::STREAM_READ
            } else {
                gl::STATIC_DRAW
            };
            gl.BindBuffer(target, buffer.raw);
            gl.BufferData(target, buffer.requirements.size as _, ptr::null(), flags);
            gl.BindBuffer(target, 0);
        }

        if let Err(err) = self.share.check() {
            panic!(
                "Error {:?} initializing buffer {:?}, memory {:?}",
                err, buffer, memory.properties
            );
        }

        Ok(())
    }

    unsafe fn map_memory<R: RangeArg<u64>>(
        &self,
        memory: &n::Memory,
        range: R,
    ) -> Result<*mut u8, mapping::Error> {
        let gl = &self.share.context;
        let buffer = match memory.first_bound_buffer.get() {
            0 => panic!("No buffer has been bound yet, can't map memory!"),
            other => other,
        };

        assert!(self.share.private_caps.buffer_role_change);
        let target = gl::PIXEL_PACK_BUFFER;
        let access = memory.map_flags();

        let offset = *range.start().unwrap_or(&0);
        let size = *range.end().unwrap_or(&memory.size) - offset;

        let ptr = {
            gl.BindBuffer(target, buffer);
            let ptr = gl.MapBufferRange(target, offset as _, size as _, access);
            gl.BindBuffer(target, 0);
            ptr as *mut _
        };

        if let Err(err) = self.share.check() {
            panic!("Error mapping memory: {:?} for memory {:?}", err, memory);
        }

        Ok(ptr)
    }

    unsafe fn unmap_memory(&self, memory: &n::Memory) {
        let gl = &self.share.context;
        let buffer = match memory.first_bound_buffer.get() {
            0 => panic!("No buffer has been bound yet, can't map memory!"),
            other => other,
        };
        let target = gl::PIXEL_PACK_BUFFER;

        gl.BindBuffer(target, buffer);
        gl.UnmapBuffer(target);
        gl.BindBuffer(target, 0);

        if let Err(err) = self.share.check() {
            panic!("Error unmapping memory: {:?} for memory {:?}", err, memory);
        }
    }

    unsafe fn flush_mapped_memory_ranges<'a, I, R>(&self, _: I) -> Result<(), d::OutOfMemory>
    where
        I: IntoIterator,
        I::Item: Borrow<(&'a n::Memory, R)>,
        R: RangeArg<u64>,
    {
        warn!("memory range invalidation not implemented!");
        Ok(())
    }

    unsafe fn invalidate_mapped_memory_ranges<'a, I, R>(
        &self,
        _ranges: I,
    ) -> Result<(), d::OutOfMemory>
    where
        I: IntoIterator,
        I::Item: Borrow<(&'a n::Memory, R)>,
        R: RangeArg<u64>,
    {
        unimplemented!()
    }

    unsafe fn create_buffer_view<R: RangeArg<u64>>(
        &self,
        _: &n::Buffer,
        _: Option<Format>,
        _: R,
    ) -> Result<n::BufferView, buffer::ViewCreationError> {
        unimplemented!()
    }

    unsafe fn create_image(
        &self,
        kind: i::Kind,
        num_levels: i::Level,
        format: Format,
        _tiling: i::Tiling,
        usage: i::Usage,
        _view_caps: i::ViewCapabilities,
    ) -> Result<n::Image, i::CreationError> {
        let gl = &self.share.context;

        let (int_format, iformat, itype) = match format {
            Format::Rgba8Unorm => (gl::RGBA8, gl::RGBA, gl::UNSIGNED_BYTE),
            Format::Rgba8Srgb => (gl::SRGB8_ALPHA8, gl::RGBA, gl::UNSIGNED_BYTE),
            _ => unimplemented!(),
        };

        let channel = format.base_format().1;

        let image = if num_levels > 1
            || usage.contains(i::Usage::STORAGE)
            || usage.contains(i::Usage::SAMPLED)
        {
            let mut name = 0;
            gl.GenTextures(1, &mut name);
            match kind {
                i::Kind::D2(w, h, 1, 1) => {
                    gl.BindTexture(gl::TEXTURE_2D, name);
                    if self.share.private_caps.image_storage {
                        gl.TexStorage2D(
                            gl::TEXTURE_2D,
                            num_levels as _,
                            int_format,
                            w as _,
                            h as _,
                        );
                    } else {
                        gl.TexParameteri(
                            gl::TEXTURE_2D,
                            gl::TEXTURE_MAX_LEVEL,
                            (num_levels - 1) as _,
                        );
                        let mut w = w;
                        let mut h = h;
                        for i in 0..num_levels {
                            gl.TexImage2D(
                                gl::TEXTURE_2D,
                                i as _,
                                int_format as _,
                                w as _,
                                h as _,
                                0,
                                iformat,
                                itype,
                                std::ptr::null(),
                            );
                            w = std::cmp::max(w / 2, 1);
                            h = std::cmp::max(h / 2, 1);
                        }
                    }
                }
                _ => unimplemented!(),
            };
            n::ImageKind::Texture(name)
        } else {
            let mut name = 0;
            gl.GenRenderbuffers(1, &mut name);
            match kind {
                i::Kind::D2(w, h, 1, 1) => {
                    gl.BindRenderbuffer(gl::RENDERBUFFER, name);
                    gl.RenderbufferStorage(gl::RENDERBUFFER, int_format, w as _, h as _);
                }
                _ => unimplemented!(),
            };
            n::ImageKind::Surface(name)
        };

        let surface_desc = format.base_format().0.desc();
        let bytes_per_texel = surface_desc.bits / 8;
        let ext = kind.extent();
        let size = (ext.width * ext.height * ext.depth) as u64 * bytes_per_texel as u64;

        if let Err(err) = self.share.check() {
            panic!(
                "Error creating image: {:?} for kind {:?} of {:?}",
                err, kind, format
            );
        }

        Ok(n::Image {
            kind: image,
            channel,
            requirements: memory::Requirements {
                size,
                alignment: 1,
                type_mask: 0x7,
            },
        })
    }

    unsafe fn get_image_requirements(&self, unbound: &n::Image) -> memory::Requirements {
        unbound.requirements
    }

    unsafe fn get_image_subresource_footprint(
        &self,
        _image: &n::Image,
        _sub: i::Subresource,
    ) -> i::SubresourceFootprint {
        unimplemented!()
    }

    unsafe fn bind_image_memory(
        &self,
        _memory: &n::Memory,
        _offset: u64,
        _image: &mut n::Image,
    ) -> Result<(), d::BindError> {
        Ok(())
    }

    unsafe fn create_image_view(
        &self,
        image: &n::Image,
        _kind: i::ViewKind,
        _format: Format,
        swizzle: Swizzle,
        range: i::SubresourceRange,
    ) -> Result<n::ImageView, i::ViewError> {
        //TODO: check if `layers.end` covers all the layers
        let level = range.levels.start;
        assert_eq!(level + 1, range.levels.end);
        //assert_eq!(format, image.format);
        assert_eq!(swizzle, Swizzle::NO);
        //TODO: check format
        match image.kind {
            n::ImageKind::Surface(surface) => {
                if range.levels.start == 0 && range.layers.start == 0 {
                    Ok(n::ImageView::Surface(surface))
                } else if level != 0 {
                    Err(i::ViewError::Level(level)) //TODO
                } else {
                    Err(i::ViewError::Layer(i::LayerError::OutOfBounds(
                        range.layers,
                    )))
                }
            }
            n::ImageKind::Texture(texture) => {
                //TODO: check that `level` exists
                if range.layers.start == 0 {
                    Ok(n::ImageView::Texture(texture, level))
                } else if range.layers.start + 1 == range.layers.end {
                    Ok(n::ImageView::TextureLayer(
                        texture,
                        level,
                        range.layers.start,
                    ))
                } else {
                    Err(i::ViewError::Layer(i::LayerError::OutOfBounds(
                        range.layers,
                    )))
                }
            }
        }
    }

    unsafe fn create_descriptor_pool<I>(
        &self,
        _: usize,
        _: I,
        _: pso::DescriptorPoolCreateFlags,
    ) -> Result<n::DescriptorPool, d::OutOfMemory>
    where
        I: IntoIterator,
        I::Item: Borrow<pso::DescriptorRangeDesc>,
    {
        Ok(n::DescriptorPool {})
    }

    unsafe fn create_descriptor_set_layout<I, J>(
        &self,
        layout: I,
        _: J,
    ) -> Result<n::DescriptorSetLayout, d::OutOfMemory>
    where
        I: IntoIterator,
        I::Item: Borrow<pso::DescriptorSetLayoutBinding>,
        J: IntoIterator,
        J::Item: Borrow<n::FatSampler>,
    {
        // Just return it
        Ok(layout.into_iter().map(|l| l.borrow().clone()).collect())
    }

    unsafe fn write_descriptor_sets<'a, I, J>(&self, writes: I)
    where
        I: IntoIterator<Item = pso::DescriptorSetWrite<'a, B, J>>,
        J: IntoIterator,
        J::Item: Borrow<pso::Descriptor<'a, B>>,
    {
        for mut write in writes {
            let set = &mut write.set;
            let mut bindings = set.bindings.lock().unwrap();
            let binding = write.binding;
            let mut offset = write.array_offset as _;

            for descriptor in write.descriptors {
                match descriptor.borrow() {
                    pso::Descriptor::Buffer(buffer, ref range) => {
                        let start = range.start.unwrap_or(0);
                        let end = range.end.unwrap_or(buffer.requirements.size);
                        let size = (end - start) as _;

                        bindings.push(n::DescSetBindings::Buffer {
                            ty: n::BindingTypes::UniformBuffers,
                            binding,
                            buffer: buffer.raw,
                            offset,
                            size,
                        });

                        offset += size;
                    }
                    pso::Descriptor::CombinedImageSampler(view, _layout, sampler) => {
                        match view {
                            n::ImageView::Texture(tex, _)
                            | n::ImageView::TextureLayer(tex, _, _) => {
                                bindings.push(n::DescSetBindings::Texture(binding, *tex))
                            }
                            n::ImageView::Surface(_) => unimplemented!(),
                        }
                        match sampler {
                            n::FatSampler::Sampler(sampler) => {
                                bindings.push(n::DescSetBindings::Sampler(binding, *sampler))
                            }
                            n::FatSampler::Info(info) => bindings
                                .push(n::DescSetBindings::SamplerInfo(binding, info.clone())),
                        }
                    }
                    pso::Descriptor::Image(view, _layout) => match view {
                        n::ImageView::Texture(tex, _) | n::ImageView::TextureLayer(tex, _, _) => {
                            bindings.push(n::DescSetBindings::Texture(binding, *tex))
                        }
                        n::ImageView::Surface(_) => panic!(
                            "Texture was created with only render target usage which is invalid."
                        ),
                    },
                    pso::Descriptor::Sampler(sampler) => match sampler {
                        n::FatSampler::Sampler(sampler) => {
                            bindings.push(n::DescSetBindings::Sampler(binding, *sampler))
                        }
                        n::FatSampler::Info(info) => {
                            bindings.push(n::DescSetBindings::SamplerInfo(binding, info.clone()))
                        }
                    },
                    pso::Descriptor::UniformTexelBuffer(_view) => unimplemented!(),
                    pso::Descriptor::StorageTexelBuffer(_view) => unimplemented!(),
                }
            }
        }
    }

    unsafe fn copy_descriptor_sets<'a, I>(&self, copies: I)
    where
        I: IntoIterator,
        I::Item: Borrow<pso::DescriptorSetCopy<'a, B>>,
    {
        for _copy in copies {
            unimplemented!()
        }
    }

    fn create_semaphore(&self) -> Result<n::Semaphore, d::OutOfMemory> {
        Ok(n::Semaphore)
    }

    fn create_fence(&self, signalled: bool) -> Result<n::Fence, d::OutOfMemory> {
        let sync = if signalled && self.share.private_caps.sync {
            let gl = &self.share.context;
            unsafe { gl.FenceSync(gl::SYNC_GPU_COMMANDS_COMPLETE, 0) }
        } else {
            ptr::null()
        };
        Ok(n::Fence::new(sync))
    }

    unsafe fn reset_fences<I>(&self, fences: I) -> Result<(), d::OutOfMemory>
    where
        I: IntoIterator,
        I::Item: Borrow<n::Fence>,
    {
        let gl = &self.share.context;
        for fence in fences {
            let fence = fence.borrow();
            let sync = fence.0.get();
            if !sync.is_null() {
                if self.share.private_caps.sync && gl.IsSync(sync) == gl::TRUE {
                    gl.DeleteSync(sync);
                }
            }
            fence.0.set(ptr::null())
        }
        Ok(())
    }

    unsafe fn wait_for_fence(
        &self,
        fence: &n::Fence,
        timeout_ns: u64,
    ) -> Result<bool, d::OomOrDeviceLost> {
        if !self.share.private_caps.sync {
            return Ok(true);
        }
        match wait_fence(fence, &self.share, timeout_ns) {
            gl::TIMEOUT_EXPIRED => Ok(false),
            gl::WAIT_FAILED => {
                if let Err(err) = self.share.check() {
                    error!("Error when waiting on fence: {:?}", err);
                }
                Ok(false)
            }
            _ => Ok(true),
        }
    }

    unsafe fn get_fence_status(&self, _: &n::Fence) -> Result<bool, d::DeviceLost> {
        unimplemented!()
    }

    unsafe fn free_memory(&self, _memory: n::Memory) {
        // Nothing to do
    }

    unsafe fn create_query_pool(
        &self,
        _ty: query::Type,
        _count: query::Id,
    ) -> Result<(), query::CreationError> {
        unimplemented!()
    }

    unsafe fn destroy_query_pool(&self, _: ()) {
        unimplemented!()
    }

    unsafe fn get_query_pool_results(
        &self,
        _pool: &(),
        _queries: Range<query::Id>,
        _data: &mut [u8],
        _stride: buffer::Offset,
        _flags: query::ResultFlags,
    ) -> Result<bool, d::OomOrDeviceLost> {
        unimplemented!()
    }

    unsafe fn destroy_shader_module(&self, _: n::ShaderModule) {
        // Assumes compiled shaders are managed internally
    }

    unsafe fn destroy_render_pass(&self, _: n::RenderPass) {
        // Nothing to do
    }

    unsafe fn destroy_pipeline_layout(&self, _: n::PipelineLayout) {
        // Nothing to do
    }

    unsafe fn destroy_graphics_pipeline(&self, pipeline: n::GraphicsPipeline) {
        self.share.context.DeleteProgram(pipeline.program);
    }

    unsafe fn destroy_compute_pipeline(&self, pipeline: n::ComputePipeline) {
        self.share.context.DeleteProgram(pipeline.program);
    }

    unsafe fn destroy_framebuffer(&self, frame_buffer: n::FrameBuffer) {
        let gl = &self.share.context;
        gl.DeleteFramebuffers(1, &frame_buffer);
    }

    unsafe fn destroy_buffer(&self, buffer: n::Buffer) {
        self.share.context.DeleteBuffers(1, &buffer.raw);
    }
    unsafe fn destroy_buffer_view(&self, _: n::BufferView) {
        // Nothing to do
    }

    unsafe fn destroy_image(&self, image: n::Image) {
        let gl = &self.share.context;
        match image.kind {
            n::ImageKind::Surface(rb) => gl.DeleteRenderbuffers(1, &rb),
            n::ImageKind::Texture(t) => gl.DeleteTextures(1, &t),
        }
    }

    unsafe fn destroy_image_view(&self, _image_view: n::ImageView) {
        // Nothing to do
    }

    unsafe fn destroy_sampler(&self, sampler: n::FatSampler) {
        let gl = &self.share.context;
        match sampler {
            n::FatSampler::Sampler(s) => gl.DeleteSamplers(1, &s),
            _ => (),
        }
    }

    unsafe fn destroy_descriptor_pool(&self, _: n::DescriptorPool) {
        // Nothing to do
    }

    unsafe fn destroy_descriptor_set_layout(&self, _: n::DescriptorSetLayout) {
        // Nothing to do
    }

    unsafe fn destroy_fence(&self, fence: n::Fence) {
        let gl = &self.share.context;
        let sync = fence.0.get();
        if self.share.private_caps.sync && gl.IsSync(sync) == gl::TRUE {
            gl.DeleteSync(sync);
        }
    }

    unsafe fn destroy_semaphore(&self, _: n::Semaphore) {
        // Nothing to do
    }

    unsafe fn create_swapchain(
        &self,
        surface: &mut Surface,
        config: c::SwapchainConfig,
        _old_swapchain: Option<Swapchain>,
    ) -> Result<(Swapchain, c::Backbuffer<B>), c::window::CreationError> {
        Ok(self.create_swapchain_impl(surface, config))
    }

    unsafe fn destroy_swapchain(&self, _swapchain: Swapchain) {
        // Nothing to do
    }

    fn wait_idle(&self) -> Result<(), error::HostExecutionError> {
        unsafe {
            self.share.context.Finish();
        }
        Ok(())
    }
}

pub(crate) fn wait_fence(fence: &n::Fence, share: &Starc<Share>, timeout_ns: u64) -> GLenum {
    // TODO:
    // This can be called by multiple objects wanting to ensure they have exclusive
    // access to a resource. How much does this call costs ? The status of the fence
    // could be cached to avoid calling this more than once (in core or in the backend ?).
    let gl = &share.context;
    unsafe {
        if share.private_caps.sync {
            gl.ClientWaitSync(fence.0.get(), gl::SYNC_FLUSH_COMMANDS_BIT, timeout_ns)
        } else {
            // We fallback to waiting for *everything* to finish
            gl.Flush();
            gl::CONDITION_SATISFIED
        }
    }
}
