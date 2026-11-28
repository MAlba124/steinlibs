//! Graphics and compute pipelines on the shared layout.

use std::ffi::CString;

use ash::vk;

use super::convert::{blend, compare, cull, err, format, topology};
use super::{Doomed, PipeRec, Vk};
use crate::pipeline::{Blend, ComputeDesc, Pipeline, RasterDesc, Spirv};
use crate::sync::TimelinePoint;
use crate::{GpuError, MAX_COLOR};

impl Vk {
    pub(crate) fn create_pipeline(&mut self, d: &RasterDesc<'_>) -> Result<Pipeline, GpuError> {
        if !self.pipelines.has_room() {
            return Err(GpuError::Full("pipeline"));
        }
        if d.colors as usize > MAX_COLOR {
            return Err(GpuError::BadDesc("more colour targets than MAX_COLOR"));
        }
        if d.samples != 1 {
            // TODO(3d): MSAA.
            return Err(GpuError::BadDesc(
                "multisampled pipelines are not supported yet",
            ));
        }
        if d.blend == Blend::DualSource && !self.dual_source {
            return Err(GpuError::Missing("dualSrcBlend"));
        }
        let vs = self.module(&d.vertex)?;
        let fs = match self.module(&d.fragment) {
            Ok(m) => m,
            Err(e) => {
                self.drop_module(vs);
                return Err(e);
            }
        };
        let made = self.raster(d, vs, fs);
        self.drop_module(vs);
        self.drop_module(fs);
        let pipeline = made?;
        self.pipelines
            .insert(PipeRec {
                pipeline,
                bind: vk::PipelineBindPoint::GRAPHICS,
            })
            .map(Pipeline)
    }

    pub(crate) fn create_compute(&mut self, d: &ComputeDesc<'_>) -> Result<Pipeline, GpuError> {
        if !self.pipelines.has_room() {
            return Err(GpuError::Full("pipeline"));
        }
        let m = self.module(&d.shader)?;
        let made = entry(&d.shader).and_then(|name| {
            let stage = vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::COMPUTE)
                .module(m)
                .name(&name);
            let info = [vk::ComputePipelineCreateInfo::default()
                .stage(stage)
                .layout(self.g.layout)];
            // SAFETY: the module and layout are live, `name` outlives the call.
            let r = unsafe {
                self.device
                    .create_compute_pipelines(vk::PipelineCache::null(), &info, None)
            };
            r.map_err(|(_, e)| err(e))
                .map(|v| v.first().copied().unwrap_or_default())
        });
        self.drop_module(m);
        let pipeline = made?;
        self.pipelines
            .insert(PipeRec {
                pipeline,
                bind: vk::PipelineBindPoint::COMPUTE,
            })
            .map(Pipeline)
    }

    pub(crate) fn destroy_pipeline(
        &mut self,
        p: Pipeline,
        after: TimelinePoint,
    ) -> Result<(), GpuError> {
        let rec = self.pipelines.remove(p.0)?;
        self.defer(after, Doomed::Pipeline(rec.pipeline));
        Ok(())
    }

    pub(super) fn pipeline(&self, p: Pipeline) -> Result<&PipeRec, GpuError> {
        self.pipelines.get(p.0)
    }

    fn module(&self, s: &Spirv<'_>) -> Result<vk::ShaderModule, GpuError> {
        if s.words.first() != Some(&crate::pipeline::SPIRV_MAGIC) {
            return Err(GpuError::BadDesc("shader is not SPIR-V"));
        }
        let info = vk::ShaderModuleCreateInfo::default().code(s.words);
        // SAFETY: a plain create, the words start with the SPIR-V magic and
        // validation checks the rest.
        unsafe { self.device.create_shader_module(&info, None) }.map_err(err)
    }

    fn drop_module(&self, m: vk::ShaderModule) {
        // SAFETY: modules are only needed during pipeline creation.
        unsafe { self.device.destroy_shader_module(m, None) };
    }

    fn raster(
        &self,
        d: &RasterDesc<'_>,
        vs: vk::ShaderModule,
        fs: vk::ShaderModule,
    ) -> Result<vk::Pipeline, GpuError> {
        let (vname, fname) = (entry(&d.vertex)?, entry(&d.fragment)?);
        let stages = [
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::VERTEX)
                .module(vs)
                .name(&vname),
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(fs)
                .name(&fname),
        ];
        let colors = &d.color[..d.colors as usize];
        let formats: Vec<vk::Format> = colors.iter().map(|f| format(*f)).collect();
        let blends = vec![blend(d.blend); colors.len()];
        let vertex = vk::PipelineVertexInputStateCreateInfo::default();
        let assembly =
            vk::PipelineInputAssemblyStateCreateInfo::default().topology(topology(d.topology));
        let viewport = vk::PipelineViewportStateCreateInfo::default()
            .viewport_count(1)
            .scissor_count(1);
        let raster = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(cull(d.cull))
            .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
            .line_width(1.0);
        let ms = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);
        let mut depth = vk::PipelineDepthStencilStateCreateInfo::default();
        if let Some(z) = d.depth {
            depth = depth
                .depth_test_enable(true)
                .depth_write_enable(z.write)
                .depth_compare_op(compare(z.test));
        }
        let cb = vk::PipelineColorBlendStateCreateInfo::default().attachments(&blends);
        let dynamic = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dyn_state = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic);
        let mut rendering =
            vk::PipelineRenderingCreateInfo::default().color_attachment_formats(&formats);
        if let Some(z) = d.depth {
            rendering = rendering.depth_attachment_format(format(z.format));
        }
        let info = [vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&vertex)
            .input_assembly_state(&assembly)
            .viewport_state(&viewport)
            .rasterization_state(&raster)
            .multisample_state(&ms)
            .depth_stencil_state(&depth)
            .color_blend_state(&cb)
            .dynamic_state(&dyn_state)
            .layout(self.g.layout)
            .push_next(&mut rendering)];
        // SAFETY: every state struct and name outlives the call, the
        // modules and layout are live.
        let r = unsafe {
            self.device
                .create_graphics_pipelines(vk::PipelineCache::null(), &info, None)
        };
        r.map_err(|(_, e)| err(e))
            .map(|v| v.first().copied().unwrap_or_default())
    }
}

fn entry(s: &Spirv<'_>) -> Result<CString, GpuError> {
    CString::new(s.entry).map_err(|_| GpuError::BadDesc("entry point name holds a NUL"))
}
