use std::sync::Arc;

use gyroflow_core::{undistortion, StabilizationManager};
use lru::LruCache;
use measure_time::*;
use ofx::*;

plugin_module!(
    "nl.smslv.gyroflowofx.fisheyestab_v1",
    ApiVersion(1),
    PluginVersion(1, 0),
    FisheyeStabilizerPlugin::new
);

#[derive(Default)]
struct FisheyeStabilizerPlugin {
}

impl FisheyeStabilizerPlugin {
    pub fn new() -> FisheyeStabilizerPlugin {
        FisheyeStabilizerPlugin::default()
    }
}
#[allow(unused)]
struct InstanceData {
    source_clip: ClipInstance,
    output_clip: ClipInstance,

    param_gyrodata: ParamHandle<String>,
    gyrodata: LruCache<String, Arc<StabilizationManager::<undistortion::RGBAf>>>,
}

impl InstanceData {
    fn gyrodata(&mut self, width: usize, height: usize) -> Result<Arc<StabilizationManager::<undistortion::RGBAf>>> {
        let gyrodata_filename = self.param_gyrodata.get_value()?;
        error!("filename: {gyrodata_filename}");
        let gyrodata = if let Some(gyrodata) = self.gyrodata.get(&gyrodata_filename) {
            gyrodata.clone()
        } else {
            let gyrodata = StabilizationManager::default();
            let gyrodata_json = gyrodata.import_gyroflow(&gyrodata_filename).map_err(|e| {
                error!("load_gyro_data error: {}", &e);
                Error::UnknownError
            })?;

            if let Some(vid_info) = gyrodata_json.get("video_info") {
                let duration_ms = vid_info.get("duration_ms").and_then(|x| x.as_f64()).unwrap_or_default();
                let fps = vid_info.get("fps").and_then(|x| x.as_f64()).unwrap_or_default();
                let vfr_fps = vid_info.get("vfr_fps").and_then(|x| x.as_f64()).unwrap_or_default();
                let num_frames = vid_info.get("num_frames").and_then(|x| x.as_u64()).unwrap_or_default() as usize;
                let width = vid_info.get("width").and_then(|x| x.as_u64()).unwrap_or_default() as usize;
                let height = vid_info.get("height").and_then(|x| x.as_u64()).unwrap_or_default() as usize;

                let mut params = gyrodata.params.write();
                params.framebuffer_inverted = true;
                params.frame_count = num_frames;
                params.fps = fps;
                params.duration_ms = duration_ms;
                params.fps_scale = if (vfr_fps - fps).abs() > 0.001 {
                    Some(vfr_fps / fps)
                } else {
                    None
                };
                params.video_size = (width, height);
            }

            if let Some(serde_json::Value::Object(vid_info)) = gyrodata_json.get("stabilization") {
                let fov = vid_info.get("fov").and_then(|x| x.as_f64()).unwrap_or_default();
                let method = vid_info.get("method").and_then(|x| x.as_str()).unwrap_or_default();
                let smoothing_params = vid_info.get("smoothing_params").and_then(|x| x.as_array()).map(|x| &x[..]).unwrap_or(&[]);

                let known_methods = gyrodata.get_smoothing_algs();
                let method_ix = known_methods.iter().enumerate().find(|(_, m)| method == m.as_str()).map(|(ix, _)| ix).unwrap_or_default();

                let mut smoothing = gyrodata.smoothing.write();
                gyrodata.params.write().fov = fov;
                smoothing.set_current(method_ix);

                for param in smoothing_params {
                    (|| -> Option<()> {
                        let name = param.get("name").and_then(|x| x.as_str())?;
                        let value = param.get("value").and_then(|x| x.as_f64())?;
                        smoothing.current().set_parameter(name, value);
                        Some(())
                    })();
                }
            }

            if let Some(serde_json::Value::Object(offsets)) = gyrodata_json.get("offsets") {
                gyrodata.gyro.write().offsets = offsets.iter().map(|(k, v)| {
                    (k.parse().unwrap(), v.as_f64().unwrap_or_default())
                }).collect();
            }

            gyrodata.recompute_blocking();

            self.gyrodata.put(gyrodata_filename.to_owned(), Arc::new(gyrodata));
            self.gyrodata.get(&gyrodata_filename).map(Arc::clone).ok_or(Error::UnknownError)?
        };

        {
            let (size, output_size) = {
                let params = gyrodata.params.read();
                (params.size, params.output_size)
            };

            if size != (width, height) || output_size != (width, height) {
                gyrodata.set_size(width, height);
                gyrodata.set_output_size(width, height);
            }
        }
        Ok(gyrodata)
    }
}

