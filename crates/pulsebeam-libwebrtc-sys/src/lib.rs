//! Low-level Rust integration with PulseBeam's local, pinned libwebrtc build.
#![allow(
    clippy::disallowed_types,
    reason = "Native callback ownership and threaded WebRTC runtimes require synchronization; this is not shard-owned SFU state."
)]

mod async_dns;
mod owned_dns_resolver;
pub use owned_dns_resolver::{
    DnsResolverContext, DnsResolverOperationError, DnsResolverQuery, OwnedDnsResolver,
    OwnedDnsResolverResult,
};
pub(crate) use owned_dns_resolver::{RustDnsCallback, owned_dns_callback_fire};
mod audio;
mod audio_source_options;
mod audio_source_view;
pub use audio_source_options::{
    AudioProcessingMode, AudioSourceControlError, AudioSourceOptions, AudioSourceState,
    AudioTrackState, AudioTrackStatus,
};
pub use audio_source_view::{AudioObserver, AudioObserverEvent, AudioSourceView};
mod audio_processing;
mod audio_processor_config;
pub use audio_processor_config::{
    ApmAdaptiveDigital, ApmAnalogGainController, ApmAnalogMicGainEmulation,
    ApmCaptureLevelAdjustment, ApmClippingPredictor, ApmClippingPredictorMode, ApmDownmixMethod,
    ApmEchoCanceller, ApmFixedDigital, ApmGainController1, ApmGainController1Mode,
    ApmGainController2, ApmHighPassFilter, ApmInputVolumeController, ApmNoiseSuppression,
    ApmNoiseSuppressionLevel, ApmPipeline, ApmPreAmplifier, ApmTransientSuppression,
    AudioProcessorConfig,
};
mod audio_echo_control;
mod audio_echo_factory_provider;
mod audio_processor;
mod audio_processor_caller_dump;
mod audio_processor_custom;
mod audio_processor_dump;
mod audio_processor_runtime;
pub use audio_echo_factory_provider::{EchoControlCreationContext, EchoControlFactoryProvider};
use audio_echo_factory_provider::{
    RustEchoEnvironment, apm_echo_environment_native, apm_echo_factory_create,
    apm_echo_factory_invalid,
};
mod audio_echo_dependencies;
pub use audio_echo_control::{
    EchoControl, EchoControlFactory, EchoControlMetrics, EchoControlSampleRate,
};
pub use audio_echo_dependencies::{EchoControlDependencies, NeuralResidualEchoEstimatorLoan};
mod audio_echo_config;
mod audio_processor_neural;
pub use audio_echo_config::{
    Aec3AlignmentMixing, Aec3Buffering, Aec3CoarseFilter, Aec3ComfortNoise, Aec3Delay,
    Aec3DelaySelectionThresholds, Aec3DominantNearendDetection, Aec3EchoAudibility, Aec3EchoModel,
    Aec3EchoRemovalControl, Aec3EpStrength, Aec3Erle, Aec3Filter, Aec3HighBandsSuppression,
    Aec3HighFrequencySuppression, Aec3MaskingThresholds, Aec3MultiChannel, Aec3RefinedFilter,
    Aec3RenderLevels, Aec3SubbandNearendDetection, Aec3SubbandRegion, Aec3Suppressor, Aec3Tuning,
    EchoCanceller3Config,
};
pub use audio_processor_neural::{
    AudioProcessingRows, NeuralEchoEstimate, NeuralEchoEstimateInput, NeuralEchoRenderBlock,
    NeuralResidualEchoEstimator, NeuralResidualEchoEstimatorProvider,
};
use audio_processor_neural::{
    apm_neural_adjust, apm_neural_adjust_invalid, apm_neural_estimate, apm_neural_invalid,
    apm_neural_reset,
};
mod audio_device_callbacks;
pub use audio_device_callbacks::{
    AudioDeviceCallbackFailure, AudioDeviceCallbackFailureKind, AudioDeviceCallbackFailureObserver,
    AudioDeviceCallbackOperation, AudioDeviceCapture, AudioDeviceCaptureResult,
    AudioDeviceObserver, AudioDeviceObserverProvider, AudioDevicePcmSpec, AudioDevicePlayoutResult,
    AudioDeviceRenderTiming, AudioDeviceSpeechActivity, AudioTransport, AudioTransportProvider,
};
use audio_device_callbacks::{
    RustAudioDeviceCallbacks, audio_device_callbacks_dispose, audio_device_callbacks_invalid,
    audio_device_observed, audio_device_recorded, audio_device_render,
};
#[cfg(all(feature = "native", target_os = "linux"))]
mod audio_device;
#[cfg(all(feature = "native", target_os = "linux"))]
pub use audio_device::{
    AudioDeviceCreationConfig, AudioDeviceCreationMethod, AudioDeviceError, AudioDeviceLayer,
    AudioDeviceModule, AudioDeviceName, AudioDeviceStatistics, InstalledAudioDevice,
    PlatformAudioProcessingState, PlatformAudioProcessingTopology,
};
use audio_processor::RustInstalledApmEnvironment;
pub use audio_processor::{
    AudioProcessingFormatState, AudioProcessingFormats, AudioProcessingModule,
    AudioProcessingStatistics, AudioProcessingStatus, AudioProcessingStream, AudioProcessorError,
};
pub use audio_processor_caller_dump::{
    AudioProcessingDump, AudioProcessingDumpConfig, AudioProcessingDumpEvent,
    AudioProcessingDumpFailure, AudioProcessingDumpFailureKind, AudioProcessingDumpFailureObserver,
    AudioProcessingDumpOperation, AudioProcessingDumpProvider, AudioProcessingDumpSamples,
    AudioProcessingDumpState,
};
use audio_processor_caller_dump::{
    RustCallerApmDump, apm_dump_audio, apm_dump_capture, apm_dump_config, apm_dump_dispose,
    apm_dump_initialize, apm_dump_runtime, apm_dump_state,
};
pub use audio_processor_custom::{
    AudioProcessingBuffer, AudioProcessingBufferMut, AudioProcessingCustomAnalyzer,
    AudioProcessingCustomAnalyzerProvider, AudioProcessingCustomFailure,
    AudioProcessingCustomFailureKind, AudioProcessingCustomFailureObserver,
    AudioProcessingCustomOperation, AudioProcessingCustomProcessor,
    AudioProcessingCustomProcessorProvider, AudioProcessingEchoDetector,
    AudioProcessingEchoDetectorMetrics, AudioProcessingEchoDetectorProvider,
    BuiltinAudioProcessorBuilder, CustomAudioProcessorBuilder,
};
use audio_processor_custom::{
    RustApmCustomModule, apm_custom_analyze, apm_custom_description, apm_custom_dispose,
    apm_custom_initialize, apm_custom_process, apm_custom_runtime, apm_custom_unqualified_render,
    apm_echo_analyze, apm_echo_initialize, apm_echo_metrics,
};
use audio_processor_dump::{RustApmDumpWorker, apm_dump_worker_queue};
pub use audio_processor_runtime::{AudioPlayoutDeviceInfo, AudioProcessingRuntimeSetting};
mod audio_echo_provider;
pub use audio_echo_provider::{
    EchoControlAudio, EchoControlAudioMut, EchoControlLinearOutput, EchoControlProvider,
};
use audio_echo_provider::{
    apm_echo_control_active, apm_echo_control_analyze, apm_echo_control_delay,
    apm_echo_control_invalid, apm_echo_control_metrics, apm_echo_control_process,
    apm_echo_control_usage,
};
mod audio_mixer;
mod audio_mixer_provider;
pub use audio_mixer::{
    AudioMixer, AudioMixerError, AudioMixerFailureObserver, AudioMixerSource,
    AudioMixerSourceProvider, MixerAudioFrame,
};
use audio_mixer::{RustMixerModule, mixer_module_failed};
use audio_mixer::{
    RustMixerSource, mixer_source_frame, mixer_source_invalid, mixer_source_rate, mixer_source_ssrc,
};
pub use audio_mixer_provider::{
    AudioMixerInputs, AudioMixerProvider, AudioMixerSourceInfo, AudioMixerSourceToken,
};
use audio_mixer_provider::{
    RustMixerProvider, mixer_provider_add, mixer_provider_invalid, mixer_provider_mix,
    mixer_provider_remove,
};
mod certificate;
mod certificate_generator;
pub use async_dns::{
    AsyncDnsResolver, AsyncDnsResolverFactory, AsyncDnsResolverFactoryProvider,
    AsyncDnsResolverResult, DnsResolutionCompletion, DnsResolutionCompletionError,
    DnsResolverResult,
};
pub(crate) use async_dns::{
    RustAsyncDnsFactory, RustAsyncDnsResolver, RustAsyncDnsResult, async_dns_factory_create,
    async_dns_factory_create_and_resolve, async_dns_factory_present, async_dns_result_address,
    async_dns_result_error, async_dns_result_failed, async_dns_result_identity,
    async_dns_result_view, async_dns_source_failed, async_dns_start,
};
pub use certificate_generator::{
    CertificateCompletion, CertificateCompletionError, CertificateGenerator,
    CertificateGeneratorProvider,
};
pub(crate) use certificate_generator::{
    RustCertificateGenerator, certificate_generator_generate, certificate_generator_present,
};
mod codec;
mod panic_payload;
mod video_factory_values;
#[cfg(test)]
mod video_factory_values_tests;
pub(crate) use codec::{decoder_factory_clone, encoder_factory_clone};
pub(crate) use video_factory_values::{
    RustVideoFactoryEnvironment, video_factory_environment_clone,
    video_factory_environment_from_sdk, video_factory_environment_native,
};
pub use video_factory_values::{
    VideoCodecCreationContext, VideoDecoderFactoryProvider, VideoEncoderFactoryProvider,
    VideoFactoryFailureObserver, VideoFactoryQueryError,
};
mod fec;
pub use fec::{
    FecCallbackOperation, FecController, FecControllerCreation, FecControllerEnvironment,
    FecControllerFactory, FecControllerFactoryProvider, FecEncodedFrameType, FecEncodingData,
    FecMaskType, FecProtectionCallback, FecProtectionCallbackError, FecProtectionParams,
    FecProtectionResponse, FecRateUpdate, FecRetransmissionMode,
};
pub(crate) use fec::{
    RustFecCompletion, RustFecController, RustFecControllerFactory, fec_async_complete,
    fec_controller_builtin, fec_controller_create, fec_controller_set_callback,
    fec_controller_set_encoding, fec_controller_set_method, fec_controller_update_encoded,
    fec_controller_update_rates, fec_controller_use_mask, fec_factory_present,
};
mod network_control;
pub use network_control::{
    GoogCcFactoryConfig, NetworkController, NetworkControllerConfig, NetworkControllerEnvironment,
    NetworkControllerFactory, NetworkControllerFactoryProvider, NetworkControllerFailure,
};
pub(crate) use network_control::{
    RustNetworkController, RustNetworkControllerFactory, network_controller_create,
    network_controller_ecn, network_controller_factory_present, network_controller_factory_retain,
    network_controller_goog_cc_estimator, network_controller_goog_cc_predictor,
    network_controller_goog_cc_rfc8888, network_controller_interval, network_controller_invalid,
    network_controller_is_goog_cc, network_controller_update,
};
mod network_state_estimator;
pub use network_state_estimator::{
    NetworkStateEstimator, NetworkStateEstimatorFactory, NetworkStateEstimatorFactoryProvider,
    NetworkStateEstimatorFailure,
};
pub(crate) use network_state_estimator::{
    RustNetworkStateEstimator, RustNetworkStateEstimatorFactory, network_estimator_create,
    network_estimator_estimate, network_estimator_factory_present, network_estimator_feedback,
    network_estimator_invalid, network_estimator_present, network_estimator_received,
    network_estimator_route,
};
mod network_control_values;
/// Network-controller input, distinct from a received simulated socket packet.
pub use network_control_values::ReceivedPacket as NetworkControlReceivedPacket;
pub use network_control_values::*;
mod network_predictor;
pub use network_predictor::{
    BandwidthUsage, NetworkStatePredictor, NetworkStatePredictorFactory,
    NetworkStatePredictorFactoryProvider,
};
pub(crate) use network_predictor::{
    RustNetworkStatePredictor, RustNetworkStatePredictorFactory, network_predictor_create,
    network_predictor_factory_present, network_predictor_present, network_predictor_update,
};
mod data_rate;
pub use data_rate::DataRate;
mod video_metadata;
pub use video_metadata::FrameInstrumentationData;
mod video_frame_metadata_headers;
pub use video_frame_metadata_headers::{
    H264NaluInfo, H264PacketizationMode, H264PacketizationType, RtpVideoHeaderCodecSpecifics,
    RtpVideoHeaderH264, RtpVideoHeaderVp8, RtpVideoHeaderVp9, Vp9GofInfo, Vp9TemporalStructureMode,
};
mod rtp_video_header;
pub use rtp_video_header::{
    RtpVideoGenericDescriptorInfo, RtpVideoHeader, RtpVideoHeaderValues, RtpVideoTypeHeader,
};
mod video_frame_metadata;
pub use video_frame_metadata::{
    VideoFrameMetadata, VideoFrameMetadataValues, VideoMetadataContentType,
    VideoMetadataDecodeTargetIndication, VideoMetadataError, VideoMetadataFrameType,
    VideoMetadataRotation,
};
mod video_bitrate;
pub(crate) use video_bitrate::{
    RustVideoBitrateAllocator, RustVideoBitrateAllocatorFactory, video_bitrate_allocator_allocate,
    video_bitrate_allocator_builtin, video_bitrate_allocator_get_allocation,
    video_bitrate_allocator_set_legacy, video_bitrate_factory_builtin,
    video_bitrate_factory_create, video_bitrate_factory_present,
};
pub use video_bitrate::{
    VideoAllocationDataRate, VideoBitrateAllocation, VideoBitrateAllocationParameters,
    VideoBitrateAllocator, VideoBitrateAllocatorEnvironment, VideoBitrateAllocatorFactory,
    VideoBitrateAllocatorFactoryProvider, VideoBitrateAllocatorProduct, VideoCodecConfiguration,
    VideoCodecLayer, VideoCodecMode, VideoCodecSpecificConfiguration, VideoCodecType,
    VideoEncoderComplexity, VideoInterLayerPrediction,
};
mod rtp_header_values;
pub use rtp_header_values::{
    AudioLevel, ColorSpace, HdrChromaticity, HdrMasteringMetadata, HdrMetadata, RtpFeedbackRequest,
    RtpHeaderExtensionValues, RtpHeaderValues, RtpValueError, VideoPlayoutDelay, VideoSendTiming,
};
mod rtp_sources;
pub use rtp_sources::{RtpSource, RtpSourceExtensions, RtpSourceType};
mod rtp_packet_values;
pub use rtp_packet_values::{AbsoluteCaptureTime, RtpReceivedPacketInfo, RtpReceivedPacketInfos};
mod local_media_stream;
pub use local_media_stream::{LocalMediaStream, MediaStreamEvent, MediaStreamObserver};
mod audio_track_processor;
pub use audio_track_processor::{
    AudioProcessingStats, AudioProcessorStatistics, AudioTrackProcessor,
};
mod pcm_view;
pub use pcm_view::{
    DeinterleavedPcm, DeinterleavedPcmMut, InterleavedPcm, InterleavedPcmMut, MonoPcm, MonoPcmMut,
    PcmLayout, PcmRead, PcmSample, PcmViewError, PcmViewInfo, PcmWrite, copy_samples,
};
mod audio_frame_mut;
mod audio_frame_values;
pub use audio_frame_mut::AudioFrameMut;
pub use audio_frame_values::{
    AudioChannel, AudioFrame, AudioFrameFields, AudioFrameSpeechType, AudioFrameVadActivity,
    AudioFrameValueError, ChannelLayout,
};
mod neteq_safety;
mod neteq_sdk_safety;
mod neteq_values;
pub(crate) use neteq_safety::receive_time_bits_are_safe as neteq_receive_time_safe;
mod neteq;
mod neteq_callback_scope;
mod neteq_factory;
mod neteq_provider;
pub use neteq::{NetEq, NetEqError};
pub use neteq_factory::{
    NetEqCreationContext, NetEqFactory, NetEqFactoryError, NetEqFactoryFailureObserver,
    NetEqFactoryProvider,
};
pub(crate) use neteq_factory::{
    RustNetEqFactory, RustNetEqFailureObserver, neteq_factory_create_product,
};
pub use neteq_provider::{NetEqFailure, NetEqFailureObserver, NetEqOutput, NetEqProvider};
pub(crate) use neteq_provider::{
    RustNetEqProvider, neteq_provider_base_minimum_delay, neteq_provider_create_decoder,
    neteq_provider_current_decoder_format, neteq_provider_current_network_statistics,
    neteq_provider_decoder_format, neteq_provider_disable_nack, neteq_provider_enable_nack,
    neteq_provider_failed, neteq_provider_filtered_current_delay, neteq_provider_flush_buffers,
    neteq_provider_get_audio, neteq_provider_insert_empty_packet, neteq_provider_insert_packet,
    neteq_provider_insert_packet_at, neteq_provider_insert_packet_info,
    neteq_provider_last_output_sample_rate, neteq_provider_lifetime_statistics,
    neteq_provider_nack_list, neteq_provider_network_statistics,
    neteq_provider_operations_and_state, neteq_provider_playout_timestamp,
    neteq_provider_register_payload_type, neteq_provider_remove_all_payload_types,
    neteq_provider_remove_payload_type, neteq_provider_set_base_minimum_delay,
    neteq_provider_set_codecs, neteq_provider_set_fast_accelerate,
    neteq_provider_set_maximum_buffer_packets, neteq_provider_set_maximum_delay,
    neteq_provider_set_minimum_delay, neteq_provider_sync_buffer_size, neteq_provider_target_delay,
};
pub use neteq_values::{
    NetEqConfig, NetEqDecoderFormat, NetEqLifetimeStatistics, NetEqMode, NetEqNetworkStatistics,
    NetEqOperation, NetEqOperationsAndState, NetEqReturnCode,
};
mod audio_codec_values;
pub use audio_codec_values::{AudioCodecInfo, AudioCodecSpec, SdpAudioFormat};
mod audio_encoder_factory_sdk;
pub use audio_encoder_factory_sdk::CallerAudioEncoderFactory;
pub(crate) use audio_encoder_factory_sdk::{
    RustAudioEncoderFactoryCall, audio_caller_encoder_create_callback,
    audio_caller_encoder_environment, audio_caller_encoder_formats_callback,
    audio_caller_encoder_query_callback, audio_caller_encoder_transfer_failed,
};
mod audio_decoder_factory_sdk;
pub use audio_decoder_factory_sdk::CallerAudioDecoderFactory;
pub(crate) use audio_decoder_factory_sdk::{
    RustAudioDecoderFactoryCall, audio_caller_decoder_create_callback,
    audio_caller_decoder_environment, audio_caller_decoder_formats_callback,
    audio_caller_decoder_supported_callback, audio_caller_decoder_transfer_failed,
};
#[cfg(test)]
mod audio_decoder_forwarding_tests;
#[cfg(test)]
mod audio_encoder_forwarding_tests;
mod audio_factory_provider;
#[cfg(test)]
mod audio_factory_transfer_tests;
pub use audio_factory_provider::{
    AudioDecoderCreationContext, AudioDecoderFactoryProvider, AudioDecoderFactoryRegistration,
    AudioEncoderCreationContext, AudioEncoderFactoryProvider, AudioEncoderFactoryRegistration,
    AudioFactoryError, AudioFactoryFailureKind, AudioFactoryFailureObserver, AudioFactoryOperation,
};
mod audio_encoder_options;
pub use audio_encoder_options::OwnedAudioEncoderOptions;
mod audio_encoder_values;
pub use audio_encoder_values::BitrateAllocationUpdate;
pub use audio_encoder_values::{
    AudioEncodedInfo, AudioEncodedInfoLeaf, AudioEncoderApplication, AudioEncoderBitrateRange,
    AudioEncoderCodecType, AudioEncoderFrameLengthRange, AudioEncoderOptions, AudioEncoderOutput,
    AudioNetworkAdaptationStatistics,
};
mod audio_encoder;
pub use audio_encoder::{AudioEncoder, AudioEncoderError};
pub(crate) use audio_encoder::{AudioEncoderTransfer, audio_encoder_transfer_native};
mod audio_decoder_product_sdk;
pub use audio_decoder_product_sdk::CallerAudioDecoderSdk;
pub(crate) use audio_decoder_product_sdk::{
    RustCallerAudioDecoderCall, RustCallerParsedFrame, audio_caller_decoder_channels_callback,
    audio_caller_decoder_compatibility_refused, audio_caller_decoder_packet_duration_callback,
    audio_caller_decoder_packet_fec_callback, audio_caller_decoder_parse_callback,
    audio_caller_decoder_plc_callback, audio_caller_decoder_plc_capacity,
    audio_caller_decoder_plc_invalid, audio_caller_decoder_reset_callback,
    audio_caller_decoder_scalar_callback, audio_caller_parsed_frame_bind,
    audio_caller_parsed_frame_from_sdk, audio_caller_parsed_frame_null,
    audio_caller_parsed_frame_present,
};
mod audio_decoder_frame_sdk;
pub use audio_decoder_frame_sdk::AudioDecoderFrameDecodeResult;
pub(crate) use audio_decoder_frame_sdk::{
    RustCallerAudioFrameCall, audio_caller_frame_decode_callback, audio_caller_frame_dtx_callback,
    audio_caller_frame_duration_callback, audio_caller_frame_guard_error,
};
mod audio_decoder_provider;
pub use audio_decoder_provider::{
    AudioDecoderFrameProduct, AudioDecoderFrameProvider, AudioDecoderProductError,
    AudioDecoderProductFailureKind, AudioDecoderProductFailureObserver,
    AudioDecoderProductOperation, AudioDecoderProvider, AudioDecoderProviderParseResult,
    CallerAudioDecoder, CallerDecodableAudioFrame, CallerParsedAudioFrame,
};
mod audio_encoder_provider;
pub use audio_encoder_provider::{
    AudioEncoderProductError, AudioEncoderProductFailureKind, AudioEncoderProductFailureObserver,
    AudioEncoderProductOperation, AudioEncoderProvider, CallerAudioEncoder,
};
mod audio_encoder_sdk_failure;
pub use audio_encoder_sdk_failure::{
    AudioEncoderSdkFailure, AudioEncoderSdkFailureObserver, AudioEncoderSdkOperation,
};
mod audio_decoder;
pub use audio_decoder::{
    AudioCodecPairId, AudioDecoder, AudioDecoderError, AudioPacketInfo, AudioSpeechType,
    DecodableAudioFrame, DecodedAudio, ParsedAudioFrame,
};
pub(crate) use audio_decoder::{AudioDecoderTransfer, audio_decoder_transfer_native};
mod configuration;
mod crypto;
mod data_channel;
mod sframe;
pub use sframe::{
    SframeCipherSuite, SframeCreateError, SframeDecrypter, SframeEncrypter, SframeEncrypterInit,
    SframeMode,
};
mod video_encoder_selector;
pub use codec::VideoEncoderLossNotification;
pub use video_encoder_selector::{
    OwnedVideoEncoderSelector, VideoEncoderSelector, VideoEncoderSelectorFailure,
    VideoEncoderSelectorFailureObserver, VideoEncoderSelectorInputError,
    VideoEncoderSelectorProvider,
};
pub(crate) use video_encoder_selector::{
    RustVideoEncoderSelector, RustVideoEncoderSelectorObserver, video_selector_provider_bitrate,
    video_selector_provider_broken, video_selector_provider_current,
    video_selector_provider_observer, video_selector_provider_resolution,
};
mod video_encoder_product;
pub use video_encoder_product::{OwnedVideoEncoder, VideoEncoderCreationError};
mod video_codec_callbacks;
mod video_decoder_product;
pub use video_codec_callbacks::{
    DecodedImageCallbackProvider, DecodedImageCallbackSink, EncodedImageCallbackProvider,
    EncodedImageCallbackSink, FecControllerOverride, FecControllerOverrideProvider,
    VideoCodecCallbackFailureObserver,
};
pub(crate) use video_codec_callbacks::{
    RustDecodedImageSink, RustEncodedImageSink, RustFecOverride, decoded_sink_provider_invoke,
    encoded_sink_provider_dropped, encoded_sink_provider_frame, fec_override_provider_allowed,
};
pub use video_decoder_product::{OwnedVideoDecoder, VideoDecoderCreationError};
mod codec_specific_info;
mod video_encoder_values;
pub use codec_specific_info::{
    CodecBufferUsage, CodecDependencyStructure, CodecDependencyTemplate, CodecSpecificInfoFields,
    CodecSpecificParameters, EncodedImageCallbackError, EncodedImageCallbackResult,
    GenericCodecFrameInfo, OwnedCodecSpecificInfo,
};
pub use video_encoder_values::{
    OwnedVideoBitrateAllocation, OwnedVideoCodec, OwnedVideoEncoderInfo, OwnedVideoRateControl,
    VideoEncoderInfoFields, VideoEncoderInfoValueError, VideoEncoderMappedResolution,
    VideoEncoderPixelFormat, VideoEncoderQpThresholds, VideoEncoderScalingSettings,
    VideoEncoderSettingsValue, VideoRateControlFields, VideoResolutionBitrateLimits,
};
mod video_decoder_values;
pub use video_decoder_values::{
    OwnedVideoDecoderInfo, OwnedVideoDecoderSettings, VideoDecoderInfoFields,
    VideoDecoderSettingsFields, VideoDecoderSettingsValueError,
};
mod video_codec_values;
pub use video_codec_values::{
    RenderResolution, SdpVideoFormat, SdpVideoFormatError, SdpVideoFormatFields,
    WellKnownSdpVideoFormat,
};
mod live_frame_transformer;
pub use live_frame_transformer::{
    FrameTransformBinding, FrameTransformCallback, FrameTransformCompletion,
    FrameTransformCompletionError, FrameTransformCompletionOutcome, FrameTransformer,
    FrameTransformerFailureObserver, FrameTransformerProvider, FrameTransformerStatistics,
};
pub(crate) use live_frame_transformer::{
    RustFrameTransformerProvider, frame_transformer_provider_binding,
    frame_transformer_provider_panics, frame_transformer_provider_transform,
};
mod frame_transformer;
pub use frame_transformer::{
    TransformableAudioFrame, TransformableAudioFrameType, TransformableFrame,
    TransformableFrameDirection, TransformableFrameError, TransformableMediaFrame,
    TransformableVideoFrame,
};
mod frame_crypto;
pub use frame_crypto::{
    DecryptedFrame, EncryptedFrame, FrameCryptoFailure, FrameCryptoOperationError,
    FrameDecryptionResult, FrameDecryptionStatus, FrameDecryptor, FrameDecryptorInstallError,
    FrameDecryptorProvider, FrameEncryptionResult, FrameEncryptor, FrameEncryptorProvider,
};
pub(crate) use frame_crypto::{
    RustFrameDecryptorProvider, RustFrameEncryptorProvider, frame_decryptor_provider_decrypt,
    frame_decryptor_provider_failure, frame_decryptor_provider_max_size,
    frame_encryptor_provider_encrypt, frame_encryptor_provider_failure,
    frame_encryptor_provider_max_size,
};
mod dtmf;
pub use dtmf::{DtmfInsertError, DtmfSender, DtmfToneChange};
mod rtp_extensions;
pub use rtp_extensions::{
    RtpExtension, RtpExtensionFilter, RtpExtensionFilterRangeError, RtpExtensionRef, RtpExtensions,
};
mod rtp_observers;
pub use rtp_observers::{RtpObserverError, RtpPacketEvent, RtpReceiverObserver, RtpSenderObserver};
mod rtp_capabilities;
pub use rtp_capabilities::{
    FecMechanism, RtpCapabilities, RtpCapabilityInstallError, RtpCodecCapability,
    RtpHeaderExtensionCapabilityValue, RtpTransceiverDirectionValue, RtpTransceiverOperationError,
    ScalabilityMode,
};
mod rtp_transceiver_init;
pub use rtp_transceiver_init::RtpTransceiverInit;
mod rtp_parameters;
pub use rtp_parameters::{DegradationPreference, RtpParameters};
mod rtp_values;
pub use rtp_values::{
    Priority, PriorityValue, Resolution, RtcpFeedback, RtcpFeedbackMessageType, RtcpFeedbackType,
    RtcpParameters, RtpCodec, RtpCodecParameter, RtpCodecParameters, RtpEncodingParameters,
    RtpHeaderExtensionId,
};
mod string_builder;
pub use string_builder::StringBuilder;
mod rtc_error;
pub use rtc_error::{
    NativeLongOutOfRange, RtcError, RtcErrorDetailType, RtcErrorMessageBuilder, RtcErrorType,
};
mod transport;
pub use transport::{
    CandidateNetworkCostOutOfRange, Dscp, DtlsTransport, DtlsTransportEvent,
    DtlsTransportInformation, DtlsTransportRole, DtlsTransportState, IceCandidatePair,
    IceCandidatePairChange, IceCandidatePairState, IceCandidateStats, IceConfig, IceConnectionInfo,
    IceInternalUnavailable, IceMode, IceNetworkRoute, IceNominationMode, IceOperationError,
    IcePacketDecryption, IcePacketEvent, IcePacketInfo, IcePacketObserverError, IcePacketOptions,
    IcePacketSendError, IcePacketState, IcePacketTimeUpdate, IceParameters, IceReceivedPacket,
    IceRouteEndpoint, IceSentPacket, IceSocketOperationResult, IceSocketOption, IceStunStats,
    IceTransport, IceTransportGatheringState, IceTransportInternal, IceTransportInternalState,
    IceTransportRole, IceTransportState, IceTransportStats, MAX_ICE_CANDIDATE_NETWORK_COST,
    PacketInfoProtocol, PacketType, SctpTransport, SctpTransportInformation, SctpTransportState,
    TransportCandidate, TransportCandidateProperties, TransportCandidateType, TransportError,
    TransportObserverError,
};
mod encoded_video;
mod event_log;
mod event_log_codec;
mod event_log_creation;
pub use event_log_codec::{
    LoggedAlrStateEvent, LoggedAudioPlayoutEvent, LoggedBweDelayBasedUpdate, LoggedStartEvent,
    LoggedStopEvent, RtcEventCodecError, RtcEventLogParseStatus, RtcEventParsedRecords,
};
mod event_log_dependency;
mod event_log_rtp;
mod event_log_values;
pub use event_log_dependency::*;
pub use event_log_rtp::*;
pub use event_log_values::*;
mod execution;
mod execution_resources;
mod field_trials;
mod mdns;
mod network;
mod network_monitor;
pub use mdns::{
    MdnsCompletionError, MdnsNameCompletion, MdnsOperationError, MdnsRemovalCompletion,
    MdnsResponder, MdnsResponderProvider,
};
pub(crate) use mdns::{
    RustMdnsCallback, RustMdnsResponder, mdns_callback_bind, mdns_callback_fire,
    mdns_source_any_failed, mdns_source_create, mdns_source_failed, mdns_source_remove,
    mdns_source_retire,
};
mod network_manager;
pub use network_manager::{
    NetworkEnumerationPermission, NetworkManager, NetworkManagerContext, NetworkManagerEvent,
    NetworkManagerFactory, NetworkManagerFactoryProvider, NetworkManagerObservationError,
    NetworkManagerObservationSource, NetworkManagerSignals, NetworkManagerStats,
    NetworkManagerSubscription,
};
pub(crate) use network_manager::{
    RustManagerCallback, RustNetworkManager, RustNetworkManagerFactory, manager_callback_fire,
    network_manager_create, network_manager_default_address, network_manager_dump,
    network_manager_factory_present, network_manager_failed, network_manager_initialize,
    network_manager_mdns, network_manager_networks, network_manager_permission,
    network_manager_present, network_manager_set_vpn, network_manager_start, network_manager_stop,
};
mod listen_socket;
pub use listen_socket::{
    AcceptedPacketSocket, ListenSocket, ListenSocketContext, ListenSocketEvents, ListenSocketState,
    PacketSocketAcceptError, PacketSocketAcceptStatus, PacketSocketAcceptTicket,
};
pub(crate) use listen_socket::{
    RustAcceptCreator, RustListenSocket, listen_create_accepted, listen_socket_failed,
    listen_socket_local, listen_socket_present, listen_socket_state,
};
mod packet_socket_factory;
pub use packet_socket_factory::{
    PacketSocketFactory, PacketSocketFactoryImplementation, PacketSocketTcpOptions,
};
pub(crate) use packet_socket_factory::{
    RustPacketSocketFactory, socket_factory_create_dns, socket_factory_create_listener,
    socket_factory_create_packet, socket_factory_present,
};
mod owned_listen_socket;
pub use owned_listen_socket::{ListenSocketSubscription, OwnedListenSocket};
pub(crate) use owned_listen_socket::{RustListenCallback, listen_callback_fire};
mod packet_socket_runtime;
pub use packet_socket_runtime::{PacketSocketRole, PacketSocketRuntime, PacketSocketRuntimeError};
mod owned_packet_socket;
pub use owned_packet_socket::OwnedPacketSocket;
pub(crate) use owned_packet_socket::RustSocketRole;
mod packet_socket_observation;
pub use packet_socket_observation::{
    PacketSocketEvent, PacketSocketEventKind, PacketSocketSubscription,
};
pub(crate) use packet_socket_observation::{RustSocketCallback, socket_callback_fire};
mod packet_socket;
pub use packet_socket::{
    PacketSocket, PacketSocketContext, PacketSocketEventError, PacketSocketEvents,
    PacketSocketOptionError,
};
pub(crate) use packet_socket::{
    RustPacketSocket, packet_socket_close, packet_socket_error, packet_socket_get_option,
    packet_socket_local, packet_socket_present, packet_socket_send, packet_socket_send_to,
    packet_socket_set_error, packet_socket_set_option, packet_socket_source_failed,
    packet_socket_state,
};
mod packet_socket_values;
pub use packet_socket_values::{
    AsyncSocketPacketOptions, DscpCodePoint, PacketDecryptionInfo, PacketInfo,
    PacketInfoProtocol as SocketPacketInfoProtocol, PacketSocketFlags, PacketSocketOption,
    PacketSocketState, PacketSocketValueError, PacketTimeUpdateParams,
    PacketType as SocketPacketType, ReceivedIpPacket, SentPacketInfo,
};
mod network_values;
pub use network_values::{
    IpNetworkMask, Network, NetworkChangeInProgress, NetworkChangeKind, NetworkChangeSubscription,
    NetworkCostGuess, NetworkValueError,
};
pub(crate) use network_values::{
    RustNetworkCallback, network_callback_failed, network_callback_fire,
};
mod default_local_address;
pub use default_local_address::{DefaultLocalAddressProvider, DefaultLocalAddressProviderHandle};
pub(crate) use default_local_address::{
    RustDefaultLocalAddressProvider, default_address_failed, default_address_get,
};
mod ntp_time;
pub use ntp_time::{NtpTime, Q32x32, Uq32x32};
mod time_delta;
pub use time_delta::TimeDelta;
mod task_queue_provider;
pub(crate) use task_queue_provider::{
    RustTaskQueueFactory, RustTaskQueueProvider, task_queue_provider_create,
    task_queue_provider_delete, task_queue_provider_failed, task_queue_provider_mark_failed,
    task_queue_provider_name, task_queue_provider_post, task_queue_provider_present,
};
pub use task_queue_provider::{
    TaskDelayPrecision, TaskQueueContext, TaskQueueFactoryProvider, TaskQueueIdentity,
    TaskQueueLocation, TaskQueuePostError, TaskQueueProvider, TaskQueueTask,
};
mod stats;
pub use stats::*;
mod media_track;
pub use media_track::{MediaTrack, MediaTrackKind, MediaTrackState};
mod media_stream;
pub use media_stream::{
    AudioProcessor, AudioProcessorStats, AudioTrackSource, MediaChange, MediaObserver,
    MediaSourceState, MediaStream, MediaTrackSource, TrackAudioOptionsCode,
    TrackAudioOptionsResult, VideoSourceInfo, VideoTrackSource,
};
mod legacy_stats;
pub use legacy_stats::*;
mod timestamp;
pub use timestamp::{FiniteTimestamp, TimeValueError, Timestamp};
mod metronome;
pub use metronome::{Metronome, MetronomeProvider, MetronomeTick, MetronomeTickError};
pub(crate) use metronome::{
    RustMetronome, metronome_present, metronome_request_tick, metronome_tick_period,
};
mod clock;
pub use clock::{Clock, ClockProvider, ClockTimeBounds, RealTimeClockUnavailable, SimulatedClock};
pub(crate) use clock::{
    RustClockProvider, clock_provider_convert_ntp, clock_provider_current_time,
};
pub use network_monitor::{
    NetworkAdapterType, NetworkBindingResult, NetworkChangeNotifier, NetworkInterfaceInfo,
    NetworkMonitor, NetworkMonitorFactory, NetworkPreference, NetworkSlice, NetworkSocket,
};
pub(crate) use network_monitor::{
    RustNetworkMonitor, RustNetworkMonitorFactory, network_monitor_bind, network_monitor_create,
    network_monitor_factory_present, network_monitor_info, network_monitor_present,
    network_monitor_start, network_monitor_stop, network_monitor_supports_binding,
};
mod local_network_permission;
mod peer;
mod peer_configuration;
mod peer_dependencies;
pub use local_network_permission::{
    LocalNetworkPermission, LocalNetworkPermissionCompletion, LocalNetworkPermissionFactory,
    LocalNetworkPermissionStatus,
};
pub(crate) use local_network_permission::{
    RustLocalNetworkPermission, RustLocalNetworkPermissionFactory, local_permission_create,
    local_permission_factory_present, local_permission_request, local_permission_should_request,
};
mod socket_address;
pub use socket_address::{
    AddressFamily, AddressParseError, InterfaceAddress, IpAddress, IpAddressType, NativeSockAddr,
    SocketAddress,
};
mod tls;
pub use peer_dependencies::PeerConnectionDependencies;
pub(crate) use tls::{
    RustTlsCertificateVerifier, rust_tls_certificate_verifier_failed, rust_tls_certificate_verify,
    rust_tls_certificate_verify_chain, tls_certificate_verifier_present,
};
pub use tls::{
    SocketTlsVerifier, TlsCertificate, TlsCertificateChain, TlsCertificateStats,
    TlsCertificateVerifier,
};
mod production;
mod sender_parameters;
pub use sender_parameters::SenderParametersOperation;
mod encoded_image;
mod raw_video_frame;
mod readiness;
mod source_encoded_video;
mod video;
mod video_sink_wants;
pub use encoded_image::{
    CorruptionDetectionFilterSettings, EncodedImage, EncodedImageError, EncodedImageFrameType,
    EncodedImageMetadata, EncodedImagePsnr, EncodedImageRotation, EncodedImageTiming,
    EncodedVideoBuffer, WritableEncodedVideoBuffer,
};
pub use raw_video_frame::{
    RawVideoBuffer, RawVideoBufferInfo, RawVideoBufferType, RawVideoError, RawVideoFrame,
    RawVideoFrameMetadata, RawVideoPlane, RawVideoPlaneKind, RawVideoSamples,
    VideoBufferPreparation, VideoFrameContentType, VideoFrameProcessingTime,
    VideoFrameRenderParameters, VideoUpdateRect, WritableI420Buffer,
};
pub use source_encoded_video::{RecordableVideoFrame, VideoSourceEncodedSink};
pub use video_sink_wants::{VideoSinkAggregates, VideoSinkFrameSize, VideoSinkWants};
mod camera_options;
mod camera_sink;
mod decoded_video_observer;
pub use decoded_video_observer::{DecodedVideoFrameObservation, DecodedVideoFrameObserver};
use decoded_video_observer::{RustDecodedVideoObserver, decoded_video_frame_observed};
mod encoded_video_observer;
pub use encoded_video_observer::{EncodedVideoFrameObservation, EncodedVideoFrameObserver};
use encoded_video_observer::{
    RustEncodedVideoObserver, encoded_video_frame_observed, encoded_video_observer_closed,
};
mod desktop_frame;
mod desktop_memory;
#[cfg(feature = "native")]
pub use desktop_memory::{
    DesktopMemory, DesktopMemoryFactory, DesktopMemoryFactoryHandle, DesktopMemoryInfo,
};
use desktop_memory::{
    RustDesktopMemoryFactory, RustDesktopMemoryLease, desktop_memory_factory_create,
    desktop_memory_factory_present,
};
mod desktop_metadata;
#[cfg(feature = "native")]
pub use desktop_metadata::{DesktopCaptureMetadata, DesktopCaptureMetadataSnapshot};
mod desktop_receiver;
#[cfg(feature = "native")]
pub use camera_options::{
    CameraCaptureOptions, CameraInitializationObserver, CameraInitializationObserverHandle,
    CameraInitializationStatus, CameraOptionSettings,
};
use camera_options::{RustCameraInitializationObserver, camera_options_initialized};
#[cfg(feature = "native")]
pub use camera_sink::{RawCameraFrame, RawCameraFrameSnapshot, RawCameraSink, RawCameraSinkHandle};
use camera_sink::{RustRawCameraSink, camera_raw_frame, camera_raw_sink_present};
#[cfg(feature = "native")]
pub use desktop_frame::{
    CapturedDesktopFrame, DesktopFrame, DesktopFrameInfo, DesktopFrameMetadata,
    DesktopFrameSnapshot, DesktopRect, DesktopSharedMemoryInfo,
};
#[cfg(feature = "native")]
pub use desktop_receiver::{
    DesktopCaptureReceiver, DesktopCaptureReceiverHandle, DesktopCaptureResult,
    DesktopSelectionObserver, DesktopSelectionObserverHandle,
};
use desktop_receiver::{
    RustDesktopCaptureReceiver, RustDesktopSelectionObserver, desktop_receiver_present,
    desktop_receiver_result, desktop_receiver_start, desktop_selection_notify,
    desktop_selection_present,
};
#[cfg(feature = "native")]
mod camera_device_info;
#[cfg(feature = "native")]
mod desktop_capture;
#[cfg(feature = "native")]
pub use camera_device_info::{CameraDeviceInfo, CameraDeviceName, CameraDeviceNameCapacities};
#[cfg(feature = "native")]
pub use desktop_capture::{
    DesktopCaptureKind, DesktopCaptureOptions, DesktopCaptureSettings, DesktopScreenCastStream,
    DesktopXDisplay, PipeWireCaptureSettings,
};

