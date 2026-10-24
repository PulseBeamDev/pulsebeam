use super::*;

// The fixed C++ ABI includes native-only entries and callback parameter lists.
#[allow(dead_code, clippy::too_many_arguments)]
#[cxx::bridge(namespace = "pulsebeam::webrtc_sys")]
pub(super) mod ffi {
    #[derive(Default)]
    struct FfiVideoHeaderVp8 {
        non_reference: bool,
        picture_id: i16,
        tl0_pic_idx: i16,
        temporal_idx: u8,
        layer_sync: bool,
        key_idx: i32,
        partition_id: i32,
        beginning_of_partition: bool,
    }
    struct FfiVp9GofInfo {
        num_frames_in_gof: usize,
        temporal_idx: Vec<u8>,
        temporal_up_switch: Vec<bool>,
        num_ref_pics: Vec<u8>,
        pid_diff: Vec<u8>,
        pid_start: u16,
    }
    struct FfiVideoHeaderVp9 {
        inter_pic_predicted: bool,
        flexible_mode: bool,
        beginning_of_frame: bool,
        end_of_frame: bool,
        ss_data_available: bool,
        non_ref_for_inter_layer_pred: bool,
        picture_id: i16,
        max_picture_id: i16,
        tl0_pic_idx: i16,
        temporal_idx: u8,
        spatial_idx: u8,
        temporal_up_switch: bool,
        inter_layer_predicted: bool,
        gof_idx: u8,
        num_ref_pics: u8,
        pid_diff: Vec<u8>,
        ref_picture_id: Vec<i16>,
        num_spatial_layers: usize,
        first_active_layer: usize,
        spatial_layer_resolution_present: bool,
        width: Vec<u16>,
        height: Vec<u16>,
        gof: FfiVp9GofInfo,
        end_of_picture: bool,
    }
    struct FfiH264NaluInfo {
        type_code: u8,
        sps_id: i32,
        pps_id: i32,
    }
    struct FfiVideoHeaderH264 {
        nalu_type: u8,
        packetization_type: i32,
        nalus: Vec<FfiH264NaluInfo>,
        packetization_mode: i32,
    }
    struct FfiVideoHeaderCodecSpecifics {
        tag: i32,
        vp8: FfiVideoHeaderVp8,
        vp9: FfiVideoHeaderVp9,
        h264: FfiVideoHeaderH264,
    }

    struct FfiRtpVideoGenericDescriptorInfo {
        frame_id: i64,
        spatial_index: i32,
        temporal_index: i32,
        decode_target_indications: Vec<i32>,
        dependencies: Vec<i64>,
        chain_diffs: Vec<i32>,
        active_decode_targets: u32,
    }
    struct FfiRtpVideoHeaderValues {
        has_generic: bool,
        generic: FfiRtpVideoGenericDescriptorInfo,
        frame_type: i32,
        width: u16,
        height: u16,
        rotation: i32,
        content_type: u8,
        is_first_packet_in_frame: bool,
        is_last_packet_in_frame: bool,
        is_last_frame_in_picture: bool,
        simulcast_idx: u8,
        codec: i32,
        has_playout_delay: bool,
        playout_delay: FfiMediaPlayoutDelay,
        video_timing: FfiMediaVideoSendTiming,
        has_color_space: bool,
        color_space: FfiMediaColorSpace,
        has_video_frame_tracking_id: bool,
        video_frame_tracking_id: u16,
        video_type_header: FfiVideoHeaderCodecSpecifics,
        legacy_generic_picture_id: u16,
        has_absolute_capture_time: bool,
        absolute_capture_time: FfiAbsoluteCaptureTime,
        frame_instrumentation_data: UniquePtr<NativeFrameInstrumentationData>,
    }
    struct FfiVideoResolutionBitrateLimits {
        frame_size_pixels: i32,
        min_start_bitrate_bps: i32,
        min_bitrate_bps: i32,
        max_bitrate_bps: i32,
    }
    struct FfiVideoEncoderFpsAllocation {
        fractions: Vec<u8>,
    }
    struct FfiOwnedVideoEncoderInfo {
        has_thresholds: bool,
        qp_low: i32,
        qp_high: i32,
        min_pixels_per_frame: i32,
        requested_resolution_alignment: u32,
        apply_alignment_to_all_simulcast_layers: bool,
        supports_native_handle: bool,
        implementation_name: Vec<u8>,
        has_trusted_rate_controller: bool,
        is_hardware_accelerated: bool,
        enable_cpu_overuse_detection: bool,
        fps_allocation: Vec<FfiVideoEncoderFpsAllocation>,
        resolution_bitrate_limits: Vec<FfiVideoResolutionBitrateLimits>,
        supports_simulcast: bool,
        preferred_pixel_formats: Vec<i32>,
        has_is_qp_trusted: bool,
        is_qp_trusted: bool,
        has_min_qp: bool,
        min_qp: i32,
        has_mapped_resolution: bool,
        mapped_width: i32,
        mapped_height: i32,
    }
    struct FfiOwnedVideoDecoderSettings {
        has_buffer_pool_size: bool,
        buffer_pool_size: i32,
        width: i32,
        height: i32,
        cores: i32,
        codec_type: i32,
    }
    struct FfiOwnedVideoDecoderInfo {
        implementation_name: Vec<u8>,
        hardware_accelerated: bool,
    }
    struct FfiSdpVideoFormatFields {
        name: Vec<u8>,
        parameters: Vec<FfiRtpCodecParameter>,
        scalability_modes: Vec<u8>,
        has_packetization: bool,
        packetization: Vec<u8>,
        has_tx_mode: bool,
        tx_mode: Vec<u8>,
    }
    struct FfiRtpByteString {
        bytes: Vec<u8>,
    }
    struct FfiTransformableFrameTime {
        present: bool,
        bits: i64,
        status: i32,
    }
    struct FfiTransformableAudioFields {
        frame_type: i32,
        has_sequence: bool,
        sequence: u16,
        has_absolute_capture: bool,
        absolute_capture: u64,
        has_level: bool,
        level: u8,
    }
    struct FfiVideoFrameMetadataValues {
        frame_type: i32,
        width: u16,
        height: u16,
        rotation: i32,
        content_type: u8,
        has_frame_id: bool,
        frame_id: i64,
        spatial_index: i32,
        temporal_index: i32,
        has_dependencies: bool,
        dependencies: Vec<i64>,
        decode_target_indications: Vec<i32>,
        is_last_frame_in_picture: bool,
        simulcast_idx: u8,
        codec: i32,
        ssrc: u32,
        csrcs: Vec<u32>,
    }

    struct FfiRtcEventStreamCodec {
        payload_name: String,
        payload_type: i32,
        rtx_payload_type: i32,
    }
    #[derive(Default)]
    struct FfiRtcEventStreamConfig {
        local_ssrc: u32,
        remote_ssrc: u32,
        rtx_ssrc: u32,
        rsid: String,
        remb: bool,
        extensions: Vec<FfiRtpHeaderExtension>,
        rtcp_mode: i32,
        codecs: Vec<FfiRtcEventStreamCodec>,
    }
    #[derive(Default)]
    struct FfiRtcEventDependencyTemplate {
        spatial_id: i32,
        temporal_id: i32,
        decode_target_indications: Vec<u8>,
        frame_diffs: Vec<i32>,
        chain_diffs: Vec<i32>,
    }
    #[derive(Default)]
    struct FfiRtpEncodingFields {
        has_ssrc: bool,
        ssrc: u32,
        bitrate_priority: f64,
        network_priority: i32,
        has_max_bitrate_bps: bool,
        max_bitrate_bps: i32,
        has_min_bitrate_bps: bool,
        min_bitrate_bps: i32,
        has_max_framerate: bool,
        max_framerate: f64,
        has_num_temporal_layers: bool,
        num_temporal_layers: i32,
        has_scale_resolution_down_by: bool,
        scale_resolution_down_by: f64,
        active: bool,
        request_key_frame: bool,
        adaptive_ptime: bool,
        has_csrcs: bool,
        csrcs: Vec<u32>,
        has_scalability_mode: bool,
        scalability_mode: Vec<u8>,
        rid: Vec<u8>,
    }

    struct FfiRtcpFeedbackFields {
        type_code: i32,
        has_message: bool,
        message_code: i32,
    }
    struct FfiRtpCodecCapabilityFields {
        has_payload_type: bool,
        payload_type: i32,
    }
    struct FfiRtpHeaderCapabilityFields {
        uri_bytes: Vec<u8>,
        encrypt: bool,
        direction: i32,
    }
    struct FfiRtpParametersFields {
        transaction_id_bytes: Vec<u8>,
        mid_bytes: Vec<u8>,
        has_degradation_preference: bool,
        degradation_preference: i32,
    }
    struct FfiRtpExtensionFields {
        uri_bytes: Vec<u8>,
        encrypt: bool,
    }
    struct FfiRtcpParametersFields {
        has_ssrc: bool,
        ssrc: u32,
        cname_bytes: Vec<u8>,
        reduced_size: bool,
        mux: bool,
    }
    struct FfiRtpCodecFields {
        name_bytes: Vec<u8>,
        kind: i32,
        has_clock_rate: bool,
        clock_rate: i32,
        has_num_channels: bool,
        num_channels: i32,
    }
    struct FfiRtpCodecParameter {
        key: Vec<u8>,
        value: Vec<u8>,
    }
    struct FfiRtpResolutionPair {
        first: i32,
        second: i32,
    }
    #[derive(Default)]
    struct FfiRtcEventResolution {
        width: i32,
        height: i32,
    }
    #[derive(Default)]
    struct FfiRtcEventDependencyStructure {
        structure_id: i32,
        num_decode_targets: i32,
        num_chains: i32,
        decode_target_protected_by_chain: Vec<i32>,
        resolutions: Vec<FfiRtcEventResolution>,
        templates: Vec<FfiRtcEventDependencyTemplate>,
    }
    #[derive(Default)]
    struct FfiRtcEventDependencyDescriptor {
        present: bool,
        parsed: bool,
        raw: Vec<u8>,
        first_packet_in_frame: bool,
        last_packet_in_frame: bool,
        frame_number: i32,
        frame_dependencies: FfiRtcEventDependencyTemplate,
        has_resolution: bool,
        resolution: FfiRtcEventResolution,
        has_active_decode_targets: bool,
        active_decode_targets: u32,
        has_attached_structure: bool,
        attached_structure: FfiRtcEventDependencyStructure,
    }
    #[derive(Default)]
    struct FfiRtcEventRate {
        bps: i64,
        kind: i8,
    }
    #[derive(Default)]
    struct FfiRtcEventSpatialLayer {
        rtp_stream_index: i32,
        spatial_id: i32,
        rates: Vec<FfiRtcEventRate>,
        width: u16,
        height: u16,
        frame_rate_fps: u8,
    }
    #[derive(Default)]
    struct FfiRtcEventRtpExtension {
        present: bool,
        parsed: bool,
        raw: Vec<u8>,
        signed_value: i32,
        unsigned_value: u32,
        sequence: u16,
        flag: bool,
        capture_timestamp: u64,
        has_optional: bool,
        capture_offset: i64,
        feedback_include_timestamps: bool,
        feedback_sequence_count: i32,
        bytes: Vec<u8>,
        text: Vec<u8>,
        min_delay_ms: i64,
        max_delay_ms: i64,
        timing_flags: u8,
        encode_start: u16,
        encode_finish: u16,
        packetization_finish: u16,
        pacer_exit: u16,
        network_timestamp: u16,
        network2_timestamp: u16,
        primary: u8,
        transfer: u8,
        matrix: u8,
        range: u8,
        chroma_horizontal: u8,
        chroma_vertical: u8,
        has_hdr: bool,
        primary_r_x: f32,
        primary_r_y: f32,
        primary_g_x: f32,
        primary_g_y: f32,
        primary_b_x: f32,
        primary_b_y: f32,
        white_x: f32,
        white_y: f32,
        luminance_max: f32,
        luminance_min: f32,
        max_content_light_level: i32,
        max_frame_average_light_level: i32,
        first_packet: bool,
        last_packet: bool,
        temporal_id: i32,
        spatial_id: i32,
        spatial_mask: u8,
        width: i32,
        height: i32,
        frame_id: u16,
        dependency_diffs: Vec<u16>,
        rtp_stream_index: i32,
        has_resolution_and_frame_rate: bool,
        allocation_layers: Vec<FfiRtcEventSpatialLayer>,
        sequence_index: i32,
        interpret_sequence_index_as_msb: bool,
        std_dev: f64,
        luma_error_threshold: i32,
        chroma_error_threshold: i32,
        sample_values: Vec<f64>,
        frame_number: i32,
        template_id: i32,
    }
    struct FfiEventParseStatus {
        ok: bool,
        message: String,
    }
    #[derive(Default)]
    struct FfiLoggedEvent {
        timestamp: i64,
        utc_time: i64,
        in_alr: bool,
        ssrc: u32,
        bitrate_bps: i32,
        detector_state: i32,
    }
    struct FfiEventParseResult {
        guard: u8,
        status: FfiEventParseStatus,
        rows: Vec<FfiLoggedEvent>,
    }
    #[derive(Default)]
    struct FfiRtcEventPayload {
        flag: bool,
        overhead: u32,
        lower_rate_bps: i64,
        lower_rate_kind: i8,
        upper_rate_bps: i64,
        upper_rate_kind: i8,
        adaptation_present: u8,
        bitrate_bps: i32,
        frame_length_ms: i32,
        uplink_packet_loss: f32,
        enable_fec: bool,
        enable_dtx: bool,
        channels: usize,
        ssrc: u32,
        stream: FfiRtcEventStreamConfig,
        detector_state: i32,
        fraction_loss: u8,
        total_packets: i32,
        ref_window_bytes: u32,
        data_in_flight_bytes: u32,
        target_rate_kbps: u32,
        smoothed_rtt_ms: u32,
        avg_queue_delay_ms: u32,
        l4s_marked_permille: u32,
        dtls_state: i32,
        ice_type: i32,
        candidate_pair_id: u32,
        local_candidate_type: i32,
        local_relay_protocol: i32,
        local_network_type: i32,
        local_address_family: i32,
        remote_candidate_type: i32,
        remote_address_family: i32,
        candidate_pair_protocol: i32,
        transaction_id: u32,
        probe_id: i32,
        min_probes: u32,
        min_bytes: u32,
        failure_reason: i32,
        bytes: Vec<u8>,
        packet_length: usize,
        header_length: usize,
        payload_length: usize,
        padding_length: usize,
        rtp_timestamp: u32,
        sequence_number: u16,
        payload_type: u8,
        marker: bool,
        has_rtx_sequence_number: bool,
        rtx_sequence_number: u16,
        probe_cluster_id: i32,
        render_time_ms: i64,
        width: i32,
        height: i32,
        video_codec: i32,
        qp: u8,
        minimum_delay_ms: i32,
    }

    struct FfiMediaPlayoutDelay {
        minimum_us: i64,
        maximum_us: i64,
    }
    struct FfiMediaHdrMetadata {
        primary_r_x: f32,
        primary_r_y: f32,
        primary_g_x: f32,
        primary_g_y: f32,
        primary_b_x: f32,
        primary_b_y: f32,
        white_point_x: f32,
        white_point_y: f32,
        luminance_max: f32,
        luminance_min: f32,
        max_content_light_level: i32,
        max_frame_average_light_level: i32,
    }
    struct FfiMediaColorSpace {
        primaries: u8,
        transfer: u8,
        matrix: u8,
        range: i32,
        chroma_siting_horizontal: i32,
        chroma_siting_vertical: i32,
        has_hdr_metadata: bool,
        hdr_metadata: FfiMediaHdrMetadata,
    }
    struct FfiMediaVideoSendTiming {
        encode_start_delta_ms: u16,
        encode_finish_delta_ms: u16,
        packetization_finish_delta_ms: u16,
        pacer_exit_delta_ms: u16,
        network_timestamp_delta_ms: u16,
        network2_timestamp_delta_ms: u16,
        flags: u8,
    }
    struct FfiMediaRtpHeaderExtension {
        has_transmission_time_offset: bool,
        transmission_time_offset: i32,
        has_absolute_send_time: bool,
        absolute_send_time: u32,
        has_absolute_capture_time: bool,
        absolute_capture_time: FfiAbsoluteCaptureTime,
        has_transport_sequence_number: bool,
        transport_sequence_number: u16,
        has_feedback_request: bool,
        feedback_include_timestamps: bool,
        feedback_sequence_count: i32,
        has_audio_level: bool,
        audio_voice_activity: bool,
        audio_level: i32,
        has_video_rotation: bool,
        video_rotation: i32,
        has_video_content_type: bool,
        video_content_type: u8,
        has_video_timing: bool,
        video_timing: FfiMediaVideoSendTiming,
        playout_delay: FfiMediaPlayoutDelay,
        stream_id: Vec<u8>,
        repaired_stream_id: Vec<u8>,
        mid: Vec<u8>,
        has_color_space: bool,
        color_space: FfiMediaColorSpace,
    }
    struct FfiMediaRtpHeader {
        marker: bool,
        payload_type: u8,
        sequence_number: u16,
        timestamp: u32,
        ssrc: u32,
        csrc_count: u8,
        csrcs: [u32; 15],
        padding_length: usize,
        header_length: usize,
        extension: FfiMediaRtpHeaderExtension,
    }
    struct FfiAbsoluteCaptureTime {
        absolute_capture_timestamp: u64,
        has_estimated_capture_clock_offset: bool,
        estimated_capture_clock_offset: i64,
    }
    struct FfiRtpSourceFields {
        timestamp_us: i64,
        source_id: u32,
        source_type: i32,
        rtp_timestamp: u32,
        has_audio_level: bool,
        audio_level: u8,
        has_absolute_capture_time: bool,
        absolute_capture_time: FfiAbsoluteCaptureTime,
        has_local_capture_clock_offset: bool,
        local_capture_clock_offset_us: i64,
    }
    struct FfiRtpReceivedPacketInfo {
        sequence_number: u16,
        ssrc: u32,
        csrcs: Vec<u32>,
        rtp_timestamp: u32,
        receive_time_us: i64,
        has_audio_level: bool,
        audio_level: u8,
        has_absolute_capture_time: bool,
        absolute_capture_time: FfiAbsoluteCaptureTime,
        has_local_capture_clock_offset: bool,
        local_capture_clock_offset_us: i64,
    }

    struct FfiMixerFrame {
        disposition: u8,
        frame: UniquePtr<NativeAudioFrame>,
        failed: bool,
    }
    struct FfiPcmViewInfo {
        valid: bool,
        channels: usize,
        samples: usize,
        size: usize,
        empty: bool,
        mono: bool,
        interleaved: bool,
    }

    struct FfiAudioFrameFields {
        timestamp: u32,
        elapsed_time_ms: i64,
        ntp_time_ms: i64,
        samples_per_channel: usize,
        sample_rate_hz: i32,
        num_channels: usize,
        speech_type: i32,
        vad_activity: i32,
        packet_infos: Vec<FfiRtpReceivedPacketInfo>,
    }

    struct FfiNetEqConfig {
        sample_rate_hz: i32,
        max_packets_in_buffer: usize,
        max_delay_ms: i32,
        min_delay_ms: i32,
        enable_fast_accelerate: bool,
        enable_muted_state: bool,
        enable_rtx_handling: bool,
        for_test_no_time_stretching: bool,
    }

    struct FfiNetEqNetworkStatistics {
        current_buffer_size_ms: u16,
        preferred_buffer_size_ms: u16,
        jitter_peaks_found: u16,
        expand_rate: u16,
        speech_expand_rate: u16,
        preemptive_rate: u16,
        accelerate_rate: u16,
        secondary_decoded_rate: u16,
        secondary_discarded_rate: u16,
        mean_waiting_time_ms: i32,
        median_waiting_time_ms: i32,
        min_waiting_time_ms: i32,
        max_waiting_time_ms: i32,
    }

    struct FfiNetEqLifetimeStatistics {
        total_samples_received: u64,
        concealed_samples: u64,
        concealment_events: u64,
        jitter_buffer_delay_ms: u64,
        jitter_buffer_emitted_count: u64,
        jitter_buffer_target_delay_ms: u64,
        jitter_buffer_minimum_delay_ms: u64,
        inserted_samples_for_deceleration: u64,
        removed_samples_for_acceleration: u64,
        silent_concealed_samples: u64,
        fec_packets_received: u64,
        fec_packets_discarded: u64,
        packets_discarded: u64,
        delayed_packet_outage_samples: u64,
        delayed_packet_outage_events: u64,
        relative_packet_arrival_delay_ms: u64,
        jitter_buffer_packets_received: u64,
        generated_noise_samples: u64,
        total_processing_delay_us: u64,
        interruption_count: i32,
        total_interruption_duration_ms: i32,
    }

    struct FfiNetEqOperationsAndState {
        preemptive_samples: u64,
        accelerate_samples: u64,
        packet_buffer_flushes: u64,
        last_waiting_time_ms: u64,
        current_buffer_size_ms: u64,
        current_frame_size_ms: u64,
        next_packet_available: bool,
    }
    struct FfiNetEqDecoderFormat {
        payload_type: i32,
        sample_rate_hz: i32,
        num_channels: i32,
        sdp_format: FfiSdpAudioFormat,
    }

    struct FfiNetEqCodec {
        payload_type: i32,
        format: FfiSdpAudioFormat,
    }
    struct FfiNetEqAudioResult {
        code: i32,
        muted: bool,
        sample_rate_hz: i32,
    }
    struct FfiNetEqNetworkStatisticsResult {
        code: i32,
        value: FfiNetEqNetworkStatistics,
    }
    struct FfiNetEqProviderAudioResult {
        code: i32,
        muted_written: bool,
        muted: bool,
        rate_written: bool,
        sample_rate_hz: i32,
    }
    struct FfiNetEqProviderStatisticsResult {
        code: i32,
        written: bool,
        value: FfiNetEqNetworkStatistics,
    }
    struct FfiNetEqDecoderFormatResult {
        present: bool,
        value: FfiNetEqDecoderFormat,
    }

    struct FfiAudioCodecParameter {
        key: Vec<u8>,
        value: Vec<u8>,
    }
    struct FfiSdpAudioFormat {
        name: Vec<u8>,
        clockrate_hz: i32,
        num_channels: usize,
        parameters: Vec<FfiAudioCodecParameter>,
    }
    struct FfiAudioCodecInfo {
        sample_rate_hz: i32,
        num_channels: usize,
        default_bitrate_bps: i32,
        min_bitrate_bps: i32,
        max_bitrate_bps: i32,
        allow_comfort_noise: bool,
        supports_network_adaption: bool,
    }
    struct FfiAudioCodecSpec {
        format: FfiSdpAudioFormat,
        info: FfiAudioCodecInfo,
    }

    struct FfiAudioEncoderBitrateStep {
        has_target: bool,
        target: i32,
        has_overhead: bool,
        overhead: usize,
        has_frame: bool,
        frame: i32,
        increasing: bool,
        occupied: bool,
    }
    struct FfiAudioEncoderBitrateProbe {
        status: u8,
        bitrates: Vec<i32>,
    }
    struct FfiAudioEncoderSmoothingProbe {
        status: u8,
        duration_applied: bool,
        has_average: bool,
        average: f32,
    }
    struct FfiAudioEncoderQuery {
        valid: bool,
        present: bool,
        info: FfiAudioCodecInfo,
    }
    struct FfiAudioEncoderCreation {
        valid: bool,
        encoder: UniquePtr<NativeAudioEncoder>,
    }
    struct FfiBitrateAllocationUpdate {
        target_bitrate: i64,
        target_kind: i8,
        packet_loss_ratio: f64,
        round_trip_time: i64,
        bwe_period: i64,
        cwnd_reduce_ratio: f64,
    }
    struct FfiAudioEncodedInfoLeaf {
        encoded_bytes: usize,
        encoded_timestamp: u32,
        payload_type: i32,
        send_even_if_empty: bool,
        speech: bool,
        encoder_type: i32,
    }
    struct FfiAudioEncoderOutput {
        status: u8,
        bytes: Vec<u8>,
        primary: FfiAudioEncodedInfoLeaf,
        redundant: Vec<FfiAudioEncodedInfoLeaf>,
    }
    struct FfiAudioEncoderRange {
        present: bool,
        minimum: i64,
        maximum: i64,
        minimum_kind: i8,
        maximum_kind: i8,
    }
    struct FfiAudioEncoderStatistics {
        counters: [u32; 6],
        present_mask: u8,
        loss: f32,
        loss_present: bool,
    }
    struct FfiAudioEncoderInfo {
        sample_rate_hz: i32,
        num_channels: usize,
        rtp_timestamp_rate_hz: i32,
        next_packet_frames: usize,
        maximum_packet_frames: usize,
        target_bitrate_bps: i32,
        dtx: bool,
    }

    struct FfiAudioDecoderCreation {
        valid: bool,
        decoder: UniquePtr<NativeAudioDecoder>,
    }
    struct FfiParsedAudioFrame {
        timestamp: u32,
        priority: i32,
        frame: UniquePtr<NativeDecodableAudioFrame>,
    }
    struct FfiParsedAudioFrames {
        valid: bool,
        frames: Vec<FfiParsedAudioFrame>,
    }
    struct FfiDecodedAudio {
        status: u8,
        speech_type: i32,
        samples: Vec<i16>,
    }
    struct FfiAudioFrameDecodeInto {
        present: bool,
        num_decoded_samples: usize,
        speech_type: i32,
    }
    struct FfiCallerParsedAudioFrame {
        timestamp: u32,
        priority: i32,
        transfer: Box<RustCallerParsedFrame>,
    }
    struct FfiAudioPacketInfo {
        valid: bool,
        duration: i32,
        redundant_duration: i32,
        has_fec: bool,
    }
    struct FfiAudioPlc {
        valid: bool,
        samples: Vec<i16>,
    }

    struct FfiCodecParameter {
        key: String,
        value: String,
    }

    struct FfiCodecFormat {
        name: String,
        parameters: Vec<FfiCodecParameter>,
        scalability_modes: Vec<String>,
    }

    #[derive(Default)]
    struct FfiNcMagnitude {
        value: i64,
        kind: i8,
    }
    #[derive(Default)]
    struct FfiNcBitrateAllocationLimits {
        min_allocatable_rate: FfiNcMagnitude,
        max_allocatable_rate: FfiNcMagnitude,
        max_padding_rate: FfiNcMagnitude,
    }
    #[derive(Default)]
    struct FfiNcStreamsConfig {
        at_time: i64,
        has_requests_alr_probing: bool,
        requests_alr_probing: bool,
        enable_repeated_initial_probing: bool,
        has_pacing_factor: bool,
        pacing_factor: f64,
        has_min_total_allocated_bitrate: bool,
        min_total_allocated_bitrate: FfiNcMagnitude,
        has_max_padding_rate: bool,
        max_padding_rate: FfiNcMagnitude,
        has_max_total_allocated_bitrate: bool,
        max_total_allocated_bitrate: FfiNcMagnitude,
    }
    #[derive(Default)]
    struct FfiNcTargetRateConstraints {
        at_time: i64,
        has_min_data_rate: bool,
        min_data_rate: FfiNcMagnitude,
        has_max_data_rate: bool,
        max_data_rate: FfiNcMagnitude,
        has_starting_rate: bool,
        starting_rate: FfiNcMagnitude,
    }
    #[derive(Default)]
    struct FfiNcNetworkAvailability {
        at_time: i64,
        network_available: bool,
    }
    #[derive(Default)]
    struct FfiNcNetworkRouteChange {
        at_time: i64,
        constraints: FfiNcTargetRateConstraints,
        restart_bwe: bool,
    }
    #[derive(Default)]
    struct FfiNcPacedPacketInfo {
        send_bitrate: FfiNcMagnitude,
        probe_cluster_id: i32,
        probe_cluster_min_probes: i32,
        probe_cluster_min_bytes: i32,
        probe_cluster_bytes_sent: i32,
    }
    #[derive(Default)]
    struct FfiNcSentPacket {
        send_time: i64,
        size: FfiNcMagnitude,
        prior_unacked_data: FfiNcMagnitude,
        pacing_info: FfiNcPacedPacketInfo,
        audio: bool,
        sequence_number: i64,
        data_in_flight: FfiNcMagnitude,
    }
    #[derive(Default)]
    struct FfiNcReceivedPacket {
        send_time: i64,
        receive_time: i64,
        size: FfiNcMagnitude,
    }
    #[derive(Default)]
    struct FfiNcRemoteBitrateReport {
        receive_time: i64,
        bandwidth: FfiNcMagnitude,
    }
    #[derive(Default)]
    struct FfiNcRoundTripTimeUpdate {
        receive_time: i64,
        round_trip_time: i64,
        smoothed: bool,
    }
    #[derive(Default)]
    struct FfiNcTransportLossReport {
        receive_time: i64,
        start_time: i64,
        end_time: i64,
        packets_lost_delta: u64,
        packets_received_delta: u64,
    }
    #[derive(Default)]
    struct FfiNcRtpPacketInfo {
        ssrc: u32,
        rtp_sequence_number: u16,
        is_retransmission: bool,
    }
    #[derive(Default)]
    struct FfiNcPacketResult {
        sent_packet: FfiNcSentPacket,
        receive_time: i64,
        has_arrival_time_offset: bool,
        arrival_time_offset: i64,
        ecn: u8,
        sent_with_ect1: bool,
        reported_lost_for_the_first_time: bool,
        reported_recovered_for_the_first_time: bool,
        has_rtp_packet_info: bool,
        rtp_packet_info: FfiNcRtpPacketInfo,
    }
    #[derive(Default)]
    struct FfiNcTransportPacketsFeedback {
        feedback_time: i64,
        data_in_flight: FfiNcMagnitude,
        transport_supports_ecn: bool,
        packet_feedbacks: Vec<FfiNcPacketResult>,
        sendless_arrival_times: Vec<i64>,
    }
    #[derive(Default)]
    struct FfiNcNetworkEstimate {
        at_time: i64,
        bandwidth: FfiNcMagnitude,
        round_trip_time: i64,
        bwe_period: i64,
        loss_rate_ratio: f32,
    }
    #[derive(Default)]
    struct FfiNcPacerConfig {
        at_time: i64,
        data_window: FfiNcMagnitude,
        time_window: i64,
        pad_window: FfiNcMagnitude,
    }
    #[derive(Default)]
    struct FfiNcProbeClusterConfig {
        at_time: i64,
        target_data_rate: FfiNcMagnitude,
        target_duration: i64,
        min_probe_delta: i64,
        target_probe_count: i32,
        id: i32,
    }
    #[derive(Default)]
    struct FfiNcTargetTransferRate {
        at_time: i64,
        network_estimate: FfiNcNetworkEstimate,
        target_rate: FfiNcMagnitude,
        cwnd_reduce_ratio: f64,
    }
    #[derive(Default)]
    struct FfiNcNetworkControlUpdate {
        has_congestion_window: bool,
        congestion_window: FfiNcMagnitude,
        has_pacer_config: bool,
        pacer_config: FfiNcPacerConfig,
        probe_cluster_configs: Vec<FfiNcProbeClusterConfig>,
        has_target_rate: bool,
        target_rate: FfiNcTargetTransferRate,
        failed: bool,
    }
    #[derive(Default)]
    struct FfiNcProcessInterval {
        at_time: i64,
        has_pacer_queue: bool,
        pacer_queue: FfiNcMagnitude,
    }
    #[derive(Default)]
    struct FfiNcNetworkStateEstimate {
        confidence: f64,
        update_time: i64,
        last_receive_time: i64,
        last_send_time: i64,
        link_capacity: FfiNcMagnitude,
        link_capacity_lower: FfiNcMagnitude,
        link_capacity_upper: FfiNcMagnitude,
        pre_link_buffer_delay: i64,
        post_link_buffer_delay: i64,
        propagation_delay: i64,
        time_delta: i64,
        last_feed_time: i64,
        cross_delay_rate: f64,
        spike_delay_rate: f64,
        link_capacity_std_dev: FfiNcMagnitude,
        link_capacity_min: FfiNcMagnitude,
        cross_traffic_ratio: f64,
    }
    #[derive(Default)]
    struct FfiNcOptionalEstimate {
        present: bool,
        value: FfiNcNetworkStateEstimate,
    }
    #[derive(Default)]
    struct FfiNcMessage {
        network_availability: FfiNcNetworkAvailability,
        network_route_change: FfiNcNetworkRouteChange,
        process_interval: FfiNcProcessInterval,
        remote_bitrate_report: FfiNcRemoteBitrateReport,
        round_trip_time_update: FfiNcRoundTripTimeUpdate,
        sent_packet: FfiNcSentPacket,
        received_packet: FfiNcReceivedPacket,
        streams_config: FfiNcStreamsConfig,
        target_rate_constraints: FfiNcTargetRateConstraints,
        transport_loss_report: FfiNcTransportLossReport,
        transport_packets_feedback: FfiNcTransportPacketsFeedback,
        network_state_estimate: FfiNcNetworkStateEstimate,
    }
    #[derive(Default)]
    struct FfiNcConfig {
        constraints: FfiNcTargetRateConstraints,
        stream_based_config: FfiNcStreamsConfig,
        default_pacing_time_window: i64,
    }

    struct FfiFecProtectionParams {
        fec_rate: i32,
        max_fec_frames: i32,
        mask_type: i32,
    }
    struct FfiFecProtectionResult {
        status: u8,
        native_result: i32,
        video_bps: u32,
        nack_bps: u32,
        fec_bps: u32,
    }
    struct FfiNetworkPredictorInput {
        send_time_ms: i64,
        arrival_time_ms: i64,
        state: i32,
    }
    struct FfiVideoBitrateCodecLayer {
        width: i32,
        height: i32,
        max_framerate: f32,
        temporal_layers: u8,
        max_bitrate_kbps: u32,
        target_bitrate_kbps: u32,
        min_bitrate_kbps: u32,
        qp_max: u32,
        active: bool,
        format: UniquePtr<NativeSdpVideoFormat>,
    }
    #[derive(Default)]
    struct FfiVideoBitrateCodec {
        codec_type: i32,
        width: u16,
        height: u16,
        start_bitrate_kbps: u32,
        max_bitrate_kbps: u32,
        min_bitrate_kbps: u32,
        max_framerate: u32,
        active: bool,
        qp_max: u32,
        number_of_simulcast_streams: u8,
        simulcast_streams: Vec<FfiVideoBitrateCodecLayer>,
        spatial_layers: Vec<FfiVideoBitrateCodecLayer>,
        mode: i32,
        expect_encode_from_texture: bool,
        timing_delay_ms: i64,
        timing_outlier_ratio_percent: u16,
        legacy_conference_mode: bool,
        scalability_mode: String,
        complexity: i32,
        frame_drop_enabled: bool,
        temporal_layers: u8,
        denoising: bool,
        automatic_resize: bool,
        key_frame_interval: i32,
        adaptive_qp: bool,
        specific_spatial_layers: u8,
        flexible_mode: bool,
        inter_layer_prediction: i32,
    }
    #[derive(Default)]
    struct FfiVideoBitrateAllocation {
        layer_bitrates_bps: [[u32; 4]; 5],
        has_layer_bitrate: [[bool; 4]; 5],
        bandwidth_limited: bool,
    }

    struct FfiVideoCodecCapability {
        format: UniquePtr<NativeSdpVideoFormat>,
        clock_rate: i32,
        preferred_payload_type: i32,
        rtcp_feedback: Vec<String>,
    }

    struct FfiAudioCodecCapability {
        format: FfiCodecFormat,
        clock_rate: i32,
        channels: i32,
    }

    struct FfiCodecSupport {
        supported: bool,
        power_efficient: bool,
    }
    struct FfiVideoFactorySupport {
        support: FfiCodecSupport,
        error: u8,
    }

    struct FfiRtpHeaderExtension {
        uri: String,
        preferred_id: i32,
        preferred_encrypt: bool,
        direction: u8,
    }

    #[derive(Default)]
    struct FfiSimulcastStream {
        width: u32,
        height: u32,
        active: bool,
        temporal_layers: u8,
    }

    #[derive(Default)]
    struct FfiEncoderSettings {
        width: u32,
        height: u32,
        start_bitrate_bps: u32,
        max_bitrate_bps: u32,
        min_bitrate_bps: u32,
        max_framerate: u32,
        cores: u32,
        max_payload_size: u32,
        has_scalability_mode: bool,
        scalability_mode: String,
        has_h264_temporal_layers: bool,
        h264_temporal_layers: u8,
        simulcast_temporal_layers: Vec<u8>,
        simulcast_streams: Vec<FfiSimulcastStream>,
    }

    struct FfiDecoderSettings {
        cores: u32,
        max_width: u32,
        max_height: u32,
    }

    #[derive(Default)]
    struct FfiCodecDependencyTemplate {
        spatial_id: i32,
        temporal_id: i32,
        indications: Vec<i32>,
        frame_diffs: Vec<i32>,
        chain_diffs: Vec<i32>,
    }
    #[derive(Default)]
    struct FfiCodecDependencyStructure {
        structure_id: i32,
        decode_targets: i32,
        chains: i32,
        protected_by_chain: Vec<i32>,
        resolutions: Vec<FfiRtcEventResolution>,
        templates: Vec<FfiCodecDependencyTemplate>,
    }
    struct FfiCodecBufferUsage {
        id: i32,
        referenced: bool,
        updated: bool,
    }
    struct FfiCodecSpecificFields {
        codec_type: i32,
        end_of_picture: bool,
        vp8_non_reference: bool,
        vp8_temporal_idx: u8,
        vp8_layer_sync: bool,
        vp8_key_idx: i8,
        vp8_explicit_dependencies: bool,
        vp8_referenced: [usize; 3],
        vp8_referenced_count: usize,
        vp8_updated: [usize; 3],
        vp8_updated_count: usize,
        vp9_first_frame: bool,
        vp9_inter_pic: bool,
        vp9_flexible: bool,
        vp9_ss_available: bool,
        vp9_non_ref_inter_layer: bool,
        vp9_temporal_idx: u8,
        vp9_temporal_up_switch: bool,
        vp9_inter_layer: bool,
        vp9_gof_idx: u8,
        vp9_spatial_layers: usize,
        vp9_first_active: usize,
        vp9_has_resolution: bool,
        vp9_width: [u16; 8],
        vp9_height: [u16; 8],
        vp9_gof: FfiVp9GofInfo,
        vp9_ref_count: u8,
        vp9_p_diff: [u8; 3],
        h264_packetization: i32,
        h264_temporal_idx: u8,
        h264_base_sync: bool,
        h264_idr: bool,
        has_generic: bool,
        generic: FfiCodecDependencyTemplate,
        buffers: Vec<FfiCodecBufferUsage>,
        part_of_chain: Vec<bool>,
        active_targets: u32,
        has_structure: bool,
        structure: FfiCodecDependencyStructure,
        has_scalability: bool,
        scalability: u8,
        instrumentation: UniquePtr<NativeFrameInstrumentationData>,
    }
    struct FfiEncodedCallbackResult {
        invoked: bool,
        error: i32,
        frame_id: u32,
        drop_next_frame: bool,
    }
    struct FfiOwnedEncoderSettings {
        loss_notification: bool,
        number_of_cores: i32,
        max_payload_size: usize,
        has_thread_limit: bool,
        thread_limit: i32,
    }
    struct FfiOwnedVideoRateControl {
        target: FfiVideoBitrateAllocation,
        effective: FfiVideoBitrateAllocation,
        framerate_fps: f64,
        bandwidth_bps: i64,
        bandwidth_kind: i8,
    }
    struct FfiVideoEncoderLossNotification {
        last_decodable: u32,
        last_received: u32,
        has_dependencies_decodable: bool,
        dependencies_decodable: bool,
        has_received_decodable: bool,
        received_decodable: bool,
    }
    struct FfiRateControl {
        bitrate_bps: u32,
        target_bitrate_bps: u32,
        framerate_fps: f64,
        bandwidth_bps: u64,
        bandwidth_limited: bool,
        target_bandwidth_limited: bool,
        layer_bitrates_bps: [[u32; 4]; 5],
        has_layer_bitrate: [[bool; 4]; 5],
        target_layer_bitrates_bps: [[u32; 4]; 5],
        has_target_layer_bitrate: [[bool; 4]; 5],
    }

    struct FfiTemporalFrameRates {
        fractions: Vec<u8>,
    }

    struct FfiEncoderInfo {
        implementation_name: String,
        hardware_accelerated: bool,
        supports_native_handle: bool,
        supports_simulcast: bool,
        fps_allocation: Vec<FfiTemporalFrameRates>,
    }

    struct FfiEncodedVideoMetadata {
        codec: u8,
        simulcast_index: i32,
        spatial_index: i32,
        temporal_index: i32,
        end_of_picture: bool,
        non_reference: bool,
        layer_sync: bool,
        key_index: i32,
        first_frame_in_picture: bool,
        inter_picture_predicted: bool,
        flexible_mode: bool,
        num_spatial_layers: u8,
        inter_layer_predicted: bool,
        temporal_up_switch: bool,
    }

    struct FfiDecoderInfo {
        implementation_name: String,
        hardware_accelerated: bool,
    }

    #[derive(Default)]
    struct FfiRtpPacketEvent {
        present: bool,
        event_type: u8,
        media_type: i32,
    }
    struct FfiDtmfToneChange {
        present: bool,
        tone: String,
        has_remaining_tones: bool,
        remaining_tones: String,
    }

    struct FfiSenderEncoding {
        rid: String,
        active: bool,
        has_max_bitrate: bool,
        max_bitrate_bps: i32,
        has_max_framerate: bool,
        max_framerate: f64,
        has_scale_by: bool,
        scale_by: f64,
        has_scale_to: bool,
        scale_to_width: i32,
        scale_to_height: i32,
        has_scalability_mode: bool,
        scalability_mode: String,
    }

    struct FfiSenderParameters {
        transaction_id: String,
        encodings: Vec<FfiSenderEncoding>,
    }

    struct FfiScreenSource {
        id: i64,
        name: String,
        display_id: i64,
    }

    #[derive(Default)]
    struct FfiDesktopOptions {
        use_update_notifications: bool,
        disable_effects: bool,
        detect_updated_region: bool,
        prefer_cursor_embedded: bool,
        pipewire_supported: bool,
        allow_pipewire: bool,
        width: u32,
        height: u32,
        pipewire_use_damage_region: bool,
        x11_supported: bool,
        has_x_display: bool,
        has_screencast_stream: bool,
        has_full_screen_window_detector: bool,
    }
    #[derive(Default)]
    struct FfiDesktopRect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }
    #[derive(Default)]
    struct FfiDesktopFrameInfo {
        width: i32,
        height: i32,
        stride: i32,
        pixel_format: u32,
        left: i32,
        top: i32,
        scale_factor: f32,
        rect_valid: bool,
        rect: FfiDesktopRect,
        dpi_x: i32,
        dpi_y: i32,
        has_device_scale: bool,
        device_scale: f32,
        may_contain_cursor: bool,
        capture_time_ms: i64,
        capturer_id: u32,
        icc_profile: Vec<u8>,
        updated_region: Vec<FfiDesktopRect>,
        has_cpu_data: bool,
        has_texture: bool,
        has_shared_memory: bool,
        shared_memory_size: usize,
        shared_memory_id: i32,
        shared_memory_has_handle: bool,
    }
    #[derive(Default)]
    struct FfiDesktopBytes {
        status: u8,
        bytes: Vec<u8>,
    }
    struct FfiDesktopMemoryInfo {
        size: usize,
        id: i32,
        has_handle: bool,
    }
    struct FfiDesktopFd {
        fd: i32,
        error: i32,
    }
    #[derive(Default)]
    struct FfiDesktopMetadata {
        session_handle: String,
        pipewire_stream_node_id: u32,
        has_proxy: bool,
        has_cancellable: bool,
    }
    #[derive(Default)]
    struct FfiDesktopControl {
        status: u8,
        flag: bool,
        sources: Vec<FfiScreenSource>,
    }
    struct FfiCameraFormat {
        width: u32,
        height: u32,
        max_fps: u32,
        pixel_format: i32,
    }

    #[derive(Default)]
    struct FfiCameraSettings {
        width: i32,
        height: i32,
        max_fps: i32,
        pixel_format: i32,
        interlaced: bool,
    }
    #[derive(Default)]
    struct FfiCameraControl {
        status: u8,
        native_result: i32,
        flag: bool,
        device_name: String,
        settings: FfiCameraSettings,
    }
    #[derive(Default)]
    struct FfiCameraDeviceControl {
        status: u8,
        native_result: i32,
        orientation: u16,
        matched_index: i32,
        capabilities: Vec<FfiCameraSettings>,
    }
    #[derive(Default)]
    struct FfiCameraInfoRequest {
        kind: u8,
        index: u32,
        id: String,
        title: String,
        name_capacity: u32,
        unique_capacity: u32,
        product_present: bool,
        product_capacity: u32,
        x: u32,
        y: u32,
        settings: FfiCameraSettings,
    }
    #[derive(Default)]
    struct FfiCameraInfoReply {
        status: u8,
        native_result: i32,
        count: u32,
        rotation: u16,
        settings: FfiCameraSettings,
        name: Vec<u8>,
        unique_id: Vec<u8>,
        product_id: Vec<u8>,
    }
    struct FfiCameraDevice {
        name: String,
        id: String,
        product_id: String,
    }

    struct FfiAudioDevice {
        index: u16,
        name: String,
        id: String,
    }

    struct FfiApmRuntimeSetting {
        kind: u8,
        float_value: f32,
        int_value: i32,
        device_id: i32,
        device_max_volume: i32,
        bool_value: bool,
    }

    struct FfiApmStatistics {
        error: u8,
        present: u16,
        voice_detected: bool,
        echo_return_loss: f64,
        echo_return_loss_enhancement: f64,
        divergent_filter_fraction: f64,
        delay_median_ms: i32,
        delay_standard_deviation_ms: i32,
        residual_echo_likelihood: f64,
        residual_echo_likelihood_recent_max: f64,
        delay_ms: i32,
    }

    struct FfiApmFormats {
        input_rate: i32,
        input_channels: usize,
        output_rate: i32,
        output_channels: usize,
        reverse_input_rate: i32,
        reverse_input_channels: usize,
        reverse_output_rate: i32,
        reverse_output_channels: usize,
    }

    struct FfiApmConfig {
        pipeline_maximum_internal_processing_rate: i32,
        pipeline_multi_channel_render: bool,
        pipeline_multi_channel_capture: bool,
        pipeline_capture_downmix_method: i32,
        pipeline_capture_downmix_method_stereo_aec: i32,
        pre_amplifier_enabled: bool,
        pre_amplifier_fixed_gain_factor: f32,
        capture_level_adjustment_enabled: bool,
        capture_level_adjustment_pre_gain_factor: f32,
        capture_level_adjustment_post_gain_factor: f32,
        capture_level_adjustment_analog_mic_gain_emulation_enabled: bool,
        capture_level_adjustment_analog_mic_gain_emulation_initial_level: i32,
        high_pass_filter_enabled: bool,
        high_pass_filter_apply_in_full_band: bool,
        echo_canceller_enabled: bool,
        echo_canceller_export_linear_aec_output: bool,
        echo_canceller_enforce_high_pass_filtering: bool,
        noise_suppression_enabled: bool,
        noise_suppression_level: i32,
        noise_suppression_analyze_linear_aec_output_when_available: bool,
        transient_suppression_enabled: bool,
        gain_controller1_enabled: bool,
        gain_controller1_mode: i32,
        gain_controller1_target_level_dbfs: i32,
        gain_controller1_compression_gain_db: i32,
        gain_controller1_enable_limiter: bool,
        gain_controller1_analog_gain_controller_enabled: bool,
        gain_controller1_analog_gain_controller_startup_min_volume: i32,
        gain_controller1_analog_gain_controller_clipped_level_min: i32,
        gain_controller1_analog_gain_controller_enable_digital_adaptive: bool,
        gain_controller1_analog_gain_controller_clipped_level_step: i32,
        gain_controller1_analog_gain_controller_clipped_ratio_threshold: f32,
        gain_controller1_analog_gain_controller_clipped_wait_frames: i32,
        gain_controller1_analog_gain_controller_clipping_predictor_enabled: bool,
        gain_controller1_analog_gain_controller_clipping_predictor_mode: i32,
        gain_controller1_analog_gain_controller_clipping_predictor_window_length: i32,
        gain_controller1_analog_gain_controller_clipping_predictor_reference_window_length: i32,
        gain_controller1_analog_gain_controller_clipping_predictor_reference_window_delay: i32,
        gain_controller1_analog_gain_controller_clipping_predictor_clipping_threshold: f32,
        gain_controller1_analog_gain_controller_clipping_predictor_crest_factor_margin: f32,
        gain_controller1_analog_gain_controller_clipping_predictor_use_predicted_step: bool,
        gain_controller2_enabled: bool,
        gain_controller2_input_volume_controller_enabled: bool,
        gain_controller2_adaptive_digital_enabled: bool,
        gain_controller2_adaptive_digital_headroom_db: f32,
        gain_controller2_adaptive_digital_max_gain_db: f32,
        gain_controller2_adaptive_digital_initial_gain_db: f32,
        gain_controller2_adaptive_digital_max_gain_change_db_per_second: f32,
        gain_controller2_adaptive_digital_max_output_noise_level_dbfs: f32,
        gain_controller2_fixed_digital_gain_db: f32,
    }

    struct FfiAudioProcessingFormatState {
        processing_rate_hz: i32,
        split_rate_hz: i32,
        input_channels: usize,
        processing_channels: usize,
        output_channels: usize,
        reverse_channels: usize,
    }

    struct FfiAudioProcessingConfig {
        enabled: bool,
        echo_cancellation: bool,
        noise_suppression: u8,
        gain_control: u8,
    }

    struct FfiAudioProcessingState {
        binding_error: u8,
        has_module: bool,
        echo_software: i8,
        echo_reported_software: i8,
        echo_reported_effective: u8,
        echo_platform_available: bool,
        echo_platform: i8,
        echo_effective: u8,
        noise_software: i8,
        noise_platform_available: bool,
        noise_platform: i8,
        noise_effective: u8,
        gain_software: i8,
        gain_platform_available: bool,
        gain_platform: i8,
        gain_effective: u8,
        highpass_software: i8,
        highpass_platform_available: bool,
        highpass_platform: i8,
        highpass_effective: u8,
    }

    struct FfiCryptoOptions {
        enable_gcm_crypto_suites: bool,
        prefer_gcm_crypto_suites: bool,
        enable_aes128_sha1_32_crypto_cipher: bool,
        enable_aes128_sha1_80_crypto_cipher: bool,
        enable_encrypted_rtp_header_extensions: bool,
        cryptex_policy: u8,
        require_frame_encryption: bool,
        key_exchange_groups: Vec<u16>,
    }

    struct FfiTlsCertificateStats {
        fingerprint: String,
        fingerprint_algorithm: String,
        base64_certificate: String,
    }

    struct FfiCertificateKeyParams {
        kind: u8,
        modulus_bits: u32,
        public_exponent: u32,
        curve: i32,
    }

    struct FfiCertificatePem {
        private_key: String,
        certificate: String,
    }

    struct FfiDtlsInformation {
        state: i32,
        has_role: bool,
        role: i32,
        has_tls_version: bool,
        tls_version: i32,
        has_ssl_cipher_suite: bool,
        ssl_cipher_suite: i32,
        has_srtp_cipher_suite: bool,
        srtp_cipher_suite: i32,
        has_ssl_group_id: bool,
        ssl_group_id: i32,
        has_remote_certificates: bool,
    }
    struct FfiSctpInformation {
        state: i32,
        has_dtls_transport: bool,
        has_max_message_size: bool,
        max_message_size: f64,
        has_max_channels: bool,
        max_channels: i32,
    }
    struct FfiRtcError {
        kind: i32,
        detail: i32,
        message_bytes: Vec<u8>,
        has_sctp_cause_code: bool,
        sctp_cause_code: u16,
    }
    struct FfiIcePacketEvent {
        kind: u8,
        flag: bool,
        state: i32,
        payload: Vec<u8>,
        source: UniquePtr<NativeSocketAddress>,
        has_arrival_time: bool,
        arrival_time: i64,
        ecn: i32,
        decryption: i32,
        packet_id: i64,
        send_time_ms: i64,
        info: FfiIcePacketInfo,
        route: FfiIceRoute,
    }

    struct FfiIceRouteEndpoint {
        adapter_type: i32,
        adapter_id: u16,
        network_id: u16,
        uses_turn: bool,
    }
    struct FfiIceRoute {
        present: bool,
        connected: bool,
        local: FfiIceRouteEndpoint,
        remote: FfiIceRouteEndpoint,
        last_sent_packet_id: i32,
        packet_overhead: i32,
    }
    struct FfiIcePacketInfo {
        included_in_feedback: bool,
        included_in_allocation: bool,
        is_media: bool,
        packet_type: i32,
        protocol: i32,
        has_network_id: bool,
        network_id: u16,
        packet_size_bytes: usize,
        turn_overhead_bytes: usize,
        ip_overhead_bytes: usize,
    }
    struct FfiIcePacketOptions {
        dscp: i32,
        ect_1: bool,
        packet_id: i64,
        rtp_sendtime_extension_id: i32,
        srtp_auth_key: Vec<u8>,
        srtp_auth_tag_len: i32,
        srtp_packet_index: i64,
        info: FfiIcePacketInfo,
        batchable: bool,
        last_packet_in_batch: bool,
    }
    struct FfiIcePacketState {
        available: bool,
        name: Vec<u8>,
        writable: bool,
        receiving: bool,
        error: i32,
    }
    struct FfiIceSocketResult {
        available: bool,
        result: i32,
        error: i32,
    }
    struct FfiIceOption {
        available: bool,
        success: bool,
        value: i32,
    }
    struct FfiIceRouteResult {
        available: bool,
        route: FfiIceRoute,
    }

    struct FfiTransportCandidate {
        id: Vec<u8>,
        protocol: Vec<u8>,
        relay_protocol: Vec<u8>,
        username: Vec<u8>,
        password: Vec<u8>,
        network_name: Vec<u8>,
        foundation: Vec<u8>,
        tcptype: Vec<u8>,
        url: Vec<u8>,
        component: i32,
        priority: u32,
        candidate_type: i32,
        network_type: i32,
        underlying_type_for_vpn: i32,
        generation: u32,
        network_id: u16,
        network_cost: u16,
        network_slice: i32,
        address: UniquePtr<NativeSocketAddress>,
        related_address: UniquePtr<NativeSocketAddress>,
    }
    struct FfiTransportCandidateParse {
        candidate: UniquePtr<NativeTransportCandidate>,
        error: UniquePtr<NativeRtcError>,
    }
    struct FfiIceCandidateStats {
        candidate: UniquePtr<NativeTransportCandidate>,
        has_stun_stats: bool,
        stun_binding_requests_sent: i32,
        stun_binding_responses_received: i32,
        stun_binding_rtt_ms_total: f64,
        stun_binding_rtt_ms_squared_total: f64,
    }
    struct FfiIceConnectionInfo {
        best_connection: bool,
        writable: bool,
        receiving: bool,
        timeout: bool,
        rtt: usize,
        sent_discarded_bytes: usize,
        sent_total_bytes: usize,
        sent_bytes_second: usize,
        sent_discarded_packets: usize,
        sent_total_packets: usize,
        sent_ping_requests_total: usize,
        sent_ping_requests_before_first_response: usize,
        sent_ping_responses: usize,
        recv_total_bytes: usize,
        recv_bytes_second: usize,
        packets_received: usize,
        recv_ping_requests: usize,
        recv_ping_responses: usize,
        local_candidate: UniquePtr<NativeTransportCandidate>,
        remote_candidate: UniquePtr<NativeTransportCandidate>,
        state: i32,
        priority: u64,
        nominated: bool,
        total_round_trip_time_ms: u64,
        has_current_round_trip_time_ms: bool,
        current_round_trip_time_ms: u32,
        has_last_data_received: bool,
        last_data_received: i64,
        has_last_data_sent: bool,
        last_data_sent: i64,
    }
    struct FfiIceStats {
        available: bool,
        success: bool,
        candidate_stats: Vec<FfiIceCandidateStats>,
        connection_infos: Vec<FfiIceConnectionInfo>,
        selected_candidate_pair_changes: u32,
        bytes_sent: u64,
        bytes_received: u64,
        packets_sent: u64,
        packets_received: u64,
        ice_role: i32,
        ice_local_username_fragment: Vec<u8>,
        ice_state: i32,
    }
    struct FfiIcePair {
        available: bool,
        local: UniquePtr<NativeTransportCandidate>,
        remote: UniquePtr<NativeTransportCandidate>,
    }

    struct FfiIceConfig {
        has_receiving_timeout: bool,
        receiving_timeout: i64,
        has_backup_connection_ping_interval: bool,
        backup_connection_ping_interval: i64,
        has_stable_writable_connection_ping_interval: bool,
        stable_writable_connection_ping_interval: i64,
        has_regather_on_failed_networks_interval: bool,
        regather_on_failed_networks_interval: i64,
        has_receiving_switching_delay: bool,
        receiving_switching_delay: i64,
        has_ice_check_interval_strong_connectivity: bool,
        ice_check_interval_strong_connectivity: i64,
        has_ice_check_interval_weak_connectivity: bool,
        ice_check_interval_weak_connectivity: i64,
        has_ice_check_min_interval: bool,
        ice_check_min_interval: i64,
        has_ice_unwritable_timeout: bool,
        ice_unwritable_timeout: i64,
        has_ice_inactive_timeout: bool,
        ice_inactive_timeout: i64,
        has_stun_keepalive_interval: bool,
        stun_keepalive_interval: i64,
        prioritize_most_likely_candidate_pairs: bool,
        presume_writable_when_fully_relayed: bool,
        surface_ice_candidates_on_ice_transport_type_changed: bool,
        dtls_handshake_in_stun: bool,
        continual_gathering_policy: u8,
        default_nomination_mode: u8,
        vpn_preference: u8,
        has_network_preference: bool,
        network_preference: i32,
        has_ice_unwritable_min_checks: bool,
        ice_unwritable_min_checks: i32,
    }

    struct FfiIceInternalValue {
        available: bool,
        present: bool,
        value: i32,
    }

    struct FfiCertificateData {
        pem: String,
        der: Vec<u8>,
        expiration_unix_seconds: i64,
        has_signature_digest_algorithm: bool,
        signature_digest_algorithm: String,
    }

    struct FfiCertificateFingerprint {
        valid: bool,
        algorithm: String,
        value: String,
        digest: Vec<u8>,
    }

    struct FfiFactoryOptions {
        disable_encryption: bool,
        disable_network_monitor: bool,
        network_ignore_mask: i32,
        ssl_max_version: i32,
    }

    struct FfiOfferAnswerOptions {
        receive_video: i32,
        receive_audio: i32,
        voice_activity_detection: bool,
        ice_restart: bool,
        use_rtp_mux: bool,
        raw_packetization_for_video: bool,
        simulcast_layers: i32,
        use_obsolete_sctp_sdp: bool,
    }

    struct FfiNetworkHandle {
        network: SharedPtr<NativeNetwork>,
    }
    struct FfiIpNetworkMask {
        ip: Vec<u8>,
        prefix_length: i32,
    }
    struct FfiDefaultLocalAddress {
        present: bool,
        ip: Vec<u8>,
    }
    struct FfiCallAdmissionStats {
        has_call: bool,
        live: bool,
        clock_bounds: FfiClockTimeBounds,
        rtp_arrival_rejections: u64,
    }
    struct FfiOwnedDnsStart {
        status: u8,
        query: UniquePtr<NativeDnsQuery>,
    }
    struct FfiOwnedDnsResult {
        status: u8,
        result: UniquePtr<NativeOwnedDnsResult>,
    }
    struct FfiOwnedSocketResult {
        status: u8,
        code: i32,
        value: i32,
        address: UniquePtr<NativeSocketAddress>,
    }
    struct FfiListenSubscriptionResult {
        status: u8,
        subscription: UniquePtr<NativeListenSocketSubscription>,
    }
    struct FfiSocketSubscriptionResult {
        status: u8,
        subscription: UniquePtr<NativePacketSocketSubscription>,
    }
    struct FfiSocketObservation {
        kind: u8,
        error: i32,
        sent: FfiSocketSentPacket,
        address: UniquePtr<NativeSocketAddress>,
        packet: FfiSocketReceivedPacket,
    }
    struct FfiSocketTcpOptions {
        options: i32,
        alpn: Vec<FfiRtpByteString>,
        curves: Vec<FfiRtpByteString>,
        verifier: SharedPtr<NativeTlsVerifier>,
    }
    struct FfiSocketCreateRequest {
        kind: u8,
        local: UniquePtr<NativeSocketAddress>,
        remote: UniquePtr<NativeSocketAddress>,
        min_port: u16,
        max_port: u16,
        tcp: FfiSocketTcpOptions,
    }
    struct FfiSocketAcceptResult {
        status: u8,
        ticket: SharedPtr<NativePacketSocketAccept>,
        socket: UniquePtr<NativeAcceptedPacketSocket>,
    }
    struct FfiSocketOptionResult {
        code: i32,
        value: i32,
    }
    struct FfiSocketPacketInfo {
        included_in_feedback: bool,
        included_in_allocation: bool,
        is_media: bool,
        packet_type: i32,
        protocol: i32,
        has_network_id: bool,
        network_id: u16,
        packet_size_bytes: usize,
        turn_overhead_bytes: usize,
        ip_overhead_bytes: usize,
    }
    struct FfiSocketPacketOptions {
        dscp: i32,
        ect_1: bool,
        packet_id: i64,
        extension_id: i32,
        auth_key: Vec<u8>,
        auth_tag_len: i32,
        packet_index: i64,
        info: FfiSocketPacketInfo,
        batchable: bool,
        last_packet_in_batch: bool,
    }
    struct FfiSocketSentPacket {
        packet_id: i64,
        send_time_ms: i64,
        info: FfiSocketPacketInfo,
    }
    struct FfiSocketReceivedPacket {
        payload: Vec<u8>,
        address: UniquePtr<NativeSocketAddress>,
        has_arrival: bool,
        arrival_us: i64,
        ecn: u8,
        decryption: u8,
    }
    struct FfiInterfaceAddress {
        ip: Vec<u8>,
        flags: i32,
    }
    struct FfiNetworkCostGuess {
        adapter: i32,
        vpn: bool,
        slice: i32,
    }
    struct FfiNetworkInterfaceInfo {
        adapter_type: i32,
        underlying_type_for_vpn: i32,
        preference: i32,
        available: bool,
        slice: i32,
    }

    struct FfiIceServer {
        uri: String,
        urls: Vec<String>,
        username: String,
        password: String,
        tls_certificate_policy: u8,
        hostname: String,
        tls_alpn_protocols: Vec<String>,
        tls_elliptic_curves: Vec<String>,
    }

    struct FfiOptionalF64 {
        present: bool,
        value: f64,
    }

    struct FfiTrackAudioOptionsResult {
        code: i32,
        message: Vec<u8>,
    }
    struct FfiAudioProcessorStats {
        typing_noise_detected: bool,
        voice_detected: i8,
        echo_return_loss: FfiOptionalF64,
        echo_return_loss_enhancement: FfiOptionalF64,
        divergent_filter_fraction: FfiOptionalF64,
        residual_echo_likelihood: FfiOptionalF64,
        residual_echo_likelihood_recent_max: FfiOptionalF64,
        delay_median_ms: FfiOptionalI32,
        delay_standard_deviation_ms: FfiOptionalI32,
        delay_ms: FfiOptionalI32,
    }
    struct FfiVideoSourceInfo {
        is_screencast: bool,
        needs_denoising: i8,
        supports_encoded_output: bool,
        has_stats: bool,
        input_width: i32,
        input_height: i32,
    }
    struct FfiOptionalI32 {
        present: bool,
        value: i32,
    }
    struct FfiOptionalI64 {
        present: bool,
        value: i64,
    }
    struct FfiNetworkMask {
        address: String,
        prefix_length: i32,
    }
    struct FfiPortAllocatorOptions {
        min_port: i32,
        max_port: i32,
        flags: u32,
    }
    struct FfiPeerConnectionOptions {
        bundle_policy: u8,
        rtcp_mux_policy: u8,
        disable_ipv6_on_wifi: bool,
        max_ipv6_networks: i32,
        disable_link_local_networks: bool,
        screencast_min_bitrate: FfiOptionalI32,
        tcp_candidate_policy: u8,
        candidate_network_policy: u8,
        audio_jitter_buffer_max_packets: i32,
        audio_jitter_buffer_fast_accelerate: bool,
        audio_jitter_buffer_min_delay_ms: i32,
        ice_connection_receiving_timeout: i32,
        ice_backup_candidate_pair_ping_interval: i32,
        continual_gathering_policy: u8,
        prioritize_most_likely_ice_candidate_pairs: bool,
        prune_turn_ports: bool,
        turn_port_prune_policy: u8,
        presume_writable_when_fully_relayed: bool,
        enable_ice_renomination: bool,
        redetermine_role_on_ice_restart: bool,
        surface_ice_candidates_on_ice_transport_type_changed: bool,
        ice_check_interval_strong_connectivity: FfiOptionalI32,
        ice_check_interval_weak_connectivity: FfiOptionalI32,
        ice_check_min_interval: FfiOptionalI32,
        ice_unwritable_timeout: FfiOptionalI32,
        ice_unwritable_min_checks: FfiOptionalI32,
        ice_inactive_timeout: FfiOptionalI32,
        stun_candidate_keepalive_interval: FfiOptionalI32,
        stable_writable_connection_ping_interval_ms: FfiOptionalI32,
        report_usage_pattern_delay_ms: FfiOptionalI32,
        network_preference: FfiOptionalI32,
        sdp_semantics: u8,
        crypto_options: FfiCryptoOptions,
        offer_extmap_allow_mixed: bool,
        turn_logging_id: String,
        enable_implicit_rollback: bool,
        vpn_preference: u8,
        vpn_list: Vec<FfiNetworkMask>,
        port_allocator: FfiPortAllocatorOptions,
        pacer_burst_interval: FfiOptionalI64,
        enable_any_address_ports: bool,
        max_sctp_streams: i32,
        enable_sctp_snap: bool,
        dscp: bool,
        stats_timestamp_with_environment_clock: bool,
        cpu_adaptation: bool,
        suspend_below_min_bitrate: bool,
        prerenderer_smoothing: bool,
        experiment_cpu_load_estimator: bool,
        audio_rtcp_report_interval_ms: i32,
        video_rtcp_report_interval_ms: i32,
    }
    struct FfiPeerConfiguration {
        ice_candidate_pool_size: i32,
        always_negotiate_data_channels: bool,
        ice_servers: Vec<FfiIceServer>,
        ice_transport_policy: u8,
        turn_tls_ca_pem: String,
        options: FfiPeerConnectionOptions,
    }

    struct FfiPeerIceSnapshot {
        legacy_state: i32,
        standardized_state: i32,
        has_can_trickle: bool,
        can_trickle: bool,
    }
    struct FfiDescriptionSnapshot {
        slot: u8,
        kind: u8,
        sdp: Vec<u8>,
    }

    struct FfiLegacyStatsValue {
        name: i32,
        display_name: Vec<u8>,
        native_string: Vec<u8>,
        kind: u8,
        integer: i64,
        decimal: f32,
        flag: bool,
        text: Vec<u8>,
        id: UniquePtr<NativeLegacyStatsId>,
    }
    struct FfiLegacyStatsReport {
        id: UniquePtr<NativeLegacyStatsId>,
        timestamp_ms: f64,
        native_type: Vec<u8>,
        values: Vec<FfiLegacyStatsValue>,
    }
    struct FfiRtcStatsBytes {
        value: Vec<u8>,
    }
    struct FfiRtcStatsUint64Entry {
        key: Vec<u8>,
        value: u64,
    }
    struct FfiRtcStatsDoubleEntry {
        key: Vec<u8>,
        value: f64,
    }
    #[derive(Default)]
    struct FfiRtcStatsValue {
        kind: u8,
        has_value: bool,
        boolean: bool,
        int32_value: i32,
        uint32_value: u32,
        int64_value: i64,
        uint64_value: u64,
        decimal: f64,
        text: Vec<u8>,
        booleans: Vec<u8>,
        int32s: Vec<i32>,
        uint32s: Vec<u32>,
        int64s: Vec<i64>,
        uint64s: Vec<u64>,
        decimals: Vec<f64>,
        texts: Vec<FfiRtcStatsBytes>,
        uint64_map: Vec<FfiRtcStatsUint64Entry>,
        decimal_map: Vec<FfiRtcStatsDoubleEntry>,
    }
    struct FfiRtcStatsAttribute {
        name: Vec<u8>,
        value: FfiRtcStatsValue,
    }
    struct FfiRtcStatsData {
        class_kind: u8,
        fields: Vec<FfiRtcStatsValue>,
    }
    struct FfiRtcStatsEntry {
        value: UniquePtr<NativeRtcStats>,
    }

    struct FfiStatsField {
        name: String,
        kind: u8,
        text: Vec<u8>,
        unsigned_value: u64,
        signed_value: i64,
        decimal: f64,
        flag: bool,
    }

    struct FfiStatsRecord {
        id: Vec<u8>,
        kind: u8,
        timestamp_us: i64,
        fields: Vec<FfiStatsField>,
    }

    struct FfiPeerEventObservation {
        dropped_control_events: u64,
        coalesced_advisories: u64,
        retained_control_events: u32,
        admitted_operations: u32,
        pending_operations: u32,
    }

    struct FfiIcePairChange {
        transport_name: Vec<u8>,
        local: UniquePtr<NativeTransportCandidate>,
        remote: UniquePtr<NativeTransportCandidate>,
        last_data_received_ms: i64,
        reason: Vec<u8>,
        estimated_disconnected_time_ms: i64,
        retained_bytes: usize,
    }
    struct FfiTrackArrival {
        receiver: UniquePtr<NativeRtpReceiver>,
        streams: UniquePtr<NativeMediaStreams>,
    }
    struct FfiPeerChannelMessage {
        unix_timestamp_ms: i64,
        datachannel_id: i32,
        label: Vec<u8>,
        direction: u8,
        data_type: u8,
        data: Vec<u8>,
    }
    struct FfiPeerEvent {
        channel_message: FfiPeerChannelMessage,
        pair_change: FfiIcePairChange,
        ice_state: i32,
        legacy_stats_reports: Vec<FfiLegacyStatsReport>,
        stats_report: UniquePtr<NativeRtcStatsReport>,
        stats_timestamp_us: i64,
        stats_records: Vec<FfiStatsRecord>,
        kind: u8,
        operation_id: u64,
        has_description: bool,
        sdp_type: u8,
        state: u8,
        event_id: u32,
        sdp_mline_index: i32,
        port: i32,
        error_code: i32,
        error_type: u8,
        sdp: Vec<u8>,
        sdp_mid: Vec<u8>,
        candidate: Vec<u8>,
        address: String,
        url: String,
        message: Vec<u8>,
    }

    struct FfiDataChannelInit {
        has_configuration: bool,
        reliable: bool,
        ordered: bool,
        has_max_retransmit_time_ms: bool,
        max_retransmit_time_ms: i32,
        has_max_retransmits: bool,
        max_retransmits: i32,
        protocol: Vec<u8>,
        negotiated: bool,
        id: i32,
        priority: i32,
    }

    struct FfiDataChannelError {
        has_error: bool,
        error_type: u8,
        error_detail: u8,
        sctp_cause_code: i32,
        message: Vec<u8>,
    }

    struct FfiDataChannelCounters {
        messages_sent: u32,
        bytes_sent: u64,
        messages_received: u32,
        bytes_received: u64,
    }

    struct FfiDataChannelReceiveQueueLimits {
        max_messages: usize,
        max_bytes: usize,
    }

    struct FfiDataChannelEvent {
        dropped_messages: u64,
        dropped_bytes: u64,
        kind: u8,
        send_id: u64,
        canceled: bool,
        error: FfiDataChannelError,
        max_message_size: i32,
        state: u8,
        binary: bool,
        sent_data_size: u64,
        data: Vec<u8>,
    }

    struct FfiDecoderStatistics {
        decoder_creations: u64,
        configure_calls: u64,
        decode_calls: u64,
        input_packets: u64,
        last_input_hash: u64,
        decoded_outputs: u64,
        decode_errors: u64,
        thread_token: u64,
        thread_mismatches: u64,
    }

    #[allow(dead_code)]
    struct FfiCodecTestResult {
        status: i32,
        encoded_frames: u32,
        decoded_frames: u32,
        checksum: u64,
    }

    struct FfiSframeInit {
        mode: i32,
        suite: i32,
    }
    struct FfiSframeEncrypterCreation {
        product: UniquePtr<NativeSframeEncrypter>,
        error: UniquePtr<NativeRtcError>,
        detached_owner: bool,
    }
    struct FfiSframeDecrypterCreation {
        product: UniquePtr<NativeSframeDecrypter>,
        error: UniquePtr<NativeRtcError>,
        detached_owner: bool,
    }
    struct FfiFrameCryptoOutput {
        status: i32,
        written: usize,
        bytes: Vec<u8>,
    }
    struct FfiFrameDecryptionResult {
        status: i32,
        written: usize,
    }
    struct FfiFrameCryptoFailure {
        kind: u8,
        first: usize,
        second: usize,
    }
    struct FfiNetEqFactoryCreation {
        product: UniquePtr<NativeNetEq>,
        observer: Box<RustNetEqFailureObserver>,
        failure: u8,
        message: String,
    }

    struct FfiAudioDevicePcm {
        frames: usize,
        bytes_per_frame: usize,
        channels: usize,
        rate: i64,
        bits: i32,
        delay_ms: u32,
        clock_drift: i32,
        mic_level: u32,
        key_pressed: bool,
        capture_time_present: bool,
        capture_time_ns: i64,
    }
    struct FfiAudioDeviceCallbackResult {
        failed: bool,
        status: i32,
        frames: usize,
        mic_level: u32,
        elapsed_ms: i64,
        ntp_ms: i64,
    }
    #[derive(Default)]
    struct FfiEchoCanceller3Config {
        buffering_excess_render_detection_interval_blocks: usize,
        buffering_max_allowed_excess_render_blocks: usize,
        delay_default_delay: usize,
        delay_down_sampling_factor: usize,
        delay_num_filters: usize,
        delay_delay_headroom_samples: usize,
        delay_hysteresis_limit_blocks: usize,
        delay_fixed_capture_delay_samples: usize,
        delay_delay_estimate_smoothing: f32,
        delay_delay_estimate_smoothing_delay_found: f32,
        delay_delay_candidate_detection_threshold: f32,
        delay_delay_selection_thresholds_initial: i32,
        delay_delay_selection_thresholds_converged: i32,
        delay_use_external_delay_estimator: bool,
        delay_log_warning_on_delay_changes: bool,
        delay_render_alignment_mixing_downmix: bool,
        delay_render_alignment_mixing_adaptive_selection: bool,
        delay_render_alignment_mixing_activity_power_threshold: f32,
        delay_render_alignment_mixing_prefer_first_two_channels: bool,
        delay_capture_alignment_mixing_downmix: bool,
        delay_capture_alignment_mixing_adaptive_selection: bool,
        delay_capture_alignment_mixing_activity_power_threshold: f32,
        delay_capture_alignment_mixing_prefer_first_two_channels: bool,
        delay_detect_pre_echo: bool,
        filter_refined_length_blocks: usize,
        filter_refined_leakage_converged: f32,
        filter_refined_leakage_diverged: f32,
        filter_refined_error_floor: f32,
        filter_refined_error_ceil: f32,
        filter_refined_noise_gate: f32,
        filter_coarse_length_blocks: usize,
        filter_coarse_rate: f32,
        filter_coarse_noise_gate: f32,
        filter_refined_initial_length_blocks: usize,
        filter_refined_initial_leakage_converged: f32,
        filter_refined_initial_leakage_diverged: f32,
        filter_refined_initial_error_floor: f32,
        filter_refined_initial_error_ceil: f32,
        filter_refined_initial_noise_gate: f32,
        filter_coarse_initial_length_blocks: usize,
        filter_coarse_initial_rate: f32,
        filter_coarse_initial_noise_gate: f32,
        filter_config_change_duration_blocks: usize,
        filter_initial_state_seconds: f32,
        filter_coarse_reset_hangover_blocks: i32,
        filter_conservative_initial_phase: bool,
        filter_enable_coarse_filter_output_usage: bool,
        filter_use_linear_filter: bool,
        filter_high_pass_filter_echo_reference: bool,
        filter_export_linear_aec_output: bool,
        erle_min: f32,
        erle_max_l: f32,
        erle_max_h: f32,
        erle_onset_detection: bool,
        erle_num_sections: usize,
        erle_clamp_quality_estimate_to_zero: bool,
        erle_clamp_quality_estimate_to_one: bool,
        ep_strength_default_gain: f32,
        ep_strength_default_len: f32,
        ep_strength_nearend_len: f32,
        ep_strength_echo_can_saturate: bool,
        ep_strength_bounded_erl: bool,
        ep_strength_erle_onset_compensation_in_dominant_nearend: bool,
        ep_strength_use_conservative_tail_frequency_response: bool,
        echo_audibility_low_render_limit: f32,
        echo_audibility_normal_render_limit: f32,
        echo_audibility_floor_power: f32,
        echo_audibility_audibility_threshold_lf: f32,
        echo_audibility_audibility_threshold_mf: f32,
        echo_audibility_audibility_threshold_hf: f32,
        echo_audibility_use_stationarity_properties: bool,
        echo_audibility_use_stationarity_properties_at_init: bool,
        render_levels_active_render_limit: f32,
        render_levels_poor_excitation_render_limit: f32,
        render_levels_poor_excitation_render_limit_ds8: f32,
        render_levels_render_power_gain_db: f32,
        echo_removal_control_has_clock_drift: bool,
        echo_removal_control_linear_and_stable_echo_path: bool,
        echo_model_noise_floor_hold: usize,
        echo_model_min_noise_floor_power: f32,
        echo_model_stationary_gate_slope: f32,
        echo_model_noise_gate_power: f32,
        echo_model_noise_gate_slope: f32,
        echo_model_render_pre_window_size: usize,
        echo_model_render_post_window_size: usize,
        echo_model_model_reverb_in_nonlinear_mode: bool,
        comfort_noise_noise_floor_dbfs: f32,
        suppressor_nearend_average_blocks: usize,
        suppressor_normal_tuning_mask_lf_enr_transparent: f32,
        suppressor_normal_tuning_mask_lf_enr_suppress: f32,
        suppressor_normal_tuning_mask_lf_emr_transparent: f32,
        suppressor_normal_tuning_mask_hf_enr_transparent: f32,
        suppressor_normal_tuning_mask_hf_enr_suppress: f32,
        suppressor_normal_tuning_mask_hf_emr_transparent: f32,
        suppressor_normal_tuning_max_inc_factor: f32,
        suppressor_normal_tuning_max_dec_factor_lf: f32,
        suppressor_nearend_tuning_mask_lf_enr_transparent: f32,
        suppressor_nearend_tuning_mask_lf_enr_suppress: f32,
        suppressor_nearend_tuning_mask_lf_emr_transparent: f32,
        suppressor_nearend_tuning_mask_hf_enr_transparent: f32,
        suppressor_nearend_tuning_mask_hf_enr_suppress: f32,
        suppressor_nearend_tuning_mask_hf_emr_transparent: f32,
        suppressor_nearend_tuning_max_inc_factor: f32,
        suppressor_nearend_tuning_max_dec_factor_lf: f32,
        suppressor_lf_smoothing_during_initial_phase: bool,
        suppressor_last_permanent_lf_smoothing_band: i32,
        suppressor_last_lf_smoothing_band: i32,
        suppressor_last_lf_band: i32,
        suppressor_first_hf_band: i32,
        suppressor_dominant_nearend_detection_enr_threshold: f32,
        suppressor_dominant_nearend_detection_enr_exit_threshold: f32,
        suppressor_dominant_nearend_detection_snr_threshold: f32,
        suppressor_dominant_nearend_detection_hold_duration: i32,
        suppressor_dominant_nearend_detection_trigger_threshold: i32,
        suppressor_dominant_nearend_detection_use_during_initial_phase: bool,
        suppressor_dominant_nearend_detection_use_unbounded_echo_spectrum: bool,
        suppressor_subband_nearend_detection_nearend_average_blocks: usize,
        suppressor_subband_nearend_detection_subband1_low: usize,
        suppressor_subband_nearend_detection_subband1_high: usize,
        suppressor_subband_nearend_detection_subband2_low: usize,
        suppressor_subband_nearend_detection_subband2_high: usize,
        suppressor_subband_nearend_detection_nearend_threshold: f32,
        suppressor_subband_nearend_detection_snr_threshold: f32,
        suppressor_use_subband_nearend_detection: bool,
        suppressor_high_bands_suppression_enr_threshold: f32,
        suppressor_high_bands_suppression_max_gain_during_echo: f32,
        suppressor_high_bands_suppression_anti_howling_activation_threshold: f32,
        suppressor_high_bands_suppression_anti_howling_gain: f32,
        suppressor_high_frequency_suppression_limiting_gain_band: i32,
        suppressor_high_frequency_suppression_bands_in_limiting_gain: i32,
        suppressor_floor_first_increase: f32,
        suppressor_conservative_hf_suppression: bool,
        multi_channel_detect_stereo_content: bool,
        multi_channel_stereo_detection_threshold: f32,
        multi_channel_stereo_detection_timeout_threshold_seconds: i32,
        multi_channel_stereo_detection_hysteresis_seconds: f32,
    }
    struct FfiNeuralEchoInput {
        render_bands: usize,
        render_channels: usize,
        render: Vec<f32>,
        microphone: Vec<f32>,
        linear_error: Vec<f32>,
        linear_echo_spectrum: Vec<f32>,
        microphone_spectrum: Vec<f32>,
        linear_error_spectrum: Vec<f32>,
        dominant_nearend: bool,
        conservative_channels: usize,
        unbounded_channels: usize,
    }
    struct FfiNeuralEchoResult {
        failed: bool,
        conservative: Vec<f32>,
        unbounded: Vec<f32>,
    }
    struct FfiEchoAudioShape {
        rate: i32,
        channels: usize,
        frames: usize,
        bands: usize,
        band_frames: usize,
        split_ready: bool,
    }
    struct FfiEchoControlAudio {
        error: u8,
        samples: Vec<f32>,
        linear: Vec<f32>,
    }
    struct FfiEchoControlMetrics {
        error: u8,
        echo_return_loss: f64,
        echo_return_loss_enhancement: f64,
        delay_ms: i32,
    }
    struct FfiAec3Selection {
        mono_present: bool,
        mono: FfiEchoCanceller3Config,
        multi_present: bool,
        multi: FfiEchoCanceller3Config,
    }
    struct FfiNativeAudioDeviceCreation {
        layer: u8,
        bypass: bool,
        helper: bool,
    }
    struct FfiAudioDeviceResult {
        status: i32,
        int_value: u32,
        bool_value: bool,
    }
    struct FfiAudioDeviceName {
        status: i32,
        name: Vec<u8>,
        guid: Vec<u8>,
    }
    struct FfiAudioDeviceStatistics {
        present: bool,
        synthesized_samples_duration_s: f64,
        synthesized_samples_events: u64,
        total_samples_duration_s: f64,
        total_playout_delay_s: f64,
        total_samples_count: u64,
        dropped_samples_duration_s: f64,
        dropped_samples_events: u64,
        total_capture_samples_duration_s: f64,
        total_capture_delay_s: f64,
        total_capture_samples_count: u64,
    }
    struct FfiAudioDevicePlatformState {
        topology: i32,
        echo_cancellation_available: bool,
        noise_suppression_available: bool,
        auto_gain_control_available: bool,
        echo_cancellation_requested: u8,
        noise_suppression_requested: u8,
        auto_gain_control_requested: u8,
        echo_cancellation_active: u8,
        noise_suppression_active: u8,
        auto_gain_control_active: u8,
        voice_processing_enabled_requested: u8,
        voice_processing_bypassed_requested: u8,
        voice_processing_agc_enabled_requested: u8,
        voice_processing_enabled_active: u8,
        voice_processing_bypassed_active: u8,
        voice_processing_agc_enabled_active: u8,
    }
    struct FfiApmEchoDetectorMetrics {
        failed: bool,
        likelihood_present: bool,
        likelihood: f64,
        recent_max_present: bool,
        recent_max: f64,
    }
    struct FfiApmDumpConfig {
        aec_enabled: bool,
        aec_delay_agnostic_enabled: bool,
        aec_drift_compensation_enabled: bool,
        aec_extended_filter_enabled: bool,
        aec_suppression_level: i32,
        agc_enabled: bool,
        agc_mode: i32,
        agc_limiter_enabled: bool,
        hpf_enabled: bool,
        ns_enabled: bool,
        ns_level: i32,
        transient_suppression_enabled: bool,
        noise_robust_agc_enabled: bool,
        pre_amplifier_enabled: bool,
        pre_amplifier_fixed_gain_factor: f32,
        experiments_description: Vec<u8>,
        api_config_string: Vec<u8>,
    }
    extern "Rust" {
        type RustMixerProvider;
        fn mixer_provider_add(
            provider: &RustMixerProvider,
            token: UniquePtr<NativeMixerSourceToken>,
            inputs: &NativeMixerInputs,
            failed: &mut bool,
        ) -> bool;
        fn mixer_provider_remove(
            provider: &RustMixerProvider,
            token: UniquePtr<NativeMixerSourceToken>,
            inputs: &NativeMixerInputs,
        ) -> bool;
        fn mixer_provider_mix(
            provider: &RustMixerProvider,
            channels: usize,
            inputs: &NativeMixerInputs,
        ) -> FfiMixerFrame;
        fn mixer_provider_invalid(provider: &RustMixerProvider);
        type RustMixerModule;
        fn mixer_module_failed(module: &RustMixerModule, error: u8);
        type RustAudioDeviceCallbacks;
        fn audio_device_callbacks_dispose(callbacks: &mut RustAudioDeviceCallbacks);
        fn audio_device_callbacks_invalid(callbacks: &RustAudioDeviceCallbacks, operation: u8);
        fn audio_device_recorded(
            callbacks: &RustAudioDeviceCallbacks,
            pcm: FfiAudioDevicePcm,
            data: &[u8],
        ) -> FfiAudioDeviceCallbackResult;
        fn audio_device_render(
            callbacks: &RustAudioDeviceCallbacks,
            pcm: FfiAudioDevicePcm,
            pull: bool,
            data: &mut [u8],
        ) -> FfiAudioDeviceCallbackResult;
        fn audio_device_observed(callbacks: &RustAudioDeviceCallbacks, speech: bool, event: i32);
        type RustInstalledApmEnvironment;
        type RustApmCustomModule;
        type RustEchoEnvironment;
        fn apm_echo_factory_create(
            module: &RustApmCustomModule,
            environment: UniquePtr<NativeEnvironment>,
            retention: &RustEchoEnvironment,
            context: &NativeEchoCreationContext,
            dependencies: Pin<&mut NativeEchoDependencies>,
            neural_overload: bool,
        ) -> UniquePtr<NativeEchoControl>;
        fn apm_echo_factory_invalid(module: &RustApmCustomModule);
        fn apm_echo_environment_native(
            environment: &RustEchoEnvironment,
        ) -> UniquePtr<NativeEnvironment>;
        fn apm_echo_control_analyze(
            module: &RustApmCustomModule,
            dependencies: Pin<&mut NativeEchoDependencies>,
            audio: &NativeEchoAudio,
            render: bool,
        ) -> bool;
        fn apm_echo_control_process(
            module: &RustApmCustomModule,
            dependencies: Pin<&mut NativeEchoDependencies>,
            audio: Pin<&mut NativeEchoAudio>,
            linear: Pin<&mut NativeEchoLinearOutput>,
            linear_overload: bool,
            level_change: bool,
        ) -> bool;
        fn apm_echo_control_metrics(
            module: &RustApmCustomModule,
            dependencies: Pin<&mut NativeEchoDependencies>,
        ) -> FfiEchoControlMetrics;
        fn apm_echo_control_delay(
            module: &RustApmCustomModule,
            dependencies: Pin<&mut NativeEchoDependencies>,
            milliseconds: i32,
        ) -> bool;
        fn apm_echo_control_usage(
            module: &RustApmCustomModule,
            dependencies: Pin<&mut NativeEchoDependencies>,
            used: bool,
        ) -> bool;
        fn apm_echo_control_active(
            module: &RustApmCustomModule,
            dependencies: Pin<&mut NativeEchoDependencies>,
            valid: &mut bool,
        ) -> bool;
        fn apm_echo_control_invalid(module: &RustApmCustomModule, audio: bool);
        fn apm_echo_initialize(
            module: &RustApmCustomModule,
            capture_rate: i32,
            capture_channels: i32,
            render_rate: i32,
            render_channels: i32,
        ) -> bool;
        fn apm_echo_analyze(module: &RustApmCustomModule, render: bool, samples: &[f32]) -> bool;
        fn apm_echo_metrics(module: &RustApmCustomModule) -> FfiApmEchoDetectorMetrics;
        fn apm_neural_estimate(
            module: &RustApmCustomModule,
            input: FfiNeuralEchoInput,
        ) -> FfiNeuralEchoResult;
        fn apm_neural_adjust(
            module: &RustApmCustomModule,
            config: FfiEchoCanceller3Config,
            valid: &mut bool,
        ) -> FfiEchoCanceller3Config;
        fn apm_neural_reset(module: &RustApmCustomModule) -> bool;
        fn apm_neural_invalid(module: &RustApmCustomModule);
        fn apm_neural_adjust_invalid(module: &RustApmCustomModule);
        fn apm_custom_dispose(module: &mut RustApmCustomModule) -> bool;
        fn apm_custom_initialize(module: &RustApmCustomModule, rate: i32, channels: i32) -> bool;
        fn apm_custom_description(module: &RustApmCustomModule, valid: &mut bool) -> Vec<u8>;
        fn apm_custom_process(
            module: &RustApmCustomModule,
            rate: i32,
            channels: usize,
            frames: usize,
            samples: &mut [f32],
        ) -> bool;
        fn apm_custom_analyze(
            module: &RustApmCustomModule,
            rate: i32,
            channels: usize,
            frames: usize,
            samples: &[f32],
        ) -> bool;
        fn apm_custom_runtime(module: &RustApmCustomModule, setting: FfiApmRuntimeSetting) -> bool;
        fn apm_custom_unqualified_render(module: &RustApmCustomModule);
        type RustCallerApmDump;
        fn apm_dump_dispose(dump: &mut RustCallerApmDump) -> bool;
        fn apm_dump_initialize(
            dump: &RustCallerApmDump,
            formats: FfiApmFormats,
            time_now_ms: i64,
        ) -> bool;
        fn apm_dump_audio(
            dump: &RustCallerApmDump,
            operation: u8,
            layout: u8,
            channels: usize,
            frames: usize,
            integers: &[i16],
            floats: &[f32],
        ) -> bool;
        fn apm_dump_state(
            dump: &RustCallerApmDump,
            delay: i32,
            drift: i32,
            volume_present: bool,
            volume: i32,
            keypress: bool,
        ) -> bool;
        fn apm_dump_capture(dump: &RustCallerApmDump) -> bool;
        fn apm_dump_runtime(dump: &RustCallerApmDump, setting: FfiApmRuntimeSetting) -> bool;
        fn apm_dump_config(dump: &RustCallerApmDump, config: FfiApmDumpConfig) -> bool;
        type RustApmDumpWorker;
        fn apm_dump_worker_queue(worker: &RustApmDumpWorker) -> &NativeTaskQueue;
        type RustMixerSource;
        fn mixer_source_frame(source: &RustMixerSource, rate: i32) -> FfiMixerFrame;
        fn mixer_source_ssrc(source: &RustMixerSource, valid: &mut bool) -> i32;
        fn mixer_source_rate(source: &RustMixerSource, valid: &mut bool) -> i32;
        fn mixer_source_invalid(source: &RustMixerSource);
        type RustDesktopMemoryFactory;
        type RustDesktopMemoryLease;
        fn desktop_memory_factory_present(source: &RustDesktopMemoryFactory) -> bool;
        fn desktop_memory_factory_create(
            source: &RustDesktopMemoryFactory,
            size: usize,
        ) -> UniquePtr<NativeDesktopMemory>;
        type RustDesktopCaptureReceiver;
        type RustDesktopSelectionObserver;
        fn desktop_receiver_present(source: &RustDesktopCaptureReceiver) -> bool;
        fn desktop_receiver_start(source: &RustDesktopCaptureReceiver);
        fn desktop_receiver_result(
            source: &RustDesktopCaptureReceiver,
            result: i32,
            frame: UniquePtr<NativeDesktopFrame>,
        );
        fn desktop_selection_present(source: &RustDesktopSelectionObserver) -> bool;
        fn desktop_selection_notify(source: &RustDesktopSelectionObserver, event: u8);
        type RustCameraInitializationObserver;
        fn camera_options_initialized(source: &RustCameraInitializationObserver, status: i32);
        type RustEncodedVideoObserver;
        fn encoded_video_observer_closed(source: &RustEncodedVideoObserver);
        fn encoded_video_frame_observed(
            source: &RustEncodedVideoObserver,
            frame: FfiEncodedVideoFrame,
            dropped: u64,
        );
        type RustDecodedVideoObserver;
        fn decoded_video_frame_observed(
            source: &RustDecodedVideoObserver,
            frame: UniquePtr<NativeRawVideoFrame>,
            native_observed_at_us: i64,
        );
        type RustRawCameraSink;
        fn camera_raw_sink_present(source: &RustRawCameraSink) -> bool;
        fn camera_raw_frame(
            source: &RustRawCameraSink,
            bytes: &mut [u8],
            settings: FfiCameraSettings,
            rotation: u16,
            capture_time: i64,
        ) -> i32;
        type RustCallerAudioDecoderCall;
        type RustCallerParsedFrame;
        fn audio_caller_decoder_scalar_callback(
            source: &RustCallerAudioDecoderCall,
            kind: u8,
        ) -> i32;
        fn audio_caller_decoder_channels_callback(source: &RustCallerAudioDecoderCall) -> usize;
        fn audio_caller_decoder_reset_callback(source: &RustCallerAudioDecoderCall);
        fn audio_caller_decoder_packet_duration_callback(
            source: &RustCallerAudioDecoderCall,
            encoded: &[u8],
            redundant: bool,
        ) -> i32;
        fn audio_caller_decoder_packet_fec_callback(
            source: &RustCallerAudioDecoderCall,
            encoded: &[u8],
        ) -> bool;
        fn audio_caller_decoder_parse_callback(
            source: &RustCallerAudioDecoderCall,
            encoded: Vec<u8>,
            timestamp: u32,
        ) -> Vec<FfiCallerParsedAudioFrame>;
        fn audio_caller_parsed_frame_present(source: &RustCallerParsedFrame) -> bool;
        fn audio_caller_parsed_frame_bind(
            source: Box<RustCallerParsedFrame>,
        ) -> Box<RustCallerAudioFrameCall>;
        fn audio_caller_parsed_frame_from_sdk(
            source: Box<RustCallerAudioFrameCall>,
        ) -> Box<RustCallerParsedFrame>;
        fn audio_caller_parsed_frame_null(
            source: &RustCallerAudioDecoderCall,
        ) -> Box<RustCallerParsedFrame>;
        fn audio_caller_decoder_plc_callback(
            source: &RustCallerAudioDecoderCall,
            requested: usize,
            prefix: &[i16],
        ) -> FfiAudioPlc;
        fn audio_caller_decoder_plc_capacity(source: &RustCallerAudioDecoderCall) -> usize;
        fn audio_caller_decoder_compatibility_refused(source: &RustCallerAudioDecoderCall);
        fn audio_caller_decoder_plc_invalid(source: &RustCallerAudioDecoderCall);
        type RustCallerAudioFrameCall;
        fn audio_caller_frame_duration_callback(source: &RustCallerAudioFrameCall) -> usize;
        fn audio_caller_frame_dtx_callback(source: &RustCallerAudioFrameCall) -> bool;
        fn audio_caller_frame_decode_callback(
            source: &RustCallerAudioFrameCall,
            capacity: usize,
        ) -> FfiDecodedAudio;
        fn audio_caller_frame_guard_error(source: &RustCallerAudioFrameCall, kind: u8);
        type RustAudioEncoderFactoryCall;
        fn audio_caller_encoder_environment(
            source: &RustAudioEncoderFactoryCall,
        ) -> UniquePtr<NativeEnvironment>;
        fn audio_caller_encoder_formats_callback(
            source: &RustAudioEncoderFactoryCall,
        ) -> Vec<FfiAudioCodecSpec>;
        fn audio_caller_encoder_query_callback(
            source: &RustAudioEncoderFactoryCall,
            format: FfiSdpAudioFormat,
        ) -> FfiAudioEncoderQuery;
        fn audio_caller_encoder_create_callback(
            source: &RustAudioEncoderFactoryCall,
            environment: UniquePtr<NativeEnvironment>,
            format: FfiSdpAudioFormat,
            options: UniquePtr<NativeAudioEncoderOptions>,
        ) -> UniquePtr<NativeAudioEncoder>;
        fn audio_caller_encoder_transfer_failed(source: &RustAudioEncoderFactoryCall);
        type RustAudioDecoderFactoryCall;
        fn audio_caller_decoder_environment(
            source: &RustAudioDecoderFactoryCall,
        ) -> UniquePtr<NativeEnvironment>;
        fn audio_caller_decoder_formats_callback(
            source: &RustAudioDecoderFactoryCall,
        ) -> Vec<FfiAudioCodecSpec>;
        fn audio_caller_decoder_supported_callback(
            source: &RustAudioDecoderFactoryCall,
            format: FfiSdpAudioFormat,
        ) -> bool;
        fn audio_caller_decoder_create_callback(
            source: &RustAudioDecoderFactoryCall,
            environment: UniquePtr<NativeEnvironment>,
            format: FfiSdpAudioFormat,
            compatibility: bool,
            pair: UniquePtr<NativeAudioCodecPairId>,
        ) -> UniquePtr<NativeAudioDecoder>;
        fn audio_caller_decoder_transfer_failed(
            source: &RustAudioDecoderFactoryCall,
            compatibility: bool,
        );
        type AudioEncoderTransfer;
        fn audio_encoder_transfer_native(source: &AudioEncoderTransfer) -> &NativeAudioEncoder;
        type AudioDecoderTransfer;
        fn audio_decoder_transfer_native(source: &AudioDecoderTransfer) -> &NativeAudioDecoder;
        type RustFrameTransformerProvider;
        fn frame_transformer_provider_transform(
            provider: &RustFrameTransformerProvider,
            frame: UniquePtr<NativeTransformableFrame>,
            completion: UniquePtr<NativeFrameTransformCompletion>,
        );
        fn frame_transformer_provider_binding(
            provider: &RustFrameTransformerProvider,
            callback: UniquePtr<NativeFrameTransformCallback>,
            registered: bool,
        );
        fn frame_transformer_provider_panics(provider: &RustFrameTransformerProvider) -> u64;
        type RustFrameEncryptorProvider;
        type RustFrameDecryptorProvider;
        fn frame_encryptor_provider_failure(
            provider: &RustFrameEncryptorProvider,
        ) -> FfiFrameCryptoFailure;
        fn frame_decryptor_provider_failure(
            provider: &RustFrameDecryptorProvider,
        ) -> FfiFrameCryptoFailure;
        fn frame_encryptor_provider_max_size(
            provider: &RustFrameEncryptorProvider,
            media: i32,
            size: usize,
        ) -> usize;
        fn frame_decryptor_provider_max_size(
            provider: &RustFrameDecryptorProvider,
            media: i32,
            size: usize,
        ) -> usize;
        fn frame_encryptor_provider_encrypt(
            provider: &RustFrameEncryptorProvider,
            media: i32,
            ssrc: u32,
            additional: &[u8],
            input: &[u8],
            capacity: usize,
        ) -> FfiFrameCryptoOutput;
        fn frame_decryptor_provider_decrypt(
            provider: &RustFrameDecryptorProvider,
            media: i32,
            csrcs: &[u32],
            additional: &[u8],
            input: &[u8],
            capacity: usize,
        ) -> FfiFrameCryptoOutput;
        type RustNetEqFactory;
        type RustNetEqFailureObserver;
        fn neteq_factory_create_product(
            source: &RustNetEqFactory,
            environment: UniquePtr<NativeEnvironment>,
            config: FfiNetEqConfig,
            decoder_factory: UniquePtr<NativeAudioDecoderFactory>,
            sdk_consumer: bool,
        ) -> FfiNetEqFactoryCreation;
        type RustNetEqProvider;
        fn neteq_provider_failed(source: &RustNetEqProvider) -> bool;
        fn neteq_provider_insert_packet(
            source: &RustNetEqProvider,
            header: FfiMediaRtpHeader,
            payload: &[u8],
        ) -> i32;
        fn neteq_provider_insert_packet_at(
            source: &RustNetEqProvider,
            header: FfiMediaRtpHeader,
            payload: &[u8],
            receive_time: i64,
        ) -> i32;
        fn neteq_provider_insert_packet_info(
            source: &RustNetEqProvider,
            header: FfiMediaRtpHeader,
            payload: &[u8],
            info: FfiRtpReceivedPacketInfo,
        ) -> i32;
        fn neteq_provider_insert_empty_packet(
            source: &RustNetEqProvider,
            header: FfiMediaRtpHeader,
        );
        fn neteq_provider_get_audio(
            source: &RustNetEqProvider,
            frame: Pin<&mut NativeAudioFrame>,
            initialized: bool,
            has_muted: bool,
            has_rate: bool,
            has_override: bool,
            operation: i32,
        ) -> FfiNetEqProviderAudioResult;
        fn neteq_provider_set_codecs(source: &RustNetEqProvider, codecs: Vec<FfiNetEqCodec>);
        fn neteq_provider_register_payload_type(
            source: &RustNetEqProvider,
            payload_type: i32,
            format: FfiSdpAudioFormat,
        ) -> bool;
        fn neteq_provider_create_decoder(source: &RustNetEqProvider, payload_type: i32) -> bool;
        fn neteq_provider_remove_payload_type(source: &RustNetEqProvider, payload_type: u8) -> i32;
        fn neteq_provider_remove_all_payload_types(source: &RustNetEqProvider);
        fn neteq_provider_set_maximum_buffer_packets(source: &RustNetEqProvider, count: usize);
        fn neteq_provider_set_fast_accelerate(source: &RustNetEqProvider, enable: bool);
        fn neteq_provider_set_minimum_delay(source: &RustNetEqProvider, delay_ms: i32) -> bool;
        fn neteq_provider_set_maximum_delay(source: &RustNetEqProvider, delay_ms: i32) -> bool;
        fn neteq_provider_set_base_minimum_delay(source: &RustNetEqProvider, delay_ms: i32)
        -> bool;
        fn neteq_provider_base_minimum_delay(source: &RustNetEqProvider) -> i32;
        fn neteq_provider_target_delay(source: &RustNetEqProvider) -> i32;
        fn neteq_provider_filtered_current_delay(source: &RustNetEqProvider) -> i32;
        fn neteq_provider_network_statistics(
            source: &RustNetEqProvider,
            has_stats: bool,
        ) -> FfiNetEqProviderStatisticsResult;
        fn neteq_provider_current_network_statistics(
            source: &RustNetEqProvider,
        ) -> FfiNetEqNetworkStatistics;
        fn neteq_provider_lifetime_statistics(
            source: &RustNetEqProvider,
        ) -> FfiNetEqLifetimeStatistics;
        fn neteq_provider_operations_and_state(
            source: &RustNetEqProvider,
        ) -> FfiNetEqOperationsAndState;
        fn neteq_provider_playout_timestamp(source: &RustNetEqProvider, present: &mut bool) -> u32;
        fn neteq_provider_last_output_sample_rate(source: &RustNetEqProvider) -> i32;
        fn neteq_provider_current_decoder_format(
            source: &RustNetEqProvider,
        ) -> FfiNetEqDecoderFormatResult;
        fn neteq_provider_decoder_format(
            source: &RustNetEqProvider,
            payload_type: i32,
        ) -> FfiNetEqDecoderFormatResult;
        fn neteq_provider_flush_buffers(source: &RustNetEqProvider);
        fn neteq_provider_enable_nack(source: &RustNetEqProvider, max_list_size: usize);
        fn neteq_provider_disable_nack(source: &RustNetEqProvider);
        fn neteq_provider_nack_list(
            source: &RustNetEqProvider,
            round_trip_time_ms: i64,
        ) -> Vec<u16>;
        fn neteq_provider_sync_buffer_size(source: &RustNetEqProvider) -> i32;
        fn neteq_receive_time_safe(minimum: i64, maximum: i64, receive: i64) -> bool;
        type RustDefaultLocalAddressProvider;
        fn default_address_get(
            source: &RustDefaultLocalAddressProvider,
            family: i32,
        ) -> FfiDefaultLocalAddress;
        fn default_address_failed(source: &RustDefaultLocalAddressProvider) -> bool;
        type RustMdnsResponder;
        type RustMdnsCallback;
        fn mdns_source_create(
            source: &RustMdnsResponder,
            ip: Vec<u8>,
            completion: SharedPtr<NativeMdnsCompletion>,
        ) -> bool;
        fn mdns_source_remove(
            source: &RustMdnsResponder,
            ip: Vec<u8>,
            completion: SharedPtr<NativeMdnsCompletion>,
        ) -> bool;
        fn mdns_source_failed(source: &RustMdnsResponder) -> bool;
        fn mdns_source_any_failed(source: &RustMdnsResponder) -> bool;
        fn mdns_source_retire(source: &RustMdnsResponder);
        fn mdns_callback_bind(callback: &RustMdnsCallback, source: &RustMdnsResponder);
        fn mdns_callback_fire(source: &RustMdnsCallback, ip: Vec<u8>, name: Vec<u8>, removed: bool);
        type RustNetworkCallback;
        fn network_callback_failed(source: &RustNetworkCallback) -> bool;
        fn network_callback_fire(source: &RustNetworkCallback, network: SharedPtr<NativeNetwork>);
        type RustManagerCallback;
        fn manager_callback_fire(source: &RustManagerCallback);
        type RustAcceptCreator;
        fn listen_create_accepted(
            source: &RustListenSocket,
            creator: Box<RustAcceptCreator>,
            ticket: SharedPtr<NativePacketSocketAccept>,
        ) -> bool;
        type RustListenCallback;
        fn listen_callback_fire(
            callback: &RustListenCallback,
            source: &RustPacketSocket,
            owned: UniquePtr<NativeOwnedPacketSocket>,
        ) -> bool;
        type RustSocketRole;
        type RustSocketCallback;
        fn socket_callback_fire(source: &RustSocketCallback, event: FfiSocketObservation) -> bool;
        type RustPacketSocketFactory;
        fn socket_factory_present(source: &RustPacketSocketFactory) -> bool;
        fn socket_factory_create_packet(
            source: &RustPacketSocketFactory,
            environment: UniquePtr<NativeEnvironment>,
            resources: SharedPtr<NativePacketSocketResources>,
            events: SharedPtr<NativePacketSocketEvents>,
            request: FfiSocketCreateRequest,
        ) -> Box<RustPacketSocket>;
        fn socket_factory_create_listener(
            source: &RustPacketSocketFactory,
            environment: UniquePtr<NativeEnvironment>,
            resources: SharedPtr<NativePacketSocketResources>,
            events: SharedPtr<NativeListenSocketEvents>,
            request: FfiSocketCreateRequest,
        ) -> Box<RustListenSocket>;
        fn socket_factory_create_dns(
            source: &RustPacketSocketFactory,
            environment: UniquePtr<NativeEnvironment>,
            admitted: bool,
        ) -> Box<RustAsyncDnsResolver>;
        type RustListenSocket;
        fn listen_socket_present(source: &RustListenSocket) -> bool;
        fn listen_socket_failed(source: &RustListenSocket) -> bool;
        fn listen_socket_state(source: &RustListenSocket) -> i32;
        fn listen_socket_local(source: &RustListenSocket) -> UniquePtr<NativeSocketAddress>;
        type RustPacketSocket;
        fn packet_socket_present(source: &RustPacketSocket) -> bool;
        fn packet_socket_source_failed(source: &RustPacketSocket) -> bool;
        fn packet_socket_local(
            source: &RustPacketSocket,
            remote: bool,
        ) -> UniquePtr<NativeSocketAddress>;
        fn packet_socket_send(
            source: &RustPacketSocket,
            payload: Vec<u8>,
            options: FfiSocketPacketOptions,
        ) -> i32;
        fn packet_socket_send_to(
            source: &RustPacketSocket,
            payload: Vec<u8>,
            address: UniquePtr<NativeSocketAddress>,
            options: FfiSocketPacketOptions,
        ) -> i32;
        fn packet_socket_close(source: &RustPacketSocket) -> i32;
        fn packet_socket_state(source: &RustPacketSocket) -> i32;
        fn packet_socket_get_option(
            source: &RustPacketSocket,
            option: i32,
        ) -> FfiSocketOptionResult;
        fn packet_socket_set_option(source: &RustPacketSocket, option: i32, value: i32) -> i32;
        fn packet_socket_error(source: &RustPacketSocket) -> i32;
        fn packet_socket_set_error(source: &RustPacketSocket, error: i32);
        type RustNetworkManagerFactory;
        type RustNetworkManager;
        fn network_manager_factory_present(source: &RustNetworkManagerFactory) -> bool;
        fn network_manager_create(
            source: &RustNetworkManagerFactory,
            trials: UniquePtr<NativeFieldTrials>,
            changes: SharedPtr<NativeNetworkChange>,
            errors: SharedPtr<NativeNetworkChange>,
            resources: SharedPtr<NativeNetworkResources>,
        ) -> Box<RustNetworkManager>;
        fn network_manager_present(source: &RustNetworkManager) -> bool;
        fn network_manager_failed(source: &RustNetworkManager) -> bool;
        fn network_manager_initialize(source: &RustNetworkManager) -> bool;
        fn network_manager_start(source: &RustNetworkManager);
        fn network_manager_stop(source: &RustNetworkManager);
        fn network_manager_networks(
            source: &RustNetworkManager,
            any: bool,
        ) -> Vec<FfiNetworkHandle>;
        fn network_manager_permission(source: &RustNetworkManager) -> bool;
        fn network_manager_default_address(
            source: &RustNetworkManager,
            family: i32,
        ) -> FfiDefaultLocalAddress;
        fn network_manager_mdns(source: &RustNetworkManager) -> SharedPtr<NativeMdnsResponder>;
        fn network_manager_dump(source: &RustNetworkManager);
        fn network_manager_set_vpn(source: &RustNetworkManager, masks: Vec<FfiIpNetworkMask>);
        type RustNetworkMonitorFactory;
        type RustNetworkMonitor;
        fn network_monitor_factory_present(factory: &RustNetworkMonitorFactory) -> bool;
        fn network_monitor_create(
            factory: &RustNetworkMonitorFactory,
            trials: UniquePtr<NativeFieldTrials>,
            changes: SharedPtr<NativeNetworkChange>,
        ) -> Box<RustNetworkMonitor>;
        fn network_monitor_present(monitor: &RustNetworkMonitor) -> bool;
        fn network_monitor_start(monitor: &RustNetworkMonitor) -> bool;
        fn network_monitor_stop(monitor: &RustNetworkMonitor);
        fn network_monitor_info(
            monitor: &RustNetworkMonitor,
            name: &[u8],
        ) -> FfiNetworkInterfaceInfo;
        fn network_monitor_supports_binding(monitor: &RustNetworkMonitor) -> bool;
        fn network_monitor_bind(
            monitor: &RustNetworkMonitor,
            socket: i32,
            address: &[u8],
            name: &[u8],
        ) -> i32;
        type RustEventLogRetention;
        type RustEnvironmentFactoryRetention;
        type RustEnvironmentFactoryRandomness;
        fn environment_factory_has_randomness(retention: &RustEnvironmentFactoryRetention) -> bool;
        fn environment_factory_randomness(
            retention: &RustEnvironmentFactoryRetention,
        ) -> Box<RustEnvironmentFactoryRandomness>;
        type RustEventLogProductStop;
        fn event_log_product_stopped(completion: &RustEventLogProductStop, failed: bool);
        type RustRtcEventLogFactory;
        type RustRtcEventLog;
        fn rust_event_log_create(
            factory: &RustRtcEventLogFactory,
            environment: UniquePtr<NativeEnvironment>,
        ) -> Box<RustRtcEventLog>;
        fn rust_event_log_present(logger: &RustRtcEventLog) -> bool;
        fn rust_event_log_failed(logger: &RustRtcEventLog) -> bool;
        fn rust_event_log_start(
            logger: &RustRtcEventLog,
            output: UniquePtr<NativeEventLogOutput>,
            period_ms: i64,
        ) -> bool;
        fn rust_event_log_stop(logger: &RustRtcEventLog);
        fn rust_event_log_stop_async(
            logger: &RustRtcEventLog,
            completion: UniquePtr<NativeEventLogStop>,
        );
        fn rust_event_log_log(logger: &RustRtcEventLog, event: UniquePtr<NativeRtcEvent>);
        type RustEventLogOutput;
        fn event_log_output_active(output: &RustEventLogOutput) -> bool;
        fn event_log_output_write(output: &RustEventLogOutput, bytes: &[u8]) -> bool;
        fn event_log_output_flush(output: &RustEventLogOutput);
        type RustReadiness;
        fn notify_readiness(readiness: &RustReadiness);
        type RustTask;
        type RustRuntimeEnvironmentRetention;
        type RustTaskQueueFactory;
        type RustTaskQueueProvider;
        fn task_queue_provider_create(
            factory: &RustTaskQueueFactory,
            name: &[u8],
            priority: u8,
        ) -> Box<RustTaskQueueProvider>;
        fn task_queue_provider_present(provider: &RustTaskQueueProvider) -> bool;
        fn task_queue_provider_failed(provider: &RustTaskQueueProvider) -> bool;
        fn task_queue_provider_mark_failed(provider: &RustTaskQueueProvider);
        fn task_queue_provider_name(provider: &RustTaskQueueProvider) -> Vec<u8>;
        fn task_queue_provider_post(
            provider: &RustTaskQueueProvider,
            task: UniquePtr<NativeTaskQueueTicket>,
            delayed: bool,
            delay: i64,
            high_precision: bool,
        );
        fn task_queue_provider_delete(provider: &RustTaskQueueProvider);
        type RustNetworkControllerFactory;
        type RustNetworkController;
        fn network_controller_factory_present(source: &RustNetworkControllerFactory) -> bool;
        fn network_controller_is_goog_cc(source: &RustNetworkControllerFactory) -> bool;
        fn network_controller_goog_cc_rfc8888(source: &RustNetworkControllerFactory) -> bool;
        fn network_controller_goog_cc_estimator(
            source: &RustNetworkControllerFactory,
        ) -> Box<RustNetworkStateEstimatorFactory>;
        fn network_controller_goog_cc_predictor(
            source: &RustNetworkControllerFactory,
        ) -> Box<RustNetworkStatePredictorFactory>;
        fn network_controller_factory_retain(
            source: &RustNetworkControllerFactory,
        ) -> Box<RustNetworkControllerFactory>;
        type RustNetworkStateEstimatorFactory;
        type RustNetworkStateEstimator;
        fn network_estimator_factory_present(source: &RustNetworkStateEstimatorFactory) -> bool;
        fn network_estimator_create(
            source: &RustNetworkStateEstimatorFactory,
            trials: UniquePtr<NativeFieldTrials>,
        ) -> Box<RustNetworkStateEstimator>;
        fn network_estimator_present(source: &RustNetworkStateEstimator) -> bool;
        fn network_estimator_estimate(source: &RustNetworkStateEstimator) -> FfiNcOptionalEstimate;
        fn network_estimator_feedback(
            source: &RustNetworkStateEstimator,
            value: FfiNcTransportPacketsFeedback,
        );
        fn network_estimator_received(source: &RustNetworkStateEstimator, value: FfiNcPacketResult);
        fn network_estimator_route(
            source: &RustNetworkStateEstimator,
            value: FfiNcNetworkRouteChange,
        );
        fn network_estimator_invalid(source: &RustNetworkStateEstimator, kind: u8);
        fn network_controller_create(
            source: &RustNetworkControllerFactory,
            environment: UniquePtr<NativeEnvironment>,
            config: FfiNcConfig,
        ) -> Box<RustNetworkController>;
        fn network_controller_interval(source: &RustNetworkControllerFactory) -> i64;
        fn network_controller_update(
            source: &RustNetworkController,
            kind: u8,
            message: FfiNcMessage,
        ) -> FfiNcNetworkControlUpdate;
        fn network_controller_ecn(source: &RustNetworkController) -> bool;
        fn network_controller_invalid(source: &RustNetworkController, kind: u8);
        type RustFecControllerFactory;
        type RustFecController;
        type RustFecCompletion;
        fn fec_factory_present(source: &RustFecControllerFactory) -> bool;
        fn fec_controller_create(
            source: &RustFecControllerFactory,
            environment: UniquePtr<NativeEnvironment>,
        ) -> Box<RustFecController>;
        fn fec_controller_builtin(source: &RustFecController) -> bool;
        fn fec_controller_set_callback(
            source: &RustFecController,
            callback: SharedPtr<NativeFecProtectionCallback>,
        );
        fn fec_controller_set_method(source: &RustFecController, fec: bool, nack: bool);
        fn fec_controller_set_encoding(
            source: &RustFecController,
            width: usize,
            height: usize,
            temporal_layers: usize,
            payload: usize,
        );
        fn fec_controller_update_rates(
            source: &RustFecController,
            bitrate: u32,
            framerate: i32,
            lost: u8,
            mask: &[u8],
            rtt_ms: i64,
        ) -> u32;
        fn fec_controller_update_encoded(
            source: &RustFecController,
            length: usize,
            frame_type: i32,
        );
        fn fec_controller_use_mask(source: &RustFecController) -> bool;
        fn fec_async_complete(source: &RustFecCompletion, result: FfiFecProtectionResult);
        type RustNetworkStatePredictorFactory;
        type RustNetworkStatePredictor;
        fn network_predictor_factory_present(source: &RustNetworkStatePredictorFactory) -> bool;
        fn network_predictor_create(
            source: &RustNetworkStatePredictorFactory,
        ) -> Box<RustNetworkStatePredictor>;
        fn network_predictor_present(source: &RustNetworkStatePredictor) -> bool;
        fn network_predictor_update(
            source: &RustNetworkStatePredictor,
            send_time_ms: i64,
            arrival_time_ms: i64,
            state: i32,
        ) -> i32;
        type RustVideoBitrateAllocatorFactory;
        type RustVideoBitrateAllocator;
        fn video_bitrate_factory_present(source: &RustVideoBitrateAllocatorFactory) -> bool;
        fn video_bitrate_factory_builtin(source: &RustVideoBitrateAllocatorFactory) -> bool;
        fn video_bitrate_factory_create(
            source: &RustVideoBitrateAllocatorFactory,
            environment: UniquePtr<NativeEnvironment>,
            codec: FfiVideoBitrateCodec,
        ) -> Box<RustVideoBitrateAllocator>;
        fn video_bitrate_allocator_builtin(source: &RustVideoBitrateAllocator) -> bool;
        fn video_bitrate_allocator_allocate(
            source: &RustVideoBitrateAllocator,
            bps: i64,
            rate_kind: i8,
            framerate: f64,
        ) -> FfiVideoBitrateAllocation;
        fn video_bitrate_allocator_get_allocation(
            source: &RustVideoBitrateAllocator,
            bps: u32,
            framerate: u32,
        ) -> FfiVideoBitrateAllocation;
        fn video_bitrate_allocator_set_legacy(source: &RustVideoBitrateAllocator, enabled: bool);
        type RustDnsCallback;
        fn owned_dns_callback_fire(callback: Box<RustDnsCallback>);
        fn async_dns_source_failed(source: &RustAsyncDnsResolver) -> bool;
        fn async_dns_result_failed(source: &RustAsyncDnsResult) -> bool;
        type RustAsyncDnsFactory;
        type RustAsyncDnsResolver;
        fn async_dns_factory_present(source: &RustAsyncDnsFactory) -> bool;
        fn async_dns_factory_create(source: &RustAsyncDnsFactory) -> Box<RustAsyncDnsResolver>;
        fn async_dns_factory_create_and_resolve(
            source: &RustAsyncDnsFactory,
            address: UniquePtr<NativeSocketAddress>,
            has_family: bool,
            family: i32,
            completion: UniquePtr<NativeDnsResolutionCompletion>,
        ) -> Box<RustAsyncDnsResolver>;
        fn async_dns_start(
            source: &RustAsyncDnsResolver,
            address: UniquePtr<NativeSocketAddress>,
            has_family: bool,
            family: i32,
            completion: UniquePtr<NativeDnsResolutionCompletion>,
        );
        type RustAsyncDnsResult;
        fn async_dns_result_view(source: &RustAsyncDnsResolver) -> Box<RustAsyncDnsResult>;
        fn async_dns_result_identity(source: &RustAsyncDnsResult) -> usize;
        fn async_dns_result_error(source: &RustAsyncDnsResult) -> i32;
        fn async_dns_result_address(
            source: &RustAsyncDnsResult,
            family: i32,
        ) -> UniquePtr<NativeSocketAddress>;
        type RustCertificateGenerator;
        fn certificate_generator_present(source: &RustCertificateGenerator) -> bool;
        fn certificate_generator_generate(
            source: &RustCertificateGenerator,
            params: FfiCertificateKeyParams,
            has_expires: bool,
            expires_ms: u64,
            completion: UniquePtr<NativeCertificateCompletion>,
        );
        type RustLocalNetworkPermissionFactory;
        type RustLocalNetworkPermission;
        fn local_permission_factory_present(factory: &RustLocalNetworkPermissionFactory) -> bool;
        fn local_permission_create(
            factory: &RustLocalNetworkPermissionFactory,
        ) -> Box<RustLocalNetworkPermission>;
        fn local_permission_should_request(
            permission: &RustLocalNetworkPermission,
            address: UniquePtr<NativeSocketAddress>,
        ) -> bool;
        fn local_permission_request(
            permission: &RustLocalNetworkPermission,
            address: UniquePtr<NativeSocketAddress>,
            completion: UniquePtr<NativeLocalNetworkPermissionCompletion>,
        );
        type RustTlsCertificateVerifier;
        fn tls_certificate_verifier_present(verifier: &RustTlsCertificateVerifier) -> bool;
        fn rust_tls_certificate_verifier_failed(verifier: &RustTlsCertificateVerifier) -> bool;
        fn rust_tls_certificate_verify(
            verifier: &RustTlsCertificateVerifier,
            certificate: UniquePtr<NativeTlsCertificate>,
        ) -> bool;
        fn rust_tls_certificate_verify_chain(
            verifier: &RustTlsCertificateVerifier,
            chain: UniquePtr<NativeTlsCertificateChain>,
        ) -> bool;
        type RustMetronome;
        fn metronome_present(source: &RustMetronome) -> bool;
        fn metronome_tick_period(source: &RustMetronome) -> i64;
        fn metronome_request_tick(source: &RustMetronome, tick: UniquePtr<NativeMetronomeTick>);
        type RustClockProvider;
        fn clock_provider_current_time(provider: &RustClockProvider) -> i64;
        fn clock_provider_convert_ntp(provider: &RustClockProvider, timestamp: i64) -> u64;
        type RustFieldTrials;
        fn field_trials_present(view: &RustFieldTrials) -> bool;
        fn rust_field_trials_lookup(view: &RustFieldTrials, key: &str) -> String;
        fn rust_field_trials_copy(view: &RustFieldTrials) -> Box<RustFieldTrials>;
        type RustVideoEncoderFactory;
        type RustVideoDecoderFactory;
        type RustVideoFactoryEnvironment;
        fn video_factory_environment_clone(
            source: &RustVideoFactoryEnvironment,
        ) -> Box<RustVideoFactoryEnvironment>;
        fn video_factory_environment_native(
            source: &RustVideoFactoryEnvironment,
        ) -> UniquePtr<NativeEnvironment>;
        fn video_factory_environment_from_sdk(
            source: UniquePtr<NativeEnvironment>,
        ) -> Box<RustVideoFactoryEnvironment>;
        fn encoder_factory_clone(source: &RustVideoEncoderFactory) -> Box<RustVideoEncoderFactory>;
        fn decoder_factory_clone(source: &RustVideoDecoderFactory) -> Box<RustVideoDecoderFactory>;
        type RustVideoEncoderSelectorObserver;
        fn video_selector_provider_observer(
            provider: &RustVideoEncoderSelector,
        ) -> Box<RustVideoEncoderSelectorObserver>;
        type RustVideoEncoderSelector;
        fn video_selector_provider_current(
            provider: &RustVideoEncoderSelector,
            format: UniquePtr<NativeSdpVideoFormat>,
        );
        fn video_selector_provider_bitrate(
            provider: &RustVideoEncoderSelector,
            bits: i64,
            kind: i8,
        ) -> UniquePtr<NativeSdpVideoFormat>;
        fn video_selector_provider_resolution(
            provider: &RustVideoEncoderSelector,
            resolution: UniquePtr<NativeRenderResolution>,
        ) -> UniquePtr<NativeSdpVideoFormat>;
        fn video_selector_provider_broken(
            provider: &RustVideoEncoderSelector,
        ) -> UniquePtr<NativeSdpVideoFormat>;
        type RustVideoEncoder;
        type RustVideoDecoder;

        fn run_task(task: Box<RustTask>);

        fn encoder_factory_selector(
            factory: &RustVideoEncoderFactory,
            valid: &mut bool,
        ) -> UniquePtr<NativeOwnedVideoEncoderSelector>;
        fn encoder_factory_formats(
            factory: &RustVideoEncoderFactory,
        ) -> UniquePtr<NativeSdpVideoFormatList>;
        fn encoder_factory_implementations(
            factory: &RustVideoEncoderFactory,
        ) -> UniquePtr<NativeSdpVideoFormatList>;
        fn encoder_factory_query(
            factory: &RustVideoEncoderFactory,
            format: UniquePtr<NativeSdpVideoFormat>,
            has_scalability_mode: bool,
            scalability_mode: &[u8],
            resolution: UniquePtr<NativeRtpResolution>,
            legacy: bool,
        ) -> FfiVideoFactorySupport;
        fn encoder_factory_create(
            factory: &RustVideoEncoderFactory,
            environment: UniquePtr<NativeEnvironment>,
            retention: &RustVideoFactoryEnvironment,
            format: UniquePtr<NativeSdpVideoFormat>,
        ) -> Box<RustVideoEncoder>;
        fn encoder_is_valid(encoder: &RustVideoEncoder) -> bool;
        fn encoder_init(encoder: &mut RustVideoEncoder, settings: FfiEncoderSettings) -> i32;
        fn encoder_set_fec_override(
            encoder: &mut RustVideoEncoder,
            value: SharedPtr<NativeFecOverride>,
        ) -> i32;
        fn encoder_init_value(
            encoder: &mut RustVideoEncoder,
            codec: UniquePtr<NativeVideoCodec>,
            settings: FfiOwnedEncoderSettings,
        ) -> i32;
        fn encoder_encode_value(
            encoder: &mut RustVideoEncoder,
            frame: UniquePtr<NativeRawVideoFrame>,
            has_types: bool,
            types: Vec<i32>,
            callback: SharedPtr<NativeEncodedImageCallback>,
        ) -> i32;
        fn encoder_set_rates_value(
            encoder: &mut RustVideoEncoder,
            rates: UniquePtr<NativeVideoRateControl>,
        ) -> i32;
        fn encoder_register_callback_value(
            encoder: &mut RustVideoEncoder,
            callback: SharedPtr<NativeEncodedImageCallback>,
        ) -> i32;
        fn encoder_register_callback(encoder: &mut RustVideoEncoder) -> i32;
        fn encoder_encode(
            encoder: &mut RustVideoEncoder,
            frame: UniquePtr<NativeVideoFrame>,
            frame_types: &[u8],
            presentation_token: i64,
            callback: SharedPtr<NativeEncodedImageCallback>,
        ) -> i32;
        fn encoder_set_rates(encoder: &mut RustVideoEncoder, rates: FfiRateControl) -> i32;
        fn encoder_packet_loss(encoder: &mut RustVideoEncoder, rate: f32) -> i32;
        fn encoder_rtt(encoder: &mut RustVideoEncoder, rtt_ms: i64) -> i32;
        fn encoder_loss_notification(
            encoder: &mut RustVideoEncoder,
            notification: FfiVideoEncoderLossNotification,
        ) -> i32;
        fn encoder_factory_report_feedback_failure(factory: &RustVideoEncoderFactory);
        fn encoder_release(encoder: &mut RustVideoEncoder) -> i32;
        fn encoder_get_info_value(encoder: &RustVideoEncoder) -> UniquePtr<NativeVideoEncoderInfo>;
        fn encoder_factory_report_codec_info_failure(factory: &RustVideoEncoderFactory);

        fn decoder_factory_formats(
            factory: &RustVideoDecoderFactory,
        ) -> UniquePtr<NativeSdpVideoFormatList>;
        fn decoder_factory_query(
            factory: &RustVideoDecoderFactory,
            format: UniquePtr<NativeSdpVideoFormat>,
            reference_scaling: bool,
            resolution: UniquePtr<NativeRtpResolution>,
            legacy: bool,
        ) -> FfiVideoFactorySupport;
        fn decoder_factory_create(
            factory: &RustVideoDecoderFactory,
            environment: UniquePtr<NativeEnvironment>,
            retention: &RustVideoFactoryEnvironment,
            format: UniquePtr<NativeSdpVideoFormat>,
        ) -> Box<RustVideoDecoder>;
        fn decoder_is_valid(decoder: &RustVideoDecoder) -> bool;
        fn decoder_configure_value(
            decoder: &mut RustVideoDecoder,
            settings: UniquePtr<NativeVideoDecoderSettings>,
        ) -> bool;
        type RustFecOverride;
        fn fec_override_provider_allowed(provider: &RustFecOverride, allowed: bool);
        type RustEncodedImageSink;
        fn encoded_sink_provider_frame(
            provider: &RustEncodedImageSink,
            image: UniquePtr<NativeEncodedImage>,
            info: UniquePtr<NativeCodecSpecificInfo>,
        ) -> FfiEncodedCallbackResult;
        fn encoded_sink_provider_dropped(
            provider: &RustEncodedImageSink,
            timestamp: u32,
            spatial_id: i32,
            end: bool,
        );
        type RustDecodedImageSink;
        fn decoded_sink_provider_invoke(
            provider: &RustDecodedImageSink,
            frame: UniquePtr<NativeRawVideoFrame>,
            overload: u8,
            time_ms: i64,
            has_time: bool,
            optional_time: i32,
            has_qp: bool,
            qp: u8,
            status: &mut i32,
        ) -> UniquePtr<NativeRawVideoFrame>;
        fn decoder_register_callback_value(
            decoder: &mut RustVideoDecoder,
            callback: SharedPtr<NativeDecodedImageCallback>,
        ) -> i32;
        fn decoder_decode_value(
            decoder: &mut RustVideoDecoder,
            frame: UniquePtr<NativeEncodedImage>,
            missing_overload: bool,
            missing: bool,
            render_time_ms: i64,
            callback: SharedPtr<NativeDecodedImageCallback>,
        ) -> i32;
        fn decoder_register_callback(decoder: &mut RustVideoDecoder) -> i32;
        fn decoder_decode(
            decoder: &mut RustVideoDecoder,
            frame: UniquePtr<NativeEncodedVideoFrame>,
            callback: SharedPtr<NativeDecodedImageCallback>,
        ) -> i32;
        fn decoder_release(decoder: &mut RustVideoDecoder) -> i32;
        fn decoder_implementation_name(decoder: &RustVideoDecoder, valid: &mut bool) -> Vec<u8>;
        fn decoder_get_info_value(decoder: &RustVideoDecoder) -> UniquePtr<NativeVideoDecoderInfo>;
    }

    #[derive(Default)]
    struct FfiEncodedVideoFrame {
        available: bool,
        data: Vec<u8>,
        mime_type: String,
        rtp_timestamp: u32,
        ssrc: u32,
        payload_type: u8,
        key_frame: bool,
        rid: String,
        has_rid: bool,
        has_capture_time: bool,
        capture_time_us: i64,
        has_receive_time: bool,
        receive_time_us: i64,
        has_frame_id: bool,
        frame_id: i64,
        spatial_index: i32,
        temporal_index: i32,
        dependencies: Vec<i64>,
        decode_target_indications: Vec<u8>,
    }

    struct FfiAudioObserverEvent {
        present: bool,
        volume: bool,
        value: f64,
    }

    struct FfiAudioProcessorStatistics {
        typing_noise_detected: bool,
        has_voice_detected: bool,
        voice_detected: bool,
        has_echo_return_loss: bool,
        echo_return_loss: f64,
        has_echo_return_loss_enhancement: bool,
        echo_return_loss_enhancement: f64,
        has_divergent_filter_fraction: bool,
        divergent_filter_fraction: f64,
        has_delay_median_ms: bool,
        delay_median_ms: i32,
        has_delay_standard_deviation_ms: bool,
        delay_standard_deviation_ms: i32,
        has_residual_echo_likelihood: bool,
        residual_echo_likelihood: f64,
        has_residual_echo_likelihood_recent_max: bool,
        residual_echo_likelihood_recent_max: f64,
        has_delay_ms: bool,
        delay_ms: i32,
    }

    struct FfiAudioTrackStatus {
        present: bool,
        id: String,
        kind: String,
        enabled: bool,
        state: u8,
        volume: f64,
        signal_level: FfiOptionalI32,
    }

    struct FfiFrameTransformBinding {
        audio: bool,
        sender: bool,
        has_ssrc: bool,
        ssrc: u32,
    }
    struct FfiFrameTransformerStatistics {
        in_flight: usize,
        delivered: u64,
        completed: u64,
        abandoned: u64,
        late_completions: u64,
        capacity_drops: u64,
        wrong_frame: u64,
        callback_panics: u64,
        callback_forwards: u64,
        late_callback_forwards: u64,
    }
    struct FfiRawUpdateRect {
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    }
    struct FfiDecodedCallbackResult {
        invoked: bool,
        status: i32,
    }
    struct FfiEncodedLayerSize {
        index: i32,
        bytes: usize,
    }
    struct FfiEncodedBufferRead {
        initialized: bool,
        data: Vec<u8>,
    }
    struct FfiEncodedImageMetadata {
        width: u32,
        height: u32,
        rtp_timestamp: u32,
        ntp_time_ms: i64,
        capture_time_ms: i64,
        frame_type: i32,
        rotation: i32,
        content_type: u8,
        qp: i32,
        simulcast_index: FfiOptionalI32,
        spatial_index: FfiOptionalI32,
        temporal_index: FfiOptionalI32,
        spatial_layer_sizes: Vec<FfiEncodedLayerSize>,
        has_presentation_time: bool,
        presentation_time_us: i64,
        has_tracking_id: bool,
        tracking_id: u16,
        has_color_space: bool,
        color_space: FfiMediaColorSpace,
        has_playout_delay: bool,
        playout_delay: FfiMediaPlayoutDelay,
        packet_infos: Vec<FfiRtpReceivedPacketInfo>,
        retransmission_allowed: bool,
        steady_state_refresh: bool,
        timing_flags: u8,
        timing_ms: [i64; 8],
        has_corruption_filter: bool,
        filter_std_dev: f64,
        filter_luma_threshold: i32,
        filter_chroma_threshold: i32,
        has_psnr: bool,
        psnr: [f64; 3],
        has_end_of_temporal_unit: bool,
        end_of_temporal_unit: bool,
    }
    struct FfiRawFrameMetadata {
        id: u16,
        timestamp_us: i64,
        rtp_timestamp: u32,
        ntp_time_ms: i64,
        has_presentation_timestamp: bool,
        presentation_timestamp_us: i64,
        has_reference_time: bool,
        reference_time_us: i64,
        rotation: u16,
        has_color_space: bool,
        color_space: FfiMediaColorSpace,
        low_latency: bool,
        has_max_composition_delay: bool,
        max_composition_delay: i32,
        has_update_rect: bool,
        update_rect: FfiRawUpdateRect,
        packet_infos: Vec<FfiRtpReceivedPacketInfo>,
        has_processing_time: bool,
        processing_start_us: i64,
        processing_finish_us: i64,
        repeat_frame: bool,
        has_content_type: bool,
        content_type: u8,
    }
    struct FfiRawBufferInfo {
        buffer_type: u8,
        width: i32,
        height: i32,
        chroma_width: i32,
        chroma_height: i32,
        stride_y: i32,
        stride_u: i32,
        stride_v: i32,
        stride_a: i32,
        stride_uv: i32,
    }
    struct FfiRawVideoPlane {
        valid: bool,
        width: i32,
        height: i32,
        stride: i32,
        bits: u8,
        data8: Vec<u8>,
        data16: Vec<u16>,
    }

    struct FfiRecordableVideoFrame {
        valid: bool,
        has_buffer: bool,
        data: Vec<u8>,
        codec: i32,
        key_frame: bool,
        width: u32,
        height: u32,
        has_rotation: bool,
        rotation: u16,
        has_color_space: bool,
        color_space: FfiMediaColorSpace,
        render_time_us: i64,
    }
    struct FfiVideoSinkFrameSize {
        width: i32,
        height: i32,
    }
    struct FfiVideoSinkWants {
        valid: bool,
        rotation_applied: bool,
        black_frames: bool,
        max_pixel_count: i32,
        has_target_pixel_count: bool,
        target_pixel_count: i32,
        max_framerate_fps: i32,
        resolution_alignment: i32,
        resolutions: Vec<FfiVideoSinkFrameSize>,
        has_requested_resolution: bool,
        requested_width: i32,
        requested_height: i32,
        is_active: bool,
        has_aggregates: bool,
        any_active_without_requested_resolution: bool,
    }

    struct FfiVideoSinkEvent {
        valid: bool,
        kind: u8,
        has_min_fps: bool,
        min_fps: f64,
        has_max_fps: bool,
        max_fps: f64,
    }

    struct FfiVideoSourceStats {
        available: bool,
        input_width: i32,
        input_height: i32,
    }

    struct FfiAudioSourceOptions {
        echo_cancellation: i8,
        echo_cancellation_mode: FfiOptionalI32,
        auto_gain_control: i8,
        auto_gain_control_mode: FfiOptionalI32,
        noise_suppression: i8,
        noise_suppression_mode: FfiOptionalI32,
        highpass_filter: i8,
        highpass_filter_mode: FfiOptionalI32,
        stereo_swapping: i8,
        audio_jitter_buffer_max_packets: FfiOptionalI32,
        audio_jitter_buffer_fast_accelerate: i8,
        audio_jitter_buffer_min_delay_ms: FfiOptionalI32,
        audio_network_adaptor: i8,
        has_adaptor_config: bool,
        adaptor_config: Vec<u8>,
        init_recording_on_send: i8,
    }

    struct FfiReceivedAudioFrame {
        valid: bool,
        sample_rate_hz: u32,
        channels: u8,
        samples_per_channel: u32,
        has_capture_time: bool,
        capture_time_ms: i64,
        samples: Vec<i16>,
    }

    struct FfiEncodedAudioFrame {
        available: bool,
        data: Vec<u8>,
        rtp_timestamp: u32,
        ssrc: u32,
        payload_type: u8,
        samples_per_channel: u32,
        has_sequence_number: bool,
        sequence_number: u16,
        has_audio_level: bool,
        audio_level_dbov: u8,
        has_voice_activity: bool,
        voice_activity: bool,
        has_capture_time: bool,
        capture_time_us: i64,
        has_receive_time: bool,
        receive_time_us: i64,
    }

    struct FfiClockValue {
        valid: bool,
        value: i64,
    }
    struct FfiClockNtpValue {
        error: u8,
        value: u64,
    }
    struct FfiClockNtpSample {
        timestamp_us: i64,
        ntp: u64,
    }
    struct FfiClockTimeBounds {
        known: bool,
        minimum_us: i64,
        maximum_us: i64,
        monotonic: bool,
    }

    unsafe extern "C++" {
        include!("pulsebeam-webrtc-sys/native/probe.h");
        include!("pulsebeam-webrtc-sys/native/execution.h");
        include!("pulsebeam-webrtc-sys/native/task_queue_provider.h");
        type NativeTaskQueueTicket;
        fn task_queue_ticket_submit(ticket: UniquePtr<NativeTaskQueueTicket>) -> bool;
        fn task_queue_current_name() -> Vec<u8>;
        fn task_queue_current_identity() -> usize;
        fn task_queue_current_post(identity: usize, task: Box<RustTask>) -> u8;
        fn task_queue_current_post_delayed(
            identity: usize,
            delay: i64,
            high_precision: bool,
            task: Box<RustTask>,
        ) -> u8;
        fn new_provider_task_queue_factory(
            provider: Box<RustTaskQueueFactory>,
        ) -> UniquePtr<NativeTaskQueueFactory>;
        include!("pulsebeam-webrtc-sys/native/metronome.h");
        type NativeMetronomeTick;
        fn metronome_tick_signal(tick: UniquePtr<NativeMetronomeTick>) -> bool;
        include!("pulsebeam-webrtc-sys/native/clock.h");
        type NativeClock;
        fn new_provider_clock(
            provider: Box<RustClockProvider>,
            monotonic: bool,
        ) -> UniquePtr<NativeClock>;
        fn new_simulated_clock(
            initial_time_us: i64,
            minimum_time_us: i64,
            maximum_time_us: i64,
        ) -> UniquePtr<NativeClock>;
        fn new_real_time_clock() -> UniquePtr<NativeClock>;
        unsafe fn environment_clock(
            environment: &NativeEnvironment,
            selected: *const NativeClock,
            manual: bool,
        ) -> UniquePtr<NativeClock>;
        fn clock_is_monotonic(clock: &NativeClock) -> bool;
        fn real_time_clock_native_sample(clock: &NativeClock) -> FfiClockNtpSample;
        fn advance_simulated_clock(clock: &NativeClock, delta_us: i64) -> bool;
        fn provider_clock_timestamp(clock: &NativeClock) -> i64;
        fn provider_clock_current_ntp(clock: &NativeClock) -> FfiClockNtpValue;
        fn provider_clock_convert_ntp(clock: &NativeClock, timestamp: i64) -> FfiClockNtpValue;
        fn clock_timestamp_operation(left: i64, right: i64, operation: u8) -> i64;
        fn clock_delta_operation(left: i64, right: i64, operation: u8) -> i64;
        fn clock_delta_size_scalar(value: i64, scalar: usize, divide: bool) -> i64;
        fn clock_delta_float_scalar(value: i64, scalar: f64, divide: bool) -> i64;
        fn clock_delta_div_float32(value: i64, scalar: f32) -> i64;
        fn clock_delta_negate(value: i64) -> i64;
        fn clock_delta_ratio(left: i64, right: i64) -> f64;
        fn clock_delta_string(value: i64) -> String;
        fn clock_timestamp_from_int(value: i64, unit: u8) -> FfiClockValue;
        fn clock_timestamp_from_float(value: f64, unit: u8) -> FfiClockValue;
        fn clock_timestamp_from_float32(value: f32, unit: u8) -> FfiClockValue;
        fn clock_timestamp_convert(value: i64, unit: u8) -> FfiClockValue;
        fn clock_timestamp_float(value: i64, unit: u8) -> f64;
        fn clock_timestamp_float32(value: i64, unit: u8) -> f32;
        fn clock_timestamp_string(value: i64) -> String;
        fn ntp_time_to_ms(value: u64) -> i64;
        fn ntp_time_to_utc_us(value: u64) -> i64;
        fn clock_ms_to_q32x32(value: i64) -> i64;
        fn clock_ms_to_uq32x32(value: i64) -> u64;
        fn clock_q32x32_to_ms(value: i64) -> i64;
        fn clock_uq32x32_to_ms(value: u64) -> i64;
        fn clock_q32x32_to_us(value: i64) -> i64;
        fn clock_uq32x32_to_us(value: u64) -> i64;
        include!("pulsebeam-webrtc-sys/native/event_log.h");
        include!("pulsebeam-webrtc-sys/native/event_log_codec.h");
        include!("pulsebeam-webrtc-sys/native/network.h");
        include!("pulsebeam-webrtc-sys/native/network_values.h");
        include!("pulsebeam-webrtc-sys/native/packet_socket.h");
        type NativeListenSocketEvents;
        fn listen_socket_events_in_scope(
            resources: SharedPtr<NativePacketSocketResources>,
            standalone: bool,
        ) -> SharedPtr<NativeListenSocketEvents>;
        type NativeAcceptedPacketSocket;
        type NativePacketSocketAccept;
        fn accepted_socket_new(
            source: Box<RustPacketSocket>,
            events: SharedPtr<NativePacketSocketEvents>,
            resources: SharedPtr<NativePacketSocketResources>,
        ) -> UniquePtr<NativeAcceptedPacketSocket>;
        fn listen_accept(
            events: &NativeListenSocketEvents,
            socket: UniquePtr<NativeAcceptedPacketSocket>,
            post: bool,
        ) -> FfiSocketAcceptResult;
        fn listen_post_creator(
            events: &NativeListenSocketEvents,
            creator: Box<RustAcceptCreator>,
        ) -> SharedPtr<NativePacketSocketAccept>;
        fn listen_accept_prepared(
            events: &NativeListenSocketEvents,
            socket: UniquePtr<NativeAcceptedPacketSocket>,
            ticket: SharedPtr<NativePacketSocketAccept>,
        ) -> FfiSocketAcceptResult;
        fn listen_accept_creation_failed(ticket: &NativePacketSocketAccept, retired: bool);
        fn listen_accept_status(ticket: &NativePacketSocketAccept) -> u8;
        fn listen_accept_cancel(ticket: &NativePacketSocketAccept) -> bool;
        type NativePacketSocketScope;
        fn packet_socket_scope_new() -> UniquePtr<NativePacketSocketScope>;
        fn packet_socket_scope_resources(
            scope: &NativePacketSocketScope,
        ) -> SharedPtr<NativePacketSocketResources>;
        type NativeOwnedListenSocket;
        type NativeListenSocketSubscription;
        fn owned_listen_socket_new(
            source: Box<RustListenSocket>,
            events: SharedPtr<NativeListenSocketEvents>,
            scope: UniquePtr<NativePacketSocketScope>,
            role: Box<RustSocketRole>,
        ) -> UniquePtr<NativeOwnedListenSocket>;
        fn listen_member_call(
            events: &NativeListenSocketEvents,
            address: bool,
        ) -> FfiOwnedSocketResult;
        fn listen_observe(
            events: &NativeListenSocketEvents,
            ownership: bool,
            callback: Box<RustListenCallback>,
        ) -> FfiListenSubscriptionResult;
        fn listen_subscription_cancel(subscription: &NativeListenSocketSubscription) -> bool;
        fn listen_subscription_active(subscription: &NativeListenSocketSubscription) -> bool;
        fn owned_socket_into_accepted(
            socket: UniquePtr<NativeOwnedPacketSocket>,
            listener: SharedPtr<NativePacketSocketResources>,
        ) -> UniquePtr<NativeAcceptedPacketSocket>;
        type NativeOwnedPacketSocket;
        fn owned_packet_socket_new(
            source: Box<RustPacketSocket>,
            events: SharedPtr<NativePacketSocketEvents>,
            scope: UniquePtr<NativePacketSocketScope>,
            role: Box<RustSocketRole>,
        ) -> UniquePtr<NativeOwnedPacketSocket>;
        fn socket_member_call(
            events: &NativePacketSocketEvents,
            operation: u8,
            payload: &[u8],
            address: UniquePtr<NativeSocketAddress>,
            options: FfiSocketPacketOptions,
            option: i32,
            value: i32,
        ) -> FfiOwnedSocketResult;
        type NativePacketSocketSubscription;
        fn socket_observe(
            events: &NativePacketSocketEvents,
            kind: u8,
            legacy: bool,
            callback: Box<RustSocketCallback>,
        ) -> FfiSocketSubscriptionResult;
        fn socket_subscription_cancel(subscription: &NativePacketSocketSubscription) -> bool;
        fn socket_subscription_active(subscription: &NativePacketSocketSubscription) -> bool;
        fn socket_subscription_bound(subscription: &NativePacketSocketSubscription) -> bool;
        type NativePacketSocketResources;
        type NativePacketSocketEvents;
        fn packet_socket_resources_status(resources: &NativePacketSocketResources) -> u8;
        fn packet_socket_events_in_scope(
            resources: SharedPtr<NativePacketSocketResources>,
        ) -> SharedPtr<NativePacketSocketEvents>;
        fn packet_socket_events_retire(events: &NativePacketSocketEvents);
        fn socket_event_simple(
            events: &NativePacketSocketEvents,
            kind: u8,
            error: i32,
            post: bool,
        ) -> u8;
        fn socket_event_received(
            events: &NativePacketSocketEvents,
            packet: FfiSocketReceivedPacket,
            post: bool,
        ) -> u8;
        fn socket_event_sent(
            events: &NativePacketSocketEvents,
            packet: FfiSocketSentPacket,
            post: bool,
        ) -> u8;
        fn socket_event_address(
            events: &NativePacketSocketEvents,
            address: UniquePtr<NativeSocketAddress>,
            post: bool,
        ) -> u8;
        fn socket_copy_information(
            events: &NativePacketSocketEvents,
            size: usize,
            info: &mut FfiSocketPacketInfo,
        ) -> u8;
        fn socket_provider_io_error() -> i32;
        fn socket_received_packet_copy(
            packet: &FfiSocketReceivedPacket,
            decryption: u8,
        ) -> FfiSocketReceivedPacket;
        fn socket_received_packet_legacy(
            payload: &[u8],
            address: &NativeSocketAddress,
            time_us: i64,
        ) -> FfiSocketReceivedPacket;
        type NativeNetworkChange;
        fn network_change_notify(changes: &NativeNetworkChange) -> bool;
        include!("pulsebeam-webrtc-sys/native/codec.h");
        include!("pulsebeam-webrtc-sys/native/audio_processor.h");
        include!("pulsebeam-webrtc-sys/native/audio_echo_config.h");
        include!("pulsebeam-webrtc-sys/native/audio_device.h");
        fn audio_echo_config_default(multichannel: bool) -> FfiEchoCanceller3Config;
        fn audio_echo_config_validate(config: &mut FfiEchoCanceller3Config) -> bool;
        type NativeAudioProcessingModule;
        type NativeApmCustomModule;
        type NativeAudioDevice;
        type NativeAudioTransport;
        type NativeAudioDeviceObserver;
        fn audio_transport_new(
            callbacks: Box<RustAudioDeviceCallbacks>,
        ) -> UniquePtr<NativeAudioTransport>;
        fn audio_device_observer_new(
            callbacks: Box<RustAudioDeviceCallbacks>,
        ) -> UniquePtr<NativeAudioDeviceObserver>;
        fn audio_device_register_transport(
            device: Pin<&mut NativeAudioDevice>,
            transport: UniquePtr<NativeAudioTransport>,
            error: &mut u8,
        ) -> i32;
        fn audio_device_register_observer(
            device: Pin<&mut NativeAudioDevice>,
            observer: UniquePtr<NativeAudioDeviceObserver>,
        ) -> i32;
        fn audio_device_from_factory(
            factory: &NativePeerConnectionFactory,
        ) -> UniquePtr<NativeAudioDevice>;
        fn factory_register_audio_device_observer(
            factory: &NativePeerConnectionFactory,
            observer: UniquePtr<NativeAudioDeviceObserver>,
        ) -> i32;
        fn audio_device_new(
            environment: &NativeEnvironment,
            worker: &NativeThread,
            layer: u8,
            bypass: bool,
            helper: bool,
            error: &mut u8,
        ) -> UniquePtr<NativeAudioDevice>;
        fn audio_device_call(
            device: Pin<&mut NativeAudioDevice>,
            operation: u8,
            argument: u32,
        ) -> FfiAudioDeviceResult;
        fn audio_device_name(
            device: Pin<&mut NativeAudioDevice>,
            recording: bool,
            index: u16,
        ) -> FfiAudioDeviceName;
        fn audio_device_statistics(device: Pin<&mut NativeAudioDevice>)
        -> FfiAudioDeviceStatistics;
        fn audio_device_platform_state(
            device: Pin<&mut NativeAudioDevice>,
        ) -> FfiAudioDevicePlatformState;
        type NativeInstalledAudioProcessor;
        fn audio_processor_into_installed(
            processor: UniquePtr<NativeAudioProcessingModule>,
            environment: Box<RustInstalledApmEnvironment>,
            error: &mut u8,
        ) -> UniquePtr<NativeInstalledAudioProcessor>;
        type NativeEchoControl;
        type NativeEchoCreationContext;
        type NativeEchoDependencies;
        fn audio_processor_echo_factory_provider_new(
            module: Box<RustApmCustomModule>,
        ) -> UniquePtr<NativeApmCustomModule>;
        fn audio_processor_echo_factory_environment(
            factory: Pin<&mut NativeApmCustomModule>,
            environment: Box<RustEchoEnvironment>,
        );
        fn audio_echo_control_retain_environment(
            control: Pin<&mut NativeEchoControl>,
            environment: Box<RustEchoEnvironment>,
        );
        fn audio_echo_context_rate(context: &NativeEchoCreationContext) -> i32;
        fn audio_echo_context_render_channels(context: &NativeEchoCreationContext) -> i32;
        fn audio_echo_context_capture_channels(context: &NativeEchoCreationContext) -> i32;
        fn audio_echo_context_create(
            factory: Pin<&mut NativeApmCustomModule>,
            context: &NativeEchoCreationContext,
            include_neural: bool,
            error: &mut u8,
        ) -> UniquePtr<NativeEchoControl>;
        fn audio_echo_context_controller(
            context: &NativeEchoCreationContext,
            include_neural: bool,
            module: Box<RustApmCustomModule>,
            error: &mut u8,
        ) -> UniquePtr<NativeEchoControl>;
        fn audio_echo_dependency_present(dependencies: &NativeEchoDependencies) -> bool;
        fn audio_echo_dependency_reset(dependencies: Pin<&mut NativeEchoDependencies>) -> u8;
        fn audio_echo_dependency_adjust(
            dependencies: Pin<&mut NativeEchoDependencies>,
            config: &FfiEchoCanceller3Config,
            error: &mut u8,
        ) -> FfiEchoCanceller3Config;
        fn audio_echo_dependency_estimate(
            dependencies: Pin<&mut NativeEchoDependencies>,
            input: FfiNeuralEchoInput,
        ) -> FfiNeuralEchoResult;
        type NativeEchoAudio;
        type NativeEchoLinearOutput;
        fn audio_echo_view_shape(audio: &NativeEchoAudio) -> FfiEchoAudioShape;
        fn audio_echo_view_channel<'a>(
            audio: &'a NativeEchoAudio,
            channel: usize,
            split: bool,
            band: usize,
            valid: &mut bool,
        ) -> &'a [f32];
        fn audio_echo_view_channel_mut<'a>(
            audio: Pin<&'a mut NativeEchoAudio>,
            channel: usize,
            split: bool,
            band: usize,
            valid: &mut bool,
        ) -> &'a mut [f32];
        fn audio_echo_linear_channels(output: &NativeEchoLinearOutput) -> usize;
        fn audio_echo_linear_frames(output: &NativeEchoLinearOutput) -> usize;
        fn audio_echo_linear_write(
            output: Pin<&mut NativeEchoLinearOutput>,
            channel: usize,
            samples: &[f32],
        ) -> bool;
        fn audio_echo_control_from_provider(
            rate: i32,
            render_channels: i32,
            capture_channels: i32,
            module: Box<RustApmCustomModule>,
            error: &mut u8,
        ) -> UniquePtr<NativeEchoControl>;
        fn audio_processor_echo_factory_new(
            constructor: u8,
            config: &FfiAec3Selection,
        ) -> UniquePtr<NativeApmCustomModule>;
        unsafe fn audio_echo_control_create(
            factory: Pin<&mut NativeApmCustomModule>,
            environment: &NativeEnvironment,
            rate: i32,
            render_channels: i32,
            capture_channels: i32,
            neural_overload: bool,
            neural: *const NativeApmCustomModule,
            error: &mut u8,
        ) -> UniquePtr<NativeEchoControl>;
        fn audio_echo_control_audio(
            control: Pin<&mut NativeEchoControl>,
            operation: u8,
            rate: i32,
            channels: usize,
            samples: &[f32],
            level_change: bool,
            linear: bool,
        ) -> FfiEchoControlAudio;
        fn audio_echo_control_metrics(
            control: Pin<&mut NativeEchoControl>,
        ) -> FfiEchoControlMetrics;
        fn audio_echo_control_delay(control: Pin<&mut NativeEchoControl>, delay: i32) -> u8;
        fn audio_echo_control_usage(control: Pin<&mut NativeEchoControl>, used: bool) -> u8;
        fn audio_echo_control_active(control: Pin<&mut NativeEchoControl>, error: &mut u8) -> bool;
        fn audio_processor_neural_new(
            module: Box<RustApmCustomModule>,
        ) -> UniquePtr<NativeApmCustomModule>;
        fn audio_processor_neural_reset(module: Pin<&mut NativeApmCustomModule>) -> u8;
        fn audio_processor_neural_adjust(
            module: Pin<&mut NativeApmCustomModule>,
            config: &FfiEchoCanceller3Config,
            error: &mut u8,
        ) -> FfiEchoCanceller3Config;
        fn audio_processor_neural_estimate(
            module: Pin<&mut NativeApmCustomModule>,
            input: FfiNeuralEchoInput,
        ) -> FfiNeuralEchoResult;
        fn audio_processor_echo_detector_new(
            module: Box<RustApmCustomModule>,
        ) -> UniquePtr<NativeApmCustomModule>;
        fn audio_processor_echo_detector_clone(
            module: &NativeApmCustomModule,
        ) -> UniquePtr<NativeApmCustomModule>;
        fn audio_processor_echo_detector_initialize(
            module: Pin<&mut NativeApmCustomModule>,
            capture_rate: i32,
            capture_channels: i32,
            render_rate: i32,
            render_channels: i32,
        ) -> u8;
        fn audio_processor_echo_detector_analyze(
            module: Pin<&mut NativeApmCustomModule>,
            render: bool,
            samples: &[f32],
        ) -> u8;
        fn audio_processor_echo_detector_metrics(
            module: Pin<&mut NativeApmCustomModule>,
        ) -> FfiApmEchoDetectorMetrics;
        fn audio_processor_custom_new(
            module: Box<RustApmCustomModule>,
            analyzer: bool,
        ) -> UniquePtr<NativeApmCustomModule>;
        fn audio_processor_custom_description(
            module: Pin<&mut NativeApmCustomModule>,
            error: &mut u8,
        ) -> Vec<u8>;
        fn audio_processor_custom_audio(
            module: Pin<&mut NativeApmCustomModule>,
            rate: i32,
            channels: usize,
            samples: &[f32],
            analyze: bool,
            error: &mut u8,
        ) -> Vec<f32>;
        fn audio_processor_custom_runtime(
            module: Pin<&mut NativeApmCustomModule>,
            setting: &FfiApmRuntimeSetting,
        ) -> u8;
        fn audio_processor_custom_initialize(
            module: Pin<&mut NativeApmCustomModule>,
            rate: i32,
            channels: i32,
        ) -> u8;
        fn audio_processor_new_custom(
            environment: &NativeEnvironment,
            config: &FfiApmConfig,
            linear_echo: bool,
            echo_config: &FfiAec3Selection,
            capture_post: UniquePtr<NativeApmCustomModule>,
            render_pre: UniquePtr<NativeApmCustomModule>,
            analyzer: UniquePtr<NativeApmCustomModule>,
            echo_detector: UniquePtr<NativeApmCustomModule>,
            neural: UniquePtr<NativeApmCustomModule>,
            echo_factory: UniquePtr<NativeApmCustomModule>,
            error: &mut u8,
        ) -> UniquePtr<NativeAudioProcessingModule>;
        fn audio_processor_dump_path(
            processor: Pin<&mut NativeAudioProcessingModule>,
            worker: Box<RustApmDumpWorker>,
            filename: &[u8],
            maximum_bytes: i64,
            error: &mut u8,
        ) -> bool;
        fn audio_processor_dump_file(
            processor: Pin<&mut NativeAudioProcessingModule>,
            worker: Box<RustApmDumpWorker>,
            descriptor: i32,
            maximum_bytes: i64,
            error: &mut u8,
        ) -> bool;
        fn audio_processor_dump_attach(
            processor: Pin<&mut NativeAudioProcessingModule>,
            dump: Box<RustCallerApmDump>,
        ) -> u8;
        fn audio_processor_dump_detach(processor: Pin<&mut NativeAudioProcessingModule>) -> u8;
        fn audio_processor_dump_config_default() -> FfiApmDumpConfig;
        fn audio_processor_dump_config_equal(
            left: &FfiApmDumpConfig,
            right: &FfiApmDumpConfig,
        ) -> bool;
        fn audio_processor_runtime_setting(
            processor: Pin<&mut NativeAudioProcessingModule>,
            setting: &FfiApmRuntimeSetting,
            post: bool,
            error: &mut u8,
        ) -> bool;
        fn audio_processor_config_equal(
            left: &FfiApmConfig,
            right: &FfiApmConfig,
            group: u8,
        ) -> bool;
        fn audio_processor_custom_build(
            processor: Pin<&mut NativeAudioProcessingModule>,
            environment: &NativeEnvironment,
        ) -> bool;
        fn audio_processor_new_config(
            environment: &NativeEnvironment,
            config: &FfiApmConfig,
            linear_echo: bool,
            error: &mut u8,
        ) -> UniquePtr<NativeAudioProcessingModule>;
        fn audio_processor_config_apply(
            processor: Pin<&mut NativeAudioProcessingModule>,
            config: &FfiApmConfig,
        ) -> u8;
        fn audio_processor_linear_output(
            processor: Pin<&mut NativeAudioProcessingModule>,
            output: &mut [f32],
            error: &mut u8,
        ) -> bool;
        fn audio_processor_statistics_default() -> FfiApmStatistics;
        fn audio_processor_statistics(
            processor: Pin<&mut NativeAudioProcessingModule>,
            has_remote_argument: bool,
            has_remote_tracks: bool,
        ) -> FfiApmStatistics;
        fn audio_processor_config_default() -> FfiApmConfig;
        fn audio_processor_config_string(config: &FfiApmConfig) -> String;
        fn audio_processor_config_get(processor: &NativeAudioProcessingModule) -> FfiApmConfig;
        fn audio_processor_new(
            environment: &NativeEnvironment,
            config: &FfiAudioProcessingConfig,
        ) -> UniquePtr<NativeAudioProcessingModule>;
        fn audio_processor_effects_admitted(
            environment: &NativeEnvironment,
            config: &FfiAudioProcessingConfig,
        ) -> bool;
        fn audio_processor_initialize(
            processor: Pin<&mut NativeAudioProcessingModule>,
            error: &mut u8,
        ) -> i32;
        fn audio_processor_formats_default() -> FfiApmFormats;
        fn audio_processor_initialize_formats(
            processor: Pin<&mut NativeAudioProcessingModule>,
            formats: &FfiApmFormats,
            admitted: &mut bool,
            error: &mut u8,
        ) -> i32;
        fn audio_processor_configure(
            processor: Pin<&mut NativeAudioProcessingModule>,
            config: &FfiAudioProcessingConfig,
        ) -> u8;
        fn audio_processor_format(
            processor: &NativeAudioProcessingModule,
        ) -> FfiAudioProcessingFormatState;
        fn audio_processor_set_muted(processor: Pin<&mut NativeAudioProcessingModule>, muted: bool);
        fn audio_processor_muted(processor: Pin<&mut NativeAudioProcessingModule>) -> bool;
        fn audio_processor_set_delay(
            processor: Pin<&mut NativeAudioProcessingModule>,
            delay: i32,
        ) -> i32;
        fn audio_processor_delay(processor: &NativeAudioProcessingModule) -> i32;
        fn audio_processor_set_analog_level(
            processor: Pin<&mut NativeAudioProcessingModule>,
            level: i32,
        );
        fn audio_processor_analog_level(processor: &NativeAudioProcessingModule) -> i32;
        fn audio_processor_set_key(processor: Pin<&mut NativeAudioProcessingModule>, pressed: bool);
        fn audio_processor_i16(
            processor: Pin<&mut NativeAudioProcessingModule>,
            rate: i32,
            channels: usize,
            samples: &mut [i16],
            reverse: bool,
            error: &mut u8,
        ) -> i32;
        fn audio_processor_float(
            processor: Pin<&mut NativeAudioProcessingModule>,
            input_rate: i32,
            input_channels: usize,
            samples: &[f32],
            output_rate: i32,
            output_channels: usize,
            destination: &mut [f32],
            reverse: bool,
            error: &mut u8,
        ) -> i32;
        fn audio_processor_analyze(
            processor: Pin<&mut NativeAudioProcessingModule>,
            rate: i32,
            channels: usize,
            samples: &[f32],
            error: &mut u8,
        ) -> i32;
        include!("pulsebeam-webrtc-sys/native/video_codec_callbacks.h");
        type NativeDecodedImageSink;
        type NativeFecOverride;
        fn fec_override_new(provider: Box<RustFecOverride>) -> SharedPtr<NativeFecOverride>;
        fn fec_override_same(a: &NativeFecOverride, b: &NativeFecOverride) -> bool;
        fn fec_override_allowed(value: &NativeFecOverride, allowed: bool) -> bool;
        fn fec_override_retired(value: &NativeFecOverride) -> bool;
        type NativeEncodedImageSink;
        fn encoded_sink_new(
            provider: Box<RustEncodedImageSink>,
        ) -> UniquePtr<NativeEncodedImageSink>;
        fn encoded_sink_clone(value: &NativeEncodedImageSink) -> UniquePtr<NativeEncodedImageSink>;
        fn encoded_sink_same(a: &NativeEncodedImageSink, b: &NativeEncodedImageSink) -> bool;
        unsafe fn encoded_sink_frame(
            value: &NativeEncodedImageSink,
            image: &NativeEncodedImage,
            info: *const NativeCodecSpecificInfo,
        ) -> FfiEncodedCallbackResult;
        fn encoded_sink_dropped(
            value: &NativeEncodedImageSink,
            timestamp: u32,
            spatial_id: i32,
            end: bool,
        );
        fn decoded_sink_new(
            provider: Box<RustDecodedImageSink>,
        ) -> UniquePtr<NativeDecodedImageSink>;
        fn decoded_sink_clone(sink: &NativeDecodedImageSink) -> UniquePtr<NativeDecodedImageSink>;
        fn decoded_sink_same(a: &NativeDecodedImageSink, b: &NativeDecodedImageSink) -> bool;
        fn decoded_sink_frame(
            sink: &NativeDecodedImageSink,
            frame: Pin<&mut NativeRawVideoFrame>,
            overload: u8,
            time: i64,
            has_time: bool,
            optional_time: i32,
            has_qp: bool,
            qp: u8,
        ) -> i32;
        include!("pulsebeam-webrtc-sys/native/encoded_image.h");
        type NativeEncodedImage;
        type NativeEncodedVideoBuffer;
        type NativeWritableEncodedBuffer;
        fn encoded_image_new() -> UniquePtr<NativeEncodedImage>;
        fn encoded_image_clone(image: &NativeEncodedImage) -> UniquePtr<NativeEncodedImage>;
        fn encoded_image_assign(image: Pin<&mut NativeEncodedImage>, other: &NativeEncodedImage);
        fn encoded_image_metadata(image: &NativeEncodedImage) -> FfiEncodedImageMetadata;
        fn encoded_image_set_metadata(
            image: Pin<&mut NativeEncodedImage>,
            value: &FfiEncodedImageMetadata,
        ) -> bool;
        fn encoded_image_capture_time(image: &NativeEncodedImage, bits: &mut i64) -> bool;
        fn encoded_image_set_encode_time(
            image: Pin<&mut NativeEncodedImage>,
            start: i64,
            finish: i64,
        );
        fn encoded_image_is_key(image: &NativeEncodedImage) -> bool;
        fn encoded_image_is_delta(image: &NativeEncodedImage) -> bool;
        fn encoded_image_size(image: &NativeEncodedImage) -> usize;
        fn encoded_image_set_size(image: Pin<&mut NativeEncodedImage>, size: usize) -> bool;
        fn encoded_image_data(image: &NativeEncodedImage) -> Vec<u8>;
        fn encoded_image_buffer(image: &NativeEncodedImage) -> UniquePtr<NativeEncodedVideoBuffer>;
        fn encoded_image_set_buffer(
            image: Pin<&mut NativeEncodedImage>,
            buffer: &NativeEncodedVideoBuffer,
        );
        fn encoded_image_clear_buffer(image: Pin<&mut NativeEncodedImage>);
        fn encoded_image_layer_size(
            image: &NativeEncodedImage,
            index: i32,
            bytes: &mut usize,
            present: &mut bool,
        ) -> bool;
        fn encoded_image_set_layer_size(
            image: Pin<&mut NativeEncodedImage>,
            index: i32,
            bytes: usize,
        ) -> bool;
        fn encoded_image_frame_type_name(value: i32, name: &mut String) -> bool;
        fn encoded_buffer_empty() -> UniquePtr<NativeEncodedVideoBuffer>;
        fn encoded_buffer_new_owned(data: Vec<u8>) -> UniquePtr<NativeEncodedVideoBuffer>;
        fn encoded_buffer_new(data: &[u8]) -> UniquePtr<NativeEncodedVideoBuffer>;
        fn encoded_buffer_clone(
            buffer: &NativeEncodedVideoBuffer,
        ) -> UniquePtr<NativeEncodedVideoBuffer>;
        fn encoded_buffer_size(buffer: &NativeEncodedVideoBuffer) -> usize;
        fn encoded_buffer_data(buffer: &NativeEncodedVideoBuffer) -> Vec<u8>;
        fn encoded_writer_new(size: usize) -> UniquePtr<NativeWritableEncodedBuffer>;
        fn encoded_writer_size(buffer: &NativeWritableEncodedBuffer) -> usize;
        fn encoded_writer_resize(buffer: Pin<&mut NativeWritableEncodedBuffer>, size: usize);
        fn encoded_writer_write(
            buffer: Pin<&mut NativeWritableEncodedBuffer>,
            offset: usize,
            data: &[u8],
        ) -> bool;
        fn encoded_writer_read(buffer: &NativeWritableEncodedBuffer) -> FfiEncodedBufferRead;
        fn encoded_writer_freeze(
            buffer: UniquePtr<NativeWritableEncodedBuffer>,
        ) -> UniquePtr<NativeEncodedVideoBuffer>;
        include!("pulsebeam-webrtc-sys/native/raw_video_frame.h");
        include!("pulsebeam-webrtc-sys/native/audio_frame_values.h");
        type NativeMixerSource;
        type NativeAudioMixer;
        fn mixer_source_new(source: Box<RustMixerSource>) -> UniquePtr<NativeMixerSource>;
        fn mixer_source_get_audio(
            source: &NativeMixerSource,
            rate: i32,
            error: &mut u8,
        ) -> FfiMixerFrame;
        fn mixer_source_get_ssrc(source: &NativeMixerSource, valid: &mut bool) -> i32;
        fn mixer_source_get_rate(source: &NativeMixerSource, valid: &mut bool) -> i32;
        type NativeMixerSourceToken;
        type NativeMixerInputs;
        fn mixer_token_clone(token: &NativeMixerSourceToken) -> UniquePtr<NativeMixerSourceToken>;
        fn mixer_token_equal(a: &NativeMixerSourceToken, b: &NativeMixerSourceToken) -> bool;
        fn mixer_input_ssrc(
            inputs: &NativeMixerInputs,
            token: &NativeMixerSourceToken,
            error: &mut u8,
        ) -> i32;
        fn mixer_input_rate(
            inputs: &NativeMixerInputs,
            token: &NativeMixerSourceToken,
            error: &mut u8,
        ) -> i32;
        fn mixer_input_frame(
            inputs: &NativeMixerInputs,
            token: &NativeMixerSourceToken,
            rate: i32,
            error: &mut u8,
        ) -> FfiMixerFrame;
        fn audio_mixer_new(module: Box<RustMixerModule>) -> UniquePtr<NativeAudioMixer>;
        fn audio_mixer_from_provider(
            module: Box<RustMixerModule>,
            provider: Box<RustMixerProvider>,
        ) -> UniquePtr<NativeAudioMixer>;
        fn audio_mixer_add(
            mixer: Pin<&mut NativeAudioMixer>,
            source: &NativeMixerSource,
            error: &mut u8,
        ) -> bool;
        fn audio_mixer_remove(
            mixer: Pin<&mut NativeAudioMixer>,
            source: &NativeMixerSource,
            error: &mut u8,
        ) -> bool;
        fn audio_mixer_mix(
            mixer: Pin<&mut NativeAudioMixer>,
            channels: usize,
            error: &mut u8,
        ) -> UniquePtr<NativeAudioFrame>;
        type NativeAudioFrame;
        fn pcm_i16_shape(
            data: &[i16],
            samples: usize,
            channels: usize,
            layout: u8,
        ) -> FfiPcmViewInfo;
        fn pcm_f32_shape(
            data: &[f32],
            samples: usize,
            channels: usize,
            layout: u8,
        ) -> FfiPcmViewInfo;
        unsafe fn pcm_i16_planar_shape(pointers: &[usize], samples: usize) -> FfiPcmViewInfo;
        unsafe fn pcm_f32_planar_shape(pointers: &[usize], samples: usize) -> FfiPcmViewInfo;
        unsafe fn pcm_i16_clear(pointers: &[usize], samples: usize);
        unsafe fn pcm_f32_clear(pointers: &[usize], samples: usize);
        fn pcm_i16_copy(
            destination: &mut [i16],
            source: &[i16],
            samples: usize,
            channels: usize,
            destination_mono: bool,
            source_mono: bool,
            method: bool,
        ) -> u8;
        fn pcm_f32_copy(
            destination: &mut [f32],
            source: &[f32],
            samples: usize,
            channels: usize,
            destination_mono: bool,
            source_mono: bool,
            method: bool,
        ) -> u8;
        fn media_audio_frame_new() -> UniquePtr<NativeAudioFrame>;
        fn media_audio_frame_with_format(
            rate: i32,
            channels: usize,
            layout: i32,
        ) -> UniquePtr<NativeAudioFrame>;
        fn media_audio_default_channel_size(rate: usize) -> usize;
        fn media_channel_layout_count(layout: i32) -> i32;
        fn media_channel_order(layout: i32, channel: i32) -> i32;
        fn media_channel_layout_guess(channels: i32) -> i32;
        fn media_channel_layout_string(layout: i32) -> String;
        fn media_audio_frame_fields(frame: &NativeAudioFrame) -> FfiAudioFrameFields;
        fn media_audio_frame_set_fields(
            frame: Pin<&mut NativeAudioFrame>,
            value: &FfiAudioFrameFields,
        ) -> bool;
        fn media_audio_frame_reset(frame: Pin<&mut NativeAudioFrame>, mute: bool);
        fn media_audio_frame_update(
            frame: Pin<&mut NativeAudioFrame>,
            timestamp: u32,
            data: &[i16],
            present: bool,
            samples: usize,
            rate: i32,
            speech: i32,
            vad: i32,
            channels: usize,
        ) -> bool;
        fn media_audio_frame_copy(
            frame: Pin<&mut NativeAudioFrame>,
            source: &NativeAudioFrame,
        ) -> bool;
        fn media_audio_frame_data(frame: &NativeAudioFrame) -> &[i16];
        fn media_audio_frame_data_view<'a>(
            frame: &'a NativeAudioFrame,
            valid: &mut bool,
        ) -> &'a [i16];
        fn media_audio_frame_mutable_data(frame: Pin<&mut NativeAudioFrame>) -> &mut [i16];
        fn media_audio_frame_mutable_shape<'a>(
            frame: Pin<&'a mut NativeAudioFrame>,
            samples: usize,
            channels: usize,
            valid: &mut bool,
        ) -> &'a mut [i16];
        fn media_audio_frame_fill_shape<'a>(
            frame: Pin<&'a mut NativeAudioFrame>,
            samples: usize,
            channels: usize,
            value: i16,
            valid: &mut bool,
        ) -> &'a mut [i16];
        fn media_audio_frame_mute(frame: Pin<&mut NativeAudioFrame>);
        fn media_audio_frame_muted(frame: &NativeAudioFrame) -> bool;
        fn media_audio_frame_max_samples(frame: &NativeAudioFrame) -> usize;
        fn media_audio_frame_samples_per_channel(frame: &NativeAudioFrame) -> usize;
        fn media_audio_frame_num_channels(frame: &NativeAudioFrame) -> usize;
        fn media_audio_frame_layout(frame: &NativeAudioFrame) -> i32;
        fn media_audio_frame_sample_rate(frame: &NativeAudioFrame) -> i32;
        fn media_audio_frame_set_layout(
            frame: Pin<&mut NativeAudioFrame>,
            layout: i32,
            channels: usize,
        ) -> bool;
        fn media_audio_frame_capture_timestamp(frame: &NativeAudioFrame, present: &mut bool)
        -> i64;
        fn media_audio_frame_set_capture_timestamp(
            frame: Pin<&mut NativeAudioFrame>,
            timestamp: i64,
        );
        fn media_audio_frame_set_sample_rate(frame: Pin<&mut NativeAudioFrame>, rate: i32) -> bool;
        include!("pulsebeam-webrtc-sys/native/media_values.h");
        fn media_playout_delay_new(minimum_us: i64, maximum_us: i64) -> FfiMediaPlayoutDelay;
        fn media_playout_delay_set(
            value: &mut FfiMediaPlayoutDelay,
            minimum_us: i64,
            maximum_us: i64,
        ) -> bool;
        fn media_hdr_validate(value: &FfiMediaHdrMetadata) -> bool;
        fn media_color_space_string(value: &FfiMediaColorSpace) -> String;
        fn media_color_space_set(value: &mut FfiMediaColorSpace, field: u8, candidate: u8) -> bool;
        fn media_absolute_send_timestamp(value: u32) -> i64;
        fn media_video_timing_difference(base_ms: i64, time_ms: i64) -> u16;
        fn media_video_timing_duration(delta_us: i64) -> u16;
        fn media_rtp_header_round_trip(
            value: &FfiMediaRtpHeader,
            valid: &mut bool,
        ) -> FfiMediaRtpHeader;
        fn media_packet_info_from_header(
            value: &FfiMediaRtpHeader,
            receive_time_us: i64,
            valid: &mut bool,
        ) -> FfiRtpReceivedPacketInfo;
        fn media_packet_info_round_trip(
            value: &FfiRtpReceivedPacketInfo,
        ) -> FfiRtpReceivedPacketInfo;
        fn media_packet_infos_round_trip(
            value: &[FfiRtpReceivedPacketInfo],
        ) -> Vec<FfiRtpReceivedPacketInfo>;
        include!("pulsebeam-webrtc-sys/native/peer.h");
        include!("pulsebeam-webrtc-sys/native/local_network_permission.h");
        include!("pulsebeam-webrtc-sys/native/async_dns.h");
        include!("pulsebeam-webrtc-sys/native/video_bitrate.h");
        include!("pulsebeam-webrtc-sys/native/network_control.h");
        fn network_controller_injection_enabled(environment: &NativeEnvironment) -> bool;
        fn network_controller_native_probe(
            environment: &NativeEnvironment,
            source: Box<RustNetworkControllerFactory>,
            messages: &[FfiNcMessage],
            kinds: &[u8],
        ) -> Vec<FfiNcNetworkControlUpdate>;
        include!("pulsebeam-webrtc-sys/native/fec.h");
        type NativeFecProtectionCallback;
        fn fec_protection_request(
            source: &NativeFecProtectionCallback,
            delta: FfiFecProtectionParams,
            key: FfiFecProtectionParams,
        ) -> FfiFecProtectionResult;
        fn fec_set_retransmission_mode(source: &NativeFecProtectionCallback, mode: i32) -> u8;
        fn fec_protection_request_async(
            source: SharedPtr<NativeFecProtectionCallback>,
            delta: FfiFecProtectionParams,
            key: FfiFecProtectionParams,
            completion: Box<RustFecCompletion>,
        );
        fn fec_set_retransmission_mode_async(
            source: SharedPtr<NativeFecProtectionCallback>,
            mode: i32,
            completion: Box<RustFecCompletion>,
        );
        fn fec_native_probe(
            environment: &NativeEnvironment,
            source: Box<RustFecControllerFactory>,
        ) -> u32;
        include!("pulsebeam-webrtc-sys/native/network_predictor.h");
        fn network_predictor_native_probe(
            source: Box<RustNetworkStatePredictorFactory>,
            inputs: &[FfiNetworkPredictorInput],
        ) -> Vec<i32>;
        type NativeDnsResolutionCompletion;
        type NativeOwnedDnsResolver;
        type NativeDnsResolverContext;
        type NativeOwnedDnsResult;
        type NativeDnsQuery;
        fn owned_dns_new(
            source: Box<RustAsyncDnsResolver>,
            role: Box<RustSocketRole>,
        ) -> UniquePtr<NativeOwnedDnsResolver>;
        fn owned_dns_context(
            resolver: &NativeOwnedDnsResolver,
        ) -> SharedPtr<NativeDnsResolverContext>;
        fn owned_dns_start(
            context: &NativeDnsResolverContext,
            address: &NativeSocketAddress,
            has_family: bool,
            family: i32,
            callback: Box<RustDnsCallback>,
        ) -> FfiOwnedDnsStart;
        fn owned_dns_result(context: &NativeDnsResolverContext) -> FfiOwnedDnsResult;
        fn owned_dns_result_call(
            result: &NativeOwnedDnsResult,
            address: bool,
            family: i32,
        ) -> FfiOwnedSocketResult;
        fn owned_dns_query_cancel(query: &NativeDnsQuery) -> bool;
        fn dns_resolution_complete(completion: &NativeDnsResolutionCompletion) -> u8;
        include!("pulsebeam-webrtc-sys/native/certificate_generator.h");
        type NativeCertificateCompletion;
        fn certificate_complete_success(
            completion: &NativeCertificateCompletion,
            certificate: &NativeCertificate,
        ) -> u8;
        fn certificate_complete_failure(completion: &NativeCertificateCompletion) -> u8;
        type NativeLocalNetworkPermissionCompletion;
        fn local_permission_completion_clone(
            completion: &NativeLocalNetworkPermissionCompletion,
        ) -> UniquePtr<NativeLocalNetworkPermissionCompletion>;
        fn local_permission_complete(
            completion: &NativeLocalNetworkPermissionCompletion,
            granted: bool,
        ) -> bool;
        include!("pulsebeam-webrtc-sys/native/socket_address.h");
        type NativeSocketAddress;
        fn address_family(kind: u8) -> i32;
        fn ip_address_parse(text: &[u8]) -> Vec<u8>;
        fn ip_address_special(family: i32, loopback: bool) -> Vec<u8>;
        fn ip_address_transform(ip: &[u8], operation: u8, length: i32) -> Vec<u8>;
        fn ip_address_text(ip: &[u8], kind: u8, flags: i32) -> Vec<u8>;
        fn ip_address_info(ip: &[u8], kind: u8) -> i64;
        fn ip_address_hash(ip: &[u8]) -> u64;
        fn ip_address_predicates(ip: &[u8]) -> u32;
        fn ip_address_compare(a: &[u8], b: &[u8]) -> i8;
        fn socket_address_new(ip: &[u8], port: u16) -> UniquePtr<NativeSocketAddress>;
        fn socket_address_host(host: &[u8], port: u16) -> UniquePtr<NativeSocketAddress>;
        fn socket_address_empty(family: i32) -> UniquePtr<NativeSocketAddress>;
        fn socket_address_clone(address: &NativeSocketAddress) -> UniquePtr<NativeSocketAddress>;
        fn socket_address_clear(address: Pin<&mut NativeSocketAddress>);
        fn socket_address_set_ip(address: Pin<&mut NativeSocketAddress>, ip: &[u8], resolved: bool);
        fn socket_address_set_host(address: Pin<&mut NativeSocketAddress>, host: &[u8]);
        fn socket_address_set_port(address: Pin<&mut NativeSocketAddress>, port: u16);
        fn socket_address_set_scope(address: Pin<&mut NativeSocketAddress>, scope: i32);
        fn socket_address_parse(address: Pin<&mut NativeSocketAddress>, text: &[u8]) -> u8;
        fn socket_address_bytes(address: &NativeSocketAddress, kind: u8) -> Vec<u8>;
        fn socket_address_info(address: &NativeSocketAddress, kind: u8) -> i64;
        fn socket_address_predicates(address: &NativeSocketAddress) -> u32;
        fn socket_address_hash(address: &NativeSocketAddress) -> u64;
        fn socket_address_compare(a: &NativeSocketAddress, b: &NativeSocketAddress) -> i8;
        fn socket_address_equal_ip(a: &NativeSocketAddress, b: &NativeSocketAddress) -> bool;
        fn socket_address_equal_port(a: &NativeSocketAddress, b: &NativeSocketAddress) -> bool;
        fn socket_address_native_size(kind: u8) -> usize;
        fn socket_address_storage(address: &NativeSocketAddress, kind: u8) -> Vec<u8>;
        fn socket_address_storage_written(address: &NativeSocketAddress, kind: u8) -> usize;
        fn socket_address_from_storage(bytes: &[u8], ipv4: bool) -> UniquePtr<NativeSocketAddress>;
        fn ip_address_from_storage(bytes: &[u8]) -> Vec<u8>;
        include!("pulsebeam-webrtc-sys/native/tls.h");
        type NativeTlsVerifier;
        fn tls_verifier_verify(
            verifier: &NativeTlsVerifier,
            certificate: &NativeTlsCertificate,
        ) -> bool;
        fn tls_verifier_verify_chain(
            verifier: &NativeTlsVerifier,
            chain: &NativeTlsCertificateChain,
        ) -> bool;
        fn tls_verifier_failed(verifier: &NativeTlsVerifier) -> bool;
        type NativeTlsCertificate;
        type NativeTlsCertificateChain;
        fn tls_certificate_from_pem(pem: &str) -> UniquePtr<NativeTlsCertificate>;
        fn tls_certificate_clone(
            certificate: &NativeTlsCertificate,
        ) -> UniquePtr<NativeTlsCertificate>;
        fn tls_certificate_pem(certificate: &NativeTlsCertificate) -> String;
        fn tls_certificate_der(certificate: &NativeTlsCertificate) -> Vec<u8>;
        fn tls_certificate_signature_digest(
            certificate: &NativeTlsCertificate,
            valid: &mut bool,
        ) -> String;
        fn tls_certificate_digest(
            certificate: &NativeTlsCertificate,
            algorithm: &str,
            capacity: usize,
            valid: &mut bool,
        ) -> Vec<u8>;
        fn tls_certificate_expiration(certificate: &NativeTlsCertificate) -> i64;
        fn tls_certificate_stats(certificate: &NativeTlsCertificate)
        -> Vec<FfiTlsCertificateStats>;
        fn tls_chain_new() -> UniquePtr<NativeTlsCertificateChain>;
        fn tls_chain_append(
            chain: Pin<&mut NativeTlsCertificateChain>,
            certificate: UniquePtr<NativeTlsCertificate>,
        );
        fn tls_chain_clone(
            chain: &NativeTlsCertificateChain,
        ) -> UniquePtr<NativeTlsCertificateChain>;
        fn tls_chain_size(chain: &NativeTlsCertificateChain) -> usize;
        fn tls_chain_get(
            chain: &NativeTlsCertificateChain,
            index: usize,
        ) -> UniquePtr<NativeTlsCertificate>;
        fn tls_chain_stats(chain: &NativeTlsCertificateChain) -> Vec<FfiTlsCertificateStats>;
        include!("pulsebeam-webrtc-sys/native/data_channel.h");
        include!("pulsebeam-webrtc-sys/native/video.h");
        include!("pulsebeam-webrtc-sys/native/media_stream.h");
        include!("pulsebeam-webrtc-sys/native/audio.h");

        type NativeRtcEvent;
        fn new_rtc_event(kind: u32, payload: &FfiRtcEventPayload) -> UniquePtr<NativeRtcEvent>;
        fn new_rtc_event_boundary(
            begin: bool,
            timestamp_us: i64,
            utc_start_us: i64,
        ) -> UniquePtr<NativeRtcEvent>;
        fn new_rtc_event_rtp(
            incoming: bool,
            packet: &[u8],
            extensions: &[FfiRtpHeaderExtension],
            probe_cluster_id: i32,
            has_rtx_sequence: bool,
            rtx_sequence: u16,
        ) -> UniquePtr<NativeRtcEvent>;
        fn rtc_event_copy(event: &NativeRtcEvent) -> UniquePtr<NativeRtcEvent>;
        type NativeEventEncoding;
        fn event_codec_encoder(kind: u32) -> UniquePtr<NativeEventEncoding>;
        fn event_codec_add(batch: Pin<&mut NativeEventEncoding>, event: &NativeRtcEvent) -> bool;
        fn event_codec_encode(batch: &NativeEventEncoding) -> Vec<u8>;
        fn event_codec_parse(kind: u32, bytes: &[u8], batched: bool) -> FfiEventParseResult;
        fn event_codec_status(
            success: bool,
            error: &str,
            file: &str,
            line: i32,
        ) -> FfiEventParseStatus;
        fn rtc_event_rtp_dependency_descriptor(
            event: &NativeRtcEvent,
            has_structure: bool,
            structure: FfiRtcEventDependencyStructure,
        ) -> FfiRtcEventDependencyDescriptor;
        fn rtc_event_rtp_extension(event: &NativeRtcEvent, kind: u8) -> FfiRtcEventRtpExtension;
        fn rtc_event_payload(event: &NativeRtcEvent) -> FfiRtcEventPayload;
        fn rtc_event_kind(event: &NativeRtcEvent) -> u32;
        fn rtc_event_is_config(event: &NativeRtcEvent) -> bool;
        fn rtc_event_group_key(event: &NativeRtcEvent) -> u32;
        fn rtc_event_timestamp_us(event: &NativeRtcEvent) -> i64;
        fn rtc_event_set_timestamp(event: Pin<&mut NativeRtcEvent>, timestamp_us: i64);
        type NativeEventLogStop;
        fn event_log_stop_complete(completion: UniquePtr<NativeEventLogStop>);
        type NativeRtcEventLogFactory;
        fn new_rtc_event_log_factory(
            factory: Box<RustRtcEventLogFactory>,
        ) -> UniquePtr<NativeRtcEventLogFactory>;
        type NativeRtcEventLogProduct;
        fn event_log_product_create(
            factory: UniquePtr<NativeRtcEventLogFactory>,
            environment: &NativeEnvironment,
            retention: Box<RustEventLogRetention>,
            controlled: bool,
            error: &mut u8,
        ) -> UniquePtr<NativeRtcEventLogProduct>;
        fn event_log_product_start(
            log: Pin<&mut NativeRtcEventLogProduct>,
            output: UniquePtr<NativeEventLogOutput>,
            period_ms: i64,
        ) -> u8;
        fn event_log_product_log(
            log: Pin<&mut NativeRtcEventLogProduct>,
            event: UniquePtr<NativeRtcEvent>,
        ) -> u8;
        fn event_log_product_stop(log: Pin<&mut NativeRtcEventLogProduct>) -> u8;
        fn event_log_product_stop_async(
            log: Pin<&mut NativeRtcEventLogProduct>,
            completion: Box<RustEventLogProductStop>,
        ) -> u8;
        fn event_log_product_close(log: Pin<&mut NativeRtcEventLogProduct>);
        type NativeEventLogUtility;
        fn environment_event_log(
            environment: &NativeEnvironment,
        ) -> UniquePtr<NativeEventLogUtility>;
        fn event_log_utility_start(
            log: Pin<&mut NativeEventLogUtility>,
            output: UniquePtr<NativeEventLogOutput>,
            period_ms: i64,
        ) -> u8;
        fn event_log_utility_log(
            log: Pin<&mut NativeEventLogUtility>,
            event: UniquePtr<NativeRtcEvent>,
        ) -> u8;
        fn event_log_utility_stop(log: Pin<&mut NativeEventLogUtility>) -> u8;
        fn event_log_utility_stop_async(
            log: Pin<&mut NativeEventLogUtility>,
            completion: Box<RustEventLogProductStop>,
            retention: Box<RustEventLogRetention>,
        ) -> u8;
        fn event_log_utility_close(log: Pin<&mut NativeEventLogUtility>);
        fn test_event_log_sdk_dispatch(
            environment: &NativeEnvironment,
            event: UniquePtr<NativeRtcEvent>,
        );
        fn test_stock_event_log_environment(
            environment: &NativeEnvironment,
            retention: Box<RustEventLogRetention>,
        ) -> UniquePtr<NativeEnvironment>;
        type NativeEventLogOutput;
        fn event_log_output_native_active(output: &NativeEventLogOutput) -> bool;
        fn event_log_output_native_write(
            output: Pin<&mut NativeEventLogOutput>,
            bytes: &[u8],
        ) -> bool;
        fn event_log_output_native_flush(output: Pin<&mut NativeEventLogOutput>);
        fn event_log_output_from_sink(
            output: Box<RustEventLogOutput>,
        ) -> UniquePtr<NativeEventLogOutput>;
        fn event_log_output_file(
            filename: &str,
            maximum_bytes: usize,
        ) -> UniquePtr<NativeEventLogOutput>;
        type NativeEnvironment;
        type NativeManualClock;
        type NativeRandomnessLease;
        type NativeTaskQueue;
        type NativeTaskQueueFactory;
        type NativeDriverThread;
        type NativeThread;
        type NativeSimulatedNetwork;
        type NativeNetworkEndpoint;
        type NativeNetworkManagerProvider;
        type NativeNetwork;
        type NativeNetworkResources;
        type NativeManagerSubscription;
        fn manager_observe(
            resources: SharedPtr<NativeNetworkResources>,
            kind: u8,
            callback: Box<RustManagerCallback>,
        ) -> UniquePtr<NativeManagerSubscription>;
        fn manager_subscription_cancel(subscription: &NativeManagerSubscription) -> bool;
        fn manager_subscription_active(subscription: &NativeManagerSubscription) -> bool;
        fn manager_observation_retired(resources: &NativeNetworkResources) -> bool;
        fn manager_observation_ready(resources: &NativeNetworkResources) -> bool;
        fn manager_observation_notify(resources: &NativeNetworkResources, kind: u8) -> u8;
        type NativeMdnsResponder;
        type NativeMdnsCompletion;
        fn mdns_responder_new(
            resources: SharedPtr<NativeNetworkResources>,
            source: Box<RustMdnsResponder>,
        ) -> SharedPtr<NativeMdnsResponder>;
        fn mdns_responder_retired(responder: &NativeMdnsResponder) -> bool;
        fn mdns_responder_same(first: &NativeMdnsResponder, second: &NativeMdnsResponder) -> bool;
        fn mdns_responder_failed(responder: &NativeMdnsResponder) -> bool;
        fn mdns_responder_request(
            responder: &NativeMdnsResponder,
            ip: &[u8],
            remove: bool,
            callback: Box<RustMdnsCallback>,
        ) -> u8;
        fn mdns_complete(
            completion: &NativeMdnsCompletion,
            ip: &[u8],
            name: &[u8],
            removed: bool,
        ) -> u8;
        fn mdns_cancel(completion: &NativeMdnsCompletion) -> bool;
        fn network_value_set_mdns(
            network: &NativeNetwork,
            provider: SharedPtr<NativeMdnsResponder>,
        );
        fn network_value_mdns(network: &NativeNetwork) -> SharedPtr<NativeMdnsResponder>;
        type NativeNetworkSubscription;
        fn network_value_subscribe(
            network: SharedPtr<NativeNetwork>,
            kind: u8,
            callback: Box<RustNetworkCallback>,
        ) -> UniquePtr<NativeNetworkSubscription>;
        fn network_subscription_cancel(subscription: &NativeNetworkSubscription) -> bool;
        fn network_subscription_failed(subscription: &NativeNetworkSubscription) -> bool;
        type NativeNetworkMask;
        type NativeDefaultLocalAddressProvider;
        fn default_address_provider_new(
            provider: Box<RustDefaultLocalAddressProvider>,
        ) -> SharedPtr<NativeDefaultLocalAddressProvider>;
        fn default_address_provider_get(
            provider: &NativeDefaultLocalAddressProvider,
            family: i32,
        ) -> FfiDefaultLocalAddress;
        fn default_address_provider_failed(provider: &NativeDefaultLocalAddressProvider) -> bool;
        fn network_value_set_default_provider(
            network: &NativeNetwork,
            provider: SharedPtr<NativeDefaultLocalAddressProvider>,
        );
        fn network_value_default_provider(
            network: &NativeNetwork,
        ) -> SharedPtr<NativeDefaultLocalAddressProvider>;
        fn network_value_new(
            name: &[u8],
            description: &[u8],
            prefix: &[u8],
            prefix_length: i32,
            adapter: i32,
        ) -> SharedPtr<NativeNetwork>;
        fn network_value_clone(network: &NativeNetwork) -> SharedPtr<NativeNetwork>;
        fn network_value_same(first: &NativeNetwork, second: &NativeNetwork) -> bool;
        fn network_value_bytes(network: &NativeNetwork, field: u8) -> Vec<u8>;
        fn network_value_info(network: &NativeNetwork, field: u8) -> i32;
        fn network_value_set(network: &NativeNetwork, field: u8, value: i32) -> bool;
        fn network_value_notify(network: &NativeNetwork, kind: u8) -> bool;
        fn network_value_add_ip(network: &NativeNetwork, ip: &[u8], flags: i32);
        fn network_value_ips(network: &NativeNetwork) -> Vec<FfiInterfaceAddress>;
        fn network_value_set_ips(
            network: &NativeNetwork,
            ips: &[FfiInterfaceAddress],
            already_changed: bool,
        ) -> bool;
        fn network_value_clear_ips(network: &NativeNetwork);
        fn network_value_cost(network: &NativeNetwork, trials: &NativeFieldTrials) -> i32;
        fn network_guess_cost(cost: i32) -> FfiNetworkCostGuess;
        fn network_adapter_is_cellular(adapter: i32) -> bool;
        fn network_make_key(name: &[u8], prefix: &[u8], prefix_length: i32) -> Vec<u8>;
        fn network_adapter_from_name(name: &[u8]) -> i32;
        fn network_mask_new(prefix: &[u8], length: i32) -> UniquePtr<NativeNetworkMask>;
        fn network_mask_address(mask: &NativeNetworkMask) -> Vec<u8>;
        fn network_mask_prefix_length(mask: &NativeNetworkMask) -> i32;
        fn network_mask_equal(first: &NativeNetworkMask, second: &NativeNetworkMask) -> bool;
        type NativePacketSocketFactoryProvider;
        type NativeSimulatedUdpSocket;
        type NativeOutboundPacket;
        type NativeReceivedPacket;
        type NativeVideoEncoderFactory;
        type NativeVideoDecoderFactory;
        type NativeVideoEncoderProduct;
        fn video_encoder_create_actor_product(
            owner: &NativePeerConnectionFactory,
            factory: &NativeVideoEncoderFactory,
            environment: &NativeEnvironment,
            retention: Box<RustVideoFactoryEnvironment>,
            format: &NativeSdpVideoFormat,
        ) -> UniquePtr<NativeVideoEncoderProduct>;
        fn video_encoder_create_product(
            factory: &NativeVideoEncoderFactory,
            environment: &NativeEnvironment,
            retention: Box<RustVideoFactoryEnvironment>,
            format: &NativeSdpVideoFormat,
        ) -> UniquePtr<NativeVideoEncoderProduct>;
        fn video_encoder_product_fec(
            product: Pin<&mut NativeVideoEncoderProduct>,
            value: SharedPtr<NativeFecOverride>,
        );
        unsafe fn video_encoder_product_register_callback(
            product: Pin<&mut NativeVideoEncoderProduct>,
            sink: *const NativeEncodedImageSink,
        ) -> i32;
        fn video_encoder_product_initialize(
            product: Pin<&mut NativeVideoEncoderProduct>,
            codec: &NativeVideoCodec,
            settings: FfiOwnedEncoderSettings,
            legacy: bool,
        ) -> i32;
        fn video_encoder_product_encode(
            product: Pin<&mut NativeVideoEncoderProduct>,
            frame: &NativeRawVideoFrame,
            has_types: bool,
            types: &[i32],
        ) -> i32;
        fn video_encoder_product_rates(
            product: Pin<&mut NativeVideoEncoderProduct>,
            rates: &NativeVideoRateControl,
        );
        fn video_encoder_product_info(
            product: &NativeVideoEncoderProduct,
        ) -> UniquePtr<NativeVideoEncoderInfo>;
        fn video_encoder_product_name(
            product: &NativeVideoEncoderProduct,
            valid: &mut bool,
        ) -> Vec<u8>;
        fn video_encoder_product_packet_loss(
            product: Pin<&mut NativeVideoEncoderProduct>,
            rate: f32,
        );
        fn video_encoder_product_rtt(product: Pin<&mut NativeVideoEncoderProduct>, rtt_ms: i64);
        fn video_encoder_product_loss_notification(
            product: Pin<&mut NativeVideoEncoderProduct>,
            notification: FfiVideoEncoderLossNotification,
        );
        fn video_encoder_product_release(product: Pin<&mut NativeVideoEncoderProduct>) -> i32;
        type NativeVideoDecoderProduct;
        fn video_decoder_create_actor_product(
            owner: &NativePeerConnectionFactory,
            factory: &NativeVideoDecoderFactory,
            environment: &NativeEnvironment,
            retention: Box<RustVideoFactoryEnvironment>,
            format: &NativeSdpVideoFormat,
            status: &mut i32,
        ) -> UniquePtr<NativeVideoDecoderProduct>;
        fn video_decoder_create_product(
            factory: &NativeVideoDecoderFactory,
            environment: &NativeEnvironment,
            retention: Box<RustVideoFactoryEnvironment>,
            format: &NativeSdpVideoFormat,
            status: &mut i32,
        ) -> UniquePtr<NativeVideoDecoderProduct>;
        fn video_decoder_product_info(
            product: &NativeVideoDecoderProduct,
        ) -> UniquePtr<NativeVideoDecoderInfo>;
        fn video_decoder_product_implementation_name(
            product: &NativeVideoDecoderProduct,
            valid: &mut bool,
        ) -> Vec<u8>;
        unsafe fn video_decoder_product_register_callback(
            product: Pin<&mut NativeVideoDecoderProduct>,
            sink: *const NativeDecodedImageSink,
        ) -> i32;
        fn video_decoder_product_decode(
            product: Pin<&mut NativeVideoDecoderProduct>,
            frame: &NativeEncodedImage,
            missing_overload: bool,
            missing: bool,
            render_time_ms: i64,
            valid: &mut bool,
        ) -> i32;
        fn video_decoder_product_configure(
            product: Pin<&mut NativeVideoDecoderProduct>,
            settings: &NativeVideoDecoderSettings,
        ) -> bool;
        fn video_decoder_product_release(product: Pin<&mut NativeVideoDecoderProduct>) -> i32;
        type NativeAudioEncoderFactory;
        type NativeAudioEncoder;
        type NativeAudioDecoderFactory;
        type NativeAudioDecoder;
        fn audio_decoder_factory_transfer_ready(decoder: &NativeAudioDecoder) -> bool;
        fn audio_decoder_forwarded_frame_count(decoder: &NativeAudioDecoder) -> usize;
        fn audio_decoder_forward_product(
            owner: Box<AudioDecoderTransfer>,
        ) -> UniquePtr<NativeAudioDecoder>;
        type NativeCallerAudioDecoderFactory;
        fn audio_caller_decoder_factory_new(
            owner: Box<RustAudioDecoderFactoryCall>,
        ) -> UniquePtr<NativeCallerAudioDecoderFactory>;
        fn audio_caller_decoder_factory_formats(
            handle: &NativeCallerAudioDecoderFactory,
            call: &RustAudioDecoderFactoryCall,
        ) -> Vec<FfiAudioCodecSpec>;
        fn audio_caller_decoder_factory_supported(
            handle: &NativeCallerAudioDecoderFactory,
            call: &RustAudioDecoderFactoryCall,
            format: &FfiSdpAudioFormat,
        ) -> bool;
        fn audio_caller_decoder_factory_create(
            handle: &NativeCallerAudioDecoderFactory,
            call: &RustAudioDecoderFactoryCall,
            environment: &NativeEnvironment,
            format: &FfiSdpAudioFormat,
            compatibility: bool,
            pair: &UniquePtr<NativeAudioCodecPairId>,
        ) -> UniquePtr<NativeAudioDecoder>;
        fn audio_caller_decoder_formats(
            source: Box<RustAudioDecoderFactoryCall>,
        ) -> Vec<FfiAudioCodecSpec>;
        fn audio_caller_decoder_supported(
            source: Box<RustAudioDecoderFactoryCall>,
            format: &FfiSdpAudioFormat,
        ) -> bool;
        fn audio_caller_decoder_create(
            source: Box<RustAudioDecoderFactoryCall>,
            environment: &NativeEnvironment,
            format: &FfiSdpAudioFormat,
            compatibility: bool,
            pair: &UniquePtr<NativeAudioCodecPairId>,
        ) -> UniquePtr<NativeAudioDecoder>;
        type NativeDecodableAudioFrame;
        type NativeAudioCodecPairId;
        type NativeAudioEncoderOptions;
        fn audio_encoder_options_default() -> UniquePtr<NativeAudioEncoderOptions>;
        fn audio_encoder_options_copy(
            source: &NativeAudioEncoderOptions,
        ) -> UniquePtr<NativeAudioEncoderOptions>;
        fn audio_encoder_options_assign(
            target: Pin<&mut NativeAudioEncoderOptions>,
            source: &NativeAudioEncoderOptions,
        );
        fn audio_encoder_options_payload_type(source: &NativeAudioEncoderOptions) -> i32;
        fn audio_encoder_options_set_payload_type(
            target: Pin<&mut NativeAudioEncoderOptions>,
            value: i32,
        );
        fn audio_encoder_options_pair_id(
            source: &NativeAudioEncoderOptions,
        ) -> UniquePtr<NativeAudioCodecPairId>;
        fn audio_encoder_options_set_pair_id(
            target: Pin<&mut NativeAudioEncoderOptions>,
            pair: &UniquePtr<NativeAudioCodecPairId>,
        );
        type NativeNetEq;
        unsafe fn neteq_factory_create(
            source: Box<RustNetEqFactory>,
            environment: &NativeEnvironment,
            config: &FfiNetEqConfig,
            decoder_factory: *const NativeAudioDecoderFactory,
        ) -> FfiNetEqFactoryCreation;
        fn neteq_factory_failed_product_contract(
            source: Box<RustNetEqFactory>,
            environment: &NativeEnvironment,
            decoder_factory: &NativeAudioDecoderFactory,
        ) -> bool;
        fn neteq_provider_create(
            environment: &NativeEnvironment,
            provider: Box<RustNetEqProvider>,
        ) -> UniquePtr<NativeNetEq>;
        fn neteq_failed_create(configured_rate: i32) -> UniquePtr<NativeNetEq>;
        fn neteq_failed_create_for_sdk(configured_rate: i32) -> UniquePtr<NativeNetEq>;
        fn neteq_is_failed(value: &NativeNetEq) -> bool;
        fn neteq_is_stock(value: &NativeNetEq) -> bool;
        unsafe fn neteq_stock_create(
            environment: &NativeEnvironment,
            config: &FfiNetEqConfig,
            factory: *const NativeAudioDecoderFactory,
            minimum_time_us: i64,
            maximum_time_us: i64,
        ) -> UniquePtr<NativeNetEq>;
        fn neteq_insert_packet(
            value: Pin<&mut NativeNetEq>,
            header: &FfiMediaRtpHeader,
            payload: &[u8],
        ) -> i32;
        fn neteq_insert_packet_at(
            value: Pin<&mut NativeNetEq>,
            header: &FfiMediaRtpHeader,
            payload: &[u8],
            receive_time_us: i64,
        ) -> i32;
        fn neteq_insert_packet_info(
            value: Pin<&mut NativeNetEq>,
            header: &FfiMediaRtpHeader,
            payload: &[u8],
            info: &FfiRtpReceivedPacketInfo,
        ) -> i32;
        fn neteq_get_audio(
            value: Pin<&mut NativeNetEq>,
            frame: Pin<&mut NativeAudioFrame>,
            want_muted: bool,
            muted: bool,
            want_rate: bool,
            rate: i32,
            has_override: bool,
            operation: i32,
        ) -> FfiNetEqAudioResult;
        fn neteq_set_codecs(value: Pin<&mut NativeNetEq>, codecs: &[FfiNetEqCodec]) -> bool;
        fn neteq_register_payload_type(
            value: Pin<&mut NativeNetEq>,
            payload_type: i32,
            format: &FfiSdpAudioFormat,
        ) -> bool;
        fn neteq_create_decoder(value: Pin<&mut NativeNetEq>, payload_type: i32) -> bool;
        fn neteq_remove_payload_type(value: Pin<&mut NativeNetEq>, payload_type: u8) -> i32;
        fn neteq_remove_all_payload_types(value: Pin<&mut NativeNetEq>) -> bool;
        fn neteq_set_maximum_buffer_packets(value: Pin<&mut NativeNetEq>, count: usize) -> bool;
        fn neteq_set_fast_accelerate(value: Pin<&mut NativeNetEq>, enable: bool);
        fn neteq_set_minimum_delay(value: Pin<&mut NativeNetEq>, delay_ms: i32) -> bool;
        fn neteq_set_maximum_delay(value: Pin<&mut NativeNetEq>, delay_ms: i32) -> bool;
        fn neteq_set_base_minimum_delay(value: Pin<&mut NativeNetEq>, delay_ms: i32) -> bool;
        fn neteq_base_minimum_delay(value: &NativeNetEq) -> i32;
        fn neteq_target_delay(value: &NativeNetEq) -> i32;
        fn neteq_filtered_current_delay(value: &NativeNetEq) -> i32;
        fn neteq_network_statistics(
            value: Pin<&mut NativeNetEq>,
        ) -> FfiNetEqNetworkStatisticsResult;
        fn neteq_current_network_statistics(value: &NativeNetEq) -> FfiNetEqNetworkStatistics;
        fn neteq_lifetime_statistics(value: &NativeNetEq) -> FfiNetEqLifetimeStatistics;
        fn neteq_operations_and_state(value: &NativeNetEq) -> FfiNetEqOperationsAndState;
        fn neteq_playout_timestamp(value: &NativeNetEq, present: &mut bool) -> u32;
        fn neteq_last_output_sample_rate(value: &NativeNetEq) -> i32;
        fn neteq_current_decoder_format(
            value: Pin<&mut NativeNetEq>,
        ) -> FfiNetEqDecoderFormatResult;
        fn neteq_decoder_format(
            value: &NativeNetEq,
            payload_type: i32,
        ) -> FfiNetEqDecoderFormatResult;
        fn neteq_flush_buffers(value: Pin<&mut NativeNetEq>);
        fn neteq_sync_buffer_size(value: &NativeNetEq) -> i32;
        fn neteq_insert_empty_packet(value: Pin<&mut NativeNetEq>, header: &FfiMediaRtpHeader);
        fn neteq_enable_nack(value: Pin<&mut NativeNetEq>, max_list_size: usize);
        fn neteq_disable_nack(value: Pin<&mut NativeNetEq>);
        fn neteq_nack_list(value: &NativeNetEq, round_trip_time_ms: i64) -> Vec<u16>;
        fn neteq_decoder_handoff_contract(
            factory: &NativeAudioDecoderFactory,
            environment: &NativeEnvironment,
        ) -> bool;
        fn neteq_config_default() -> FfiNetEqConfig;
        fn neteq_config_description(value: &FfiNetEqConfig) -> String;
        fn neteq_stock_config_safe(value: &FfiNetEqConfig) -> bool;
        fn neteq_stock_payload_size_safe(size: usize) -> bool;
        fn neteq_stock_buffer_packets_safe(count: usize) -> bool;
        fn neteq_stock_red_payload_types(payload: &[u8], types: &mut Vec<u8>) -> bool;
        fn neteq_config_round_trip(value: &FfiNetEqConfig) -> FfiNetEqConfig;
        fn neteq_network_statistics_round_trip(
            value: &FfiNetEqNetworkStatistics,
        ) -> FfiNetEqNetworkStatistics;
        fn neteq_lifetime_statistics_round_trip(
            value: &FfiNetEqLifetimeStatistics,
        ) -> FfiNetEqLifetimeStatistics;
        fn neteq_operations_and_state_round_trip(
            value: &FfiNetEqOperationsAndState,
        ) -> FfiNetEqOperationsAndState;
        fn neteq_decoder_format_round_trip(value: &FfiNetEqDecoderFormat) -> FfiNetEqDecoderFormat;
        fn audio_codec_pair_id_create() -> UniquePtr<NativeAudioCodecPairId>;
        fn audio_codec_pair_id_copy(
            source: &NativeAudioCodecPairId,
        ) -> UniquePtr<NativeAudioCodecPairId>;
        fn audio_codec_pair_id_numeric(id: &NativeAudioCodecPairId) -> u64;
        type NativeRawVideoFrame;
        type NativeRawVideoBuffer;
        type NativeWritableI420;
        type NativeVideoBufferPreparation;
        fn raw_buffer_type_name(buffer_type: u8) -> String;
        fn raw_frame_from_render_time(
            buffer: &NativeRawVideoBuffer,
            rtp_timestamp: u32,
            milliseconds: i64,
            rotation: u16,
        ) -> UniquePtr<NativeRawVideoFrame>;
        fn raw_frame_clone(frame: &NativeRawVideoFrame) -> UniquePtr<NativeRawVideoFrame>;
        fn raw_frame_from_buffer(buffer: &NativeRawVideoBuffer) -> UniquePtr<NativeRawVideoFrame>;
        fn raw_frame_metadata(frame: &NativeRawVideoFrame) -> FfiRawFrameMetadata;
        fn raw_frame_set_metadata(
            frame: Pin<&mut NativeRawVideoFrame>,
            metadata: &FfiRawFrameMetadata,
        ) -> bool;
        fn raw_frame_buffer(frame: &NativeRawVideoFrame) -> UniquePtr<NativeRawVideoBuffer>;
        fn raw_frame_set_buffer(
            frame: Pin<&mut NativeRawVideoFrame>,
            buffer: &NativeRawVideoBuffer,
        ) -> bool;
        fn raw_frame_timestamp_us(frame: &NativeRawVideoFrame) -> i64;
        fn raw_frame_width(frame: &NativeRawVideoFrame) -> i32;
        fn raw_frame_height(frame: &NativeRawVideoFrame) -> i32;
        fn raw_frame_is_texture(frame: &NativeRawVideoFrame) -> bool;
        fn raw_frame_update_rect(frame: &NativeRawVideoFrame) -> FfiRawUpdateRect;
        fn raw_frame_size(frame: &NativeRawVideoFrame) -> u32;
        fn raw_frame_render_time_ms(frame: &NativeRawVideoFrame) -> i64;
        fn raw_frame_legacy(frame: &NativeRawVideoFrame) -> UniquePtr<NativeVideoFrame>;
        fn raw_buffer_clone(buffer: &NativeRawVideoBuffer) -> UniquePtr<NativeRawVideoBuffer>;
        fn raw_buffer_info(buffer: &NativeRawVideoBuffer) -> FfiRawBufferInfo;
        fn raw_buffer_storage(buffer: &NativeRawVideoBuffer) -> Vec<u8>;
        fn raw_buffer_to_i420(buffer: &NativeRawVideoBuffer) -> UniquePtr<NativeRawVideoBuffer>;
        fn raw_buffer_get_i420(buffer: &NativeRawVideoBuffer) -> UniquePtr<NativeRawVideoBuffer>;
        fn raw_buffer_crop_scale(
            buffer: &NativeRawVideoBuffer,
            x: i32,
            y: i32,
            crop_width: i32,
            crop_height: i32,
            width: i32,
            height: i32,
        ) -> UniquePtr<NativeRawVideoBuffer>;
        fn raw_buffer_mapped(
            buffer: &NativeRawVideoBuffer,
            types: &[u8],
        ) -> UniquePtr<NativeRawVideoBuffer>;
        fn raw_buffer_prepare(
            buffer: &NativeRawVideoBuffer,
            width: usize,
            height: usize,
            identifier: usize,
        ) -> UniquePtr<NativeVideoBufferPreparation>;
        fn raw_buffer_prepared(
            request: &NativeVideoBufferPreparation,
            identifier: &mut usize,
        ) -> bool;
        fn raw_buffer_preparation_close(request: &NativeVideoBufferPreparation);
        fn raw_buffer_plane(buffer: &NativeRawVideoBuffer, plane: u8) -> FfiRawVideoPlane;
        fn raw_i420_create(
            width: i32,
            height: i32,
            stride_y: i32,
            stride_u: i32,
            stride_v: i32,
        ) -> UniquePtr<NativeWritableI420>;
        fn raw_i420_copy(buffer: &NativeRawVideoBuffer) -> UniquePtr<NativeWritableI420>;
        fn raw_i420_rotate(
            buffer: &NativeRawVideoBuffer,
            rotation: u16,
        ) -> UniquePtr<NativeWritableI420>;
        fn raw_i420_freeze(
            buffer: UniquePtr<NativeWritableI420>,
        ) -> UniquePtr<NativeRawVideoBuffer>;
        fn raw_i420_info(buffer: &NativeWritableI420) -> FfiRawBufferInfo;
        fn raw_i420_initialized(buffer: &NativeWritableI420) -> bool;
        fn raw_i420_plane(buffer: &NativeWritableI420, plane: u8) -> FfiRawVideoPlane;
        fn raw_i420_write_plane(
            buffer: Pin<&mut NativeWritableI420>,
            plane: u8,
            data: &[u8],
            stride: i32,
        ) -> bool;
        fn raw_i420_initialize(buffer: Pin<&mut NativeWritableI420>, black: bool);
        fn raw_i420_crop_from(
            target: Pin<&mut NativeWritableI420>,
            source: &NativeRawVideoBuffer,
            x: i32,
            y: i32,
            width: i32,
            height: i32,
        ) -> bool;
        fn raw_i420_scale_from(
            target: Pin<&mut NativeWritableI420>,
            source: &NativeRawVideoBuffer,
            center_crop: bool,
        ) -> bool;
        fn raw_update_rect_operation(
            a: FfiRawUpdateRect,
            b: FfiRawUpdateRect,
            operation: u8,
            geometry: &[i32],
            valid: &mut bool,
        ) -> FfiRawUpdateRect;
        type NativeVideoFrame;
        type NativeEncodedVideoFrame;
        type NativeEncodedImageCallback;
        type NativeDecodedImageCallback;
        type NativeCertificate;
        type NativePeerConnectionFactory;
        fn factory_requires_deferred_teardown(factory: &NativePeerConnectionFactory) -> bool;
        type NativePeerConnection;
        type NativeDataChannel;
        type NativeDesktopMemory;
        fn desktop_memory_new(
            size: usize,
            id: i32,
            heap: bool,
            error: &mut i32,
        ) -> UniquePtr<NativeDesktopMemory>;
        fn desktop_memory_map(
            fd: i32,
            size: usize,
            id: i32,
            error: &mut i32,
        ) -> UniquePtr<NativeDesktopMemory>;
        fn desktop_memory_info(memory: &NativeDesktopMemory) -> FfiDesktopMemoryInfo;
        fn desktop_memory_read(
            memory: &NativeDesktopMemory,
            offset: usize,
            size: usize,
        ) -> FfiDesktopBytes;
        fn desktop_memory_write(
            memory: Pin<&mut NativeDesktopMemory>,
            offset: usize,
            bytes: &[u8],
        ) -> bool;
        fn desktop_memory_fd(memory: &NativeDesktopMemory) -> FfiDesktopFd;
        fn desktop_memory_retain(
            memory: Pin<&mut NativeDesktopMemory>,
            lease: Box<RustDesktopMemoryLease>,
        );
        type NativeDesktopFrame;
        fn desktop_frame_shared(
            width: i32,
            height: i32,
            stride: i32,
            format: u32,
            offset: usize,
            memory: UniquePtr<NativeDesktopMemory>,
            factory: Box<RustDesktopMemoryFactory>,
            create: bool,
            error: &mut u8,
        ) -> UniquePtr<NativeDesktopFrame>;
        fn desktop_frame_fd(frame: &NativeDesktopFrame) -> FfiDesktopFd;
        fn desktop_frame_new(width: i32, height: i32, format: u32)
        -> UniquePtr<NativeDesktopFrame>;
        fn desktop_frame_copy(frame: &NativeDesktopFrame) -> UniquePtr<NativeDesktopFrame>;
        fn desktop_frame_info(frame: &NativeDesktopFrame) -> FfiDesktopFrameInfo;
        fn desktop_frame_write_info(
            frame: Pin<&mut NativeDesktopFrame>,
            info: &FfiDesktopFrameInfo,
        ) -> u8;
        fn desktop_frame_pixels(
            frame: &NativeDesktopFrame,
            pixel: bool,
            x: i32,
            y: i32,
        ) -> FfiDesktopBytes;
        fn desktop_frame_black(frame: Pin<&mut NativeDesktopFrame>, write: bool) -> u8;
        fn desktop_frame_is_black(frame: &NativeDesktopFrame) -> u8;
        fn desktop_frame_copy_metadata(
            frame: Pin<&mut NativeDesktopFrame>,
            source: &NativeDesktopFrame,
        ) -> u8;
        fn desktop_frame_copy_buffer(
            frame: Pin<&mut NativeDesktopFrame>,
            bytes: &[u8],
            offset: usize,
            stride: i32,
            dest: &FfiDesktopRect,
        ) -> u8;
        fn desktop_frame_copy_pixels(
            frame: Pin<&mut NativeDesktopFrame>,
            source: &NativeDesktopFrame,
            x: i32,
            y: i32,
            dest: &FfiDesktopRect,
        ) -> u8;
        fn desktop_frame_copy_info(
            frame: Pin<&mut NativeDesktopFrame>,
            source: Pin<&mut NativeDesktopFrame>,
            move_info: bool,
        ) -> u8;
        fn desktop_frame_copy_intersection(
            frame: Pin<&mut NativeDesktopFrame>,
            source: &NativeDesktopFrame,
            horizontal_scale: f64,
            vertical_scale: f64,
        ) -> u8;
        type NativeCameraOptions;
        fn camera_options_new(
            factory: &NativePeerConnectionFactory,
        ) -> UniquePtr<NativeCameraOptions>;
        fn camera_options_clone(options: &NativeCameraOptions) -> UniquePtr<NativeCameraOptions>;
        fn camera_options_flags(options: &NativeCameraOptions) -> u8;
        fn camera_options_write(options: &NativeCameraOptions, v4l2: bool, pipewire: bool) -> bool;
        fn camera_options_set_fd(options: &NativeCameraOptions, fd: i32) -> bool;
        fn camera_options_initialize(
            options: &NativeCameraOptions,
            observer: &RustCameraInitializationObserver,
        ) -> i32;
        type NativeCameraDeviceInfo;
        fn camera_info_new(
            factory: &NativePeerConnectionFactory,
        ) -> UniquePtr<NativeCameraDeviceInfo>;
        fn camera_info_new_with_options(
            factory: &NativePeerConnectionFactory,
            options: &NativeCameraOptions,
        ) -> UniquePtr<NativeCameraDeviceInfo>;
        fn camera_info_query(
            info: &NativeCameraDeviceInfo,
            request: &FfiCameraInfoRequest,
        ) -> FfiCameraInfoReply;
        fn camera_info_close(info: &NativeCameraDeviceInfo);
        type NativeDesktopOptions;
        fn desktop_options_new(
            factory: &NativePeerConnectionFactory,
            environment: &NativeEnvironment,
            defaults: bool,
            selected_environment: bool,
        ) -> UniquePtr<NativeDesktopOptions>;
        fn desktop_options_clone(options: &NativeDesktopOptions)
        -> UniquePtr<NativeDesktopOptions>;
        fn desktop_options_read(options: &NativeDesktopOptions) -> FfiDesktopOptions;
        fn desktop_is_wayland(options: &NativeDesktopOptions) -> u8;
        fn desktop_options_write(options: &NativeDesktopOptions, fields: &FfiDesktopOptions) -> u8;
        fn desktop_options_time_us(options: &NativeDesktopOptions) -> i64;
        fn desktop_options_clock_matches(
            options: &NativeDesktopOptions,
            clock: &NativeClock,
        ) -> bool;
        fn desktop_options_resource(
            options: &NativeDesktopOptions,
            kind: u8,
        ) -> UniquePtr<NativeDesktopOptions>;
        fn desktop_resource_create(
            options: &NativeDesktopOptions,
            kind: u8,
            name: &str,
        ) -> UniquePtr<NativeDesktopOptions>;
        fn desktop_options_set_resource(
            options: &NativeDesktopOptions,
            kind: u8,
            present: bool,
            resource: &NativeDesktopOptions,
        ) -> u8;
        fn desktop_resource_equal(
            a: &NativeDesktopOptions,
            b: &NativeDesktopOptions,
            kind: u8,
        ) -> bool;
        fn create_desktop_capture(
            source: &NativeVideoSource,
            options: &NativeDesktopOptions,
            kind: u8,
            select: bool,
            id: i64,
            receiver: Box<RustDesktopCaptureReceiver>,
            observer: Box<RustDesktopSelectionObserver>,
            error: &mut String,
        ) -> UniquePtr<NativeScreenCapture>;
        type NativeScreenCapture;
        type NativeCamera;
        type NativeVideoSource;
        type NativeMediaTrack;
        type NativeTrackSource;
        type NativeAudioProcessor;
        type NativeMediaObserver;
        type NativeMediaStream;
        type NativeMediaStreams;
        type NativeMediaTracks;
        fn media_track_source(
            track: &NativeMediaTrack,
            factory: &NativePeerConnectionFactory,
            video: bool,
        ) -> UniquePtr<NativeTrackSource>;
        fn track_source_state(source: &NativeTrackSource) -> i32;
        fn track_source_remote(source: &NativeTrackSource) -> bool;
        fn track_source_same_resource(a: &NativeTrackSource, b: &NativeTrackSource) -> bool;
        fn track_source_audio_options(source: &NativeTrackSource) -> FfiAudioSourceOptions;
        fn track_source_video_info(source: &NativeTrackSource) -> FfiVideoSourceInfo;
        fn media_track_audio_volume(track: &NativeMediaTrack) -> FfiOptionalF64;
        fn media_track_set_volume(track: &NativeMediaTrack, volume: f64);
        fn media_track_set_should_receive(track: &NativeMediaTrack, receive: bool);
        fn media_track_set_content_hint(track: &NativeMediaTrack, hint: i32);
        fn media_track_set_audio_options(
            track: &NativeMediaTrack,
            options: &FfiAudioSourceOptions,
        ) -> FfiTrackAudioOptionsResult;
        fn media_track_signal_level(track: &NativeMediaTrack) -> FfiOptionalI32;
        fn media_track_should_receive(track: &NativeMediaTrack) -> i32;
        fn media_track_content_hint(track: &NativeMediaTrack) -> i32;
        fn media_track_audio_processor(
            track: &NativeMediaTrack,
            factory: &NativePeerConnectionFactory,
        ) -> UniquePtr<NativeAudioProcessor>;
        fn audio_processor_stats(
            processor: &NativeAudioProcessor,
            has_remote_tracks: bool,
        ) -> FfiAudioProcessorStats;
        fn audio_processor_same_resource(
            a: &NativeAudioProcessor,
            b: &NativeAudioProcessor,
        ) -> bool;
        fn rtp_receiver_streams(receiver: &NativeRtpReceiver) -> UniquePtr<NativeMediaStreams>;
        fn media_streams_len(streams: &NativeMediaStreams) -> usize;
        fn media_streams_get(
            streams: &NativeMediaStreams,
            index: usize,
        ) -> UniquePtr<NativeMediaStream>;
        fn peer_take_stream_event(
            peer: &NativePeerConnection,
            id: u64,
        ) -> UniquePtr<NativeMediaStream>;
        fn peer_take_track_event(peer: &NativePeerConnection, id: u64) -> FfiTrackArrival;
        fn peer_reconfigure_bandwidth_estimation(
            peer: &NativePeerConnection,
            allow_probe: bool,
        ) -> bool;
        fn peer_observe_channel_messages(peer: &NativePeerConnection, enabled: bool) -> u8;
        fn peer_factory_create_stream(
            factory: &NativePeerConnectionFactory,
            id: &[u8],
        ) -> UniquePtr<NativeMediaStream>;
        fn peer_mutate_stream(
            peer: &NativePeerConnection,
            stream: &NativeMediaStream,
            remove: bool,
        ) -> u8;
        fn peer_streams(peer: &NativePeerConnection, remote: bool)
        -> UniquePtr<NativeMediaStreams>;
        fn media_stream_mutate_track(
            stream: &NativeMediaStream,
            track: &NativeMediaTrack,
            remove: bool,
        ) -> bool;
        fn media_stream_id(stream: &NativeMediaStream) -> Vec<u8>;
        fn media_stream_same_resource(a: &NativeMediaStream, b: &NativeMediaStream) -> bool;
        fn media_stream_tracks(
            stream: &NativeMediaStream,
            video: bool,
        ) -> UniquePtr<NativeMediaTracks>;
        fn media_tracks_len(tracks: &NativeMediaTracks) -> usize;
        fn media_tracks_get(
            tracks: &NativeMediaTracks,
            index: usize,
        ) -> UniquePtr<NativeMediaTrack>;
        fn media_stream_find(
            stream: &NativeMediaStream,
            video: bool,
            id: &[u8],
        ) -> UniquePtr<NativeMediaTrack>;
        fn media_track_observe(
            track: &NativeMediaTrack,
            factory: &NativePeerConnectionFactory,
            capacity: usize,
        ) -> UniquePtr<NativeMediaObserver>;
        fn media_stream_observe(
            stream: &NativeMediaStream,
            factory: &NativePeerConnectionFactory,
            capacity: usize,
        ) -> UniquePtr<NativeMediaObserver>;
        fn media_observer_take(observer: &NativeMediaObserver) -> bool;
        fn media_observer_take_dropped(observer: &NativeMediaObserver) -> u64;
        fn media_track_id(track: &NativeMediaTrack) -> Vec<u8>;
        fn media_track_kind(track: &NativeMediaTrack) -> Vec<u8>;
        fn media_track_state(track: &NativeMediaTrack) -> i32;
        fn media_track_enabled(track: &NativeMediaTrack) -> bool;
        fn media_track_set_enabled(track: &NativeMediaTrack, enabled: bool) -> bool;
        fn media_track_same_resource(a: &NativeMediaTrack, b: &NativeMediaTrack) -> bool;
        fn video_track_reference(track: &NativeVideoTrack) -> UniquePtr<NativeMediaTrack>;
        fn audio_track_reference(track: &NativeAudioTrack) -> UniquePtr<NativeMediaTrack>;
        fn rtp_sender_track_reference(sender: &NativeRtpSender) -> UniquePtr<NativeMediaTrack>;
        fn rtp_receiver_track_reference(
            receiver: &NativeRtpReceiver,
        ) -> UniquePtr<NativeMediaTrack>;
        type NativeVideoSourceView;
        type NativeVideoSourceEncodedSink;
        type NativeVideoObserver;
        type NativeVideoTrack;
        type NativeAudioSource;
        type NativeAudioTrack;
        type NativeAudioSink;
        type NativeEncodedAudioSink;
        type NativeVideoSink;
        type NativeEncodedVideoSink;
        include!("pulsebeam-webrtc-sys/native/rtp_values.h");
        include!("pulsebeam-webrtc-sys/native/video_metadata.h");
        include!("pulsebeam-webrtc-sys/native/video_codec_values.h");
        type NativeVideoDecoderSettings;
        type NativeVideoDecoderInfo;
        type NativeVideoEncoderInfo;
        type NativeCodecSpecificInfo;
        fn codec_specific_new() -> UniquePtr<NativeCodecSpecificInfo>;
        fn codec_specific_clone(
            value: &NativeCodecSpecificInfo,
        ) -> UniquePtr<NativeCodecSpecificInfo>;
        fn codec_specific_fields(value: &NativeCodecSpecificInfo) -> FfiCodecSpecificFields;
        fn codec_specific_set_fields(
            value: Pin<&mut NativeCodecSpecificInfo>,
            fields: FfiCodecSpecificFields,
        ) -> bool;
        type NativeVideoCodec;
        fn video_codec_new() -> UniquePtr<NativeVideoCodec>;
        fn video_codec_clone(value: &NativeVideoCodec) -> UniquePtr<NativeVideoCodec>;
        fn video_codec_fields(value: &NativeVideoCodec) -> FfiVideoBitrateCodec;
        fn video_codec_set_fields(
            value: Pin<&mut NativeVideoCodec>,
            fields: FfiVideoBitrateCodec,
        ) -> bool;
        type NativeVideoBitrateAllocation;
        fn video_allocation_new() -> UniquePtr<NativeVideoBitrateAllocation>;
        fn video_allocation_clone(
            value: &NativeVideoBitrateAllocation,
        ) -> UniquePtr<NativeVideoBitrateAllocation>;
        fn video_allocation_fields(
            value: &NativeVideoBitrateAllocation,
        ) -> FfiVideoBitrateAllocation;
        fn video_allocation_set_fields(
            value: Pin<&mut NativeVideoBitrateAllocation>,
            fields: FfiVideoBitrateAllocation,
        ) -> bool;
        fn video_allocation_set(
            value: Pin<&mut NativeVideoBitrateAllocation>,
            spatial: usize,
            temporal: usize,
            bits: u32,
        ) -> bool;
        fn video_allocation_sum(
            value: &NativeVideoBitrateAllocation,
            spatial: usize,
            temporal: usize,
            operation: u8,
        ) -> u32;
        fn video_allocation_temporal(
            value: &NativeVideoBitrateAllocation,
            spatial: usize,
        ) -> Vec<u32>;
        fn video_allocation_simulcast(
            value: &NativeVideoBitrateAllocation,
            spatial: usize,
        ) -> UniquePtr<NativeVideoBitrateAllocation>;
        fn video_allocation_equal(
            a: &NativeVideoBitrateAllocation,
            b: &NativeVideoBitrateAllocation,
        ) -> bool;
        fn video_allocation_string(value: &NativeVideoBitrateAllocation) -> String;
        type NativeVideoRateControl;
        fn video_rate_control_new() -> UniquePtr<NativeVideoRateControl>;
        fn video_rate_control_clone(
            value: &NativeVideoRateControl,
        ) -> UniquePtr<NativeVideoRateControl>;
        fn video_rate_control_fields(value: &NativeVideoRateControl) -> FfiOwnedVideoRateControl;
        fn video_rate_control_set_fields(
            value: Pin<&mut NativeVideoRateControl>,
            fields: FfiOwnedVideoRateControl,
        ) -> bool;
        fn video_rate_control_equal(a: &NativeVideoRateControl, b: &NativeVideoRateControl)
        -> bool;
        fn video_rate_control_construct(
            allocation: FfiVideoBitrateAllocation,
            fps: f64,
            has_bandwidth: bool,
            bandwidth: i64,
            kind: i8,
        ) -> UniquePtr<NativeVideoRateControl>;
        fn video_encoder_info_new() -> UniquePtr<NativeVideoEncoderInfo>;
        fn video_encoder_info_clone(
            value: &NativeVideoEncoderInfo,
        ) -> UniquePtr<NativeVideoEncoderInfo>;
        fn video_encoder_info_assign(
            target: Pin<&mut NativeVideoEncoderInfo>,
            source: &NativeVideoEncoderInfo,
        );
        fn video_encoder_info_fields(value: &NativeVideoEncoderInfo) -> FfiOwnedVideoEncoderInfo;
        fn video_encoder_info_set_fields(
            target: Pin<&mut NativeVideoEncoderInfo>,
            fields: FfiOwnedVideoEncoderInfo,
        ) -> bool;
        fn video_encoder_info_equal(a: &NativeVideoEncoderInfo, b: &NativeVideoEncoderInfo)
        -> bool;
        fn video_encoder_info_string(value: &NativeVideoEncoderInfo, valid: &mut bool) -> Vec<u8>;
        fn video_encoder_info_limits(
            value: &NativeVideoEncoderInfo,
            pixels: i32,
            valid: &mut bool,
            found: &mut bool,
        ) -> FfiVideoResolutionBitrateLimits;
        fn video_resolution_bitrate_limits_equal(
            a: FfiVideoResolutionBitrateLimits,
            b: FfiVideoResolutionBitrateLimits,
        ) -> bool;
        fn video_decoder_settings_new() -> UniquePtr<NativeVideoDecoderSettings>;
        fn video_decoder_settings_clone(
            source: &NativeVideoDecoderSettings,
        ) -> UniquePtr<NativeVideoDecoderSettings>;
        fn video_decoder_settings_assign(
            target: Pin<&mut NativeVideoDecoderSettings>,
            source: &NativeVideoDecoderSettings,
        );
        fn video_decoder_settings_fields(
            source: &NativeVideoDecoderSettings,
        ) -> FfiOwnedVideoDecoderSettings;
        fn video_decoder_settings_set_fields(
            target: Pin<&mut NativeVideoDecoderSettings>,
            fields: FfiOwnedVideoDecoderSettings,
        ) -> i32;
        fn video_decoder_info_new() -> UniquePtr<NativeVideoDecoderInfo>;
        fn video_decoder_info_clone(
            source: &NativeVideoDecoderInfo,
        ) -> UniquePtr<NativeVideoDecoderInfo>;
        fn video_decoder_info_assign(
            target: Pin<&mut NativeVideoDecoderInfo>,
            source: &NativeVideoDecoderInfo,
        );
        fn video_decoder_info_fields(source: &NativeVideoDecoderInfo) -> FfiOwnedVideoDecoderInfo;
        fn video_decoder_info_set_fields(
            target: Pin<&mut NativeVideoDecoderInfo>,
            fields: FfiOwnedVideoDecoderInfo,
        );
        fn video_decoder_info_equal(
            left: &NativeVideoDecoderInfo,
            right: &NativeVideoDecoderInfo,
        ) -> bool;
        fn video_decoder_info_string(value: &NativeVideoDecoderInfo) -> Vec<u8>;
        type NativeSdpVideoFormat;
        type NativeSdpVideoFormatList;
        include!("pulsebeam-webrtc-sys/native/video_encoder_selector.h");
        type NativeOwnedVideoEncoderSelector;
        type NativeVideoEncoderSelector;
        fn video_selector_owned_observer(
            value: &NativeOwnedVideoEncoderSelector,
        ) -> Box<RustVideoEncoderSelectorObserver>;
        fn video_encoder_factory_selector(
            factory: &NativeVideoEncoderFactory,
            valid: &mut bool,
        ) -> UniquePtr<NativeOwnedVideoEncoderSelector>;
        fn video_selector_owned_new(
            provider: Box<RustVideoEncoderSelector>,
        ) -> UniquePtr<NativeOwnedVideoEncoderSelector>;
        fn video_selector_into_shared(
            owner: UniquePtr<NativeOwnedVideoEncoderSelector>,
        ) -> UniquePtr<NativeVideoEncoderSelector>;
        fn video_selector_owned_current(
            value: &NativeOwnedVideoEncoderSelector,
            format: &NativeSdpVideoFormat,
        );
        fn video_selector_owned_bitrate(
            value: &NativeOwnedVideoEncoderSelector,
            bits: i64,
            kind: i8,
            status: &mut i32,
        ) -> UniquePtr<NativeSdpVideoFormat>;
        fn video_selector_owned_resolution(
            value: &NativeOwnedVideoEncoderSelector,
            resolution: &NativeRenderResolution,
        ) -> UniquePtr<NativeSdpVideoFormat>;
        fn video_selector_owned_broken(
            value: &NativeOwnedVideoEncoderSelector,
        ) -> UniquePtr<NativeSdpVideoFormat>;
        fn video_selector_clone(
            value: &NativeVideoEncoderSelector,
        ) -> UniquePtr<NativeVideoEncoderSelector>;
        fn video_selector_same(
            a: &NativeVideoEncoderSelector,
            b: &NativeVideoEncoderSelector,
        ) -> bool;
        fn video_selector_current(
            value: &NativeVideoEncoderSelector,
            format: &NativeSdpVideoFormat,
        );
        fn video_selector_bitrate(
            value: &NativeVideoEncoderSelector,
            bits: i64,
            kind: i8,
            status: &mut i32,
        ) -> UniquePtr<NativeSdpVideoFormat>;
        fn video_selector_resolution(
            value: &NativeVideoEncoderSelector,
            resolution: &NativeRenderResolution,
        ) -> UniquePtr<NativeSdpVideoFormat>;
        fn video_selector_broken(
            value: &NativeVideoEncoderSelector,
        ) -> UniquePtr<NativeSdpVideoFormat>;
        fn string_builder_append_sdp_video_format(
            target: Pin<&mut NativeStringBuilder>,
            value: &NativeSdpVideoFormat,
        ) -> bool;
        fn rtc_error_builder_append_sdp_video_format(
            target: Pin<&mut NativeRtcError>,
            value: &NativeSdpVideoFormat,
        ) -> bool;
        fn rtc_error_stream_sdp_video_format(
            target: UniquePtr<NativeRtcError>,
            value: &NativeSdpVideoFormat,
            valid: &mut bool,
        ) -> UniquePtr<NativeRtcError>;
        fn sdp_video_format_new(name: &[u8]) -> UniquePtr<NativeSdpVideoFormat>;
        fn sdp_video_format_with_parameters(
            name: &[u8],
            parameters: &[FfiRtpCodecParameter],
            modes: &[u8],
            with_modes: bool,
        ) -> UniquePtr<NativeSdpVideoFormat>;
        fn sdp_video_format_with_modes(
            source: &NativeSdpVideoFormat,
            modes: &[u8],
        ) -> UniquePtr<NativeSdpVideoFormat>;
        fn sdp_video_format_clone(source: &NativeSdpVideoFormat)
        -> UniquePtr<NativeSdpVideoFormat>;
        fn sdp_video_format_move(
            source: UniquePtr<NativeSdpVideoFormat>,
        ) -> UniquePtr<NativeSdpVideoFormat>;
        fn sdp_video_format_assign(
            target: Pin<&mut NativeSdpVideoFormat>,
            source: &NativeSdpVideoFormat,
        );
        fn sdp_video_format_move_assign(
            target: Pin<&mut NativeSdpVideoFormat>,
            source: UniquePtr<NativeSdpVideoFormat>,
        );
        fn sdp_video_format_fields(value: &NativeSdpVideoFormat) -> FfiSdpVideoFormatFields;
        fn sdp_video_format_set_fields(
            value: Pin<&mut NativeSdpVideoFormat>,
            fields: FfiSdpVideoFormatFields,
        );
        fn sdp_video_format_equal(
            left: &NativeSdpVideoFormat,
            right: &NativeSdpVideoFormat,
        ) -> bool;
        fn sdp_video_format_same_codec(
            left: &NativeSdpVideoFormat,
            right: &NativeSdpVideoFormat,
        ) -> bool;
        fn sdp_video_format_string(value: &NativeSdpVideoFormat, valid: &mut bool) -> Vec<u8>;
        fn sdp_video_format_named(kind: u8) -> UniquePtr<NativeSdpVideoFormat>;
        fn sdp_video_format_list_valid(list: &NativeSdpVideoFormatList) -> bool;
        fn sdp_video_format_list_set_valid(list: Pin<&mut NativeSdpVideoFormatList>, valid: bool);
        fn sdp_video_format_list_new() -> UniquePtr<NativeSdpVideoFormatList>;
        fn sdp_video_format_list_len(list: &NativeSdpVideoFormatList) -> usize;
        fn sdp_video_format_list_get(
            list: &NativeSdpVideoFormatList,
            index: usize,
        ) -> UniquePtr<NativeSdpVideoFormat>;
        fn video_encoder_formats_value(
            factory: &NativeVideoEncoderFactory,
        ) -> UniquePtr<NativeSdpVideoFormatList>;
        fn video_encoder_implementations_value(
            factory: &NativeVideoEncoderFactory,
        ) -> UniquePtr<NativeSdpVideoFormatList>;
        fn video_decoder_formats_value(
            factory: &NativeVideoDecoderFactory,
        ) -> UniquePtr<NativeSdpVideoFormatList>;
        fn sdp_video_format_list_add(
            list: Pin<&mut NativeSdpVideoFormatList>,
            value: &NativeSdpVideoFormat,
        );
        fn sdp_video_format_in_list(
            value: &NativeSdpVideoFormat,
            list: &NativeSdpVideoFormatList,
        ) -> bool;
        fn sdp_video_format_fuzzy_match(
            list: &NativeSdpVideoFormatList,
            request: &NativeSdpVideoFormat,
            status: &mut i32,
        ) -> UniquePtr<NativeSdpVideoFormat>;
        type NativeRenderResolution;
        fn render_resolution_default() -> UniquePtr<NativeRenderResolution>;
        fn render_resolution_new(width: i32, height: i32) -> UniquePtr<NativeRenderResolution>;
        fn render_resolution_clone(
            value: &NativeRenderResolution,
        ) -> UniquePtr<NativeRenderResolution>;
        fn render_resolution_assign(
            target: Pin<&mut NativeRenderResolution>,
            source: &NativeRenderResolution,
        );
        fn render_resolution_width(value: &NativeRenderResolution) -> i32;
        fn render_resolution_height(value: &NativeRenderResolution) -> i32;
        fn render_resolution_valid(value: &NativeRenderResolution) -> bool;
        fn render_resolution_equal(
            left: &NativeRenderResolution,
            right: &NativeRenderResolution,
        ) -> bool;
        include!("pulsebeam-webrtc-sys/native/frame_transformer.h");
        include!("pulsebeam-webrtc-sys/native/live_frame_transformer.h");
        type NativeTransformableFrame;
        fn transformable_frame_is_audio(value: &NativeTransformableFrame) -> bool;
        fn transformable_frame_data(value: &NativeTransformableFrame) -> &[u8];
        fn transformable_frame_set_data(value: Pin<&mut NativeTransformableFrame>, data: &[u8]);
        fn transformable_frame_payload_type(value: &NativeTransformableFrame) -> u8;
        fn transformable_frame_can_set_payload_type(value: &NativeTransformableFrame) -> bool;
        fn transformable_frame_set_payload_type(
            value: Pin<&mut NativeTransformableFrame>,
            payload: u8,
        ) -> bool;
        fn transformable_frame_ssrc(value: &NativeTransformableFrame) -> u32;
        fn transformable_frame_timestamp(value: &NativeTransformableFrame) -> u32;
        fn transformable_frame_set_timestamp(
            value: Pin<&mut NativeTransformableFrame>,
            timestamp: u32,
        );
        fn transformable_frame_direction(value: &NativeTransformableFrame) -> i32;
        fn transformable_frame_mime(value: &NativeTransformableFrame, status: &mut i32) -> Vec<u8>;
        fn transformable_frame_time(
            value: &NativeTransformableFrame,
            query: u8,
        ) -> FfiTransformableFrameTime;
        fn transformable_frame_can_set_capture_time(value: &NativeTransformableFrame) -> bool;
        fn transformable_frame_set_capture_time(
            value: Pin<&mut NativeTransformableFrame>,
            present: bool,
            bits: i64,
        ) -> i32;
        fn transformable_frame_clone(
            value: Pin<&mut NativeTransformableFrame>,
            status: &mut i32,
        ) -> UniquePtr<NativeTransformableFrame>;
        fn transformable_audio_fields(
            value: &NativeTransformableFrame,
        ) -> FfiTransformableAudioFields;
        fn transformable_audio_csrcs(value: &NativeTransformableFrame) -> Vec<u32>;
        fn transformable_audio_can_set_level(value: &NativeTransformableFrame) -> bool;
        fn transformable_audio_set_level(
            value: Pin<&mut NativeTransformableFrame>,
            present: bool,
            level: u8,
        ) -> bool;
        fn transformable_video_is_key(value: &NativeTransformableFrame) -> bool;
        fn transformable_video_rid(value: &NativeTransformableFrame, present: &mut bool)
        -> Vec<u8>;
        fn transformable_video_metadata(
            value: &NativeTransformableFrame,
            status: &mut i32,
        ) -> UniquePtr<NativeVideoFrameMetadata>;
        fn transformable_video_header(
            value: &NativeTransformableFrame,
        ) -> UniquePtr<NativeRtpVideoHeader>;
        fn transformable_video_set_metadata(
            value: Pin<&mut NativeTransformableFrame>,
            metadata: &NativeVideoFrameMetadata,
        ) -> i32;
        type NativeVideoFrameMetadata;
        type NativeRtpVideoHeader;
        fn rtp_video_header_new() -> UniquePtr<NativeRtpVideoHeader>;
        fn rtp_video_header_clone(value: &NativeRtpVideoHeader) -> UniquePtr<NativeRtpVideoHeader>;
        fn rtp_video_header_assign(
            target: Pin<&mut NativeRtpVideoHeader>,
            source: &NativeRtpVideoHeader,
        );
        fn rtp_video_header_values(value: &NativeRtpVideoHeader) -> FfiRtpVideoHeaderValues;
        fn rtp_video_header_set_values(
            target: Pin<&mut NativeRtpVideoHeader>,
            value: FfiRtpVideoHeaderValues,
        ) -> i32;
        fn rtp_video_header_as_metadata(
            value: &NativeRtpVideoHeader,
            status: &mut i32,
        ) -> UniquePtr<NativeVideoFrameMetadata>;
        fn rtp_video_header_set_metadata(
            target: Pin<&mut NativeRtpVideoHeader>,
            value: &NativeVideoFrameMetadata,
        ) -> i32;
        fn rtp_video_header_from_metadata(
            value: &NativeVideoFrameMetadata,
            status: &mut i32,
        ) -> UniquePtr<NativeRtpVideoHeader>;
        fn video_frame_metadata_new() -> UniquePtr<NativeVideoFrameMetadata>;
        fn video_frame_metadata_specifics(
            source: &NativeVideoFrameMetadata,
        ) -> FfiVideoHeaderCodecSpecifics;
        fn video_frame_metadata_set_specifics(
            target: Pin<&mut NativeVideoFrameMetadata>,
            input: FfiVideoHeaderCodecSpecifics,
        ) -> i32;
        fn video_header_vp8_defaults() -> FfiVideoHeaderVp8;
        fn video_header_vp9_defaults() -> FfiVideoHeaderVp9;
        fn video_header_h264_defaults() -> FfiVideoHeaderH264;
        fn video_frame_metadata_equals(
            left: &NativeVideoFrameMetadata,
            right: &NativeVideoFrameMetadata,
        ) -> i32;
        fn video_header_vp8_init(value: &mut FfiVideoHeaderVp8);
        fn video_header_vp9_init(value: &mut FfiVideoHeaderVp9) -> i32;
        fn video_header_vp8_equals(left: FfiVideoHeaderVp8, right: FfiVideoHeaderVp8) -> i32;
        fn video_header_vp9_equals(left: FfiVideoHeaderVp9, right: FfiVideoHeaderVp9) -> i32;
        fn video_header_h264_equals(left: FfiVideoHeaderH264, right: FfiVideoHeaderH264) -> i32;
        fn vp9_gof_set_structure(value: &mut FfiVp9GofInfo, mode: i32) -> i32;
        fn vp9_gof_copy_prefix(target: &mut FfiVp9GofInfo, source: FfiVp9GofInfo) -> i32;
        fn vp9_gof_equals(left: FfiVp9GofInfo, right: FfiVp9GofInfo) -> i32;
        fn video_frame_metadata_clone(
            source: &NativeVideoFrameMetadata,
        ) -> UniquePtr<NativeVideoFrameMetadata>;
        fn video_frame_metadata_assign(
            target: Pin<&mut NativeVideoFrameMetadata>,
            source: &NativeVideoFrameMetadata,
        );
        fn video_frame_metadata_values(
            source: &NativeVideoFrameMetadata,
        ) -> FfiVideoFrameMetadataValues;
        fn video_frame_metadata_set_values(
            target: Pin<&mut NativeVideoFrameMetadata>,
            values: FfiVideoFrameMetadataValues,
        ) -> i32;
        type NativeFrameInstrumentationData;
        fn frame_instrumentation_new() -> UniquePtr<NativeFrameInstrumentationData>;
        fn frame_instrumentation_clone(
            source: &NativeFrameInstrumentationData,
        ) -> UniquePtr<NativeFrameInstrumentationData>;
        fn frame_instrumentation_assign(
            target: Pin<&mut NativeFrameInstrumentationData>,
            source: &NativeFrameInstrumentationData,
        );
        fn frame_instrumentation_samples(source: &NativeFrameInstrumentationData) -> Vec<f64>;
        fn frame_instrumentation_set_samples(
            target: Pin<&mut NativeFrameInstrumentationData>,
            samples: &[f64],
        ) -> bool;
        fn frame_instrumentation_set_samples_owned(
            target: Pin<&mut NativeFrameInstrumentationData>,
            samples: Vec<f64>,
        ) -> bool;
        fn sequence_index(self: &NativeFrameInstrumentationData) -> i32;
        #[cxx_name = "SetSequenceIndex"]
        fn set_sequence_index(self: Pin<&mut NativeFrameInstrumentationData>, index: i32) -> bool;
        fn is_droppable(self: &NativeFrameInstrumentationData) -> bool;
        fn set_droppable(self: Pin<&mut NativeFrameInstrumentationData>, droppable: bool);
        fn std_dev(self: &NativeFrameInstrumentationData) -> f64;
        #[cxx_name = "SetStdDev"]
        fn set_std_dev(self: Pin<&mut NativeFrameInstrumentationData>, value: f64) -> bool;
        fn luma_error_threshold(self: &NativeFrameInstrumentationData) -> i32;
        #[cxx_name = "SetLumaErrorThreshold"]
        fn set_luma_error_threshold(
            self: Pin<&mut NativeFrameInstrumentationData>,
            value: i32,
        ) -> bool;
        fn chroma_error_threshold(self: &NativeFrameInstrumentationData) -> i32;
        #[cxx_name = "SetChromaErrorThreshold"]
        fn set_chroma_error_threshold(
            self: Pin<&mut NativeFrameInstrumentationData>,
            value: i32,
        ) -> bool;
        fn holds_upper_bits(self: &NativeFrameInstrumentationData) -> bool;
        fn is_sync_only(self: &NativeFrameInstrumentationData) -> bool;

        fn string_builder_append_codec(
            output: Pin<&mut NativeStringBuilder>,
            input: &NativeRtpCodec,
        ) -> bool;
        fn rtc_error_builder_append_codec(
            output: Pin<&mut NativeRtcError>,
            input: &NativeRtpCodec,
        ) -> bool;
        fn rtc_error_stream_codec(
            error: UniquePtr<NativeRtcError>,
            input: &NativeRtpCodec,
            valid: &mut bool,
        ) -> UniquePtr<NativeRtcError>;
        fn string_builder_append_encoding(
            output: Pin<&mut NativeStringBuilder>,
            input: &NativeRtpEncoding,
        ) -> bool;
        fn rtc_error_builder_append_encoding(
            output: Pin<&mut NativeRtcError>,
            input: &NativeRtpEncoding,
        ) -> bool;
        fn rtc_error_stream_encoding(
            error: UniquePtr<NativeRtcError>,
            input: &NativeRtpEncoding,
            valid: &mut bool,
        ) -> UniquePtr<NativeRtcError>;
        fn string_builder_append_media_type(
            output: Pin<&mut NativeStringBuilder>,
            kind: i32,
        ) -> bool;
        fn rtc_error_builder_append_media_type(output: Pin<&mut NativeRtcError>, kind: i32)
        -> bool;
        fn rtc_error_stream_media_type(
            error: UniquePtr<NativeRtcError>,
            kind: i32,
            valid: &mut bool,
        ) -> UniquePtr<NativeRtcError>;
        fn string_builder_append_feedback_type(
            output: Pin<&mut NativeStringBuilder>,
            kind: i32,
        ) -> bool;
        fn rtc_error_builder_append_feedback_type(
            output: Pin<&mut NativeRtcError>,
            kind: i32,
        ) -> bool;
        fn rtc_error_stream_feedback_type(
            error: UniquePtr<NativeRtcError>,
            kind: i32,
            valid: &mut bool,
        ) -> UniquePtr<NativeRtcError>;
        fn string_builder_append_optional_feedback_type(
            output: Pin<&mut NativeStringBuilder>,
            present: bool,
            kind: i32,
        ) -> bool;
        fn rtc_error_builder_append_optional_feedback_type(
            output: Pin<&mut NativeRtcError>,
            present: bool,
            kind: i32,
        ) -> bool;
        fn rtc_error_stream_optional_feedback_type(
            error: UniquePtr<NativeRtcError>,
            present: bool,
            kind: i32,
            valid: &mut bool,
        ) -> UniquePtr<NativeRtcError>;

        type NativeRtpCodecParameters;
        fn rtp_codec_parameters_default() -> UniquePtr<NativeRtpCodecParameters>;
        fn rtp_codec_parameters_clone(
            source: &NativeRtpCodecParameters,
        ) -> UniquePtr<NativeRtpCodecParameters>;
        fn rtp_codec_parameters_assign(
            target: Pin<&mut NativeRtpCodecParameters>,
            source: &NativeRtpCodecParameters,
        );
        fn rtp_codec_parameters_equal(
            lhs: &NativeRtpCodecParameters,
            rhs: &NativeRtpCodecParameters,
        ) -> bool;
        type NativeRtcpParameters;
        fn rtcp_parameters_default() -> UniquePtr<NativeRtcpParameters>;
        fn rtcp_parameters_clone(source: &NativeRtcpParameters) -> UniquePtr<NativeRtcpParameters>;
        fn rtcp_parameters_assign(
            target: Pin<&mut NativeRtcpParameters>,
            source: &NativeRtcpParameters,
        );
        fn rtcp_parameters_equal(lhs: &NativeRtcpParameters, rhs: &NativeRtcpParameters) -> bool;
        fn rtp_codec_parameters_payload_type(value: &NativeRtpCodecParameters) -> i32;
        fn rtp_codec_parameters_set_payload_type(
            value: Pin<&mut NativeRtpCodecParameters>,
            payload: i32,
        );
        fn rtp_codec_parameters_base(value: &NativeRtpCodecParameters)
        -> UniquePtr<NativeRtpCodec>;
        fn rtp_codec_parameters_set_base(
            value: Pin<&mut NativeRtpCodecParameters>,
            codec: &NativeRtpCodec,
        );
        fn rtp_codec_parameters_mime_type(
            value: &NativeRtpCodecParameters,
            valid: &mut bool,
        ) -> Vec<u8>;
        fn rtp_codec_parameters_string(
            value: &NativeRtpCodecParameters,
            valid: &mut bool,
        ) -> Vec<u8>;
        fn rtp_codec_parameters_is_resiliency(value: &NativeRtpCodecParameters) -> bool;
        fn rtp_codec_parameters_is_media(value: &NativeRtpCodecParameters) -> bool;
        fn rtcp_parameters_fields(value: &NativeRtcpParameters) -> FfiRtcpParametersFields;
        fn rtcp_parameters_set_ssrc(
            value: Pin<&mut NativeRtcpParameters>,
            present: bool,
            ssrc: u32,
        );
        fn rtcp_parameters_set_cname(value: Pin<&mut NativeRtcpParameters>, cname: &[u8]);
        fn rtcp_parameters_set_reduced_size(value: Pin<&mut NativeRtcpParameters>, reduced: bool);
        fn rtcp_parameters_set_mux(value: Pin<&mut NativeRtcpParameters>, mux: bool);
        fn rtcp_parameters_string(value: &NativeRtcpParameters) -> Vec<u8>;

        fn string_builder_append_codec_parameters(
            output: Pin<&mut NativeStringBuilder>,
            input: &NativeRtpCodecParameters,
        ) -> bool;
        fn rtc_error_builder_append_codec_parameters(
            output: Pin<&mut NativeRtcError>,
            input: &NativeRtpCodecParameters,
        ) -> bool;
        fn rtc_error_stream_codec_parameters(
            error: UniquePtr<NativeRtcError>,
            input: &NativeRtpCodecParameters,
            valid: &mut bool,
        ) -> UniquePtr<NativeRtcError>;
        fn string_builder_append_rtcp_parameters(
            output: Pin<&mut NativeStringBuilder>,
            input: &NativeRtcpParameters,
        ) -> bool;
        fn rtc_error_builder_append_rtcp_parameters(
            output: Pin<&mut NativeRtcError>,
            input: &NativeRtcpParameters,
        ) -> bool;
        fn rtc_error_stream_rtcp_parameters(
            error: UniquePtr<NativeRtcError>,
            input: &NativeRtcpParameters,
            valid: &mut bool,
        ) -> UniquePtr<NativeRtcError>;

        type NativeRtpHeaderExtensionId;
        fn rtp_header_id_default() -> UniquePtr<NativeRtpHeaderExtensionId>;
        fn rtp_header_id_not_set() -> UniquePtr<NativeRtpHeaderExtensionId>;
        fn rtp_header_id_new(value: i32) -> UniquePtr<NativeRtpHeaderExtensionId>;
        fn rtp_header_id_constant(index: u32) -> UniquePtr<NativeRtpHeaderExtensionId>;
        fn rtp_header_id_clone(
            source: &NativeRtpHeaderExtensionId,
        ) -> UniquePtr<NativeRtpHeaderExtensionId>;
        fn rtp_header_id_assign(
            target: Pin<&mut NativeRtpHeaderExtensionId>,
            source: &NativeRtpHeaderExtensionId,
        );
        fn rtp_header_id_to_int(value: &NativeRtpHeaderExtensionId) -> i32;
        fn rtp_header_id_valid(value: &NativeRtpHeaderExtensionId) -> bool;
        fn rtp_header_id_is_set(value: &NativeRtpHeaderExtensionId) -> bool;
        fn rtp_header_id_string(value: &NativeRtpHeaderExtensionId) -> Vec<u8>;
        fn rtp_header_id_hash(value: &NativeRtpHeaderExtensionId) -> usize;
        fn rtp_header_id_ref(value: &NativeRtpHeaderExtensionId) -> &i32;
        fn rtp_header_id_ref_mut(value: Pin<&mut NativeRtpHeaderExtensionId>) -> &mut i32;
        fn rtp_header_id_deref(value: &NativeRtpHeaderExtensionId) -> &i32;
        fn rtp_header_id_deref_mut(value: Pin<&mut NativeRtpHeaderExtensionId>) -> &mut i32;
        fn rtp_header_id_arrow(value: &NativeRtpHeaderExtensionId) -> &i32;
        fn rtp_header_id_arrow_mut(value: Pin<&mut NativeRtpHeaderExtensionId>) -> &mut i32;
        fn rtp_header_id_conversion(value: &NativeRtpHeaderExtensionId) -> &i32;
        fn rtp_header_id_into_value(value: UniquePtr<NativeRtpHeaderExtensionId>) -> i32;
        fn rtp_header_id_into_const_value(value: UniquePtr<NativeRtpHeaderExtensionId>) -> i32;
        fn rtp_header_id_into_deref(value: UniquePtr<NativeRtpHeaderExtensionId>) -> i32;
        fn rtp_header_id_into_const_deref(value: UniquePtr<NativeRtpHeaderExtensionId>) -> i32;
        fn rtp_header_id_equal(
            lhs: &NativeRtpHeaderExtensionId,
            rhs: &NativeRtpHeaderExtensionId,
        ) -> bool;
        fn rtp_header_id_not_equal(
            lhs: &NativeRtpHeaderExtensionId,
            rhs: &NativeRtpHeaderExtensionId,
        ) -> bool;
        fn rtp_header_id_less(
            lhs: &NativeRtpHeaderExtensionId,
            rhs: &NativeRtpHeaderExtensionId,
        ) -> bool;
        fn rtp_header_id_less_equal(
            lhs: &NativeRtpHeaderExtensionId,
            rhs: &NativeRtpHeaderExtensionId,
        ) -> bool;
        fn rtp_header_id_greater(
            lhs: &NativeRtpHeaderExtensionId,
            rhs: &NativeRtpHeaderExtensionId,
        ) -> bool;
        fn rtp_header_id_greater_equal(
            lhs: &NativeRtpHeaderExtensionId,
            rhs: &NativeRtpHeaderExtensionId,
        ) -> bool;

        type NativeRtpExtension;
        type NativeRtpExtensionList;
        type NativeRtpExtensionBorrowed;
        fn rtp_extension_default() -> UniquePtr<NativeRtpExtension>;
        fn rtp_extension_new(
            uri: &[u8],
            id: &NativeRtpHeaderExtensionId,
            explicit_encrypt: bool,
            encrypt: bool,
        ) -> UniquePtr<NativeRtpExtension>;
        fn rtp_extension_new_int(
            uri: &[u8],
            id: i32,
            explicit_encrypt: bool,
            encrypt: bool,
        ) -> UniquePtr<NativeRtpExtension>;
        fn rtp_extension_clone(value: &NativeRtpExtension) -> UniquePtr<NativeRtpExtension>;
        fn rtp_extension_assign(target: Pin<&mut NativeRtpExtension>, source: &NativeRtpExtension);
        fn rtp_extension_equal(lhs: &NativeRtpExtension, rhs: &NativeRtpExtension) -> bool;
        fn rtp_extension_fields(value: &NativeRtpExtension) -> FfiRtpExtensionFields;
        fn rtp_extension_id(value: &NativeRtpExtension) -> UniquePtr<NativeRtpHeaderExtensionId>;
        fn rtp_extension_set_uri(value: Pin<&mut NativeRtpExtension>, uri: &[u8]);
        fn rtp_extension_set_id(
            value: Pin<&mut NativeRtpExtension>,
            id: &NativeRtpHeaderExtensionId,
        );
        fn rtp_extension_set_encrypt(value: Pin<&mut NativeRtpExtension>, encrypt: bool);
        fn rtp_extension_to_string(value: &NativeRtpExtension) -> Vec<u8>;
        fn rtp_extension_string(value: &NativeRtpExtension) -> Vec<u8>;
        fn rtp_extension_supported_audio(uri: &[u8]) -> bool;
        fn rtp_extension_supported_video(uri: &[u8]) -> bool;
        fn rtp_extension_encryption_supported(uri: &[u8]) -> bool;
        fn rtp_extension_list_new() -> UniquePtr<NativeRtpExtensionList>;
        fn rtp_extension_list_clone(
            values: &NativeRtpExtensionList,
        ) -> UniquePtr<NativeRtpExtensionList>;
        fn rtp_extension_list_assign(
            target: Pin<&mut NativeRtpExtensionList>,
            source: &NativeRtpExtensionList,
        );
        fn rtp_extension_list_len(values: &NativeRtpExtensionList) -> usize;
        fn rtp_extension_list_add(
            values: Pin<&mut NativeRtpExtensionList>,
            value: &NativeRtpExtension,
        );
        fn rtp_extension_list_clear(values: Pin<&mut NativeRtpExtensionList>);
        fn rtp_extension_list_get(
            values: &NativeRtpExtensionList,
            index: usize,
        ) -> UniquePtr<NativeRtpExtension>;
        fn rtp_extension_list_find(
            values: &NativeRtpExtensionList,
            uri: &[u8],
            filter: i32,
            valid: &mut bool,
        ) -> UniquePtr<NativeRtpExtensionBorrowed>;
        fn rtp_extension_list_find_encryption(
            values: &NativeRtpExtensionList,
            uri: &[u8],
            encrypt: bool,
        ) -> UniquePtr<NativeRtpExtensionBorrowed>;
        fn rtp_extension_list_deduplicate(
            values: &NativeRtpExtensionList,
            filter: i32,
        ) -> UniquePtr<NativeRtpExtensionList>;
        fn rtp_extension_borrowed_clone(
            value: &NativeRtpExtensionBorrowed,
        ) -> UniquePtr<NativeRtpExtension>;
        fn rtp_extension_borrowed_fields(
            value: &NativeRtpExtensionBorrowed,
        ) -> FfiRtpExtensionFields;
        fn rtp_extension_borrowed_id(
            value: &NativeRtpExtensionBorrowed,
        ) -> UniquePtr<NativeRtpHeaderExtensionId>;
        fn rtp_extension_borrowed_to_string(value: &NativeRtpExtensionBorrowed) -> Vec<u8>;
        fn rtp_extension_borrowed_string(value: &NativeRtpExtensionBorrowed) -> Vec<u8>;

        fn rtp_extension_encrypt_header_extensions_uri() -> Vec<u8>;
        fn rtp_extension_audio_level_uri() -> Vec<u8>;
        fn rtp_extension_timestamp_offset_uri() -> Vec<u8>;
        fn rtp_extension_abs_send_time_uri() -> Vec<u8>;
        fn rtp_extension_absolute_capture_time_uri() -> Vec<u8>;
        fn rtp_extension_video_rotation_uri() -> Vec<u8>;
        fn rtp_extension_video_content_type_uri() -> Vec<u8>;
        fn rtp_extension_video_timing_uri() -> Vec<u8>;
        fn rtp_extension_generic_frame_descriptor_uri_00() -> Vec<u8>;
        fn rtp_extension_dependency_descriptor_uri() -> Vec<u8>;
        fn rtp_extension_video_layers_allocation_uri() -> Vec<u8>;
        fn rtp_extension_transport_sequence_number_uri() -> Vec<u8>;
        fn rtp_extension_transport_sequence_number_v2_uri() -> Vec<u8>;
        fn rtp_extension_playout_delay_uri() -> Vec<u8>;
        fn rtp_extension_color_space_uri() -> Vec<u8>;
        fn rtp_extension_mid_uri() -> Vec<u8>;
        fn rtp_extension_rid_uri() -> Vec<u8>;
        fn rtp_extension_repaired_rid_uri() -> Vec<u8>;
        fn rtp_extension_video_frame_tracking_id_uri() -> Vec<u8>;
        fn rtp_extension_csrc_audio_levels_uri() -> Vec<u8>;
        fn rtp_extension_corruption_detection_uri() -> Vec<u8>;
        fn rtp_extension_min_id() -> UniquePtr<NativeRtpHeaderExtensionId>;
        fn rtp_extension_max_id() -> UniquePtr<NativeRtpHeaderExtensionId>;
        fn rtp_extension_one_byte_max_id() -> UniquePtr<NativeRtpHeaderExtensionId>;
        fn rtp_extension_max_value_size() -> i32;
        fn rtp_extension_one_byte_max_value_size() -> i32;

        fn string_builder_append_header_extension_id(
            output: Pin<&mut NativeStringBuilder>,
            input: &NativeRtpHeaderExtensionId,
        );
        fn rtc_error_builder_append_header_extension_id(
            output: Pin<&mut NativeRtcError>,
            input: &NativeRtpHeaderExtensionId,
        );
        fn rtc_error_stream_header_extension_id(
            error: UniquePtr<NativeRtcError>,
            input: &NativeRtpHeaderExtensionId,
        ) -> UniquePtr<NativeRtcError>;
        fn string_builder_append_rtp_extension(
            output: Pin<&mut NativeStringBuilder>,
            input: &NativeRtpExtension,
        );
        fn rtc_error_builder_append_rtp_extension(
            output: Pin<&mut NativeRtcError>,
            input: &NativeRtpExtension,
        );
        fn rtc_error_stream_rtp_extension(
            error: UniquePtr<NativeRtcError>,
            input: &NativeRtpExtension,
        ) -> UniquePtr<NativeRtcError>;
        fn string_builder_append_rtp_extension_ref(
            output: Pin<&mut NativeStringBuilder>,
            input: &NativeRtpExtensionBorrowed,
        );
        fn rtc_error_builder_append_rtp_extension_ref(
            output: Pin<&mut NativeRtcError>,
            input: &NativeRtpExtensionBorrowed,
        );
        fn rtc_error_stream_rtp_extension_ref(
            error: UniquePtr<NativeRtcError>,
            input: &NativeRtpExtensionBorrowed,
        ) -> UniquePtr<NativeRtcError>;

        type NativeRtpCodecCapability;
        fn rtp_codec_capability_default() -> UniquePtr<NativeRtpCodecCapability>;
        fn rtp_codec_capability_clone(
            value: &NativeRtpCodecCapability,
        ) -> UniquePtr<NativeRtpCodecCapability>;
        fn rtp_codec_capability_assign(
            target: Pin<&mut NativeRtpCodecCapability>,
            source: &NativeRtpCodecCapability,
        );
        fn rtp_codec_capability_equal(
            lhs: &NativeRtpCodecCapability,
            rhs: &NativeRtpCodecCapability,
        ) -> bool;
        type NativeRtpHeaderExtensionCapability;
        fn rtp_header_capability_default() -> UniquePtr<NativeRtpHeaderExtensionCapability>;
        fn rtp_header_capability_clone(
            value: &NativeRtpHeaderExtensionCapability,
        ) -> UniquePtr<NativeRtpHeaderExtensionCapability>;
        fn rtp_header_capability_assign(
            target: Pin<&mut NativeRtpHeaderExtensionCapability>,
            source: &NativeRtpHeaderExtensionCapability,
        );
        fn rtp_header_capability_equal(
            lhs: &NativeRtpHeaderExtensionCapability,
            rhs: &NativeRtpHeaderExtensionCapability,
        ) -> bool;
        type NativeRtpCapabilities;
        fn rtp_capabilities_default() -> UniquePtr<NativeRtpCapabilities>;
        fn rtp_capabilities_clone(
            value: &NativeRtpCapabilities,
        ) -> UniquePtr<NativeRtpCapabilities>;
        fn rtp_capabilities_assign(
            target: Pin<&mut NativeRtpCapabilities>,
            source: &NativeRtpCapabilities,
        );
        fn rtp_capabilities_equal(lhs: &NativeRtpCapabilities, rhs: &NativeRtpCapabilities)
        -> bool;
        fn rtp_codec_capability_fields(
            value: &NativeRtpCodecCapability,
        ) -> FfiRtpCodecCapabilityFields;
        fn rtp_codec_capability_set_payload_type(
            value: Pin<&mut NativeRtpCodecCapability>,
            present: bool,
            input: i32,
        );
        fn rtp_codec_capability_base(value: &NativeRtpCodecCapability)
        -> UniquePtr<NativeRtpCodec>;
        fn rtp_codec_capability_set_base(
            value: Pin<&mut NativeRtpCodecCapability>,
            codec: &NativeRtpCodec,
        );
        fn rtp_codec_capability_modes(value: &NativeRtpCodecCapability) -> Vec<u8>;
        fn rtp_codec_capability_set_modes(value: Pin<&mut NativeRtpCodecCapability>, modes: &[u8]);
        fn rtp_codec_capability_string(value: &NativeRtpCodecCapability) -> Vec<u8>;
        fn rtp_codec_capability_mime_type(
            value: &NativeRtpCodecCapability,
            valid: &mut bool,
        ) -> Vec<u8>;
        fn rtp_codec_capability_is_resiliency(value: &NativeRtpCodecCapability) -> bool;
        fn rtp_codec_capability_is_media(value: &NativeRtpCodecCapability) -> bool;
        fn rtp_header_capability_new(
            uri: &[u8],
            id: &NativeRtpHeaderExtensionId,
            encrypt: bool,
            direction: i32,
            constructor: u8,
        ) -> UniquePtr<NativeRtpHeaderExtensionCapability>;
        fn rtp_header_capability_fields(
            value: &NativeRtpHeaderExtensionCapability,
        ) -> FfiRtpHeaderCapabilityFields;
        fn rtp_header_capability_id(
            value: &NativeRtpHeaderExtensionCapability,
        ) -> UniquePtr<NativeRtpHeaderExtensionId>;
        fn rtp_header_capability_set_id(
            value: Pin<&mut NativeRtpHeaderExtensionCapability>,
            id: &NativeRtpHeaderExtensionId,
        );
        fn rtp_header_capability_clear_id(value: Pin<&mut NativeRtpHeaderExtensionCapability>);
        fn rtp_header_capability_set_uri(
            value: Pin<&mut NativeRtpHeaderExtensionCapability>,
            uri: &[u8],
        );
        fn rtp_header_capability_set_encrypt(
            value: Pin<&mut NativeRtpHeaderExtensionCapability>,
            encrypt: bool,
        );
        fn rtp_header_capability_set_direction(
            value: Pin<&mut NativeRtpHeaderExtensionCapability>,
            direction: i32,
        );
        fn rtp_capabilities_fec(value: &NativeRtpCapabilities) -> Vec<i32>;
        fn rtp_capabilities_set_fec(value: Pin<&mut NativeRtpCapabilities>, fec: &[i32]);
        fn factory_rtp_capabilities_value(
            factory: &NativePeerConnectionFactory,
            kind: i32,
            sender: bool,
        ) -> UniquePtr<NativeRtpCapabilities>;
        fn rtp_direction_string(direction: i32) -> Vec<u8>;
        fn string_builder_append_rtp_direction(
            output: Pin<&mut NativeStringBuilder>,
            direction: i32,
        );
        fn rtc_error_builder_append_rtp_direction(output: Pin<&mut NativeRtcError>, direction: i32);
        fn rtc_error_stream_rtp_direction(
            error: UniquePtr<NativeRtcError>,
            direction: i32,
        ) -> UniquePtr<NativeRtcError>;
        fn rtp_scalability_modes_all() -> Vec<u8>;
        fn rtp_scalability_mode_string(present: bool, mode: u8, valid: &mut bool) -> Vec<u8>;
        fn string_builder_append_rtp_codec_capability(
            output: Pin<&mut NativeStringBuilder>,
            input: &NativeRtpCodecCapability,
        );
        fn rtc_error_builder_append_rtp_codec_capability(
            output: Pin<&mut NativeRtcError>,
            input: &NativeRtpCodecCapability,
        );
        fn rtc_error_stream_rtp_codec_capability(
            error: UniquePtr<NativeRtcError>,
            input: &NativeRtpCodecCapability,
        ) -> UniquePtr<NativeRtcError>;
        fn rtp_capabilities_codecs_len(value: &NativeRtpCapabilities) -> usize;
        fn rtp_capabilities_codecs_get(
            value: &NativeRtpCapabilities,
            index: usize,
        ) -> UniquePtr<NativeRtpCodecCapability>;
        fn rtp_capabilities_codecs_clear(value: Pin<&mut NativeRtpCapabilities>);
        fn rtp_capabilities_codecs_add(
            value: Pin<&mut NativeRtpCapabilities>,
            element: &NativeRtpCodecCapability,
        );
        fn rtp_capabilities_header_extensions_len(value: &NativeRtpCapabilities) -> usize;
        fn rtp_capabilities_header_extensions_get(
            value: &NativeRtpCapabilities,
            index: usize,
        ) -> UniquePtr<NativeRtpHeaderExtensionCapability>;
        fn rtp_capabilities_header_extensions_clear(value: Pin<&mut NativeRtpCapabilities>);
        fn rtp_capabilities_header_extensions_add(
            value: Pin<&mut NativeRtpCapabilities>,
            element: &NativeRtpHeaderExtensionCapability,
        );

        type NativeRtpTransceiverInit;
        fn rtp_transceiver_init_default() -> UniquePtr<NativeRtpTransceiverInit>;
        fn rtp_transceiver_init_clone(
            source: &NativeRtpTransceiverInit,
        ) -> UniquePtr<NativeRtpTransceiverInit>;
        fn rtp_transceiver_init_assign(
            target: Pin<&mut NativeRtpTransceiverInit>,
            source: &NativeRtpTransceiverInit,
        );
        fn rtp_transceiver_init_direction(value: &NativeRtpTransceiverInit) -> i32;
        fn rtp_transceiver_init_set_direction(
            value: Pin<&mut NativeRtpTransceiverInit>,
            direction: i32,
        );
        fn rtp_transceiver_init_stream_ids_len(value: &NativeRtpTransceiverInit) -> usize;
        fn rtp_transceiver_init_stream_id_get(
            value: &NativeRtpTransceiverInit,
            index: usize,
        ) -> Vec<u8>;
        fn rtp_transceiver_init_stream_ids_clear(value: Pin<&mut NativeRtpTransceiverInit>);
        fn rtp_transceiver_init_stream_id_add(value: Pin<&mut NativeRtpTransceiverInit>, id: &[u8]);
        fn rtp_transceiver_init_encodings_len(value: &NativeRtpTransceiverInit) -> usize;
        fn rtp_transceiver_init_encoding_get(
            value: &NativeRtpTransceiverInit,
            index: usize,
        ) -> UniquePtr<NativeRtpEncoding>;
        fn rtp_transceiver_init_encodings_clear(value: Pin<&mut NativeRtpTransceiverInit>);
        fn rtp_transceiver_init_encoding_add(
            value: Pin<&mut NativeRtpTransceiverInit>,
            encoding: &NativeRtpEncoding,
        );

        type NativeRtpSource;
        type NativeRtpSourceList;
        fn rtp_source_new(fields: &FfiRtpSourceFields) -> UniquePtr<NativeRtpSource>;
        fn rtp_source_clone(value: &NativeRtpSource) -> UniquePtr<NativeRtpSource>;
        fn rtp_source_assign(target: Pin<&mut NativeRtpSource>, source: &NativeRtpSource);
        fn rtp_source_equal(left: &NativeRtpSource, right: &NativeRtpSource) -> bool;
        fn rtp_source_fields(value: &NativeRtpSource) -> FfiRtpSourceFields;
        fn rtp_source_set_audio_level(value: Pin<&mut NativeRtpSource>, present: bool, level: u8);
        fn rtp_source_string(value: &NativeRtpSource) -> Vec<u8>;
        fn rtp_receiver_sources(receiver: &NativeRtpReceiver) -> UniquePtr<NativeRtpSourceList>;
        fn rtp_source_list_len(value: &NativeRtpSourceList) -> usize;
        fn rtp_source_list_get(
            value: &NativeRtpSourceList,
            index: usize,
        ) -> UniquePtr<NativeRtpSource>;
        fn string_builder_append_rtp_source(
            output: Pin<&mut NativeStringBuilder>,
            value: &NativeRtpSource,
        );
        fn rtc_error_builder_append_rtp_source(
            output: Pin<&mut NativeRtcError>,
            value: &NativeRtpSource,
        );
        fn rtc_error_stream_rtp_source(
            error: UniquePtr<NativeRtcError>,
            value: &NativeRtpSource,
        ) -> UniquePtr<NativeRtcError>;

        type NativeRtpParameters;
        type NativeSetParametersOperation;
        fn rtp_sender_set_parameters_value(
            sender: &NativeRtpSender,
            parameters: &NativeRtpParameters,
            valid: &mut bool,
        ) -> UniquePtr<NativeRtcError>;
        fn rtp_sender_set_parameters_async(
            sender: &NativeRtpSender,
            parameters: &NativeRtpParameters,
            valid: &mut bool,
        ) -> SharedPtr<NativeSetParametersOperation>;
        fn set_parameters_operation_ready(operation: &NativeSetParametersOperation) -> bool;
        fn set_parameters_operation_take(
            operation: &NativeSetParametersOperation,
        ) -> UniquePtr<NativeRtcError>;
        fn rtp_sender_parameters_value(sender: &NativeRtpSender) -> UniquePtr<NativeRtpParameters>;
        fn rtp_receiver_parameters_value(
            receiver: &NativeRtpReceiver,
        ) -> UniquePtr<NativeRtpParameters>;
        fn rtp_receiver_set_parameters_value(
            receiver: &NativeRtpReceiver,
            parameters: &NativeRtpParameters,
        ) -> bool;
        fn rtp_receiver_set_jitter_buffer_minimum_delay(
            receiver: &NativeRtpReceiver,
            present: bool,
            seconds: f64,
        ) -> bool;
        fn rtp_parameters_default() -> UniquePtr<NativeRtpParameters>;
        fn rtp_parameters_clone(value: &NativeRtpParameters) -> UniquePtr<NativeRtpParameters>;
        fn rtp_parameters_assign(
            target: Pin<&mut NativeRtpParameters>,
            source: &NativeRtpParameters,
        );
        fn rtp_parameters_equal(lhs: &NativeRtpParameters, rhs: &NativeRtpParameters) -> bool;
        fn rtp_parameters_not_equal(lhs: &NativeRtpParameters, rhs: &NativeRtpParameters) -> bool;
        fn rtp_parameters_fields(value: &NativeRtpParameters) -> FfiRtpParametersFields;
        fn rtp_parameters_set_transaction_id(value: Pin<&mut NativeRtpParameters>, bytes: &[u8]);
        fn rtp_parameters_set_mid(value: Pin<&mut NativeRtpParameters>, bytes: &[u8]);
        fn rtp_parameters_set_degradation_preference(
            value: Pin<&mut NativeRtpParameters>,
            present: bool,
            raw: i32,
        );
        fn rtp_parameters_rtcp(value: &NativeRtpParameters) -> UniquePtr<NativeRtcpParameters>;
        fn rtp_parameters_set_rtcp(
            value: Pin<&mut NativeRtpParameters>,
            rtcp: &NativeRtcpParameters,
        );
        fn rtp_parameters_header_extensions(
            value: &NativeRtpParameters,
        ) -> UniquePtr<NativeRtpExtensionList>;
        fn rtp_parameters_set_header_extensions(
            value: Pin<&mut NativeRtpParameters>,
            extensions: &NativeRtpExtensionList,
        );
        fn rtp_parameters_is_mixed_codec(value: &NativeRtpParameters) -> bool;
        fn rtp_parameters_string(value: &NativeRtpParameters, valid: &mut bool) -> Vec<u8>;
        fn rtp_degradation_preference_string(raw: i32, valid: &mut bool) -> Vec<u8>;
        fn rtp_parameters_codecs_len(value: &NativeRtpParameters) -> usize;
        fn rtp_parameters_codecs_get(
            value: &NativeRtpParameters,
            index: usize,
        ) -> UniquePtr<NativeRtpCodecParameters>;
        fn rtp_parameters_codecs_clear(value: Pin<&mut NativeRtpParameters>);
        fn rtp_parameters_codecs_add(
            value: Pin<&mut NativeRtpParameters>,
            element: &NativeRtpCodecParameters,
        );
        fn rtp_parameters_encodings_len(value: &NativeRtpParameters) -> usize;
        fn rtp_parameters_encodings_get(
            value: &NativeRtpParameters,
            index: usize,
        ) -> UniquePtr<NativeRtpEncoding>;
        fn rtp_parameters_encodings_clear(value: Pin<&mut NativeRtpParameters>);
        fn rtp_parameters_encodings_add(
            value: Pin<&mut NativeRtpParameters>,
            element: &NativeRtpEncoding,
        );
        fn string_builder_append_rtp_parameters(
            output: Pin<&mut NativeStringBuilder>,
            input: &NativeRtpParameters,
        ) -> bool;
        fn rtc_error_builder_append_rtp_parameters(
            output: Pin<&mut NativeRtcError>,
            input: &NativeRtpParameters,
        ) -> bool;
        fn rtc_error_stream_rtp_parameters(
            error: UniquePtr<NativeRtcError>,
            input: &NativeRtpParameters,
            valid: &mut bool,
        ) -> UniquePtr<NativeRtcError>;

        type NativePriorityValue;
        fn priority_value_new(value: u16) -> UniquePtr<NativePriorityValue>;
        fn priority_value_from_priority(priority: i32) -> UniquePtr<NativePriorityValue>;
        fn priority_value_clone(source: &NativePriorityValue) -> UniquePtr<NativePriorityValue>;
        fn priority_value_assign(
            target: Pin<&mut NativePriorityValue>,
            source: &NativePriorityValue,
        );
        fn priority_value_ref(value: &NativePriorityValue) -> &u16;
        fn priority_value_mut(value: Pin<&mut NativePriorityValue>) -> &mut u16;
        fn priority_value_deref(value: &NativePriorityValue) -> &u16;
        fn priority_value_deref_mut(value: Pin<&mut NativePriorityValue>) -> &mut u16;
        fn priority_value_arrow(value: &NativePriorityValue) -> &u16;
        fn priority_value_arrow_mut(value: Pin<&mut NativePriorityValue>) -> &mut u16;
        fn priority_value_conversion(value: &NativePriorityValue) -> &u16;
        fn priority_value_into_value(value: UniquePtr<NativePriorityValue>) -> u16;
        fn priority_value_into_const_value(value: UniquePtr<NativePriorityValue>) -> u16;
        fn priority_value_into_deref(value: UniquePtr<NativePriorityValue>) -> u16;
        fn priority_value_into_const_deref(value: UniquePtr<NativePriorityValue>) -> u16;
        fn priority_value_equal(lhs: &NativePriorityValue, rhs: &NativePriorityValue) -> bool;
        fn priority_value_not_equal(lhs: &NativePriorityValue, rhs: &NativePriorityValue) -> bool;
        fn priority_value_less(lhs: &NativePriorityValue, rhs: &NativePriorityValue) -> bool;
        fn priority_value_less_equal(lhs: &NativePriorityValue, rhs: &NativePriorityValue) -> bool;
        fn priority_value_greater(lhs: &NativePriorityValue, rhs: &NativePriorityValue) -> bool;
        fn priority_value_greater_equal(
            lhs: &NativePriorityValue,
            rhs: &NativePriorityValue,
        ) -> bool;
        fn priority_value_hash(value: &NativePriorityValue) -> usize;

        type NativeRtpEncoding;
        type NativeRtpEncodingList;
        fn rtp_encoding_default() -> UniquePtr<NativeRtpEncoding>;
        fn rtp_encoding_clone(source: &NativeRtpEncoding) -> UniquePtr<NativeRtpEncoding>;
        fn rtp_encoding_assign(target: Pin<&mut NativeRtpEncoding>, source: &NativeRtpEncoding);
        fn rtp_encoding_equal(lhs: &NativeRtpEncoding, rhs: &NativeRtpEncoding) -> bool;
        fn rtp_encoding_fields(value: &NativeRtpEncoding) -> FfiRtpEncodingFields;
        fn rtp_encoding_set_ssrc(value: Pin<&mut NativeRtpEncoding>, present: bool, input: u32);
        fn rtp_encoding_set_bitrate_priority(value: Pin<&mut NativeRtpEncoding>, input: f64);
        fn rtp_encoding_set_network_priority(value: Pin<&mut NativeRtpEncoding>, input: i32);
        fn rtp_encoding_set_max_bitrate_bps(
            value: Pin<&mut NativeRtpEncoding>,
            present: bool,
            input: i32,
        );
        fn rtp_encoding_set_min_bitrate_bps(
            value: Pin<&mut NativeRtpEncoding>,
            present: bool,
            input: i32,
        );
        fn rtp_encoding_set_max_framerate(
            value: Pin<&mut NativeRtpEncoding>,
            present: bool,
            input: f64,
        );
        fn rtp_encoding_set_num_temporal_layers(
            value: Pin<&mut NativeRtpEncoding>,
            present: bool,
            input: i32,
        );
        fn rtp_encoding_set_scale_resolution_down_by(
            value: Pin<&mut NativeRtpEncoding>,
            present: bool,
            input: f64,
        );
        fn rtp_encoding_set_active(value: Pin<&mut NativeRtpEncoding>, input: bool);
        fn rtp_encoding_set_request_key_frame(value: Pin<&mut NativeRtpEncoding>, input: bool);
        fn rtp_encoding_set_adaptive_ptime(value: Pin<&mut NativeRtpEncoding>, input: bool);
        fn rtp_encoding_set_csrcs(value: Pin<&mut NativeRtpEncoding>, present: bool, csrcs: &[u32]);
        fn rtp_encoding_set_rid(value: Pin<&mut NativeRtpEncoding>, rid: &[u8]);
        fn rtp_encoding_set_scalability_mode(
            value: Pin<&mut NativeRtpEncoding>,
            present: bool,
            mode: &[u8],
        );
        fn rtp_encoding_scale_to(value: &NativeRtpEncoding) -> UniquePtr<NativeRtpResolution>;
        fn rtp_encoding_set_scale_to(
            value: Pin<&mut NativeRtpEncoding>,
            resolution: &NativeRtpResolution,
        );
        fn rtp_encoding_clear_scale_to(value: Pin<&mut NativeRtpEncoding>);
        fn rtp_encoding_codec(value: &NativeRtpEncoding) -> UniquePtr<NativeRtpCodec>;
        fn rtp_encoding_set_codec(value: Pin<&mut NativeRtpEncoding>, codec: &NativeRtpCodec);
        fn rtp_encoding_clear_codec(value: Pin<&mut NativeRtpEncoding>);
        fn rtp_encoding_string(value: &NativeRtpEncoding, valid: &mut bool) -> Vec<u8>;
        fn rtp_encoding_list_len(values: &NativeRtpEncodingList) -> usize;
        fn rtp_encoding_list_get(
            values: &NativeRtpEncodingList,
            index: usize,
        ) -> UniquePtr<NativeRtpEncoding>;
        fn rtp_sender_initial_encodings(
            sender: &NativeRtpSender,
        ) -> UniquePtr<NativeRtpEncodingList>;

        type NativeRtcpFeedback;
        type NativeRtpCodec;
        fn rtcp_feedback_default() -> UniquePtr<NativeRtcpFeedback>;
        fn rtcp_feedback_for_type(type_code: i32) -> UniquePtr<NativeRtcpFeedback>;
        fn rtcp_feedback_new(type_code: i32, message: i32) -> UniquePtr<NativeRtcpFeedback>;
        fn rtcp_feedback_clone(source: &NativeRtcpFeedback) -> UniquePtr<NativeRtcpFeedback>;
        fn rtcp_feedback_assign(target: Pin<&mut NativeRtcpFeedback>, source: &NativeRtcpFeedback);
        fn rtcp_feedback_equal(lhs: &NativeRtcpFeedback, rhs: &NativeRtcpFeedback) -> bool;
        fn rtcp_feedback_fields(value: &NativeRtcpFeedback) -> FfiRtcpFeedbackFields;
        fn rtcp_feedback_set_type(value: Pin<&mut NativeRtcpFeedback>, type_code: i32);
        fn rtcp_feedback_set_message(
            value: Pin<&mut NativeRtcpFeedback>,
            present: bool,
            message: i32,
        );
        fn rtcp_feedback_type_string(type_code: i32) -> String;
        fn rtcp_feedback_optional_type_string(present: bool, type_code: i32) -> String;
        fn rtp_media_type_string(type_code: i32, valid: &mut bool) -> String;
        fn rtp_codec_default() -> UniquePtr<NativeRtpCodec>;
        fn rtp_codec_clone(source: &NativeRtpCodec) -> UniquePtr<NativeRtpCodec>;
        fn rtp_codec_assign(target: Pin<&mut NativeRtpCodec>, source: &NativeRtpCodec);
        fn rtp_codec_equal(lhs: &NativeRtpCodec, rhs: &NativeRtpCodec) -> bool;
        fn rtp_codec_fields(value: &NativeRtpCodec) -> FfiRtpCodecFields;
        fn rtp_codec_set_name(value: Pin<&mut NativeRtpCodec>, name: &[u8]);
        fn rtp_codec_set_kind(value: Pin<&mut NativeRtpCodec>, kind: i32);
        fn rtp_codec_set_clock_rate(value: Pin<&mut NativeRtpCodec>, present: bool, rate: i32);
        fn rtp_codec_set_num_channels(value: Pin<&mut NativeRtpCodec>, present: bool, count: i32);
        fn rtp_codec_feedback_len(value: &NativeRtpCodec) -> usize;
        fn rtp_codec_feedback_get(
            value: &NativeRtpCodec,
            index: usize,
        ) -> UniquePtr<NativeRtcpFeedback>;
        fn rtp_codec_feedback_clear(value: Pin<&mut NativeRtpCodec>);
        fn rtp_codec_feedback_add(value: Pin<&mut NativeRtpCodec>, feedback: &NativeRtcpFeedback);
        fn rtp_codec_parameters(value: &NativeRtpCodec) -> Vec<FfiRtpCodecParameter>;
        fn rtp_codec_set_parameters(
            value: Pin<&mut NativeRtpCodec>,
            parameters: &[FfiRtpCodecParameter],
        );
        fn rtp_codec_mime_type(value: &NativeRtpCodec, valid: &mut bool) -> Vec<u8>;
        fn rtp_codec_string(value: &NativeRtpCodec, valid: &mut bool) -> Vec<u8>;
        fn rtp_codec_is_resiliency(value: &NativeRtpCodec) -> bool;
        fn rtp_codec_is_media(value: &NativeRtpCodec) -> bool;
        fn rtp_codec_write_fmtp(
            value: &NativeRtpCodec,
            output: Pin<&mut NativeStringBuilder>,
        ) -> bool;
        fn rtp_codec_parse_fmtp(
            value: Pin<&mut NativeRtpCodec>,
            text: &[u8],
        ) -> UniquePtr<NativeRtcError>;

        type NativeRtpResolution;
        fn rtp_resolution_new(width: i32, height: i32) -> UniquePtr<NativeRtpResolution>;
        fn rtp_resolution_clone(source: &NativeRtpResolution) -> UniquePtr<NativeRtpResolution>;
        fn rtp_resolution_assign(
            target: Pin<&mut NativeRtpResolution>,
            source: &NativeRtpResolution,
        );
        fn rtp_resolution_equal(lhs: &NativeRtpResolution, rhs: &NativeRtpResolution) -> bool;
        fn rtp_resolution_width(value: &NativeRtpResolution) -> i32;
        fn rtp_resolution_height(value: &NativeRtpResolution) -> i32;
        fn rtp_resolution_set_width(value: Pin<&mut NativeRtpResolution>, width: i32);
        fn rtp_resolution_set_height(value: Pin<&mut NativeRtpResolution>, height: i32);
        fn rtp_resolution_pixel_count(value: &NativeRtpResolution, valid: &mut bool) -> i32;
        fn rtp_resolution_to_pair(value: &NativeRtpResolution) -> FfiRtpResolutionPair;

        include!("pulsebeam-webrtc-sys/native/string_builder.h");
        type NativeStringBuilder;
        fn string_builder_default() -> UniquePtr<NativeStringBuilder>;
        fn string_builder_new(text: &[u8]) -> UniquePtr<NativeStringBuilder>;
        fn string_builder_clone(value: &NativeStringBuilder) -> UniquePtr<NativeStringBuilder>;
        fn string_builder_assign(
            target: Pin<&mut NativeStringBuilder>,
            source: &NativeStringBuilder,
        );
        fn string_builder_bytes<'a>(value: &'a NativeStringBuilder) -> &'a [u8];
        fn string_builder_size(value: &NativeStringBuilder) -> usize;
        fn string_builder_clear(value: Pin<&mut NativeStringBuilder>);
        fn string_builder_release(value: Pin<&mut NativeStringBuilder>) -> Vec<u8>;
        fn string_builder_append_bytes(value: Pin<&mut NativeStringBuilder>, input: &[u8]);
        fn string_builder_append_byte(value: Pin<&mut NativeStringBuilder>, input: u8);
        fn string_builder_append_i32(value: Pin<&mut NativeStringBuilder>, input: i32);
        fn string_builder_append_u32(value: Pin<&mut NativeStringBuilder>, input: u32);
        fn string_builder_append_i64(value: Pin<&mut NativeStringBuilder>, input: i64);
        fn string_builder_append_u64(value: Pin<&mut NativeStringBuilder>, input: u64);
        fn string_builder_append_long(value: Pin<&mut NativeStringBuilder>, input: i64) -> bool;
        fn string_builder_append_unsigned_long(
            value: Pin<&mut NativeStringBuilder>,
            input: u64,
        ) -> bool;
        fn string_builder_append_f32(value: Pin<&mut NativeStringBuilder>, input: f32);
        fn string_builder_append_f64(value: Pin<&mut NativeStringBuilder>, input: f64);
        fn rtc_error_copy_message_builder(error: &NativeRtcError)
        -> UniquePtr<NativeStringBuilder>;
        fn rtc_error_assign_message_builder(
            error: Pin<&mut NativeRtcError>,
            source: &NativeStringBuilder,
        );

        include!("pulsebeam-webrtc-sys/native/stats.h");
        type NativeLegacyStatsId;
        fn legacy_stats_id_type(value: &NativeLegacyStatsId) -> i32;
        fn legacy_stats_id_string(value: &NativeLegacyStatsId) -> Vec<u8>;
        fn legacy_stats_id_copy(value: &NativeLegacyStatsId) -> UniquePtr<NativeLegacyStatsId>;
        fn legacy_stats_id_same(a: &NativeLegacyStatsId, b: &NativeLegacyStatsId) -> bool;
        fn legacy_stats_id_equal(a: &NativeLegacyStatsId, b: &NativeLegacyStatsId) -> bool;
        type NativeRtcStats;
        type NativeRtcStatsAttribute;
        type NativeRtcStatsReport;
        type NativeRtcStatsReportBuilder;
        fn rtc_stats_new(class_kind: u8, id: &[u8], timestamp: i64) -> UniquePtr<NativeRtcStats>;
        fn rtc_stats_copy(value: &NativeRtcStats) -> UniquePtr<NativeRtcStats>;
        fn rtc_stats_copy_as(value: &NativeRtcStats, class_kind: u8) -> UniquePtr<NativeRtcStats>;
        fn rtc_stats_class(value: &NativeRtcStats) -> u8;
        fn rtc_stats_id(value: &NativeRtcStats) -> Vec<u8>;
        fn rtc_stats_type(value: &NativeRtcStats) -> Vec<u8>;
        fn rtc_stats_timestamp(value: &NativeRtcStats) -> i64;
        fn rtc_stats_set_timestamp(value: Pin<&mut NativeRtcStats>, timestamp: i64);
        fn rtc_stats_data(value: &NativeRtcStats) -> FfiRtcStatsData;
        fn rtc_stats_set_data(value: Pin<&mut NativeRtcStats>, data: &FfiRtcStatsData) -> bool;
        fn rtc_stats_attributes(value: &NativeRtcStats) -> Vec<FfiRtcStatsAttribute>;
        fn rtc_stats_equal(a: &NativeRtcStats, b: &NativeRtcStats) -> bool;
        fn rtc_stats_json(value: &NativeRtcStats) -> Vec<u8>;
        fn rtc_stats_attribute_new(
            name: &[u8],
            value: &FfiRtcStatsValue,
        ) -> UniquePtr<NativeRtcStatsAttribute>;
        fn rtc_stats_attribute_copy(
            value: &NativeRtcStatsAttribute,
        ) -> UniquePtr<NativeRtcStatsAttribute>;
        fn rtc_stats_attribute_data(value: &NativeRtcStatsAttribute) -> FfiRtcStatsAttribute;
        fn rtc_stats_attribute_has_value(value: &NativeRtcStatsAttribute) -> bool;
        fn rtc_stats_attribute_is_sequence(value: &NativeRtcStatsAttribute) -> bool;
        fn rtc_stats_attribute_is_string(value: &NativeRtcStatsAttribute) -> bool;
        fn rtc_stats_attribute_string(value: &NativeRtcStatsAttribute) -> Vec<u8>;
        fn rtc_stats_attribute_equal(
            a: &NativeRtcStatsAttribute,
            b: &NativeRtcStatsAttribute,
        ) -> bool;
        fn rtc_stats_report_clone(value: &NativeRtcStatsReport) -> UniquePtr<NativeRtcStatsReport>;
        fn rtc_stats_report_same(a: &NativeRtcStatsReport, b: &NativeRtcStatsReport) -> bool;
        fn rtc_stats_report_timestamp(value: &NativeRtcStatsReport) -> i64;
        fn rtc_stats_report_size(value: &NativeRtcStatsReport) -> usize;
        fn rtc_stats_report_keys(value: &NativeRtcStatsReport) -> Vec<FfiRtcStatsBytes>;
        fn rtc_stats_report_get(
            value: &NativeRtcStatsReport,
            id: &[u8],
            class_kind: u8,
        ) -> UniquePtr<NativeRtcStats>;
        fn rtc_stats_report_of_class(
            value: &NativeRtcStatsReport,
            class_kind: u8,
        ) -> Vec<FfiRtcStatsEntry>;
        fn rtc_stats_report_serializable(value: &NativeRtcStatsReport) -> bool;
        fn rtc_stats_report_json(value: &NativeRtcStatsReport) -> Vec<u8>;
        fn rtc_stats_report_copy(
            value: &NativeRtcStatsReport,
        ) -> UniquePtr<NativeRtcStatsReportBuilder>;
        fn rtc_stats_report_new(timestamp: i64) -> UniquePtr<NativeRtcStatsReportBuilder>;
        fn rtc_stats_builder_size(value: &NativeRtcStatsReportBuilder) -> usize;
        fn rtc_stats_builder_add(
            value: Pin<&mut NativeRtcStatsReportBuilder>,
            stat: UniquePtr<NativeRtcStats>,
            try_add: bool,
        ) -> bool;
        fn rtc_stats_builder_take(
            value: Pin<&mut NativeRtcStatsReportBuilder>,
            id: &[u8],
        ) -> UniquePtr<NativeRtcStats>;
        fn rtc_stats_builder_merge(
            value: Pin<&mut NativeRtcStatsReportBuilder>,
            other: Pin<&mut NativeRtcStatsReportBuilder>,
        ) -> bool;
        fn rtc_stats_report_freeze(
            value: UniquePtr<NativeRtcStatsReportBuilder>,
        ) -> UniquePtr<NativeRtcStatsReport>;

        include!("pulsebeam-webrtc-sys/native/rtc_error.h");
        type NativeRtcError;
        fn rtc_error_default() -> UniquePtr<NativeRtcError>;
        fn rtc_error_for_type(kind: i32) -> UniquePtr<NativeRtcError>;
        fn rtc_error_new(kind: i32, message: &[u8]) -> UniquePtr<NativeRtcError>;
        fn rtc_error_factory(kind: i32, message: &[u8]) -> UniquePtr<NativeRtcError>;
        fn rtc_error_clone(error: &NativeRtcError) -> UniquePtr<NativeRtcError>;
        fn rtc_error_assign(target: Pin<&mut NativeRtcError>, source: &NativeRtcError);
        fn rtc_error_move_assign(
            target: Pin<&mut NativeRtcError>,
            source: UniquePtr<NativeRtcError>,
        );
        fn rtc_error_fields(error: &NativeRtcError) -> FfiRtcError;
        fn rtc_error_is_ok(error: &NativeRtcError) -> bool;
        fn rtc_error_set_type(error: Pin<&mut NativeRtcError>, kind: i32);
        fn rtc_error_set_message(error: Pin<&mut NativeRtcError>, message: &[u8]);
        fn rtc_error_set_detail(error: Pin<&mut NativeRtcError>, detail: i32);
        fn rtc_error_set_sctp_cause(error: Pin<&mut NativeRtcError>, cause: u16);
        fn rtc_error_append_text(error: Pin<&mut NativeRtcError>, text: &str);
        fn rtc_error_with_appended_bytes(
            error: UniquePtr<NativeRtcError>,
            text: &[u8],
        ) -> UniquePtr<NativeRtcError>;
        fn rtc_error_stream_byte(
            error: UniquePtr<NativeRtcError>,
            input: u8,
        ) -> UniquePtr<NativeRtcError>;
        fn rtc_error_stream_i32(
            error: UniquePtr<NativeRtcError>,
            input: i32,
        ) -> UniquePtr<NativeRtcError>;
        fn rtc_error_stream_u32(
            error: UniquePtr<NativeRtcError>,
            input: u32,
        ) -> UniquePtr<NativeRtcError>;
        fn rtc_error_stream_i64(
            error: UniquePtr<NativeRtcError>,
            input: i64,
        ) -> UniquePtr<NativeRtcError>;
        fn rtc_error_stream_u64(
            error: UniquePtr<NativeRtcError>,
            input: u64,
        ) -> UniquePtr<NativeRtcError>;
        fn rtc_error_stream_f32(
            error: UniquePtr<NativeRtcError>,
            input: f32,
        ) -> UniquePtr<NativeRtcError>;
        fn rtc_error_stream_f64(
            error: UniquePtr<NativeRtcError>,
            input: f64,
        ) -> UniquePtr<NativeRtcError>;
        fn rtc_error_stream_bool(
            error: UniquePtr<NativeRtcError>,
            input: bool,
        ) -> UniquePtr<NativeRtcError>;
        fn rtc_error_stream_long(
            error: UniquePtr<NativeRtcError>,
            input: i64,
            valid: &mut bool,
        ) -> UniquePtr<NativeRtcError>;
        fn rtc_error_stream_unsigned_long(
            error: UniquePtr<NativeRtcError>,
            input: u64,
            valid: &mut bool,
        ) -> UniquePtr<NativeRtcError>;
        fn rtc_error_type_name(kind: i32, valid: &mut bool) -> String;
        fn rtc_error_detail_name(detail: i32, valid: &mut bool) -> String;
        fn rtc_error_string(error: &NativeRtcError, valid: &mut bool) -> Vec<u8>;
        fn rtc_error_builder_bytes<'a>(error: Pin<&'a mut NativeRtcError>) -> &'a [u8];
        fn rtc_error_builder_size(error: &NativeRtcError) -> usize;
        fn rtc_error_builder_clear(error: Pin<&mut NativeRtcError>);
        fn rtc_error_builder_release(error: Pin<&mut NativeRtcError>) -> Vec<u8>;
        fn rtc_error_append_byte(error: Pin<&mut NativeRtcError>, value: u8);
        fn rtc_error_append_bytes(error: Pin<&mut NativeRtcError>, value: &[u8]);
        fn rtc_error_append_i32(error: Pin<&mut NativeRtcError>, value: i32);
        fn rtc_error_append_u32(error: Pin<&mut NativeRtcError>, value: u32);
        fn rtc_error_append_i64(error: Pin<&mut NativeRtcError>, value: i64);
        fn rtc_error_append_u64(error: Pin<&mut NativeRtcError>, value: u64);
        fn rtc_error_append_long(error: Pin<&mut NativeRtcError>, value: i64) -> bool;
        fn rtc_error_append_unsigned_long(error: Pin<&mut NativeRtcError>, value: u64) -> bool;
        fn rtc_error_append_f32(error: Pin<&mut NativeRtcError>, value: f32);
        fn rtc_error_append_f64(error: Pin<&mut NativeRtcError>, value: f64);

        include!("pulsebeam-webrtc-sys/native/transport.h");
        type NativeDtlsTransport;
        type NativeSctpTransport;
        fn ice_transport_register_packet_observer(
            transport: &NativeIceTransport,
            capacity: usize,
        ) -> u8;
        fn ice_transport_unregister_packet_observer(transport: &NativeIceTransport) -> bool;
        fn ice_transport_packet_event(transport: &NativeIceTransport) -> FfiIcePacketEvent;
        fn ice_transport_packet_dropped(transport: &NativeIceTransport) -> u64;
        fn ice_packet_options_default() -> FfiIcePacketOptions;
        fn ice_transport_packet_state(transport: &NativeIceTransport) -> FfiIcePacketState;
        fn ice_transport_send_packet(
            transport: &NativeIceTransport,
            data: &[u8],
            options: &FfiIcePacketOptions,
            flags: i32,
        ) -> FfiIceSocketResult;
        fn ice_transport_set_option(
            transport: &NativeIceTransport,
            option: u8,
            value: i32,
        ) -> FfiIceSocketResult;
        fn ice_transport_get_option(transport: &NativeIceTransport, option: u8) -> FfiIceOption;
        fn ice_transport_route(transport: &NativeIceTransport) -> FfiIceRouteResult;
        type NativeTransportCandidate;
        fn transport_candidate_default() -> UniquePtr<NativeTransportCandidate>;
        fn transport_candidate_clone(
            candidate: &NativeTransportCandidate,
        ) -> UniquePtr<NativeTransportCandidate>;
        fn transport_candidate_fields(
            candidate: &NativeTransportCandidate,
        ) -> FfiTransportCandidate;
        fn transport_candidate_parse(bytes: &[u8]) -> FfiTransportCandidateParse;
        fn transport_candidate_verify(
            candidate: &NativeTransportCandidate,
        ) -> UniquePtr<NativeRtcError>;
        fn transport_candidate_set_bytes(
            candidate: Pin<&mut NativeTransportCandidate>,
            field: u8,
            value: &[u8],
        );
        fn transport_candidate_set_integer(
            candidate: Pin<&mut NativeTransportCandidate>,
            field: u8,
            value: i64,
        );
        fn transport_candidate_set_address(
            candidate: Pin<&mut NativeTransportCandidate>,
            related: bool,
            value: &NativeSocketAddress,
        );
        fn transport_candidate_serialize(
            candidate: &NativeTransportCandidate,
            kind: u8,
            include_ufrag: bool,
        ) -> Vec<u8>;
        fn transport_candidate_equivalent(
            first: &NativeTransportCandidate,
            second: &NativeTransportCandidate,
            removal: bool,
        ) -> bool;
        fn ice_transport_candidate(
            transport: &NativeIceTransport,
            candidate: &NativeTransportCandidate,
            remove: bool,
        ) -> bool;
        fn ice_transport_remove_all_candidates(transport: &NativeIceTransport) -> bool;
        fn ice_transport_stats(transport: &NativeIceTransport) -> FfiIceStats;
        fn ice_transport_selected_pair(transport: &NativeIceTransport) -> FfiIcePair;

        type NativeIceTransport;
        fn ice_config_default() -> FfiIceConfig;
        fn ice_config_resolve(config: &FfiIceConfig) -> FfiIceConfig;
        fn ice_config_validate(config: &FfiIceConfig) -> UniquePtr<NativeRtcError>;
        fn ice_transport_config(transport: &NativeIceTransport, config: &mut FfiIceConfig) -> bool;
        fn ice_transport_set_config(
            transport: &NativeIceTransport,
            config: &FfiIceConfig,
        ) -> UniquePtr<NativeRtcError>;
        fn ice_transport_set_role(transport: &NativeIceTransport, role: i32) -> bool;
        fn ice_transport_set_parameters(
            transport: &NativeIceTransport,
            remote: bool,
            ufrag: &[u8],
            password: &[u8],
            renomination: bool,
        ) -> bool;
        fn ice_transport_set_remote_mode(transport: &NativeIceTransport, mode: u8) -> bool;
        fn ice_transport_maybe_start_gathering(transport: &NativeIceTransport) -> bool;
        type NativeDtlsInformation;
        type NativeSctpInformation;
        type NativeDtlsEvent;
        fn peer_dtls_transport_by_mid(
            peer: &NativePeerConnection,
            mid: &[u8],
        ) -> UniquePtr<NativeDtlsTransport>;
        fn ice_transport_same_resource(
            first: &NativeIceTransport,
            second: &NativeIceTransport,
        ) -> bool;
        fn peer_sctp_transport(peer: &NativePeerConnection) -> UniquePtr<NativeSctpTransport>;
        fn dtls_transport_identity(transport: &NativeDtlsTransport) -> usize;
        fn sctp_transport_identity(transport: &NativeSctpTransport) -> usize;
        fn dtls_transport_information(
            transport: &NativeDtlsTransport,
        ) -> UniquePtr<NativeDtlsInformation>;
        fn sctp_transport_information(
            transport: &NativeSctpTransport,
        ) -> UniquePtr<NativeSctpInformation>;
        fn sctp_transport_dtls(transport: &NativeSctpTransport) -> UniquePtr<NativeDtlsTransport>;
        fn sctp_information_dtls(
            information: &NativeSctpInformation,
            peer: &NativePeerConnection,
        ) -> UniquePtr<NativeDtlsTransport>;
        fn dtls_information_fields(information: &NativeDtlsInformation) -> FfiDtlsInformation;
        fn sctp_information_fields(information: &NativeSctpInformation) -> FfiSctpInformation;
        fn dtls_information_remote_certificates(
            information: &NativeDtlsInformation,
        ) -> Vec<FfiCertificateData>;
        fn dtls_information_default() -> UniquePtr<NativeDtlsInformation>;
        fn dtls_information_new(state: i32) -> UniquePtr<NativeDtlsInformation>;
        fn sctp_information_default() -> UniquePtr<NativeSctpInformation>;
        fn sctp_information_new(state: i32) -> UniquePtr<NativeSctpInformation>;
        fn dtls_information_from_parts(
            fields: &FfiDtlsInformation,
            chain: UniquePtr<NativeTlsCertificateChain>,
        ) -> UniquePtr<NativeDtlsInformation>;
        fn dtls_information_from_legacy_parts(
            fields: &FfiDtlsInformation,
            chain: UniquePtr<NativeTlsCertificateChain>,
        ) -> UniquePtr<NativeDtlsInformation>;
        fn dtls_information_assign(
            target: Pin<&mut NativeDtlsInformation>,
            source: &NativeDtlsInformation,
        );
        fn dtls_information_move_assign(
            target: Pin<&mut NativeDtlsInformation>,
            source: UniquePtr<NativeDtlsInformation>,
        );
        fn dtls_information_certificate_chain(
            information: &NativeDtlsInformation,
        ) -> UniquePtr<NativeTlsCertificateChain>;
        fn sctp_information_without_dtls(
            fields: &FfiSctpInformation,
        ) -> UniquePtr<NativeSctpInformation>;
        fn sctp_information_with_dtls(
            fields: &FfiSctpInformation,
            transport: &NativeDtlsTransport,
        ) -> UniquePtr<NativeSctpInformation>;
        fn sctp_information_assign(
            target: Pin<&mut NativeSctpInformation>,
            source: &NativeSctpInformation,
        );
        fn sctp_information_string(information: &NativeSctpInformation) -> String;
        fn sctp_state_string(state: i32) -> String;
        fn dtls_information_clone(
            information: &NativeDtlsInformation,
        ) -> UniquePtr<NativeDtlsInformation>;
        fn sctp_information_clone(
            information: &NativeSctpInformation,
        ) -> UniquePtr<NativeSctpInformation>;
        fn dtls_transport_register_observer(
            transport: &NativeDtlsTransport,
            capacity: usize,
        ) -> bool;
        fn sctp_transport_register_observer(
            transport: &NativeSctpTransport,
            capacity: usize,
        ) -> bool;
        fn dtls_transport_unregister_observer(transport: &NativeDtlsTransport);
        fn sctp_transport_unregister_observer(transport: &NativeSctpTransport);
        fn dtls_transport_take_event(transport: &NativeDtlsTransport)
        -> UniquePtr<NativeDtlsEvent>;
        fn sctp_transport_take_event(
            transport: &NativeSctpTransport,
        ) -> UniquePtr<NativeSctpInformation>;
        fn dtls_transport_take_dropped(transport: &NativeDtlsTransport) -> u64;
        fn sctp_transport_take_dropped(transport: &NativeSctpTransport) -> u64;
        fn dtls_event_information(
            event: Pin<&mut NativeDtlsEvent>,
        ) -> UniquePtr<NativeDtlsInformation>;
        fn dtls_event_error(event: &NativeDtlsEvent) -> UniquePtr<NativeRtcError>;
        fn dtls_transport_ice(transport: &NativeDtlsTransport) -> UniquePtr<NativeIceTransport>;
        fn ice_transport_has_internal(transport: &NativeIceTransport) -> bool;
        fn ice_transport_internal_value(
            transport: &NativeIceTransport,
            field: u8,
        ) -> FfiIceInternalValue;
        fn rtp_sender_set_media_track(sender: &NativeRtpSender, track: &NativeMediaTrack) -> bool;
        type NativeRtpEndpoints;
        fn peer_endpoints(peer: &NativePeerConnection) -> UniquePtr<NativeRtpEndpoints>;
        fn endpoint_list_len(list: &NativeRtpEndpoints, kind: u8) -> usize;
        fn endpoint_list_unified(list: &NativeRtpEndpoints) -> bool;
        fn endpoint_list_sender(
            list: &NativeRtpEndpoints,
            index: usize,
        ) -> UniquePtr<NativeRtpSender>;
        fn endpoint_list_receiver(
            list: &NativeRtpEndpoints,
            index: usize,
        ) -> UniquePtr<NativeRtpReceiver>;
        fn endpoint_list_transceiver(
            list: &NativeRtpEndpoints,
            index: usize,
        ) -> UniquePtr<NativeRtpTransceiver>;
        type NativeRtpSender;
        type NativeRtpSenderObserver;
        fn rtp_sender_observer(value: &NativeRtpSender) -> UniquePtr<NativeRtpSenderObserver>;
        fn rtp_sender_observer_identity(value: &NativeRtpSenderObserver) -> usize;
        fn rtp_sender_observer_register(value: &NativeRtpSenderObserver, capacity: usize) -> bool;
        fn rtp_sender_observer_unregister(value: &NativeRtpSenderObserver);
        fn rtp_sender_observer_take(value: &NativeRtpSenderObserver) -> FfiRtpPacketEvent;
        fn rtp_sender_observer_take_dropped(value: &NativeRtpSenderObserver) -> u64;
        type NativeRtpReceiverObserver;
        fn rtp_receiver_observer(value: &NativeRtpReceiver)
        -> UniquePtr<NativeRtpReceiverObserver>;
        fn rtp_receiver_observer_identity(value: &NativeRtpReceiverObserver) -> usize;
        fn rtp_receiver_observer_register(
            value: &NativeRtpReceiverObserver,
            capacity: usize,
        ) -> bool;
        fn rtp_receiver_observer_unregister(value: &NativeRtpReceiverObserver);
        fn rtp_receiver_observer_take(value: &NativeRtpReceiverObserver) -> FfiRtpPacketEvent;
        fn rtp_receiver_observer_take_dropped(value: &NativeRtpReceiverObserver) -> u64;

        type NativeDtmfSender;
        type NativeRtpReceiver;
        type NativeRtpTransceiver;
        type NativeTransceiverList;

        fn bridge_identity() -> &'static str;

        fn new_manual_clock(initial_time_us: i64) -> UniquePtr<NativeManualClock>;
        fn manual_clock_time_us(clock: &NativeManualClock) -> i64;
        fn advance_manual_clock(clock: &NativeManualClock, delta_us: i64) -> bool;
        fn system_clock_time_us() -> i64;

        fn new_default_task_queue_factory() -> UniquePtr<NativeTaskQueueFactory>;
        fn new_cooperative_task_queue_factory(
            clock: &NativeManualClock,
        ) -> UniquePtr<NativeTaskQueueFactory>;
        fn task_queue_factory_is_cooperative(factory: &NativeTaskQueueFactory) -> bool;
        fn task_queue_factory_is_current(factory: &NativeTaskQueueFactory) -> bool;
        fn task_queue_factory_uses_clock(
            factory: &NativeTaskQueueFactory,
            clock: &NativeManualClock,
        ) -> bool;
        fn create_task_queue(
            factory: &NativeTaskQueueFactory,
            name: &[u8],
            priority: u8,
        ) -> UniquePtr<NativeTaskQueue>;
        type NativeProductionLease;
        fn new_production_lease() -> UniquePtr<NativeProductionLease>;
        type NativeReadiness;
        fn new_readiness(readiness: Box<RustReadiness>) -> UniquePtr<NativeReadiness>;
        fn post_task(queue: &NativeTaskQueue, task: Box<RustTask>) -> bool;
        fn post_delayed_task(queue: &NativeTaskQueue, delay_us: i64, task: Box<RustTask>) -> bool;
        fn task_queue_identity(queue: &NativeTaskQueue) -> usize;
        fn task_queue_is_current(queue: &NativeTaskQueue) -> bool;
        fn task_queue_name(queue: &NativeTaskQueue) -> Vec<u8>;
        fn task_queue_has_failed(queue: &NativeTaskQueue) -> bool;
        fn post_native_delayed_task(
            queue: &NativeTaskQueue,
            delay: i64,
            high_precision: bool,
            task: Box<RustTask>,
        ) -> u8;
        fn run_ready_tasks(factory: &NativeTaskQueueFactory) -> usize;
        fn pump_ready_tasks(factory: &NativeTaskQueueFactory, budget: usize) -> usize;
        fn next_task_deadline_us(factory: &NativeTaskQueueFactory) -> i64;

        unsafe fn create_environment(
            clock: *const NativeManualClock,
            custom_clock: *const NativeClock,
            factory: &NativeTaskQueueFactory,
            field_trials: Box<RustFieldTrials>,
            event_log: Box<RustRtcEventLog>,
            clock_minimum_us: i64,
            clock_maximum_us: i64,
        ) -> UniquePtr<NativeEnvironment>;
        type NativeEnvironmentFactory;
        fn new_environment_factory(
            bounds: FfiClockTimeBounds,
        ) -> UniquePtr<NativeEnvironmentFactory>;
        fn environment_factory_from_environment(
            environment: &NativeEnvironment,
        ) -> UniquePtr<NativeEnvironmentFactory>;
        fn clone_environment_factory(
            factory: &NativeEnvironmentFactory,
        ) -> UniquePtr<NativeEnvironmentFactory>;
        fn environment_factory_set_trials(
            factory: Pin<&mut NativeEnvironmentFactory>,
            trials: Box<RustFieldTrials>,
        );
        fn environment_factory_set_manual_clock(
            factory: Pin<&mut NativeEnvironmentFactory>,
            clock: &NativeManualClock,
            bounds: FfiClockTimeBounds,
        );
        fn environment_factory_set_clock(
            factory: Pin<&mut NativeEnvironmentFactory>,
            clock: &NativeClock,
            bounds: FfiClockTimeBounds,
        );
        fn environment_factory_set_queues(
            factory: Pin<&mut NativeEnvironmentFactory>,
            queues: &NativeTaskQueueFactory,
        );
        fn environment_factory_set_log(
            factory: Pin<&mut NativeEnvironmentFactory>,
            log: Box<RustRtcEventLog>,
        );
        fn environment_factory_set_log_product(
            factory: Pin<&mut NativeEnvironmentFactory>,
            log: UniquePtr<NativeRtcEventLogProduct>,
        ) -> u8;
        fn environment_factory_create(
            factory: &NativeEnvironmentFactory,
            retention: Box<RustEnvironmentFactoryRetention>,
        ) -> UniquePtr<NativeEnvironment>;
        fn environment_default_queue_metadata(
            environment: &NativeEnvironment,
        ) -> UniquePtr<NativeTaskQueueFactory>;
        type NativeFieldTrials;
        fn field_trials_validate(configuration: &str) -> bool;
        fn environment_field_trials(
            environment: &NativeEnvironment,
        ) -> UniquePtr<NativeFieldTrials>;
        fn field_trials_copy(view: &NativeFieldTrials) -> UniquePtr<NativeFieldTrials>;
        fn field_trials_lookup(view: &NativeFieldTrials, key: &str) -> String;
        fn field_trials_is_enabled(view: &NativeFieldTrials, key: &str) -> bool;
        fn field_trials_is_disabled(view: &NativeFieldTrials, key: &str) -> bool;
        fn bind_driver_environment(
            driver: &NativeDriverThread,
            environment: &NativeEnvironment,
            retained: Box<RustRuntimeEnvironmentRetention>,
        ) -> UniquePtr<NativeEnvironment>;
        fn clone_environment(environment: &NativeEnvironment) -> UniquePtr<NativeEnvironment>;
        fn environment_task_queue_factory(
            environment: &NativeEnvironment,
            selected: &NativeTaskQueueFactory,
        ) -> UniquePtr<NativeTaskQueueFactory>;
        fn environment_time_us(environment: &NativeEnvironment) -> i64;
        fn environment_timestamp(environment: &NativeEnvironment) -> i64;
        fn environment_clock_time_bounds(environment: &NativeEnvironment) -> FfiClockTimeBounds;
        fn bind_environment_selection(
            environment: UniquePtr<NativeEnvironment>,
            selected: &NativeEnvironment,
        ) -> UniquePtr<NativeEnvironment>;
        fn environment_clock_identity_matches(
            environment: &NativeEnvironment,
            other: &NativeEnvironment,
        ) -> bool;
        fn environment_queue_factory_identity_matches(
            environment: &NativeEnvironment,
            other: &NativeEnvironment,
        ) -> bool;
        fn environment_context_retention_contract(
            environment: &NativeEnvironment,
            other: &NativeEnvironment,
        ) -> bool;

        fn new_seeded_randomness(seed: u64) -> UniquePtr<NativeRandomnessLease>;
        fn next_seeded_random_u64(lease: &NativeRandomnessLease) -> u64;

        fn new_thread(network: bool) -> UniquePtr<NativeThread>;
        fn new_driver_thread(clock: &NativeManualClock) -> UniquePtr<NativeDriverThread>;
        fn new_seeded_driver_thread(
            clock: &NativeManualClock,
            seed: u64,
        ) -> UniquePtr<NativeDriverThread>;
        fn driver_pump(driver: &NativeDriverThread, budget: usize) -> usize;
        fn borrow_driver_thread(driver: &NativeDriverThread) -> UniquePtr<NativeThread>;
        fn driver_run_ready(driver: &NativeDriverThread) -> bool;
        fn test_driver_lifecycle_yield(
            driver: &NativeDriverThread,
            factory: &NativeTaskQueueFactory,
        ) -> bool;
        fn test_driver_capture_cancellation(driver: &NativeDriverThread) -> bool;
        fn driver_next_deadline_us(driver: &NativeDriverThread) -> i64;
        fn driver_is_current(driver: &NativeDriverThread) -> bool;
        fn thread_is_owned(thread: &NativeThread) -> bool;
        fn thread_is_current(thread: &NativeThread) -> bool;
        fn thread_requires_retirement(thread: &NativeThread) -> bool;
        fn thread_post_task(thread: &NativeThread, task: Box<RustTask>) -> bool;
        fn thread_post_native_delayed_task(
            thread: &NativeThread,
            delay: i64,
            high_precision: bool,
            task: Box<RustTask>,
        ) -> u8;
        fn thread_post_delayed_task(
            thread: &NativeThread,
            delay_us: i64,
            task: Box<RustTask>,
        ) -> bool;

        fn new_simulated_network(clock: &NativeManualClock) -> UniquePtr<NativeSimulatedNetwork>;
        fn new_controlled_simulated_network(
            clock: &NativeManualClock,
            driver: &NativeDriverThread,
        ) -> UniquePtr<NativeSimulatedNetwork>;
        fn add_simulated_dns_record(
            network: &NativeSimulatedNetwork,
            hostname: &str,
            ip: &[u8],
        ) -> bool;
        fn register_network_endpoint(
            network: &NativeSimulatedNetwork,
            ip: &[u8],
            error: &mut u8,
        ) -> UniquePtr<NativeNetworkEndpoint>;
        fn close_network_endpoint(endpoint: Pin<&mut NativeNetworkEndpoint>);
        fn new_network_manager_provider(
            endpoint: &NativeNetworkEndpoint,
        ) -> UniquePtr<NativeNetworkManagerProvider>;
        fn network_manager_provider_is_valid(provider: &NativeNetworkManagerProvider) -> bool;
        fn new_packet_socket_factory_provider(
            endpoint: &NativeNetworkEndpoint,
        ) -> UniquePtr<NativePacketSocketFactoryProvider>;
        fn packet_socket_factory_supports_udp(provider: &NativePacketSocketFactoryProvider)
        -> bool;
        fn packet_socket_factory_supports_tcp(provider: &NativePacketSocketFactoryProvider)
        -> bool;
        fn packet_socket_factory_supports_dns(provider: &NativePacketSocketFactoryProvider)
        -> bool;
        fn create_simulated_udp_socket(
            provider: &NativePacketSocketFactoryProvider,
            port: u16,
            error: &mut u8,
        ) -> UniquePtr<NativeSimulatedUdpSocket>;
        fn simulated_udp_local_ip(socket: &NativeSimulatedUdpSocket) -> Vec<u8>;
        fn simulated_udp_local_port(socket: &NativeSimulatedUdpSocket) -> u16;
        fn simulated_udp_send_to(
            socket: &NativeSimulatedUdpSocket,
            destination_ip: &[u8],
            destination_port: u16,
            payload: Vec<u8>,
            error: &mut u8,
        ) -> bool;
        fn simulated_udp_take_received(
            socket: &NativeSimulatedUdpSocket,
        ) -> UniquePtr<NativeReceivedPacket>;
        fn close_simulated_udp_socket(socket: Pin<&mut NativeSimulatedUdpSocket>);
        fn take_outbound_packet(
            network: &NativeSimulatedNetwork,
        ) -> UniquePtr<NativeOutboundPacket>;
        fn outbound_packet_id(packet: &NativeOutboundPacket) -> u64;
        fn outbound_packet_kind(packet: &NativeOutboundPacket) -> u8;
        fn outbound_packet_source_ip(packet: &NativeOutboundPacket) -> Vec<u8>;
        fn outbound_packet_source_port(packet: &NativeOutboundPacket) -> u16;
        fn outbound_packet_destination_ip(packet: &NativeOutboundPacket) -> Vec<u8>;
        fn outbound_packet_destination_port(packet: &NativeOutboundPacket) -> u16;
        fn outbound_packet_payload(packet: &NativeOutboundPacket) -> Vec<u8>;
        fn outbound_packet_deadline_us(packet: &NativeOutboundPacket) -> i64;
        fn deliver_outbound_packet(
            network: &NativeSimulatedNetwork,
            packet_id: u64,
            keep_pending: bool,
            error: &mut u8,
        ) -> bool;
        fn drop_outbound_packet(
            network: &NativeSimulatedNetwork,
            packet_id: u64,
            error: &mut u8,
        ) -> bool;
        fn inject_simulated_tcp_data(
            network: &NativeSimulatedNetwork,
            source_ip: &[u8],
            source_port: u16,
            destination_ip: &[u8],
            destination_port: u16,
            data: &[u8],
            error: &mut u8,
        ) -> bool;
        fn received_packet_source_ip(packet: &NativeReceivedPacket) -> Vec<u8>;
        fn received_packet_source_port(packet: &NativeReceivedPacket) -> u16;
        fn received_packet_payload(packet: &NativeReceivedPacket) -> Vec<u8>;

        fn new_video_encoder_factory(
            factory: Box<RustVideoEncoderFactory>,
        ) -> UniquePtr<NativeVideoEncoderFactory>;
        fn new_video_decoder_factory(
            factory: Box<RustVideoDecoderFactory>,
        ) -> UniquePtr<NativeVideoDecoderFactory>;
        fn new_builtin_audio_encoder_factory() -> UniquePtr<NativeAudioEncoderFactory>;
        fn new_opus_carrier_audio_encoder_factory() -> UniquePtr<NativeAudioEncoderFactory>;
        fn audio_encoder_opus_handoff_failures(factory: &NativeAudioEncoderFactory) -> u64;
        fn new_builtin_audio_decoder_factory() -> UniquePtr<NativeAudioDecoderFactory>;
        fn new_builtin_video_encoder_factory() -> UniquePtr<NativeVideoEncoderFactory>;
        fn new_builtin_video_decoder_factory() -> UniquePtr<NativeVideoDecoderFactory>;
        fn new_builtin_vp8_decoder_factory() -> UniquePtr<NativeVideoDecoderFactory>;
        fn new_builtin_opus_decoder_factory() -> UniquePtr<NativeAudioDecoderFactory>;
        fn video_decoder_statistics(factory: &NativeVideoDecoderFactory) -> FfiDecoderStatistics;
        fn audio_decoder_statistics(factory: &NativeAudioDecoderFactory) -> FfiDecoderStatistics;
        fn audio_encoder_formats(factory: &NativeAudioEncoderFactory) -> Vec<FfiAudioCodecSpec>;
        fn audio_encoder_query(
            factory: &NativeAudioEncoderFactory,
            format: &FfiSdpAudioFormat,
        ) -> FfiAudioEncoderQuery;
        fn audio_encoder_create(
            factory: &NativeAudioEncoderFactory,
            environment: &NativeEnvironment,
            format: &FfiSdpAudioFormat,
            payload_type: i32,
            pair_id: &UniquePtr<NativeAudioCodecPairId>,
        ) -> FfiAudioEncoderCreation;
        fn audio_encoder_create_with_options(
            factory: &NativeAudioEncoderFactory,
            environment: &NativeEnvironment,
            format: &FfiSdpAudioFormat,
            options: &NativeAudioEncoderOptions,
        ) -> FfiAudioEncoderCreation;
        fn audio_encoder_default_allocation_arguments(
            target: i64,
            kind: i8,
            period: i64,
            valid: &mut bool,
            bitrate: &mut i32,
            milliseconds: &mut i64,
        );
        fn audio_encoder_adaptor_smoothing_inputs_safe(
            bitrate: i32,
            has_period: bool,
            period: i64,
        ) -> bool;
        fn audio_encoder_adaptor_initialization_safe(duration: i32, clock_span_ms: u64) -> bool;
        fn audio_encoder_adaptor_bitrate_probe(
            initial_frame: i32,
            increase: i32,
            decrease: i32,
            steps: &[FfiAudioEncoderBitrateStep],
        ) -> FfiAudioEncoderBitrateProbe;
        fn audio_encoder_adaptor_smoothing_probe(
            encoder: &NativeAudioEncoder,
            first_sample: i32,
            sample: i32,
            first_time_us: i64,
            sample_time_us: i64,
            average_time_us: i64,
            has_duration: bool,
            duration: i32,
        ) -> FfiAudioEncoderSmoothingProbe;
        fn audio_encoder_check_network_adaptor_scoring(
            encoder: &NativeAudioEncoder,
            bytes: &[u8],
            has_bandwidth: bool,
            bandwidth: i32,
            has_loss: bool,
            loss: f32,
        ) -> u8;
        fn audio_encoder_check_network_adaptor_smoothing(
            encoder: &NativeAudioEncoder,
            bytes: &[u8],
        ) -> u8;
        fn audio_encoder_check_network_adaptor_arithmetic(
            encoder: &NativeAudioEncoder,
            bytes: &[u8],
            has_overhead: bool,
            overhead: usize,
            has_target: bool,
            target: i32,
            has_uplink: bool,
            uplink_min: i32,
            uplink_max: i32,
        ) -> u8;
        fn audio_encoder_check_network_adaptor_config(
            encoder: &NativeAudioEncoder,
            bytes: &[u8],
        ) -> u8;
        type NativeAudioEncoderFailureObserver;
        fn audio_encoder_sdk_failure_observer(
            encoder: &NativeAudioEncoder,
        ) -> UniquePtr<NativeAudioEncoderFailureObserver>;
        fn audio_encoder_sdk_failure(source: &NativeAudioEncoderFailureObserver) -> u32;
        fn audio_encoder_sdk_failure_clone(
            source: &NativeAudioEncoderFailureObserver,
        ) -> UniquePtr<NativeAudioEncoderFailureObserver>;
        fn audio_encoder_forward_product(
            source: Box<AudioEncoderTransfer>,
        ) -> UniquePtr<NativeAudioEncoder>;
        type NativeCallerAudioEncoderFactory;
        fn audio_caller_encoder_factory_new(
            owner: Box<RustAudioEncoderFactoryCall>,
        ) -> UniquePtr<NativeCallerAudioEncoderFactory>;
        fn audio_caller_encoder_factory_formats(
            handle: &NativeCallerAudioEncoderFactory,
            call: &RustAudioEncoderFactoryCall,
        ) -> Vec<FfiAudioCodecSpec>;
        fn audio_caller_encoder_factory_query(
            handle: &NativeCallerAudioEncoderFactory,
            call: &RustAudioEncoderFactoryCall,
            format: &FfiSdpAudioFormat,
        ) -> FfiAudioEncoderQuery;
        fn audio_caller_encoder_factory_create(
            handle: &NativeCallerAudioEncoderFactory,
            call: &RustAudioEncoderFactoryCall,
            environment: &NativeEnvironment,
            format: &FfiSdpAudioFormat,
            options: &NativeAudioEncoderOptions,
        ) -> UniquePtr<NativeAudioEncoder>;
        fn audio_caller_encoder_formats(
            source: Box<RustAudioEncoderFactoryCall>,
        ) -> Vec<FfiAudioCodecSpec>;
        fn audio_caller_encoder_query(
            source: Box<RustAudioEncoderFactoryCall>,
            format: &FfiSdpAudioFormat,
        ) -> FfiAudioEncoderQuery;
        fn audio_caller_encoder_create(
            source: Box<RustAudioEncoderFactoryCall>,
            environment: &NativeEnvironment,
            format: &FfiSdpAudioFormat,
            options: &NativeAudioEncoderOptions,
        ) -> UniquePtr<NativeAudioEncoder>;
        fn audio_encoder_forward_encode_sdk(
            source: &NativeAudioEncoder,
            timestamp: u32,
            audio: &[i16],
            prefix: &[u8],
        ) -> FfiAudioEncoderOutput;
        fn audio_encoder_forward_target_sdk(source: &NativeAudioEncoder, bitrate: i32);
        fn audio_encoder_forward_loss_sdk(source: &NativeAudioEncoder, fraction: f32);
        fn audio_encoder_forward_adaptor_sdk(source: &NativeAudioEncoder, config: &[u8]) -> bool;
        fn audio_encoder_info(encoder: &NativeAudioEncoder) -> FfiAudioEncoderInfo;
        fn audio_encoder_frame_length_range(encoder: &NativeAudioEncoder) -> FfiAudioEncoderRange;
        fn audio_encoder_bitrate_range(encoder: &NativeAudioEncoder) -> FfiAudioEncoderRange;
        fn audio_encoder_statistics(encoder: &NativeAudioEncoder) -> FfiAudioEncoderStatistics;
        fn audio_encoder_encode(
            encoder: &NativeAudioEncoder,
            timestamp: u32,
            audio: &[i16],
        ) -> FfiAudioEncoderOutput;
        fn audio_encoder_reset(encoder: &NativeAudioEncoder);
        fn audio_encoder_set_fec(encoder: &NativeAudioEncoder, enable: bool) -> bool;
        fn audio_encoder_set_dtx(encoder: &NativeAudioEncoder, enable: bool) -> bool;
        fn audio_encoder_set_application(encoder: &NativeAudioEncoder, speech: bool) -> bool;
        fn audio_encoder_enable_network_adaptor(
            encoder: &NativeAudioEncoder,
            config: &[u8],
            error: &mut u8,
        ) -> bool;
        fn audio_encoder_disable_network_adaptor(encoder: &NativeAudioEncoder);
        fn audio_encoder_set_max_playback_rate(encoder: &NativeAudioEncoder, frequency: i32);
        fn audio_encoder_set_target_bitrate(encoder: &NativeAudioEncoder, bitrate: i32) -> bool;
        fn audio_encoder_on_target_bitrate(encoder: &NativeAudioEncoder, bitrate: i32) -> bool;
        fn audio_encoder_on_bandwidth(
            encoder: &NativeAudioEncoder,
            bitrate: i32,
            has_period: bool,
            period: i64,
        ) -> bool;
        fn audio_encoder_on_allocation(
            encoder: &NativeAudioEncoder,
            update: &FfiBitrateAllocationUpdate,
        ) -> bool;
        fn audio_encoder_on_overhead(encoder: &NativeAudioEncoder, overhead: usize) -> bool;
        fn audio_encoder_on_packet_loss(encoder: &NativeAudioEncoder, fraction: f32) -> u8;
        fn audio_encoder_on_rtt(encoder: &NativeAudioEncoder, rtt: i32) -> bool;
        fn audio_encoder_set_receiver_frame_length_range(
            encoder: &NativeAudioEncoder,
            minimum: i32,
            maximum: i32,
        ) -> bool;
        fn audio_decoder_formats(factory: &NativeAudioDecoderFactory) -> Vec<FfiAudioCodecSpec>;
        fn audio_decoder_factory_builtin_kind(factory: &NativeAudioDecoderFactory) -> u8;
        fn audio_decoder_factory_context_contract(
            factory: &NativeAudioDecoderFactory,
            other: &NativeAudioDecoderFactory,
            environment: &NativeEnvironment,
        ) -> bool;
        fn audio_decoder_supported(
            factory: &NativeAudioDecoderFactory,
            format: &FfiSdpAudioFormat,
        ) -> i8;
        fn audio_decoder_create(
            factory: &NativeAudioDecoderFactory,
            environment: &NativeEnvironment,
            format: &FfiSdpAudioFormat,
        ) -> FfiAudioDecoderCreation;
        fn audio_decoder_create_with_pair_id(
            factory: &NativeAudioDecoderFactory,
            environment: &NativeEnvironment,
            format: &FfiSdpAudioFormat,
            pair_id: &UniquePtr<NativeAudioCodecPairId>,
        ) -> FfiAudioDecoderCreation;
        fn audio_decoder_sample_rate(decoder: &NativeAudioDecoder) -> i32;
        fn audio_decoder_channels(decoder: &NativeAudioDecoder) -> usize;
        fn audio_decoder_reset(decoder: &NativeAudioDecoder);
        fn audio_decoder_error_code(decoder: &NativeAudioDecoder) -> i32;
        fn audio_decoder_packet_info(
            decoder: &NativeAudioDecoder,
            encoded: &[u8],
        ) -> FfiAudioPacketInfo;
        fn audio_decoder_parse(
            decoder: &NativeAudioDecoder,
            encoded: &[u8],
            timestamp: u32,
        ) -> FfiParsedAudioFrames;
        fn audio_decoder_generate_plc(
            decoder: &NativeAudioDecoder,
            requested_samples: usize,
            prefix: &[i16],
        ) -> FfiAudioPlc;
        fn audio_frame_duration(frame: &NativeDecodableAudioFrame) -> usize;
        fn audio_frame_is_dtx(frame: &NativeDecodableAudioFrame) -> bool;
        fn audio_frame_decode(frame: &NativeDecodableAudioFrame) -> FfiDecodedAudio;
        fn audio_caller_frame_sdk_duration(source: Box<RustCallerAudioFrameCall>) -> usize;
        fn audio_caller_frame_sdk_dtx(source: Box<RustCallerAudioFrameCall>) -> bool;
        fn audio_caller_frame_sdk_decode(
            source: Box<RustCallerAudioFrameCall>,
            output: &mut [i16],
        ) -> FfiAudioFrameDecodeInto;
        type NativeCallerAudioDecoderProduct;
        fn audio_caller_decoder_product_new(
            owner: Box<RustCallerAudioDecoderCall>,
        ) -> UniquePtr<NativeCallerAudioDecoderProduct>;
        fn audio_caller_decoder_product_scalar(
            owner: &NativeCallerAudioDecoderProduct,
            call: &RustCallerAudioDecoderCall,
            kind: u8,
        ) -> i32;
        fn audio_caller_decoder_product_channels(
            owner: &NativeCallerAudioDecoderProduct,
            call: &RustCallerAudioDecoderCall,
        ) -> usize;
        fn audio_caller_decoder_product_reset(
            owner: &NativeCallerAudioDecoderProduct,
            call: &RustCallerAudioDecoderCall,
        );
        fn audio_caller_decoder_product_duration(
            owner: &NativeCallerAudioDecoderProduct,
            call: &RustCallerAudioDecoderCall,
            encoded: &[u8],
            redundant: bool,
        ) -> i32;
        fn audio_caller_decoder_product_fec(
            owner: &NativeCallerAudioDecoderProduct,
            call: &RustCallerAudioDecoderCall,
            encoded: &[u8],
        ) -> bool;
        fn audio_caller_decoder_product_parse(
            owner: &NativeCallerAudioDecoderProduct,
            call: &RustCallerAudioDecoderCall,
            encoded: &[u8],
            timestamp: u32,
        ) -> Vec<FfiCallerParsedAudioFrame>;
        fn audio_caller_decoder_product_plc(
            owner: &NativeCallerAudioDecoderProduct,
            call: &RustCallerAudioDecoderCall,
            requested: usize,
            prefix: &[i16],
        ) -> FfiAudioPlc;
        fn audio_caller_decoder_sdk_scalar(
            source: Box<RustCallerAudioDecoderCall>,
            kind: u8,
        ) -> i32;
        fn audio_caller_decoder_sdk_channels(source: Box<RustCallerAudioDecoderCall>) -> usize;
        fn audio_caller_decoder_sdk_reset(source: Box<RustCallerAudioDecoderCall>);
        fn audio_caller_decoder_sdk_packet_duration(
            source: Box<RustCallerAudioDecoderCall>,
            encoded: &[u8],
            redundant: bool,
        ) -> i32;
        fn audio_caller_decoder_sdk_packet_fec(
            source: Box<RustCallerAudioDecoderCall>,
            encoded: &[u8],
        ) -> bool;
        fn audio_caller_decoder_sdk_parse(
            source: Box<RustCallerAudioDecoderCall>,
            encoded: &[u8],
            timestamp: u32,
        ) -> Vec<FfiCallerParsedAudioFrame>;
        fn audio_caller_decoder_sdk_plc(
            source: Box<RustCallerAudioDecoderCall>,
            requested: usize,
            prefix: &[i16],
        ) -> FfiAudioPlc;
        fn audio_caller_decoder_sdk_legacy_refusal(
            source: Box<RustCallerAudioDecoderCall>,
            redundant: bool,
            output: &mut [i16],
            speech: &mut i32,
        ) -> i32;
        fn audio_format_description(format: &FfiSdpAudioFormat) -> Vec<u8>;
        fn audio_codec_info_description(info: &FfiAudioCodecInfo) -> Vec<u8>;
        fn audio_codec_info_has_fixed_bitrate(info: &FfiAudioCodecInfo, valid: &mut bool) -> bool;
        fn audio_codec_info_create(
            sample_rate_hz: i32,
            num_channels: usize,
            default_bitrate_bps: i32,
            min_bitrate_bps: i32,
            max_bitrate_bps: i32,
            use_range: bool,
            valid: &mut bool,
        ) -> FfiAudioCodecInfo;
        fn audio_codec_spec_description(spec: &FfiAudioCodecSpec) -> Vec<u8>;
        fn native_codec_thread_token() -> u64;
        fn video_scalability_mode_valid(name: &str) -> bool;
        unsafe fn video_encoder_query_value(
            factory: &NativeVideoEncoderFactory,
            format: &NativeSdpVideoFormat,
            has_mode: bool,
            mode: &[u8],
            resolution: *const NativeRtpResolution,
            legacy: bool,
        ) -> FfiVideoFactorySupport;
        unsafe fn video_decoder_query_value(
            factory: &NativeVideoDecoderFactory,
            format: &NativeSdpVideoFormat,
            reference_scaling: bool,
            resolution: *const NativeRtpResolution,
            legacy: bool,
        ) -> FfiVideoFactorySupport;
        fn video_encoder_formats(factory: &NativeVideoEncoderFactory) -> Vec<FfiCodecFormat>;
        fn video_encoder_query(
            factory: &NativeVideoEncoderFactory,
            format: &FfiCodecFormat,
            has_scalability_mode: bool,
            scalability_mode: &str,
            has_resolution: bool,
            width: u32,
            height: u32,
        ) -> FfiCodecSupport;
        fn video_decoder_formats(factory: &NativeVideoDecoderFactory) -> Vec<FfiCodecFormat>;
        fn video_decoder_query(
            factory: &NativeVideoDecoderFactory,
            format: &FfiCodecFormat,
            reference_scaling: bool,
            has_resolution: bool,
            width: u32,
            height: u32,
        ) -> FfiCodecSupport;
        fn native_video_frame_width(frame: &NativeVideoFrame) -> u32;
        fn native_video_frame_height(frame: &NativeVideoFrame) -> u32;
        fn native_video_frame_timestamp_us(frame: &NativeVideoFrame) -> i64;
        fn native_video_frame_rtp_timestamp(frame: &NativeVideoFrame) -> u32;
        fn native_video_frame_rotation(frame: &NativeVideoFrame) -> u16;
        fn native_video_frame_i420(frame: &NativeVideoFrame) -> Vec<u8>;
        fn native_encoded_frame_width(frame: &NativeEncodedVideoFrame) -> u32;
        fn native_encoded_frame_height(frame: &NativeEncodedVideoFrame) -> u32;
        fn native_encoded_frame_rtp_timestamp(frame: &NativeEncodedVideoFrame) -> u32;
        fn native_encoded_frame_key(frame: &NativeEncodedVideoFrame) -> bool;
        fn native_encoded_frame_qp(frame: &NativeEncodedVideoFrame) -> i32;
        fn native_encoded_frame_data(frame: &NativeEncodedVideoFrame) -> Vec<u8>;
        fn encoded_callback_same(
            a: &NativeEncodedImageCallback,
            b: &NativeEncodedImageCallback,
        ) -> bool;
        fn encoded_callback_retired(callback: &NativeEncodedImageCallback) -> bool;
        unsafe fn encoded_callback_frame(
            callback: &NativeEncodedImageCallback,
            image: &NativeEncodedImage,
            info: *const NativeCodecSpecificInfo,
        ) -> FfiEncodedCallbackResult;
        fn encoded_callback_dropped(
            callback: &NativeEncodedImageCallback,
            timestamp: u32,
            spatial_id: i32,
            end: bool,
        ) -> bool;
        fn encoded_callback_emit(
            callback: &NativeEncodedImageCallback,
            data: &[u8],
            width: u32,
            height: u32,
            rtp_timestamp: u32,
            key_frame: bool,
            qp: i32,
            metadata: &FfiEncodedVideoMetadata,
        ) -> bool;
        fn decoded_callback_frame(
            callback: &NativeDecodedImageCallback,
            frame: Pin<&mut NativeRawVideoFrame>,
            overload: u8,
            time_ms: i64,
            has_time: bool,
            optional_time_ms: i32,
            has_qp: bool,
            qp: u8,
        ) -> FfiDecodedCallbackResult;
        fn decoded_callback_retired(callback: &NativeDecodedImageCallback) -> bool;
        fn decoded_callback_same(
            a: &NativeDecodedImageCallback,
            b: &NativeDecodedImageCallback,
        ) -> bool;
        fn decoded_callback_emit(
            callback: &NativeDecodedImageCallback,
            data: &[u8],
            width: u32,
            height: u32,
            timestamp_us: i64,
            rtp_timestamp: u32,
            rotation: u16,
        ) -> bool;
        #[allow(dead_code)]
        fn test_codec_roundtrip(
            encoder: &NativeVideoEncoderFactory,
            decoder: &NativeVideoDecoderFactory,
            frames: u32,
        ) -> FfiCodecTestResult;
        #[allow(dead_code)]
        fn test_encoder_factory_cross_thread(factory: &NativeVideoEncoderFactory) -> bool;

        unsafe fn new_peer_connection_factory(
            environment: &NativeEnvironment,
            network_thread: &NativeThread,
            worker_thread: &NativeThread,
            signaling_thread: &NativeThread,
            network_manager: *const NativeNetworkManagerProvider,
            network_manager_factory: Box<RustNetworkManagerFactory>,
            network_monitor_factory: Box<RustNetworkMonitorFactory>,
            network_predictor_factory: Box<RustNetworkStatePredictorFactory>,
            fec_factory: Box<RustFecControllerFactory>,
            network_controller_factory: Box<RustNetworkControllerFactory>,
            packet_socket_factory: *const NativePacketSocketFactoryProvider,
            socket_factory: Box<RustPacketSocketFactory>,
            audio_encoder: *const NativeAudioEncoderFactory,
            audio_decoder: *const NativeAudioDecoderFactory,
            video_encoder: *const NativeVideoEncoderFactory,
            video_decoder: *const NativeVideoDecoderFactory,
            video_creation_environment: Box<RustVideoFactoryEnvironment>,
            production_system_clock: bool,
            native_audio: bool,
            native_audio_creation: &FfiNativeAudioDeviceCreation,
            audio_device_module: UniquePtr<NativeAudioDevice>,
            event_logging: bool,
            event_log_retention: Box<RustEventLogRetention>,
            event_log_factory: *const NativeRtcEventLogFactory,
            encode_metronome: Box<RustMetronome>,
            decode_metronome: Box<RustMetronome>,
            readiness: *const NativeReadiness,
            codec_core_budget: i32,
            processing: &FfiAudioProcessingConfig,
            audio_mixer: UniquePtr<NativeAudioMixer>,
            audio_processor: UniquePtr<NativeInstalledAudioProcessor>,
            error: &mut String,
        ) -> UniquePtr<NativePeerConnectionFactory>;
        fn certificate_default_key_params(rsa: bool) -> FfiCertificateKeyParams;
        fn certificate_key_params_valid(params: &FfiCertificateKeyParams) -> bool;
        fn certificate_generate(
            params: &FfiCertificateKeyParams,
            has_lifetime: bool,
            lifetime_ms: u64,
        ) -> SharedPtr<NativeCertificate>;
        fn certificate_from_pem(
            private_key: &str,
            certificate: &str,
            chain: bool,
        ) -> SharedPtr<NativeCertificate>;
        fn certificate_to_pem(certificate: &NativeCertificate) -> FfiCertificatePem;
        fn certificate_expires(certificate: &NativeCertificate) -> u64;
        fn certificate_has_expired(certificate: &NativeCertificate, now: u64) -> bool;
        fn certificate_chain(certificate: &NativeCertificate) -> Vec<FfiCertificateData>;
        fn certificate_fingerprint(
            certificate: &NativeCertificate,
            algorithm: &str,
        ) -> FfiCertificateFingerprint;
        fn certificate_equal(a: &NativeCertificate, b: &NativeCertificate) -> bool;
        fn certificate_same_resource(a: &NativeCertificate, b: &NativeCertificate) -> bool;
        fn crypto_default_options() -> FfiCryptoOptions;
        fn crypto_supported_srtp_suites(options: &FfiCryptoOptions, suites: &mut Vec<u16>) -> bool;
        fn crypto_supported_key_exchange_groups() -> Vec<u16>;
        fn crypto_key_exchange_group_name(group: u16, name: &mut String) -> bool;
        fn crypto_update_key_exchange_groups(
            environment: &NativeEnvironment,
            options: &FfiCryptoOptions,
            disabled: &[u16],
        ) -> FfiCryptoOptions;
        fn factory_set_options(factory: &NativePeerConnectionFactory, options: &FfiFactoryOptions);
        fn factory_audio_encoder_failure_observer(
            factory: &NativePeerConnectionFactory,
        ) -> UniquePtr<NativeAudioEncoderFailureObserver>;
        fn factory_audio_processor_config(
            factory: &NativePeerConnectionFactory,
            error: &mut u8,
        ) -> FfiApmConfig;
        fn factory_audio_processor_apply(
            factory: &NativePeerConnectionFactory,
            config: &FfiApmConfig,
        ) -> u8;
        fn factory_audio_processor_reverse(
            factory: &NativePeerConnectionFactory,
            rate: i32,
            channels: usize,
            samples: &mut [i16],
            error: &mut u8,
        ) -> i32;
        fn factory_audio_processor_delay(
            factory: &NativePeerConnectionFactory,
            delay: i32,
            error: &mut u8,
        ) -> i32;
        fn factory_audio_processing_state(
            factory: &NativePeerConnectionFactory,
        ) -> FfiAudioProcessingState;
        fn factory_audio_devices(
            factory: &NativePeerConnectionFactory,
            recording: bool,
            devices: &mut Vec<FfiAudioDevice>,
            error: &mut String,
        ) -> bool;
        fn factory_select_audio_device(
            factory: &NativePeerConnectionFactory,
            recording: bool,
            index: u16,
            error: &mut String,
        ) -> bool;
        type NativePeerConfiguration;
        fn peer_configuration_default(aggressive: bool) -> FfiPeerConfiguration;
        fn peer_configuration_new(
            values: &FfiPeerConfiguration,
        ) -> UniquePtr<NativePeerConfiguration>;
        fn peer_configuration_add_certificate(
            config: Pin<&mut NativePeerConfiguration>,
            certificate: &NativeCertificate,
        );
        fn peer_configuration_values(config: &NativePeerConfiguration) -> FfiPeerConfiguration;
        fn peer_configuration_certificate_count(config: &NativePeerConfiguration) -> usize;
        fn peer_configuration_certificate(
            config: &NativePeerConfiguration,
            index: usize,
        ) -> SharedPtr<NativeCertificate>;
        fn peer_call_admission_stats(peer: &NativePeerConnection) -> FfiCallAdmissionStats;
        fn peer_get_configuration(
            peer: &NativePeerConnection,
        ) -> UniquePtr<NativePeerConfiguration>;
        fn peer_set_configuration(
            peer: &NativePeerConnection,
            config: &NativePeerConfiguration,
            error: &mut String,
        ) -> u8;
        fn create_peer_connection(
            factory: &NativePeerConnectionFactory,
            config: &NativePeerConfiguration,
            field_trials: Box<RustFieldTrials>,
            tls_verifier: Box<RustTlsCertificateVerifier>,
            local_permission: Box<RustLocalNetworkPermissionFactory>,
            certificate_generator: Box<RustCertificateGenerator>,
            async_dns_factory: Box<RustAsyncDnsFactory>,
            video_bitrate_factory: Box<RustVideoBitrateAllocatorFactory>,
            error_kind: &mut u8,
            error: &mut String,
        ) -> UniquePtr<NativePeerConnection>;
        fn peer_create_offer(
            peer: &NativePeerConnection,
            operation_id: u64,
            options: &FfiOfferAnswerOptions,
        ) -> u8;
        fn peer_create_answer(
            peer: &NativePeerConnection,
            operation_id: u64,
            options: &FfiOfferAnswerOptions,
        ) -> u8;
        fn peer_start_event_log(
            peer: &NativePeerConnection,
            output: UniquePtr<NativeEventLogOutput>,
            has_period: bool,
            period_ms: i64,
        ) -> u8;
        fn peer_stop_event_log(peer: &NativePeerConnection) -> u8;
        fn peer_restart_ice(peer: &NativePeerConnection) -> bool;
        fn peer_should_fire_negotiation_needed(peer: &NativePeerConnection, event_id: u32) -> bool;
        fn peer_set_local_description_auto(
            peer: &NativePeerConnection,
            operation_id: u64,
            legacy: bool,
        ) -> u8;
        fn peer_modify_ice_candidate(
            peer: &NativePeerConnection,
            remove: bool,
            sdp_mid: &[u8],
            sdp_mline_index: i32,
            candidate: &[u8],
            message: &mut Vec<u8>,
        ) -> u8;
        fn peer_request_stats(peer: &NativePeerConnection, operation_id: u64) -> u8;
        fn peer_request_sender_stats(
            peer: &NativePeerConnection,
            sender: &NativeRtpSender,
            operation_id: u64,
        ) -> u8;
        fn peer_request_receiver_stats(
            peer: &NativePeerConnection,
            receiver: &NativeRtpReceiver,
            operation_id: u64,
        ) -> u8;
        fn peer_request_stats_with_empty_selector(
            peer: &NativePeerConnection,
            receiver: bool,
            operation_id: u64,
        ) -> u8;
        fn peer_request_legacy_stats(
            peer: &NativePeerConnection,
            level: u8,
            operation_id: u64,
        ) -> u8;
        fn peer_request_legacy_stats_with_track(
            peer: &NativePeerConnection,
            selector: &NativeMediaTrack,
            level: u8,
            operation_id: u64,
        ) -> u8;
        fn peer_clear_stats_cache(peer: &NativePeerConnection);
        fn peer_set_bitrate(
            peer: &NativePeerConnection,
            minimum: i32,
            start: i32,
            maximum: i32,
            error_type: &mut u8,
            message: &mut String,
        ) -> bool;
        fn peer_native_state(peer: &NativePeerConnection, field: u8) -> u8;
        fn peer_ice_snapshot(peer: &NativePeerConnection) -> FfiPeerIceSnapshot;
        fn peer_descriptions(
            peer: &NativePeerConnection,
            descriptions: &mut Vec<FfiDescriptionSnapshot>,
        ) -> u8;
        fn peer_set_local_description(
            peer: &NativePeerConnection,
            operation_id: u64,
            sdp_type: u8,
            sdp: &[u8],
            legacy: bool,
        ) -> u8;
        fn peer_set_remote_description(
            peer: &NativePeerConnection,
            operation_id: u64,
            sdp_type: u8,
            sdp: &[u8],
            legacy: bool,
        ) -> u8;
        fn peer_add_ice_candidate(
            peer: &NativePeerConnection,
            operation_id: u64,
            sdp_mid: &[u8],
            sdp_mline_index: i32,
            candidate: &[u8],
        ) -> u8;
        fn peer_take_event(peer: &NativePeerConnection) -> FfiPeerEvent;
        fn peer_event_observation(peer: &NativePeerConnection) -> FfiPeerEventObservation;
        fn test_peer_candidate_observation(
            peer: &NativePeerConnection,
            mid: &[u8],
            index: i32,
            sdp: &[u8],
            removed: bool,
        ) -> bool;
        fn test_peer_ice_observations(
            peer: &NativePeerConnection,
            legacy: i32,
            standardized: i32,
            repetitions: u32,
        ) -> bool;
        fn test_peer_control_observations(
            peer: &NativePeerConnection,
            message_bytes: u32,
            arrivals: bool,
        ) -> bool;
        fn peer_take_data_channel(
            peer: &NativePeerConnection,
            arrival_id: u64,
        ) -> UniquePtr<NativeDataChannel>;
        fn factory_in_native_execution_context(factory: &NativePeerConnectionFactory) -> bool;
        fn factory_control_call_allowed(factory: &NativePeerConnectionFactory) -> bool;
        fn peer_control_call_allowed(peer: &NativePeerConnection) -> bool;
        fn close_peer_connection(peer: &NativePeerConnection) -> bool;
        fn peer_set_native_audio_enabled(
            peer: &NativePeerConnection,
            recording: bool,
            enabled: bool,
            error: &mut String,
        ) -> bool;

        fn create_data_channel(
            peer: &NativePeerConnection,
            label: &[u8],
            configuration: &FfiDataChannelInit,
            error_type: &mut u8,
            error: &mut Vec<u8>,
        ) -> UniquePtr<NativeDataChannel>;
        fn data_channel_label(channel: &NativeDataChannel) -> Vec<u8>;
        fn data_channel_ordered(channel: &NativeDataChannel) -> bool;
        fn data_channel_reliable(channel: &NativeDataChannel) -> bool;
        fn data_channel_counters(channel: &NativeDataChannel) -> FfiDataChannelCounters;
        fn data_channel_max_retransmit_time_ms(channel: &NativeDataChannel) -> i32;
        fn data_channel_max_retransmits(channel: &NativeDataChannel) -> i32;
        fn data_channel_protocol(channel: &NativeDataChannel) -> Vec<u8>;
        fn data_channel_negotiated(channel: &NativeDataChannel) -> bool;
        fn data_channel_id(channel: &NativeDataChannel) -> i32;
        fn data_channel_priority(channel: &NativeDataChannel) -> u16;
        fn data_channel_state(channel: &NativeDataChannel) -> u8;
        fn data_channel_buffered_amount(channel: &NativeDataChannel) -> u64;
        fn data_channel_send(channel: &NativeDataChannel, data: &[u8], binary: bool) -> u8;
        fn data_channel_send_async(channel: &NativeDataChannel, data: &[u8], binary: bool) -> u64;
        fn data_channel_receive_queue_limits() -> FfiDataChannelReceiveQueueLimits;
        fn data_channel_take_event(channel: &NativeDataChannel) -> FfiDataChannelEvent;
        fn close_data_channel(channel: &NativeDataChannel) -> bool;
        fn data_channel_error(channel: &NativeDataChannel) -> FfiDataChannelError;
        fn data_channel_send_queue_capacity() -> u64;
        fn data_channel_set_event_observation(channel: &NativeDataChannel, enabled: bool);

        fn audio_source_push_opus(
            source: &NativeAudioSource,
            payload: &[u8],
            rtp_timestamp: u32,
            samples_per_channel: u32,
        ) -> bool;
        fn audio_source_push_opus_at(
            source: &NativeAudioSource,
            payload: &[u8],
            rtp_timestamp: u32,
            samples_per_channel: u32,
            capture_time_us: i64,
        ) -> bool;
        fn audio_source_push_opus_with_level_at(
            source: &NativeAudioSource,
            payload: &[u8],
            rtp_timestamp: u32,
            samples_per_channel: u32,
            capture_time_us: i64,
            has_level: bool,
            level_dbov: u8,
            voice_activity: bool,
        ) -> bool;
        fn create_audio_source(
            factory: &NativePeerConnectionFactory,
            failure: &mut u8,
        ) -> UniquePtr<NativeAudioSource>;
        fn create_encoded_audio_source(
            factory: &NativePeerConnectionFactory,
            channels: u8,
        ) -> UniquePtr<NativeAudioSource>;
        fn peer_audio_codec_capabilities(
            factory: &NativePeerConnectionFactory,
            sender: bool,
        ) -> Vec<FfiAudioCodecCapability>;
        fn rtp_transceiver_set_audio_codec_preferences(
            transceiver: &NativeRtpTransceiver,
            factory: &NativePeerConnectionFactory,
            formats: &[FfiCodecFormat],
            error_type: &mut u8,
            error: &mut String,
        ) -> bool;
        fn audio_source_capture_active(source: &NativeAudioSource) -> bool;
        fn close_audio_source(source: &NativeAudioSource) -> bool;
        fn audio_source_push_pcm(
            source: &NativeAudioSource,
            samples: &[i16],
            sample_rate_hz: u32,
            channels: u8,
            timestamp_us: i64,
            capture_time_us: i64,
            failure: &mut u8,
            processing_error: &mut i32,
            wrapper_error: &mut u8,
        ) -> bool;
        fn audio_track_attach_sink(track: &NativeAudioTrack) -> UniquePtr<NativeAudioSink>;
        fn rtp_sender_set_audio_track(sender: &NativeRtpSender, track: &NativeAudioTrack) -> bool;
        fn create_microphone_track(
            factory: &NativePeerConnectionFactory,
            id: &str,
        ) -> UniquePtr<NativeAudioTrack>;
        fn create_audio_track(
            factory: &NativePeerConnectionFactory,
            source: &NativeAudioSource,
            id: &str,
        ) -> UniquePtr<NativeAudioTrack>;
        fn audio_options_default() -> FfiAudioSourceOptions;
        fn audio_options_equal(a: &FfiAudioSourceOptions, b: &FfiAudioSourceOptions) -> bool;
        fn audio_options_set_all(
            value: &FfiAudioSourceOptions,
            changes: &FfiAudioSourceOptions,
        ) -> FfiAudioSourceOptions;
        fn audio_options_description(value: &FfiAudioSourceOptions) -> String;
        type NativeAudioTrackProcessor;
        fn audio_track_processor(track: &NativeAudioTrack) -> UniquePtr<NativeAudioTrackProcessor>;
        fn audio_track_processors_same(
            a: &NativeAudioTrackProcessor,
            b: &NativeAudioTrackProcessor,
        ) -> bool;
        fn audio_track_processor_stats(
            processor: &NativeAudioTrackProcessor,
            has_remote_tracks: bool,
        ) -> FfiAudioProcessorStatistics;
        fn audio_track_processor_statistics_default() -> FfiAudioProcessorStatistics;
        type NativeAudioSourceView;
        type NativeAudioObserver;
        fn audio_source_view(source: &NativeAudioSource) -> UniquePtr<NativeAudioSourceView>;
        fn audio_track_source_view(track: &NativeAudioTrack) -> UniquePtr<NativeAudioSourceView>;
        fn rtp_receiver_audio_track(
            peer: &NativePeerConnection,
            receiver: &NativeRtpReceiver,
        ) -> UniquePtr<NativeAudioTrack>;
        fn rtp_sender_audio_track(
            peer: &NativePeerConnection,
            sender: &NativeRtpSender,
        ) -> UniquePtr<NativeAudioTrack>;
        fn audio_tracks_same(a: &NativeAudioTrack, b: &NativeAudioTrack) -> bool;
        fn audio_source_view_same(a: &NativeAudioSourceView, b: &NativeAudioSourceView) -> bool;
        fn audio_source_view_state(view: &NativeAudioSourceView) -> u8;
        fn audio_source_view_remote(view: &NativeAudioSourceView) -> bool;
        fn audio_source_view_options(view: &NativeAudioSourceView) -> FfiAudioSourceOptions;
        fn audio_source_view_set_options(
            view: &NativeAudioSourceView,
            options: &FfiAudioSourceOptions,
        ) -> u8;
        fn audio_source_view_volume(view: &NativeAudioSourceView) -> f64;
        fn audio_source_view_set_volume(view: &NativeAudioSourceView, volume: f64) -> u8;
        fn audio_source_view_observe(
            view: &NativeAudioSourceView,
            volume: bool,
            capacity: usize,
        ) -> UniquePtr<NativeAudioObserver>;
        fn audio_track_observe(
            track: &NativeAudioTrack,
            capacity: usize,
        ) -> UniquePtr<NativeAudioObserver>;
        fn audio_observer_take(observer: &NativeAudioObserver) -> FfiAudioObserverEvent;
        fn audio_observer_take_dropped(observer: &NativeAudioObserver) -> u64;
        fn audio_observer_close(observer: &NativeAudioObserver);
        fn audio_source_set_options(
            source: &NativeAudioSource,
            options: &FfiAudioSourceOptions,
        ) -> u8;
        fn audio_source_set_volume(source: &NativeAudioSource, volume: f64) -> u8;
        fn audio_track_set_volume(track: &NativeAudioTrack, volume: f64) -> u8;
        fn rtp_sender_set_audio_track_volume(
            peer: &NativePeerConnection,
            sender: &NativeRtpSender,
            volume: f64,
        ) -> u8;
        fn audio_source_options(source: &NativeAudioSource) -> FfiAudioSourceOptions;
        fn audio_source_state(source: &NativeAudioSource) -> u8;
        fn audio_source_remote(source: &NativeAudioSource) -> bool;
        fn audio_source_volume(source: &NativeAudioSource) -> f64;
        fn audio_track_status(track: &NativeAudioTrack) -> FfiAudioTrackStatus;
        fn rtp_sender_audio_track_status(
            peer: &NativePeerConnection,
            sender: &NativeRtpSender,
        ) -> FfiAudioTrackStatus;
        fn audio_track_state(track: &NativeAudioTrack) -> u8;
        fn audio_track_signal_level(track: &NativeAudioTrack) -> FfiOptionalI32;
        fn audio_track_volume(track: &NativeAudioTrack) -> f64;
        fn audio_track_kind(track: &NativeAudioTrack) -> String;
        fn audio_track_id(track: &NativeAudioTrack) -> String;
        fn audio_track_enabled(track: &NativeAudioTrack) -> bool;
        fn audio_track_set_enabled(track: &NativeAudioTrack, enabled: bool) -> bool;
        fn audio_track_set_processing_options(
            track: &NativeAudioTrack,
            echo: u8,
            noise: u8,
            gain: u8,
            error: &mut String,
        ) -> bool;
        fn peer_add_audio_transceiver(
            peer: &NativePeerConnection,
            track: &NativeAudioTrack,
            direction: u8,
            error_type: &mut u8,
            error: &mut String,
        ) -> UniquePtr<NativeRtpTransceiver>;
        fn rtp_receiver_attach_audio_sink(
            peer: &NativePeerConnection,
            receiver: &NativeRtpReceiver,
        ) -> UniquePtr<NativeAudioSink>;
        fn audio_sink_take_frame(sink: &NativeAudioSink) -> FfiReceivedAudioFrame;
        fn audio_sink_dropped_frames(sink: &NativeAudioSink) -> u64;
        fn close_audio_sink(sink: &NativeAudioSink) -> bool;
        fn rtp_receiver_attach_encoded_audio_sink(
            peer: &NativePeerConnection,
            receiver: &NativeRtpReceiver,
        ) -> UniquePtr<NativeEncodedAudioSink>;
        fn encoded_audio_sink_take_frame(sink: &NativeEncodedAudioSink) -> FfiEncodedAudioFrame;
        fn encoded_audio_sink_dropped_frames(sink: &NativeEncodedAudioSink) -> u64;
        fn close_encoded_audio_sink(sink: &NativeEncodedAudioSink) -> bool;

        fn screen_sources(
            windows: bool,
            screens: &mut Vec<FfiScreenSource>,
            error: &mut String,
        ) -> bool;
        fn open_screen(
            source: &NativeVideoSource,
            screen_id: i64,
            window: bool,
            error: &mut String,
        ) -> UniquePtr<NativeScreenCapture>;
        fn screen_capture_next_frame(capture: &NativeScreenCapture) -> bool;
        fn screen_capture_status(capture: &NativeScreenCapture) -> u8;
        fn screen_capture_failed_frames(capture: &NativeScreenCapture) -> u64;
        fn close_screen(capture: &NativeScreenCapture) -> bool;
        fn desktop_control(
            capture: &NativeScreenCapture,
            kind: u8,
            id: i64,
            x: i32,
            y: i32,
            width: u32,
            height: u32,
        ) -> FfiDesktopControl;
        type NativeDesktopMetadata;
        fn desktop_capture_metadata(
            capture: &NativeScreenCapture,
        ) -> UniquePtr<NativeDesktopMetadata>;
        fn desktop_metadata_clone(
            metadata: &NativeDesktopMetadata,
        ) -> UniquePtr<NativeDesktopMetadata>;
        fn desktop_metadata_read(metadata: &NativeDesktopMetadata) -> FfiDesktopMetadata;
        fn desktop_metadata_equal(
            a: &NativeDesktopMetadata,
            b: &NativeDesktopMetadata,
            cancellable: bool,
        ) -> bool;
        fn desktop_set_memory_factory(
            capture: &NativeScreenCapture,
            source: Box<RustDesktopMemoryFactory>,
        ) -> u8;
        fn desktop_set_receiver(
            capture: &NativeScreenCapture,
            source: Box<RustDesktopCaptureReceiver>,
        ) -> u8;
        fn desktop_set_selection_observer(
            capture: &NativeScreenCapture,
            source: Box<RustDesktopSelectionObserver>,
        ) -> u8;
        fn camera_devices(devices: &mut Vec<FfiCameraDevice>, error: &mut String) -> bool;
        fn camera_device_control(
            device_id: &str,
            kind: u8,
            settings: &FfiCameraSettings,
        ) -> FfiCameraDeviceControl;
        fn camera_formats(
            device_id: &str,
            formats: &mut Vec<FfiCameraFormat>,
            error: &mut String,
        ) -> bool;
        fn create_camera(
            source: &NativeVideoSource,
            device_id: &str,
            error: &mut String,
        ) -> UniquePtr<NativeCamera>;
        fn create_camera_with_options(
            source: &NativeVideoSource,
            options: &NativeCameraOptions,
            device_id: &str,
            error: &mut String,
        ) -> UniquePtr<NativeCamera>;
        fn camera_set_raw_sink(camera: &NativeCamera, source: Box<RustRawCameraSink>) -> u8;
        fn open_camera(
            source: &NativeVideoSource,
            device_id: &str,
            width: u32,
            height: u32,
            fps: u32,
            error: &mut String,
        ) -> UniquePtr<NativeCamera>;
        fn camera_capture_status(camera: &NativeCamera, stale_after_ms: u64) -> u8;
        fn close_camera(camera: &NativeCamera) -> bool;
        fn camera_control(
            camera: &NativeCamera,
            kind: u8,
            rotation: u16,
            flag: bool,
            settings: &FfiCameraSettings,
        ) -> FfiCameraControl;
        fn create_video_source(
            factory: &NativePeerConnectionFactory,
        ) -> UniquePtr<NativeVideoSource>;
        fn close_video_source(source: &NativeVideoSource) -> bool;
        fn video_source_state(source: &NativeVideoSource) -> u8;
        fn video_source_rejected_sink_requests(source: &NativeVideoSource) -> u64;
        fn video_source_view_rejected_sink_requests(
            source: &NativeVideoSourceView,
            count: &mut u64,
        ) -> bool;
        fn video_source_as_view(source: &NativeVideoSource) -> UniquePtr<NativeVideoSourceView>;
        fn video_track_source(
            track: &NativeVideoTrack,
            factory: &NativePeerConnectionFactory,
        ) -> UniquePtr<NativeVideoSourceView>;
        fn video_track_source_from_producer(
            track: &NativeVideoTrack,
            factory: &NativePeerConnectionFactory,
            source: &NativeVideoSource,
        ) -> UniquePtr<NativeVideoSourceView>;
        fn video_source_view_request_refresh(source: &NativeVideoSourceView);
        fn video_source_view_generate_key_frame(source: &NativeVideoSourceView);
        fn video_source_view_process_constraints(
            source: &NativeVideoSourceView,
            has_min: bool,
            min: f64,
            has_max: bool,
            max: f64,
        );
        fn video_track_request_refresh(track: &NativeVideoTrack);
        fn video_source_views_same_interface(
            a: &NativeVideoSourceView,
            b: &NativeVideoSourceView,
        ) -> bool;
        fn video_source_view_state(source: &NativeVideoSourceView) -> u8;
        fn video_source_view_remote(source: &NativeVideoSourceView) -> bool;
        fn video_source_view_screencast(source: &NativeVideoSourceView) -> bool;
        fn video_source_view_denoising(source: &NativeVideoSourceView) -> i8;
        fn video_source_view_encoded_output(source: &NativeVideoSourceView) -> bool;
        fn video_source_view_stats(source: &NativeVideoSourceView) -> FfiVideoSourceStats;
        fn video_source_view_observe(
            source: &NativeVideoSourceView,
            capacity: usize,
        ) -> UniquePtr<NativeVideoObserver>;
        fn video_track_observe(
            track: &NativeVideoTrack,
            factory: &NativePeerConnectionFactory,
            capacity: usize,
        ) -> UniquePtr<NativeVideoObserver>;
        fn video_observer_take(observer: &NativeVideoObserver) -> bool;
        fn video_observer_take_dropped(observer: &NativeVideoObserver) -> u64;
        fn video_observer_close(observer: &NativeVideoObserver);
        fn video_source_push_encoded_trigger(
            source: &NativeVideoSource,
            width: u32,
            height: u32,
            timestamp_us: i64,
            token: i64,
        ) -> bool;
        type NativeLocalMediaStream;
        type NativeLocalMediaStreamObserver;
        fn create_media_stream(
            factory: &NativePeerConnectionFactory,
            id: &str,
        ) -> UniquePtr<NativeLocalMediaStream>;
        fn local_media_stream_id(stream: &NativeLocalMediaStream) -> String;
        fn local_media_stream_track_keys(
            stream: &NativeLocalMediaStream,
            audio: bool,
        ) -> Vec<usize>;
        fn local_media_stream_find_track(
            stream: &NativeLocalMediaStream,
            audio: bool,
            id: &str,
        ) -> usize;
        fn audio_track_identity(track: &NativeAudioTrack) -> usize;
        fn video_track_identity(track: &NativeVideoTrack) -> usize;
        fn local_media_stream_audio_track(
            stream: &NativeLocalMediaStream,
            track: &NativeAudioTrack,
            add: bool,
        ) -> bool;
        fn local_media_stream_video_track(
            stream: &NativeLocalMediaStream,
            track: &NativeVideoTrack,
            add: bool,
        ) -> bool;
        fn local_media_stream_observe(
            stream: &NativeLocalMediaStream,
            capacity: usize,
        ) -> UniquePtr<NativeLocalMediaStreamObserver>;
        fn local_media_stream_observer_take(observer: &NativeLocalMediaStreamObserver) -> bool;
        fn local_media_stream_observer_take_dropped(
            observer: &NativeLocalMediaStreamObserver,
        ) -> u64;
        fn local_media_stream_observer_close(observer: &NativeLocalMediaStreamObserver);
        fn video_source_i420_allocation_size(width: u32, height: u32) -> usize;
        fn video_source_push_native_frame(
            source: &NativeVideoSource,
            frame: &NativeRawVideoFrame,
        ) -> bool;
        fn video_sink_take_native_frame(sink: &NativeVideoSink) -> UniquePtr<NativeRawVideoFrame>;
        fn video_source_push_frame(
            source: &NativeVideoSource,
            data: &[u8],
            width: u32,
            height: u32,
            timestamp_us: i64,
            rtp_timestamp: u32,
            rotation: u16,
        ) -> bool;
        fn create_video_track(
            factory: &NativePeerConnectionFactory,
            source: &NativeVideoSource,
            id: &str,
        ) -> UniquePtr<NativeVideoTrack>;
        fn video_track_id(track: &NativeVideoTrack) -> String;
        fn video_track_enabled(track: &NativeVideoTrack) -> bool;
        fn video_track_set_enabled(track: &NativeVideoTrack, enabled: bool) -> bool;
        fn video_track_state(track: &NativeVideoTrack) -> u8;
        fn video_track_kind(track: &NativeVideoTrack) -> String;
        fn video_track_should_receive(track: &NativeVideoTrack) -> bool;
        fn video_track_set_should_receive(track: &NativeVideoTrack, receive: bool);
        fn video_track_content_hint(track: &NativeVideoTrack) -> u8;
        fn video_track_set_content_hint(track: &NativeVideoTrack, hint: u8);
        fn video_source_attach_encoded_output(
            source: &NativeVideoSourceView,
        ) -> UniquePtr<NativeVideoSourceEncodedSink>;
        fn video_source_encoded_take_frame(
            sink: &NativeVideoSourceEncodedSink,
        ) -> FfiRecordableVideoFrame;
        fn video_source_encoded_take_event(
            sink: &NativeVideoSourceEncodedSink,
        ) -> FfiVideoSinkEvent;
        fn video_source_encoded_dropped_frames(sink: &NativeVideoSourceEncodedSink) -> u64;
        fn video_source_encoded_dropped_events(sink: &NativeVideoSourceEncodedSink) -> u64;
        fn video_source_encoded_close(sink: &NativeVideoSourceEncodedSink);
        fn video_sink_default_wants() -> FfiVideoSinkWants;
        fn video_track_attach_sink(
            track: &NativeVideoTrack,
            source: &NativeVideoSourceView,
            wants: &FfiVideoSinkWants,
            status: &mut u8,
        ) -> UniquePtr<NativeVideoSink>;
        fn video_track_attach_observed_sink(
            track: &NativeVideoTrack,
            source: &NativeVideoSourceView,
            wants: &FfiVideoSinkWants,
            observer: Box<RustDecodedVideoObserver>,
            status: &mut u8,
        ) -> UniquePtr<NativeVideoSink>;
        fn video_source_attach_sink(
            source: &NativeVideoSourceView,
            wants: &FfiVideoSinkWants,
            status: &mut u8,
        ) -> UniquePtr<NativeVideoSink>;
        fn video_sink_update_wants(sink: &NativeVideoSink, wants: &FfiVideoSinkWants) -> u8;
        fn video_sink_wants(sink: &NativeVideoSink) -> FfiVideoSinkWants;
        fn video_sink_take_frame(sink: &NativeVideoSink) -> UniquePtr<NativeVideoFrame>;
        fn video_sink_dropped_frames(sink: &NativeVideoSink) -> u64;
        fn video_sink_take_event(sink: &NativeVideoSink) -> FfiVideoSinkEvent;
        fn video_sink_dropped_events(sink: &NativeVideoSink) -> u64;
        fn close_video_sink(sink: &NativeVideoSink) -> bool;
        fn peer_audio_transceivers(peer: &NativePeerConnection)
        -> UniquePtr<NativeTransceiverList>;
        fn peer_video_transceivers(peer: &NativePeerConnection)
        -> UniquePtr<NativeTransceiverList>;
        fn peer_video_codec_capabilities(
            factory: &NativePeerConnectionFactory,
            sender: bool,
        ) -> Vec<FfiVideoCodecCapability>;
        fn transceiver_list_len(list: &NativeTransceiverList) -> usize;
        fn transceiver_list_at(
            list: &NativeTransceiverList,
            index: usize,
        ) -> UniquePtr<NativeRtpTransceiver>;
        fn peer_add_transceiver_value(
            peer: &NativePeerConnection,
            kind: i32,
            init: &NativeRtpTransceiverInit,
            valid: &mut bool,
            error: &mut UniquePtr<NativeRtcError>,
        ) -> UniquePtr<NativeRtpTransceiver>;
        fn peer_add_audio_transceiver_value(
            peer: &NativePeerConnection,
            track: &NativeAudioTrack,
            init: &NativeRtpTransceiverInit,
            valid: &mut bool,
            error: &mut UniquePtr<NativeRtcError>,
        ) -> UniquePtr<NativeRtpTransceiver>;
        fn peer_add_video_transceiver_value(
            peer: &NativePeerConnection,
            track: &NativeVideoTrack,
            init: &NativeRtpTransceiverInit,
            valid: &mut bool,
            error: &mut UniquePtr<NativeRtcError>,
        ) -> UniquePtr<NativeRtpTransceiver>;
        fn peer_add_media_transceiver_value(
            peer: &NativePeerConnection,
            track: &NativeMediaTrack,
            init: &NativeRtpTransceiverInit,
            valid: &mut bool,
            error: &mut UniquePtr<NativeRtcError>,
        ) -> UniquePtr<NativeRtpTransceiver>;
        fn peer_add_video_transceiver(
            peer: &NativePeerConnection,
            track: &NativeVideoTrack,
            direction: u8,
            rids: &[String],
            error_type: &mut u8,
            error: &mut String,
        ) -> UniquePtr<NativeRtpTransceiver>;
        fn peer_remove_track(
            peer: &NativePeerConnection,
            sender: &NativeRtpSender,
            error_type: &mut u8,
            error: &mut String,
        ) -> bool;
        fn peer_take_transceiver(
            peer: &NativePeerConnection,
            arrival_id: u64,
        ) -> UniquePtr<NativeRtpTransceiver>;
        fn peer_take_receiver(
            peer: &NativePeerConnection,
            arrival_id: u64,
        ) -> UniquePtr<NativeRtpReceiver>;
        fn rtp_transceiver_sender(transceiver: &NativeRtpTransceiver)
        -> UniquePtr<NativeRtpSender>;
        fn rtp_transceiver_receiver(
            transceiver: &NativeRtpTransceiver,
        ) -> UniquePtr<NativeRtpReceiver>;
        fn rtp_transceiver_direction(transceiver: &NativeRtpTransceiver) -> u8;
        fn rtp_transceiver_current_direction(transceiver: &NativeRtpTransceiver) -> i8;
        fn rtp_transceiver_direction_value(transceiver: &NativeRtpTransceiver) -> i32;
        fn rtp_transceiver_current_direction_value(
            transceiver: &NativeRtpTransceiver,
            direction: &mut i32,
        ) -> bool;
        fn rtp_transceiver_codec_preferences_value(
            transceiver: &NativeRtpTransceiver,
        ) -> UniquePtr<NativeRtpCapabilities>;
        fn rtp_transceiver_header_capabilities_value(
            transceiver: &NativeRtpTransceiver,
            negotiated: bool,
        ) -> UniquePtr<NativeRtpCapabilities>;
        fn rtp_transceiver_set_codec_preferences_value(
            transceiver: &NativeRtpTransceiver,
            values: &NativeRtpCapabilities,
        ) -> UniquePtr<NativeRtcError>;
        fn rtp_transceiver_set_header_capabilities_value(
            transceiver: &NativeRtpTransceiver,
            values: &NativeRtpCapabilities,
        ) -> UniquePtr<NativeRtcError>;
        fn rtp_transceiver_header_extensions(
            transceiver: &NativeRtpTransceiver,
            negotiated: bool,
        ) -> Vec<FfiRtpHeaderExtension>;
        fn rtp_transceiver_set_header_extensions(
            transceiver: &NativeRtpTransceiver,
            extensions: &[FfiRtpHeaderExtension],
            error_type: &mut u8,
            error: &mut String,
        ) -> bool;
        fn rtp_transceiver_set_direction_value(
            transceiver: &NativeRtpTransceiver,
            direction: i32,
            safe: &mut bool,
        ) -> UniquePtr<NativeRtcError>;
        fn rtp_transceiver_stop_standard(
            transceiver: &NativeRtpTransceiver,
        ) -> UniquePtr<NativeRtcError>;
        fn rtp_transceiver_stopped(transceiver: &NativeRtpTransceiver) -> bool;
        fn rtp_transceiver_media_type(transceiver: &NativeRtpTransceiver) -> i32;
        fn rtp_transceiver_stopping(transceiver: &NativeRtpTransceiver) -> bool;
        fn rtp_transceiver_receptive(transceiver: &NativeRtpTransceiver) -> bool;
        fn rtp_transceiver_fired_direction(
            transceiver: &NativeRtpTransceiver,
            direction: &mut i32,
        ) -> bool;
        fn rtp_transceiver_sframe_enabled(
            transceiver: &NativeRtpTransceiver,
            enabled: &mut bool,
        ) -> bool;
        fn rtp_transceiver_mid(transceiver: &NativeRtpTransceiver, mid: &mut String) -> bool;
        fn rtp_transceiver_set_video_codec_preferences(
            transceiver: &NativeRtpTransceiver,
            factory: &NativePeerConnectionFactory,
            formats: &[FfiCodecFormat],
            error_type: &mut u8,
            error: &mut String,
        ) -> bool;
        fn rtp_transceiver_stop(
            transceiver: &NativeRtpTransceiver,
            error_type: &mut u8,
            error: &mut String,
        ) -> bool;
        fn rtp_transceiver_set_direction(
            transceiver: &NativeRtpTransceiver,
            direction: u8,
            error_type: &mut u8,
            error: &mut String,
        ) -> bool;
        fn rtp_sender_identity(sender: &NativeRtpSender) -> usize;
        fn rtp_sender_copy(sender: &NativeRtpSender) -> UniquePtr<NativeRtpSender>;
        fn rtp_sender_same_resource(first: &NativeRtpSender, second: &NativeRtpSender) -> bool;
        fn rtp_receiver_same_resource(
            first: &NativeRtpReceiver,
            second: &NativeRtpReceiver,
        ) -> bool;
        fn rtp_transceiver_same_resource(
            first: &NativeRtpTransceiver,
            second: &NativeRtpTransceiver,
        ) -> bool;
        fn dtmf_sender_identity(sender: &NativeDtmfSender) -> usize;
        fn rtp_sender_dtmf(
            sender: &NativeRtpSender,
            peer: &NativePeerConnection,
        ) -> UniquePtr<NativeDtmfSender>;
        fn dtmf_sender_can_insert(sender: &NativeDtmfSender) -> bool;
        fn dtmf_sender_insert(
            sender: &NativeDtmfSender,
            tones: &str,
            duration: i32,
            gap: i32,
            has_comma_delay: bool,
            comma_delay: i32,
        ) -> u8;
        fn dtmf_sender_tones(sender: &NativeDtmfSender) -> String;
        fn dtmf_sender_duration(sender: &NativeDtmfSender) -> i32;
        fn dtmf_sender_inter_tone_gap(sender: &NativeDtmfSender) -> i32;
        fn dtmf_sender_comma_delay(sender: &NativeDtmfSender) -> i32;
        fn dtmf_sender_register_observer(sender: &NativeDtmfSender, capacity: usize) -> bool;
        fn dtmf_sender_unregister_observer(sender: &NativeDtmfSender);
        fn dtmf_sender_take_tone_change(sender: &NativeDtmfSender) -> FfiDtmfToneChange;
        fn dtmf_sender_take_dropped(sender: &NativeDtmfSender) -> u64;
        fn media_type_to_string(kind: i32, name: &mut String) -> bool;
        fn rtp_sender_dtls(
            sender: &NativeRtpSender,
            peer: &NativePeerConnection,
        ) -> UniquePtr<NativeDtlsTransport>;
        fn rtp_receiver_dtls(
            receiver: &NativeRtpReceiver,
            peer: &NativePeerConnection,
        ) -> UniquePtr<NativeDtlsTransport>;
        fn rtp_sender_ssrc(sender: &NativeRtpSender) -> u32;
        fn rtp_sender_media_type(sender: &NativeRtpSender) -> i32;
        fn rtp_sender_stream_ids(sender: &NativeRtpSender) -> Vec<String>;
        fn rtp_sender_stream_id_bytes(sender: &NativeRtpSender) -> Vec<FfiRtpByteString>;
        fn rtp_sender_generate_keyframe(
            sender: &NativeRtpSender,
            peer: &NativePeerConnection,
            rids: &[FfiRtpByteString],
        ) -> UniquePtr<NativeRtcError>;
        fn rtp_sender_set_streams(sender: &NativeRtpSender, ids: &[String]);
        fn rtp_sender_set_stream_id_bytes(sender: &NativeRtpSender, ids: &[FfiRtpByteString]);
        fn rtp_sender_id(sender: &NativeRtpSender) -> String;
        fn rtp_sender_request_keyframe(
            sender: &NativeRtpSender,
            peer: &NativePeerConnection,
            rids: &[String],
            error_type: &mut u8,
            error: &mut String,
        ) -> bool;
        fn rtp_sender_get_parameters(
            sender: &NativeRtpSender,
            parameters: &mut FfiSenderParameters,
        ) -> bool;
        fn rtp_sender_set_parameters(
            sender: &NativeRtpSender,
            parameters: &FfiSenderParameters,
            error_type: &mut u8,
            error: &mut String,
        ) -> bool;
        fn rtp_sender_track(sender: &NativeRtpSender) -> UniquePtr<NativeVideoTrack>;
        fn rtp_sender_set_video_track(sender: &NativeRtpSender, track: &NativeVideoTrack) -> bool;
        type NativeSframeEncrypterInit;
        type NativeSframeEncrypter;
        type NativeSframeDecrypter;
        fn sframe_init_new(mode: i32, suite: i32) -> UniquePtr<NativeSframeEncrypterInit>;
        fn sframe_init_clone(
            source: &NativeSframeEncrypterInit,
        ) -> UniquePtr<NativeSframeEncrypterInit>;
        fn sframe_init_assign(
            target: Pin<&mut NativeSframeEncrypterInit>,
            source: &NativeSframeEncrypterInit,
        );
        fn sframe_init_fields(value: &NativeSframeEncrypterInit) -> FfiSframeInit;
        fn sframe_init_set_mode(value: Pin<&mut NativeSframeEncrypterInit>, mode: i32);
        fn sframe_init_set_suite(value: Pin<&mut NativeSframeEncrypterInit>, suite: i32);
        fn rtp_sender_create_sframe(
            sender: &NativeRtpSender,
            peer: &NativePeerConnection,
            init: &NativeSframeEncrypterInit,
        ) -> FfiSframeEncrypterCreation;
        fn rtp_receiver_create_sframe(
            receiver: &NativeRtpReceiver,
            peer: &NativePeerConnection,
            suite: i32,
        ) -> FfiSframeDecrypterCreation;
        fn sframe_encrypter_clone(
            value: &NativeSframeEncrypter,
        ) -> UniquePtr<NativeSframeEncrypter>;
        fn sframe_decrypter_clone(
            value: &NativeSframeDecrypter,
        ) -> UniquePtr<NativeSframeDecrypter>;
        fn sframe_set_encryption_key(
            value: &NativeSframeEncrypter,
            id: u64,
            key: &[u8],
        ) -> UniquePtr<NativeRtcError>;
        fn sframe_add_decryption_key(
            value: &NativeSframeDecrypter,
            id: u64,
            key: &[u8],
        ) -> UniquePtr<NativeRtcError>;
        fn sframe_remove_decryption_key(
            value: &NativeSframeDecrypter,
            id: u64,
        ) -> UniquePtr<NativeRtcError>;
        type NativeFrameDecryptionResult;
        fn frame_decryption_result_new(
            status: i32,
            written: usize,
        ) -> UniquePtr<NativeFrameDecryptionResult>;
        fn frame_decryption_result_clone(
            value: &NativeFrameDecryptionResult,
        ) -> UniquePtr<NativeFrameDecryptionResult>;
        fn frame_decryption_result_fields(
            value: &NativeFrameDecryptionResult,
        ) -> FfiFrameDecryptionResult;
        fn frame_decryption_result_is_ok(value: &NativeFrameDecryptionResult) -> bool;
        fn frame_decryption_result_assign(
            target: Pin<&mut NativeFrameDecryptionResult>,
            source: &NativeFrameDecryptionResult,
        );
        fn frame_decryption_result_set_status(
            value: Pin<&mut NativeFrameDecryptionResult>,
            status: i32,
        );
        fn frame_decryption_result_set_written(
            value: Pin<&mut NativeFrameDecryptionResult>,
            written: usize,
        );
        type NativeFrameTransformer;
        type NativeFrameTransformCompletion;
        type NativeFrameTransformCallback;
        fn frame_transform_completion_callback(
            completion: &NativeFrameTransformCompletion,
        ) -> UniquePtr<NativeFrameTransformCallback>;
        fn frame_transform_callback_copy(
            callback: &NativeFrameTransformCallback,
        ) -> UniquePtr<NativeFrameTransformCallback>;
        fn frame_transform_callback_same(
            a: &NativeFrameTransformCallback,
            b: &NativeFrameTransformCallback,
        ) -> bool;
        fn frame_transform_callback_binding(
            callback: &NativeFrameTransformCallback,
        ) -> FfiFrameTransformBinding;
        fn frame_transform_callback_retired(callback: &NativeFrameTransformCallback) -> bool;
        fn frame_transform_callback_short_circuit(callback: &NativeFrameTransformCallback) -> bool;
        fn frame_transform_callback_forward(
            callback: &NativeFrameTransformCallback,
            frame: UniquePtr<NativeTransformableFrame>,
        ) -> u8;
        unsafe fn rtp_sender_select_frame_transformer(
            peer: &NativePeerConnection,
            sender: &NativeRtpSender,
            provider: *const NativeFrameTransformer,
        ) -> bool;
        unsafe fn rtp_receiver_select_frame_transformer(
            peer: &NativePeerConnection,
            receiver: &NativeRtpReceiver,
            provider: *const NativeFrameTransformer,
        ) -> bool;
        fn frame_transformer_new(
            provider: Box<RustFrameTransformerProvider>,
            maximum_in_flight: usize,
        ) -> UniquePtr<NativeFrameTransformer>;
        fn frame_transformer_copy(
            value: &NativeFrameTransformer,
        ) -> UniquePtr<NativeFrameTransformer>;
        fn frame_transformer_same(a: &NativeFrameTransformer, b: &NativeFrameTransformer) -> bool;
        fn frame_transformer_close(value: &NativeFrameTransformer);
        fn frame_transformer_closed(value: &NativeFrameTransformer) -> bool;
        fn frame_transformer_statistics(
            value: &NativeFrameTransformer,
        ) -> FfiFrameTransformerStatistics;
        fn frame_transform_completion_binding(
            value: &NativeFrameTransformCompletion,
        ) -> FfiFrameTransformBinding;
        fn frame_transform_complete(
            completion: UniquePtr<NativeFrameTransformCompletion>,
            frame: UniquePtr<NativeTransformableFrame>,
        ) -> u8;
        fn frame_transform_short_circuit(value: &NativeFrameTransformCompletion) -> bool;
        type NativeFrameEncryptor;
        type NativeFrameDecryptor;
        fn frame_crypto_max_output_capacity() -> usize;
        fn frame_encryptor_new(
            provider: Box<RustFrameEncryptorProvider>,
        ) -> UniquePtr<NativeFrameEncryptor>;
        fn frame_encryptor_clone(value: &NativeFrameEncryptor) -> UniquePtr<NativeFrameEncryptor>;
        fn frame_encryptor_same(lhs: &NativeFrameEncryptor, rhs: &NativeFrameEncryptor) -> bool;
        fn frame_encryptor_failure(value: &NativeFrameEncryptor) -> FfiFrameCryptoFailure;
        fn frame_encryptor_max_size(value: &NativeFrameEncryptor, media: i32, size: usize)
        -> usize;
        fn frame_encryptor_encrypt(
            value: &NativeFrameEncryptor,
            media: i32,
            ssrc: u32,
            additional: &[u8],
            input: &[u8],
            capacity: usize,
        ) -> FfiFrameCryptoOutput;
        fn frame_decryptor_new(
            receiver: &NativeRtpReceiver,
            provider: Box<RustFrameDecryptorProvider>,
        ) -> UniquePtr<NativeFrameDecryptor>;
        fn frame_decryptor_clone(value: &NativeFrameDecryptor) -> UniquePtr<NativeFrameDecryptor>;
        fn frame_decryptor_same(lhs: &NativeFrameDecryptor, rhs: &NativeFrameDecryptor) -> bool;
        fn frame_decryptor_failure(value: &NativeFrameDecryptor) -> FfiFrameCryptoFailure;
        fn frame_decryptor_max_size(value: &NativeFrameDecryptor, media: i32, size: usize)
        -> usize;
        fn frame_decryptor_decrypt(
            value: &NativeFrameDecryptor,
            media: i32,
            csrcs: &[u32],
            additional: &[u8],
            input: &[u8],
            capacity: usize,
        ) -> FfiFrameCryptoOutput;
        unsafe fn rtp_sender_set_encoder_selector(
            sender: &NativeRtpSender,
            selector: *const NativeVideoEncoderSelector,
        );
        fn rtp_sender_set_owned_encoder_selector(
            sender: &NativeRtpSender,
            selector: UniquePtr<NativeOwnedVideoEncoderSelector>,
        );
        unsafe fn rtp_sender_set_frame_encryptor(
            sender: &NativeRtpSender,
            value: *const NativeFrameEncryptor,
        );
        fn rtp_sender_frame_encryptor(sender: &NativeRtpSender) -> UniquePtr<NativeFrameEncryptor>;
        unsafe fn rtp_receiver_set_frame_decryptor(
            receiver: &NativeRtpReceiver,
            value: *const NativeFrameDecryptor,
        ) -> bool;
        fn rtp_receiver_frame_decryptor(
            receiver: &NativeRtpReceiver,
        ) -> UniquePtr<NativeFrameDecryptor>;
        fn rtp_sender_clear_track(sender: &NativeRtpSender) -> bool;
        fn rtp_receiver_media_type(receiver: &NativeRtpReceiver) -> i32;
        fn rtp_receiver_stream_ids(receiver: &NativeRtpReceiver) -> Vec<String>;
        fn rtp_receiver_stream_id_bytes(receiver: &NativeRtpReceiver) -> Vec<FfiRtpByteString>;
        fn rtp_receiver_id_bytes(receiver: &NativeRtpReceiver) -> Vec<u8>;
        fn rtp_sender_id_bytes(sender: &NativeRtpSender) -> Vec<u8>;
        fn rtp_receiver_id(receiver: &NativeRtpReceiver) -> String;
        fn rtp_receiver_track(receiver: &NativeRtpReceiver) -> UniquePtr<NativeVideoTrack>;
        fn rtp_receiver_request_keyframe(
            peer: &NativePeerConnection,
            receiver: &NativeRtpReceiver,
        ) -> bool;
        fn rtp_receiver_attach_encoded_video_sink(
            peer: &NativePeerConnection,
            receiver: &NativeRtpReceiver,
            ordered: bool,
        ) -> UniquePtr<NativeEncodedVideoSink>;
        fn rtp_receiver_attach_observed_encoded_video_sink(
            peer: &NativePeerConnection,
            receiver: &NativeRtpReceiver,
            observer: Box<RustEncodedVideoObserver>,
        ) -> UniquePtr<NativeEncodedVideoSink>;
        fn encoded_video_sink_take_frame(sink: &NativeEncodedVideoSink) -> FfiEncodedVideoFrame;
        fn encoded_video_sink_dropped_frames(sink: &NativeEncodedVideoSink) -> u64;
        fn close_encoded_video_sink(sink: &NativeEncodedVideoSink) -> bool;
    }
}