struct PerFrameParams {
}

const PARAM_MAIN_NAME: &str = "Main";

const PARAM_GYRODATA: &str = "gyrodata";

const PARAM_GYRODATA_LABEL: &str = "Gyroflow file";

impl Execute for FisheyeStabilizerPlugin {
    #[allow(clippy::float_cmp)]
    fn execute(&mut self, _plugin_context: &PluginContext, action: &mut Action) -> Result<Int> {
        use Action::*;

        match *action {
            Render(ref mut effect, ref in_args) => {
                let time = in_args.get_time()?;
                let instance_data: &mut InstanceData = effect.get_instance_data()?;

                let source_image = instance_data.source_clip.get_image(time)?;
                let output_image = instance_data.output_clip.get_image_mut(time)?;
                let output_image = output_image.borrow_mut();

                let src = source_image.get_descriptor::<RGBAColourF>()?;
                let dst = output_image.get_descriptor::<RGBAColourF>()?;

                let mut src_buf = src.data();
                let mut dst_buf = dst.data();

                let processed = {
                    let stab = instance_data.gyrodata(dst_buf.dimensions().0 as usize, dst_buf.dimensions().1 as usize)?;
                    let stab_params = stab.params.read();
                    let fps = stab_params.fps;
                    let timestamp_us = (time / fps * 1_000_000.0) as i64; 

                    stab.process_pixels(
                        timestamp_us,
                        src_buf.dimensions().0 as usize,
                        src_buf.dimensions().1 as usize,
                        src_buf.stride_bytes().abs() as usize,
                        dst_buf.dimensions().0 as usize,
                        dst_buf.dimensions().1 as usize,
                        dst_buf.stride_bytes().abs() as usize,
                        unsafe { std::slice::from_raw_parts_mut(src_buf.ptr_mut(0), src_buf.bytes()) },
                        unsafe { std::slice::from_raw_parts_mut(dst_buf.ptr_mut(0), dst_buf.bytes()) }
                    )
                };

                if effect.abort()? || !processed {
                    FAILED
                } else {
                    OK
                }
            }

            CreateInstance(ref mut effect) => {
                let param_set = effect.parameter_set()?;

                let source_clip = effect.get_simple_input_clip()?;
                let output_clip = effect.get_output_clip()?;

                let param_gyrodata = param_set.parameter(PARAM_GYRODATA)?;

                effect.set_instance_data(InstanceData {
                    source_clip,
                    output_clip,
                    param_gyrodata,
                    gyrodata: LruCache::new(1),
                })?;

                OK
            }

            DestroyInstance(ref mut _effect) => OK,

            DescribeInContext(ref mut effect, ref _in_args) => {
                let mut output_clip = effect.new_output_clip()?;
                output_clip.set_supported_components(&[ImageComponent::RGBA])?;

                let mut input_clip = effect.new_simple_input_clip()?;
                input_clip.set_supported_components(&[ImageComponent::RGBA])?;

                let mut param_set = effect.parameter_set()?;

                let mut param_props = param_set.param_define_string(PARAM_GYRODATA)?;

                param_props.set_string_type(ParamStringType::FilePath)?;
                param_props.set_label(PARAM_GYRODATA_LABEL)?;
                param_props.set_hint(PARAM_GYRODATA_LABEL)?;
                param_props.set_script_name(PARAM_GYRODATA)?;

                if let Some(parent) = None {
                    param_props.set_parent(parent)?;
                }

                param_set
                    .param_define_page(PARAM_MAIN_NAME)?
                    .set_children(&[
                        PARAM_GYRODATA,
                    ])?;

                OK
            }

            Describe(ref mut effect) => {
                let mut effect_properties: EffectDescriptor = effect.properties()?;
                effect_properties.set_grouping("Warp")?;

                effect_properties.set_label("Gyroflow (1.0)")?;
                effect_properties.set_short_label("Gyroflow")?;
                effect_properties.set_long_label("Gyroflow (>= 1.0)")?;

                effect_properties.set_supported_pixel_depths(&[BitDepth::Float])?;
                effect_properties.set_supported_contexts(&[ImageEffectContext::Filter])?;
                effect_properties.set_supports_tiles(false)?;

                effect_properties.set_single_instance(false)?;
                effect_properties.set_host_frame_threading(false)?;
                effect_properties.set_render_thread_safety(ImageEffectRender::FullySafe)?;

                OK
            }

            Load => OK,

            _ => REPLY_DEFAULT,
        }
    }
}

impl InstanceData {
    #[allow(unused)]
    fn get_per_frame_params(&self) -> Result<PerFrameParams> {
        Ok(PerFrameParams {
        })
    }
}