pub(crate) use codec::{
    RustVideoDecoder, RustVideoDecoderFactory, RustVideoEncoder, RustVideoEncoderFactory,
    decoder_configure_value, decoder_decode, decoder_decode_value, decoder_factory_create,
    decoder_factory_formats, decoder_factory_query, decoder_get_info_value,
    decoder_implementation_name, decoder_is_valid, decoder_register_callback,
    decoder_register_callback_value, decoder_release, encoder_encode, encoder_encode_value,
    encoder_factory_create, encoder_factory_formats, encoder_factory_implementations,
    encoder_factory_query, encoder_factory_report_codec_info_failure,
    encoder_factory_report_feedback_failure, encoder_factory_selector, encoder_get_info_value,
    encoder_init, encoder_init_value, encoder_is_valid, encoder_loss_notification,
    encoder_packet_loss, encoder_register_callback, encoder_register_callback_value,
    encoder_release, encoder_rtt, encoder_set_fec_override, encoder_set_rates,
    encoder_set_rates_value,
};
pub use event_log::{
    EventLogEnvironment, EventLogStopCompletion, RtcEvent, RtcEventKind, RtcEventLogFactory,
    RtcEventLogger,
};
pub use event_log::{EventLogError, EventLogOutput, EventLogOutputPeriod, EventLogOutputSink};
mod event_log_product;
#[cfg(test)]
mod provider_callback_admission_tests;
mod provider_panic;
pub(crate) use event_log::{
    RustEventLogOutput, event_log_output_active, event_log_output_flush, event_log_output_write,
};
pub(crate) use event_log::{
    RustRtcEventLog, RustRtcEventLogFactory, rust_event_log_create, rust_event_log_failed,
    rust_event_log_log, rust_event_log_present, rust_event_log_start, rust_event_log_stop,
    rust_event_log_stop_async,
};
pub use event_log_product::{
    RtcEventLogFactoryHandle, RtcEventLogHandle, RtcEventLogStopOperation, RtcEventLogUtility,
};
pub(crate) use event_log_product::{
    RustEventLogProductStop, RustEventLogRetention, event_log_product_stopped,
};
pub(crate) use execution::{
    RustEnvironmentFactoryRandomness, RustEnvironmentFactoryRetention,
    RustRuntimeEnvironmentRetention, RustTask, environment_factory_has_randomness,
    environment_factory_randomness, run_task,
};
pub use field_trials::{FieldTrials, FieldTrialsError, FieldTrialsSnapshot, FieldTrialsView};
pub(crate) use field_trials::{
    RustFieldTrials, field_trials_present, rust_field_trials_copy, rust_field_trials_lookup,
};
pub(crate) use readiness::{RustReadiness, notify_readiness};

