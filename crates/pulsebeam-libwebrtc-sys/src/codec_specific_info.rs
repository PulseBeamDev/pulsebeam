//! Owning SDK encoder-output metadata. Values are not RTP header projections.
use crate::{VideoCodecType, Vp9GofInfo, ffi};
use std::fmt;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CodecDependencyTemplate {
    pub spatial_id: i32,
    pub temporal_id: i32,
    /// Full fixed-int DecodeTargetIndication storage, not a two-bit wire value.
    pub decode_target_indications: Vec<i32>,
    pub frame_diffs: Vec<i32>,
    pub chain_diffs: Vec<i32>,
}
impl CodecDependencyTemplate {
    fn native(&self) -> ffi::FfiCodecDependencyTemplate {
        ffi::FfiCodecDependencyTemplate {
            spatial_id: self.spatial_id,
            temporal_id: self.temporal_id,
            indications: self.decode_target_indications.clone(),
            frame_diffs: self.frame_diffs.clone(),
            chain_diffs: self.chain_diffs.clone(),
        }
    }
    fn from_native(v: ffi::FfiCodecDependencyTemplate) -> Self {
        Self {
            spatial_id: v.spatial_id,
            temporal_id: v.temporal_id,
            decode_target_indications: v.indications,
            frame_diffs: v.frame_diffs,
            chain_diffs: v.chain_diffs,
        }
    }
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CodecDependencyStructure {
    pub structure_id: i32,
    pub num_decode_targets: i32,
    pub num_chains: i32,
    pub decode_target_protected_by_chain: Vec<i32>,
    pub resolutions: Vec<crate::RtcEventRenderResolution>,
    pub templates: Vec<CodecDependencyTemplate>,
}
impl CodecDependencyStructure {
    fn native(&self) -> ffi::FfiCodecDependencyStructure {
        ffi::FfiCodecDependencyStructure {
            structure_id: self.structure_id,
            decode_targets: self.num_decode_targets,
            chains: self.num_chains,
            protected_by_chain: self.decode_target_protected_by_chain.clone(),
            resolutions: self
                .resolutions
                .iter()
                .map(|r| ffi::FfiRtcEventResolution {
                    width: r.width,
                    height: r.height,
                })
                .collect(),
            templates: self
                .templates
                .iter()
                .map(CodecDependencyTemplate::native)
                .collect(),
        }
    }
    fn from_native(v: ffi::FfiCodecDependencyStructure) -> Self {
        Self {
            structure_id: v.structure_id,
            num_decode_targets: v.decode_targets,
            num_chains: v.chains,
            decode_target_protected_by_chain: v.protected_by_chain,
            resolutions: v
                .resolutions
                .into_iter()
                .map(|r| crate::RtcEventRenderResolution {
                    width: r.width,
                    height: r.height,
                })
                .collect(),
            templates: v
                .templates
                .into_iter()
                .map(CodecDependencyTemplate::from_native)
                .collect(),
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CodecBufferUsage {
    pub id: i32,
    pub referenced: bool,
    pub updated: bool,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GenericCodecFrameInfo {
    pub dependencies: CodecDependencyTemplate,
    pub encoder_buffers: Vec<CodecBufferUsage>,
    pub part_of_chain: Vec<bool>,
    pub active_decode_targets: u32,
}
#[derive(Clone, Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "Preserve the public owning value variants without introducing indirection."
)]
pub enum CodecSpecificParameters {
    Vp8 {
        non_reference: bool,
        temporal_idx: u8,
        layer_sync: bool,
        key_idx: i8,
        use_explicit_dependencies: bool,
        referenced_buffers: [usize; 3],
        referenced_buffers_count: usize,
        updated_buffers: [usize; 3],
        updated_buffers_count: usize,
    },
    Vp9 {
        first_frame_in_picture: bool,
        inter_pic_predicted: bool,
        flexible_mode: bool,
        ss_data_available: bool,
        non_ref_for_inter_layer_pred: bool,
        temporal_idx: u8,
        temporal_up_switch: bool,
        inter_layer_predicted: bool,
        gof_idx: u8,
        num_spatial_layers: usize,
        first_active_layer: usize,
        spatial_layer_resolution_present: bool,
        width: [u16; 8],
        height: [u16; 8],
        gof: Vp9GofInfo,
        num_ref_pics: u8,
        p_diff: [u8; 3],
    },
    H264 {
        packetization_mode: i32,
        temporal_idx: u8,
        base_layer_sync: bool,
        idr_frame: bool,
    },
}
#[derive(Clone, Debug)]
pub struct CodecSpecificInfoFields {
    pub codec_type: VideoCodecType,
    pub codec_specific: Option<CodecSpecificParameters>,
    pub end_of_picture: bool,
    pub generic_frame_info: Option<GenericCodecFrameInfo>,
    pub template_structure: Option<CodecDependencyStructure>,
    /// Full native fixed-u8 scalability enum, distinct from its fallible name.
    pub scalability_mode: Option<u8>,
    pub frame_instrumentation_data: Option<crate::FrameInstrumentationData>,
}
/// Independent native value. Merely storing metadata does not promise its
/// safety for an installed packetizer, whose current consumer context matters.
pub struct OwnedCodecSpecificInfo {
    native: cxx::UniquePtr<ffi::NativeCodecSpecificInfo>,
}
// SAFETY: all fields and transitive data are owned values with no callbacks.
unsafe impl Send for OwnedCodecSpecificInfo {}
unsafe impl Sync for OwnedCodecSpecificInfo {}
impl Default for OwnedCodecSpecificInfo {
    fn default() -> Self {
        Self {
            native: ffi::codec_specific_new(),
        }
    }
}
impl Clone for OwnedCodecSpecificInfo {
    fn clone(&self) -> Self {
        Self {
            native: ffi::codec_specific_clone(self.native()),
        }
    }
}
impl fmt::Debug for OwnedCodecSpecificInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.fields().fmt(f)
    }
}
impl OwnedCodecSpecificInfo {
    pub(crate) fn native(&self) -> &ffi::NativeCodecSpecificInfo {
        self.native.as_ref().expect("owning codec metadata")
    }
    pub(crate) fn from_native(native: cxx::UniquePtr<ffi::NativeCodecSpecificInfo>) -> Self {
        assert!(!native.is_null());
        Self { native }
    }
    pub fn fields(&self) -> CodecSpecificInfoFields {
        let v = ffi::codec_specific_fields(self.native());
        let codec_type = VideoCodecType::from_native(v.codec_type);
        let codec_specific = match codec_type {
            VideoCodecType::Vp8 => Some(CodecSpecificParameters::Vp8 {
                non_reference: v.vp8_non_reference,
                temporal_idx: v.vp8_temporal_idx,
                layer_sync: v.vp8_layer_sync,
                key_idx: v.vp8_key_idx,
                use_explicit_dependencies: v.vp8_explicit_dependencies,
                referenced_buffers: v.vp8_referenced,
                referenced_buffers_count: v.vp8_referenced_count,
                updated_buffers: v.vp8_updated,
                updated_buffers_count: v.vp8_updated_count,
            }),
            VideoCodecType::Vp9 => Some(CodecSpecificParameters::Vp9 {
                first_frame_in_picture: v.vp9_first_frame,
                inter_pic_predicted: v.vp9_inter_pic,
                flexible_mode: v.vp9_flexible,
                ss_data_available: v.vp9_ss_available,
                non_ref_for_inter_layer_pred: v.vp9_non_ref_inter_layer,
                temporal_idx: v.vp9_temporal_idx,
                temporal_up_switch: v.vp9_temporal_up_switch,
                inter_layer_predicted: v.vp9_inter_layer,
                gof_idx: v.vp9_gof_idx,
                num_spatial_layers: v.vp9_spatial_layers,
                first_active_layer: v.vp9_first_active,
                spatial_layer_resolution_present: v.vp9_has_resolution,
                width: v.vp9_width,
                height: v.vp9_height,
                gof: Vp9GofInfo::from_native(v.vp9_gof),
                num_ref_pics: v.vp9_ref_count,
                p_diff: v.vp9_p_diff,
            }),
            VideoCodecType::H264 => Some(CodecSpecificParameters::H264 {
                packetization_mode: v.h264_packetization,
                temporal_idx: v.h264_temporal_idx,
                base_layer_sync: v.h264_base_sync,
                idr_frame: v.h264_idr,
            }),
            _ => None,
        };
        CodecSpecificInfoFields {
            codec_type,
            codec_specific,
            end_of_picture: v.end_of_picture,
            generic_frame_info: v.has_generic.then(|| GenericCodecFrameInfo {
                dependencies: CodecDependencyTemplate::from_native(v.generic),
                encoder_buffers: v
                    .buffers
                    .into_iter()
                    .map(|b| CodecBufferUsage {
                        id: b.id,
                        referenced: b.referenced,
                        updated: b.updated,
                    })
                    .collect(),
                part_of_chain: v.part_of_chain,
                active_decode_targets: v.active_targets,
            }),
            template_structure: v
                .has_structure
                .then(|| CodecDependencyStructure::from_native(v.structure)),
            scalability_mode: v.has_scalability.then_some(v.scalability),
            frame_instrumentation_data: (!v.instrumentation.is_null()).then(|| {
                crate::FrameInstrumentationData {
                    native: v.instrumentation,
                }
            }),
        }
    }
    pub fn set_fields(&mut self, fields: CodecSpecificInfoFields) -> Result<(), crate::CodecError> {
        let mut v = ffi::codec_specific_fields(self.native());
        v.codec_type = match fields.codec_type {
            VideoCodecType::Generic => 0,
            VideoCodecType::Vp8 => 1,
            VideoCodecType::Vp9 => 2,
            VideoCodecType::Av1 => 3,
            VideoCodecType::H264 => 4,
            VideoCodecType::H265 => 5,
            VideoCodecType::Native(v) => v,
        };
        match (fields.codec_type, fields.codec_specific) {
            (
                VideoCodecType::Vp8,
                Some(CodecSpecificParameters::Vp8 {
                    non_reference,
                    temporal_idx,
                    layer_sync,
                    key_idx,
                    use_explicit_dependencies,
                    referenced_buffers,
                    referenced_buffers_count,
                    updated_buffers,
                    updated_buffers_count,
                }),
            ) => {
                v.vp8_non_reference = non_reference;
                v.vp8_temporal_idx = temporal_idx;
                v.vp8_layer_sync = layer_sync;
                v.vp8_key_idx = key_idx;
                v.vp8_explicit_dependencies = use_explicit_dependencies;
                v.vp8_referenced = referenced_buffers;
                v.vp8_referenced_count = referenced_buffers_count;
                v.vp8_updated = updated_buffers;
                v.vp8_updated_count = updated_buffers_count;
            }
            (
                VideoCodecType::Vp9,
                Some(CodecSpecificParameters::Vp9 {
                    first_frame_in_picture,
                    inter_pic_predicted,
                    flexible_mode,
                    ss_data_available,
                    non_ref_for_inter_layer_pred,
                    temporal_idx,
                    temporal_up_switch,
                    inter_layer_predicted,
                    gof_idx,
                    num_spatial_layers,
                    first_active_layer,
                    spatial_layer_resolution_present,
                    width,
                    height,
                    gof,
                    num_ref_pics,
                    p_diff,
                }),
            ) => {
                v.vp9_first_frame = first_frame_in_picture;
                v.vp9_inter_pic = inter_pic_predicted;
                v.vp9_flexible = flexible_mode;
                v.vp9_ss_available = ss_data_available;
                v.vp9_non_ref_inter_layer = non_ref_for_inter_layer_pred;
                v.vp9_temporal_idx = temporal_idx;
                v.vp9_temporal_up_switch = temporal_up_switch;
                v.vp9_inter_layer = inter_layer_predicted;
                v.vp9_gof_idx = gof_idx;
                v.vp9_spatial_layers = num_spatial_layers;
                v.vp9_first_active = first_active_layer;
                v.vp9_has_resolution = spatial_layer_resolution_present;
                v.vp9_width = width;
                v.vp9_height = height;
                v.vp9_gof = gof.to_native();
                v.vp9_ref_count = num_ref_pics;
                v.vp9_p_diff = p_diff;
            }
            (
                VideoCodecType::H264,
                Some(CodecSpecificParameters::H264 {
                    packetization_mode,
                    temporal_idx,
                    base_layer_sync,
                    idr_frame,
                }),
            ) => {
                v.h264_packetization = packetization_mode;
                v.h264_temporal_idx = temporal_idx;
                v.h264_base_sync = base_layer_sync;
                v.h264_idr = idr_frame;
            }
            (VideoCodecType::Vp8 | VideoCodecType::Vp9 | VideoCodecType::H264, _) => {
                return Err(crate::CodecError::InvalidConfiguration);
            }
            (_, None) => {}
            _ => return Err(crate::CodecError::InvalidConfiguration),
        }
        v.end_of_picture = fields.end_of_picture;
        v.has_generic = fields.generic_frame_info.is_some();
        if let Some(g) = fields.generic_frame_info {
            v.generic = g.dependencies.native();
            v.buffers = g
                .encoder_buffers
                .into_iter()
                .map(|b| ffi::FfiCodecBufferUsage {
                    id: b.id,
                    referenced: b.referenced,
                    updated: b.updated,
                })
                .collect();
            v.part_of_chain = g.part_of_chain;
            v.active_targets = g.active_decode_targets;
        }
        v.has_structure = fields.template_structure.is_some();
        if let Some(s) = fields.template_structure {
            v.structure = s.native();
        }
        v.has_scalability = fields.scalability_mode.is_some();
        v.scalability = fields.scalability_mode.unwrap_or(0);
        v.instrumentation = fields
            .frame_instrumentation_data
            .map_or_else(cxx::UniquePtr::null, |d| d.native);
        if ffi::codec_specific_set_fields(self.native.pin_mut(), v) {
            Ok(())
        } else {
            Err(crate::CodecError::InvalidConfiguration)
        }
    }
}
/// Exact EncodedImageCallback::Result, not a boolean success projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EncodedImageCallbackError {
    Ok,
    SendFailed,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EncodedImageCallbackResult {
    pub error: EncodedImageCallbackError,
    pub frame_id: u32,
    pub drop_next_frame: bool,
}
impl EncodedImageCallbackResult {
    pub(crate) fn native(self) -> ffi::FfiEncodedCallbackResult {
        ffi::FfiEncodedCallbackResult {
            invoked: true,
            error: match self.error {
                EncodedImageCallbackError::Ok => 0,
                EncodedImageCallbackError::SendFailed => 1,
            },
            frame_id: self.frame_id,
            drop_next_frame: self.drop_next_frame,
        }
    }
    pub(crate) fn from_native(v: ffi::FfiEncodedCallbackResult) -> Self {
        Self {
            error: if v.error == 0 {
                EncodedImageCallbackError::Ok
            } else {
                EncodedImageCallbackError::SendFailed
            },
            frame_id: v.frame_id,
            drop_next_frame: v.drop_next_frame,
        }
    }
}