#[cfg(feature = "native")]
pub use audio::AudioDevice;
pub use audio::{
    AudioCodecCapability, AudioFrameError, AudioPcmFrame, AudioSampleFormat, AudioSink,
    AudioSource, AudioTrack, AudioTrackSink, DecodedAudioFrame, EncodedAudioFrame,
    EncodedAudioSink, EncodedAudioSource, OpusAudioLevel, OpusInputError, OpusInputFrame,
    ReceivedAudioFrame,
};
pub use audio_processing::{
    AudioProcessingConfig, AudioProcessingOptions, AudioProcessingState, GainControl,
    NoiseSuppression, ProcessingChoice, ProcessingComponentState, ProcessingImplementation,
};
pub use certificate::{
    Certificate, CertificateCurve, CertificateData, CertificateFingerprint, CertificateKeyParams,
    CertificatePem,
};
pub use codec::{
    AudioDecoderFactory, AudioEncoderFactory, CodecError, CodecParameter, CodecSupport,
    DecodedImageCallback, DecoderStatistics, EncodedImageCallback, EncodedVideoCodec,
    EncodedVideoFrame, EncodedVideoMetadata, Nv12Planes, VideoCodecFormat, VideoDecoder,
    VideoDecoderFactory, VideoDecoderFactoryHandle, VideoDecoderInfo, VideoDecoderSettings,
    VideoEncoder, VideoEncoderFactory, VideoEncoderFactoryHandle, VideoEncoderInfo,
    VideoEncoderSettings, VideoFrame, VideoFrameBuffer, VideoFrameType, VideoPlane,
    VideoRateControl, VideoResolution, VideoRotation, VideoScalabilityMode, VideoSimulcastStream,
};
pub use configuration::{FactoryOptions, NetworkAdapterMask, SslProtocolVersion};
pub use crypto::{
    CryptexPolicy, CryptoOptions, DtlsSrtpCipherSuite, KeyExchangeGroup, KeyExchangeGroups,
    SrtpCryptoOptions,
};
pub use data_channel::{
    DataChannel, DataChannelConfiguration, DataChannelCounters, DataChannelError,
    DataChannelErrorDetail, DataChannelEvent, DataChannelMessage, DataChannelMessageKind,
    DataChannelPriority, DataChannelReceiveQueueLimits, DataChannelSendCompletion,
    DataChannelSendError, DataChannelSendId, DataChannelSendResult, DataChannelState,
};
pub use encoded_video::{
    EncodedH264Input, EncodedH264Source, EncodedVideoAccessUnit, EncodedVideoInput,
    EncodedVideoSource, H264AccessUnit, H264SourceProfile,
};
pub use execution::{
    BuildEnvironmentError, ControlledPeerDriver, ControlledWorld, Environment, EnvironmentBuilder,
    EnvironmentClockError, EnvironmentFactory, MAX_CONTROLLED_TIME, ManualClock, NetworkThread,
    PumpResult, QueuePriority, RandomnessLease, RandomnessLeaseError, SignalingThread, SystemClock,
    TaskQueue, TaskQueueFactory, ThreadStartError, WorkerThread, WorldAcquireError,
};
pub use execution_resources::{CodecCoreBudget, ExecutionResources, InvalidCodecCoreBudget};
pub use network::{
    ControlledSimulatedNetwork, NetworkAddress, NetworkEndpoint, NetworkError,
    NetworkManagerProvider, OutboundKind, OutboundPacket, PacketSocketFactoryProvider,
    ReceivedPacket, SimulatedNetwork, SimulatedUdpSocket,
};
pub use peer::{
    BandwidthEstimationSettings, CallAdmissionStats, CandidatePairStats, ConnectionState,
    DataChannelMessageDirection, DataChannelStats, IceCandidate, IceCandidateBytes,
    IceCandidateDisposition, IceGatheringState, IceServer, IceTransportPolicy, InboundRtpStats,
    OfferAnswerOptions, OperationCompletion, OperationId, OutboundRtpStats, PeerConfiguration,
    PeerConnection, PeerConnectionEvent, PeerConnectionFactory, PeerConnectionFactoryBuilder,
    PeerDataChannelMessage, PeerDescriptionBytes, PeerDescriptions, PeerError, PeerErrorBytes,
    PeerErrorKind, PeerEventObservation, PeerIceConnectionState, PeerIceSnapshot, PeerStatsRecord,
    PeerStatsSnapshot, SessionDescription, SessionDescriptionBytes, SessionDescriptionType,
    SignalingState, TransportStats,
};
pub use peer_configuration::{
    BundlePolicy, CandidateNetworkPolicy, ContinualGatheringPolicy, NativeTimeDelta, NetworkMask,
    PeerConnectionOptions, PortAllocatorOptions, RtcConfiguration, RtcIceServer, RtcpMuxPolicy,
    SdpSemantics, TcpCandidatePolicy, TlsCertificatePolicy, TurnPortPrunePolicy, VpnPreference,
};
pub use production::{
    ManagedRuntime, ProductionSession, ProductionSessionBuilder, ProductionSessionConfig, Session,
    SessionAudioObserverId, SessionAudioProcessorId, SessionAudioSourceViewId,
    SessionAudioTrackProcessorId, SessionBuilder, SessionCameraDeviceInfoId,
    SessionCameraOptionsId, SessionChannelId, SessionConfig, SessionDecodedImageCallbackId,
    SessionDesktopMetadataId, SessionDesktopOptionsId, SessionDtlsTransportId, SessionDtmfError,
    SessionDtmfSenderId, SessionEncodedImageCallbackId, SessionEncodedImageId,
    SessionEncodedVideoBufferId, SessionEvent, SessionFecOverrideId, SessionFrameDecryptorId,
    SessionFrameEncryptorId, SessionFrameTransformerId, SessionIceTransportId,
    SessionLocalMediaStreamId, SessionMediaStreamId, SessionMediaStreamObserverId,
    SessionMediaTrackId, SessionPeerId, SessionRawVideoBufferId, SessionRawVideoFrameId,
    SessionReceiverId, SessionRuntime, SessionSctpTransportId, SessionSctpTransportInformation,
    SessionSenderId, SessionSenderParametersOperationId, SessionSframeDecrypterId,
    SessionSframeEncrypterId, SessionSourceId, SessionTrackId, SessionTrackSourceId,
    SessionTransceiverId, SessionTransportError, SessionVideoBufferPreparationId,
    SessionVideoDecoderId, SessionVideoEncoderId, SessionVideoEncoderSelectorId,
    SessionVideoObserverId, SessionVideoSinkId, SessionVideoSourceEncodedSinkId,
    SessionVideoSourceViewId, SessionWritableEncodedBufferId, SessionWritableI420BufferId,
    ThreadedRuntime,
};
#[cfg(feature = "native")]
pub use video::{
    CameraCapture, CameraCaptureStatus, CameraControlError, CameraDevice, CameraFormat,
    CameraPixelFormat, CameraSettings, ScreenCapture, ScreenCaptureStatus, ScreenSource,
    WindowCapture, WindowSource,
};
pub use video::{
    DecodeTargetIndication, EncodedReceivedVideoFrame, EncodedVideoSink, MediaType,
    ReceivedVideoFrame, RtpHeaderExtensionCapability, RtpHeaderExtensionDirection, RtpReceiver,
    RtpReceiverOperationError, RtpSender, RtpSenderEncoding, RtpSenderOperationError,
    RtpSenderParameters, RtpTransceiver, RtpTransceiverDirection, VideoCodecCapability,
    VideoContentHint, VideoObserver, VideoObserverEvent, VideoSink, VideoSinkEvent, VideoSource,
    VideoSourceConstraints, VideoSourceSink, VideoSourceState, VideoSourceStats, VideoSourceStatus,
    VideoSourceView, VideoTrack, VideoTrackState, VideoTrackStatus,
};

mod bridge;
use bridge::ffi;

/// Returns the identity shared by this Rust bridge and its native implementation.
///
/// The underlying CXX module remains private:
///
/// ```compile_fail
/// use pulsebeam_webrtc_sys::ffi;
/// ```
pub fn bridge_identity() -> &'static str {
    ffi::bridge_identity()
}
