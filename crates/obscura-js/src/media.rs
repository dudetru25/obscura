//! Bounded VP8 video coding and the WebM subset used by browser recording.
//!
//! libvpx owns codec memory. This module validates dimensions before decoding
//! each keyframe and bounds compressed input, decoded output and frame count.

use std::mem::MaybeUninit;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, Ordering};

use vpx_sys as vpx;

pub(crate) const MAX_BYTES: usize = 128 * 1024 * 1024;
pub(crate) const MAX_FRAMES: usize = 600;
pub(crate) const MAX_DURATION_MS: f64 = 120_000.0;
const MAX_DIMENSION: u32 = 4096;
const MAX_PIXELS: usize = 4_194_304;
const DECODE_PACKET_BUDGET: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum MediaError {
    #[error("media_dimensions_invalid")]
    Dimensions,
    #[error("media_frame_limit_exceeded")]
    FrameLimit,
    #[error("media_buffer_limit_exceeded")]
    BufferLimit,
    #[error("media_timestamps_invalid")]
    Timestamps,
    #[error("media_duration_invalid")]
    Duration,
    #[error("media_webm_invalid")]
    InvalidWebm,
    #[error("media_webm_unsupported")]
    UnsupportedWebm,
    #[error("media_encode_failed")]
    Encode,
    #[error("media_decode_failed")]
    Decode,
    #[error("media_allocation_failed")]
    Allocation,
    #[error("media_aborted")]
    Cancelled,
    #[error("media_frame_index_invalid")]
    FrameIndex,
}

type Result<T> = std::result::Result<T, MediaError>;

pub(crate) fn frame_bytes(width: u32, height: u32) -> Result<usize> {
    let pixels = (width as usize).checked_mul(height as usize)
        .ok_or(MediaError::Dimensions)?;
    if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION
        || pixels > MAX_PIXELS {
        return Err(MediaError::Dimensions);
    }
    Ok(pixels * 4)
}

pub(crate) fn validate_encode(
    width: u32,
    height: u32,
    timestamps: &[f64],
    duration_ms: f64,
    rgba_len: usize,
) -> Result<()> {
    let bytes = frame_bytes(width, height)?;
    if timestamps.is_empty() || timestamps.len() > MAX_FRAMES {
        return Err(MediaError::FrameLimit);
    }
    if rgba_len > MAX_BYTES || bytes.checked_mul(timestamps.len()) != Some(rgba_len) {
        return Err(MediaError::BufferLimit);
    }
    if !timestamps.iter().all(|time| time.is_finite() && *time >= 0.0
        && *time <= MAX_DURATION_MS && time.fract() == 0.0)
        || timestamps.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(MediaError::Timestamps);
    }
    if !duration_ms.is_finite() || duration_ms < 0.0 || duration_ms > MAX_DURATION_MS
        || duration_ms != 0.0 && duration_ms <= timestamps[timestamps.len() - 1] {
        return Err(MediaError::Duration);
    }
    Ok(())
}

struct Codec(vpx::vpx_codec_ctx_t);

impl Drop for Codec {
    fn drop(&mut self) {
        // Only successfully initialized contexts are wrapped by Codec.
        unsafe { vpx::vpx_codec_destroy(&mut self.0); }
    }
}

struct Image(NonNull<vpx::vpx_image_t>);

impl Drop for Image {
    fn drop(&mut self) {
        unsafe { vpx::vpx_img_free(self.0.as_ptr()); }
    }
}

struct EncodedFrame {
    timestamp_ms: u64,
    keyframe: bool,
    bytes: Vec<u8>,
}

fn reserve(output: &mut Vec<u8>, additional: usize) -> Result<()> {
    if output.len().checked_add(additional).is_none_or(|size| size > MAX_BYTES) {
        return Err(MediaError::BufferLimit);
    }
    output.try_reserve(additional).map_err(|_| MediaError::Allocation)
}

fn channel(value: i32) -> u8 { value.clamp(0, 255) as u8 }

unsafe fn fill_i420(image: &mut vpx::vpx_image_t, rgba: &[u8], width: usize, height: usize) {
    for y in 0..height {
        for x in 0..width {
            let pixel = &rgba[(y * width + x) * 4..][..4];
            let alpha = i32::from(pixel[3]);
            let r = i32::from(pixel[0]) * alpha / 255;
            let g = i32::from(pixel[1]) * alpha / 255;
            let b = i32::from(pixel[2]) * alpha / 255;
            *image.planes[0].add(y * image.stride[0] as usize + x) =
                channel(((66 * r + 129 * g + 25 * b + 128) >> 8) + 16);
        }
    }
    for y in (0..height).step_by(2) {
        for x in (0..width).step_by(2) {
            let (mut r, mut g, mut b, mut count) = (0, 0, 0, 0);
            for row in y..(y + 2).min(height) {
                for col in x..(x + 2).min(width) {
                    let pixel = &rgba[(row * width + col) * 4..][..4];
                    let alpha = i32::from(pixel[3]);
                    r += i32::from(pixel[0]) * alpha / 255;
                    g += i32::from(pixel[1]) * alpha / 255;
                    b += i32::from(pixel[2]) * alpha / 255;
                    count += 1;
                }
            }
            r /= count;
            g /= count;
            b /= count;
            let offset_u = y / 2 * image.stride[1] as usize + x / 2;
            let offset_v = y / 2 * image.stride[2] as usize + x / 2;
            *image.planes[1].add(offset_u) = channel(((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128);
            *image.planes[2].add(offset_v) = channel(((112 * r - 94 * g - 18 * b + 128) >> 8) + 128);
        }
    }
}

unsafe fn collect_packets(codec: &mut Codec, frames: &mut Vec<EncodedFrame>, total: &mut usize) -> Result<()> {
    let mut iterator = ptr::null();
    loop {
        let packet = vpx::vpx_codec_get_cx_data(&mut codec.0, &mut iterator);
        if packet.is_null() { break; }
        if (*packet).kind != vpx::vpx_codec_cx_pkt_kind::VPX_CODEC_CX_FRAME_PKT { continue; }
        let frame = (*packet).data.frame;
        let length = usize::try_from(frame.sz).map_err(|_| MediaError::BufferLimit)?;
        if frame.buf.is_null() || length == 0 || frame.pts < 0 {
            return Err(MediaError::Encode);
        }
        *total = total.checked_add(length).ok_or(MediaError::BufferLimit)?;
        if *total > MAX_BYTES || frames.len() >= MAX_FRAMES { return Err(MediaError::BufferLimit); }
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(length).map_err(|_| MediaError::Allocation)?;
        bytes.extend_from_slice(std::slice::from_raw_parts(frame.buf.cast::<u8>(), length));
        frames.push(EncodedFrame { timestamp_ms: frame.pts as u64,
            keyframe: frame.flags & vpx::VPX_FRAME_IS_KEY != 0, bytes });
    }
    Ok(())
}

pub(crate) fn encode_webm(
    width: u32,
    height: u32,
    timestamps: &[f64],
    duration_ms: f64,
    include_header: bool,
    rgba: &[u8],
) -> Result<Vec<u8>> {
    validate_encode(width, height, timestamps, duration_ms, rgba.len())?;
    let bytes_per_frame = frame_bytes(width, height)?;
    let mut frames = Vec::new();
    let mut encoded_bytes = 0;
    unsafe {
        let interface = vpx::vpx_codec_vp8_cx();
        let mut config = MaybeUninit::<vpx::vpx_codec_enc_cfg_t>::uninit();
        if vpx::vpx_codec_enc_config_default(interface, config.as_mut_ptr(), 0)
            != vpx::vpx_codec_err_t::VPX_CODEC_OK { return Err(MediaError::Encode); }
        let mut config = config.assume_init();
        config.g_w = width;
        config.g_h = height;
        config.g_threads = 1;
        config.g_timebase.num = 1;
        config.g_timebase.den = 1000;
        config.g_lag_in_frames = 0;
        config.rc_dropframe_thresh = 0;
        config.rc_target_bitrate = (width * height * 4 / 1000).clamp(128, 16_000);
        config.rc_min_quantizer = 4;
        config.rc_max_quantizer = 32;
        let mut context = std::mem::zeroed::<vpx::vpx_codec_ctx_t>();
        if vpx::vpx_codec_enc_init_ver(&mut context, interface, &config, 0,
            vpx::VPX_ENCODER_ABI_VERSION as i32) != vpx::vpx_codec_err_t::VPX_CODEC_OK {
            return Err(MediaError::Encode);
        }
        let mut codec = Codec(context);
        let image = Image(NonNull::new(vpx::vpx_img_alloc(ptr::null_mut(),
            vpx::vpx_img_fmt_t::VPX_IMG_FMT_I420, width, height, 1)).ok_or(MediaError::Allocation)?);
        for (index, pixels) in rgba.chunks_exact(bytes_per_frame).enumerate() {
            fill_i420(&mut *image.0.as_ptr(), pixels, width as usize, height as usize);
            let next = timestamps.get(index + 1).copied().unwrap_or_else(|| {
                if duration_ms > 0.0 { duration_ms } else { timestamps[index] + 1.0 }
            });
            if vpx::vpx_codec_encode(&mut codec.0, image.0.as_ptr(), timestamps[index] as i64,
                (next - timestamps[index]).max(1.0) as _,
                if index == 0 { vpx::VPX_EFLAG_FORCE_KF as _ } else { 0 },
                vpx::VPX_DL_REALTIME as _) != vpx::vpx_codec_err_t::VPX_CODEC_OK {
                return Err(MediaError::Encode);
            }
            collect_packets(&mut codec, &mut frames, &mut encoded_bytes)?;
        }
        if vpx::vpx_codec_encode(&mut codec.0, ptr::null(), 0, 1, 0, vpx::VPX_DL_REALTIME as _)
            != vpx::vpx_codec_err_t::VPX_CODEC_OK { return Err(MediaError::Encode); }
        collect_packets(&mut codec, &mut frames, &mut encoded_bytes)?;
    }
    if frames.len() != timestamps.len() { return Err(MediaError::Encode); }
    mux_webm(width, height, &frames, duration_ms, include_header)
}

fn element(id: &[u8], body: &[u8], output: &mut Vec<u8>) -> Result<()> {
    reserve(output, id.len() + 8 + body.len())?;
    output.extend_from_slice(id);
    let length = body.len() as u64;
    let width = (1..=8).find(|width| length < (1u64 << (7 * width)) - 1)
        .ok_or(MediaError::BufferLimit)?;
    let encoded = length | (1u64 << (7 * width));
    output.extend_from_slice(&encoded.to_be_bytes()[8 - width..]);
    output.extend_from_slice(body);
    Ok(())
}

fn uint(id: &[u8], value: u64, output: &mut Vec<u8>) -> Result<()> {
    let bytes = value.to_be_bytes();
    let start = bytes.iter().position(|byte| *byte != 0).unwrap_or(7);
    element(id, &bytes[start..], output)
}

fn mux_webm(width: u32, height: u32, frames: &[EncodedFrame], duration_ms: f64,
    include_header: bool) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    if include_header {
        let mut header = Vec::new();
        uint(&[0x42, 0x86], 1, &mut header)?;
        uint(&[0x42, 0xf7], 1, &mut header)?;
        uint(&[0x42, 0xf2], 4, &mut header)?;
        uint(&[0x42, 0xf3], 8, &mut header)?;
        element(&[0x42, 0x82], b"webm", &mut header)?;
        uint(&[0x42, 0x87], 4, &mut header)?;
        uint(&[0x42, 0x85], 2, &mut header)?;
        element(&[0x1a, 0x45, 0xdf, 0xa3], &header, &mut output)?;
        // Unknown Segment length permits later MediaRecorder chunks to append
        // Clusters to this same Segment without repeating track headers.
        output.extend_from_slice(&[0x18, 0x53, 0x80, 0x67, 0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
        let mut info = Vec::new();
        uint(&[0x2a, 0xd7, 0xb1], 1_000_000, &mut info)?;
        element(&[0x4d, 0x80], b"Obscura", &mut info)?;
        element(&[0x57, 0x41], b"Obscura", &mut info)?;
        if duration_ms > 0.0 { element(&[0x44, 0x89], &duration_ms.to_be_bytes(), &mut info)?; }
        element(&[0x15, 0x49, 0xa9, 0x66], &info, &mut output)?;
        let mut video = Vec::new();
        uint(&[0xb0], u64::from(width), &mut video)?;
        uint(&[0xba], u64::from(height), &mut video)?;
        let mut track = Vec::new();
        uint(&[0xd7], 1, &mut track)?;
        uint(&[0x73, 0xc5], 1, &mut track)?;
        uint(&[0x83], 1, &mut track)?;
        uint(&[0x9c], 0, &mut track)?;
        element(&[0x86], b"V_VP8", &mut track)?;
        element(&[0xe0], &video, &mut track)?;
        let mut tracks = Vec::new();
        element(&[0xae], &track, &mut tracks)?;
        element(&[0x16, 0x54, 0xae, 0x6b], &tracks, &mut output)?;
    }
    for frame in frames {
        let mut cluster = Vec::new();
        uint(&[0xe7], frame.timestamp_ms, &mut cluster)?;
        let mut block = Vec::new();
        reserve(&mut block, frame.bytes.len() + 4)?;
        block.extend_from_slice(&[0x81, 0, 0, if frame.keyframe { 0x80 } else { 0 }]);
        block.extend_from_slice(&frame.bytes);
        element(&[0xa3], &block, &mut cluster)?;
        element(&[0x1f, 0x43, 0xb6, 0x75], &cluster, &mut output)?;
    }
    Ok(output)
}

struct Element<'a> { id: u64, body: &'a [u8], unknown: bool }

struct Reader<'a> { data: &'a [u8], offset: usize }

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self { Self { data, offset: 0 } }
    fn next(&mut self, budget: &mut usize) -> Result<Option<Element<'a>>> {
        if self.offset == self.data.len() { return Ok(None); }
        *budget = budget.checked_sub(1).ok_or(MediaError::BufferLimit)?;
        let (id, id_len, _) = vint(&self.data[self.offset..], false)?;
        if id_len > 4 { return Err(MediaError::InvalidWebm); }
        self.offset += id_len;
        let (length, size_len, unknown) = vint(&self.data[self.offset..], true)?;
        self.offset += size_len;
        let end = if unknown { self.data.len() } else {
            self.offset.checked_add(usize::try_from(length).map_err(|_| MediaError::InvalidWebm)?)
                .filter(|end| *end <= self.data.len()).ok_or(MediaError::InvalidWebm)?
        };
        let body = &self.data[self.offset..end];
        self.offset = end;
        Ok(Some(Element { id, body, unknown }))
    }
}

fn vint(bytes: &[u8], strip_marker: bool) -> Result<(u64, usize, bool)> {
    let first = *bytes.first().filter(|byte| **byte != 0).ok_or(MediaError::InvalidWebm)?;
    let width = first.leading_zeros() as usize + 1;
    if width > 8 || bytes.len() < width { return Err(MediaError::InvalidWebm); }
    let marker = 1u8 << (8 - width);
    let mut value = u64::from(if strip_marker { first & !marker } else { first });
    for byte in &bytes[1..width] { value = (value << 8) | u64::from(*byte); }
    Ok((value, width, strip_marker && value == (1u64 << (7 * width)) - 1))
}

fn number(body: &[u8]) -> Result<u64> {
    if body.is_empty() || body.len() > 8 { return Err(MediaError::InvalidWebm); }
    Ok(body.iter().fold(0, |value, byte| value << 8 | u64::from(*byte)))
}

fn float(body: &[u8]) -> Result<f64> {
    let value = match body.len() {
        4 => f32::from_be_bytes(body.try_into().map_err(|_| MediaError::InvalidWebm)?) as f64,
        8 => f64::from_be_bytes(body.try_into().map_err(|_| MediaError::InvalidWebm)?),
        _ => return Err(MediaError::InvalidWebm),
    };
    if !value.is_finite() || value <= 0.0 { return Err(MediaError::Duration); }
    Ok(value)
}

#[derive(Default)]
struct VideoTrack { id: u64, width: u32, height: u32, default_duration_ns: Option<u64> }

fn tracks(body: &[u8], budget: &mut usize) -> Result<VideoTrack> {
    let mut reader = Reader::new(body);
    let mut selected = None;
    let mut ids = std::collections::HashSet::new();
    while let Some(entry) = reader.next(budget)? {
        if entry.unknown { return Err(MediaError::InvalidWebm); }
        if entry.id != 0xae { continue; }
        let mut fields = Reader::new(entry.body);
        let (mut id, mut kind, mut codec, mut video, mut default_duration_ns) = (None, None, None, None, None);
        let mut content_encoding = false;
        while let Some(field) = fields.next(budget)? {
            if field.unknown { return Err(MediaError::InvalidWebm); }
            match field.id {
                0xd7 if id.is_none() => id = Some(number(field.body)?),
                0x83 if kind.is_none() => kind = Some(number(field.body)?),
                0x86 if codec.is_none() => codec = Some(field.body),
                0xe0 if video.is_none() => video = Some(field.body),
                0x23e383 if default_duration_ns.is_none() => default_duration_ns = Some(number(field.body)?),
                0x6d80 => content_encoding = true,
                0xd7 | 0x83 | 0x86 | 0xe0 | 0x23e383 => return Err(MediaError::InvalidWebm),
                _ => {},
            }
        }
        let id = id.filter(|id| *id > 0).ok_or(MediaError::InvalidWebm)?;
        if !ids.insert(id) || kind.is_none() || codec.is_none() { return Err(MediaError::InvalidWebm); }
        if kind != Some(1) { continue; }
        if selected.is_some() || codec != Some(b"V_VP8".as_slice()) || content_encoding {
            return Err(MediaError::UnsupportedWebm);
        }
        let mut dimensions = Reader::new(video.ok_or(MediaError::InvalidWebm)?);
        let (mut width, mut height) = (None, None);
        while let Some(field) = dimensions.next(budget)? {
            if field.unknown { return Err(MediaError::InvalidWebm); }
            match field.id {
                0xb0 if width.is_none() => width = Some(u32::try_from(number(field.body)?).map_err(|_| MediaError::Dimensions)?),
                0xba if height.is_none() => height = Some(u32::try_from(number(field.body)?).map_err(|_| MediaError::Dimensions)?),
                0xb0 | 0xba => return Err(MediaError::InvalidWebm),
                _ => {},
            }
        }
        let width = width.ok_or(MediaError::InvalidWebm)?;
        let height = height.ok_or(MediaError::InvalidWebm)?;
        frame_bytes(width, height)?;
        if default_duration_ns.is_some_and(|duration| duration == 0 || duration > 120_000_000_000) {
            return Err(MediaError::Duration);
        }
        selected = Some(VideoTrack { id, width, height, default_duration_ns });
    }
    selected.ok_or(MediaError::UnsupportedWebm)
}

fn cluster_end(body: &[u8], budget: &mut usize) -> Result<usize> {
    let mut reader = Reader::new(body);
    loop {
        let start = reader.offset;
        let Some(element) = reader.next(budget)? else { return Ok(body.len()); };
        if matches!(element.id, 0x1f43b675 | 0x1549a966 | 0x1654ae6b | 0x1c53bb6b | 0x114d9b74 | 0x1254c367) {
            return Ok(start);
        }
        if element.unknown { return Err(MediaError::InvalidWebm); }
    }
}

struct Frame<'a> { timestamp_ms: f64, bytes: &'a [u8], visible: bool }

fn block<'a>(body: &'a [u8], cluster_time: u64, scale_ns: u64, track: &VideoTrack,
    frames: &mut Vec<Frame<'a>>) -> Result<()> {
    let (id, prefix, unknown) = vint(body, true)?;
    if unknown || body.len() < prefix + 4 { return Err(MediaError::InvalidWebm); }
    if id != track.id { return Ok(()); }
    if body[prefix + 2] & 0x06 != 0 { return Err(MediaError::UnsupportedWebm); }
    let relative = i16::from_be_bytes([body[prefix], body[prefix + 1]]);
    let ticks = i128::from(cluster_time) + i128::from(relative);
    let timestamp_ms = ticks as f64 * scale_ns as f64 / 1_000_000.0;
    if ticks < 0 || !timestamp_ms.is_finite() || timestamp_ms > MAX_DURATION_MS
        || frames.last().is_some_and(|frame| timestamp_ms < frame.timestamp_ms) {
        return Err(MediaError::Timestamps);
    }
    if frames.len() >= MAX_FRAMES { return Err(MediaError::FrameLimit); }
    frames.push(Frame { timestamp_ms, bytes: &body[prefix + 3..], visible: body[prefix + 2] & 0x08 == 0 });
    Ok(())
}

fn cluster<'a>(body: &'a [u8], scale_ns: u64, track: &VideoTrack, frames: &mut Vec<Frame<'a>>,
    budget: &mut usize) -> Result<()> {
    let mut reader = Reader::new(body);
    let mut time = None;
    while let Some(element) = reader.next(budget)? {
        if element.unknown { return Err(MediaError::InvalidWebm); }
        if element.id == 0xe7 {
            if time.is_some() { return Err(MediaError::InvalidWebm); }
            time = Some(number(element.body)?);
        }
    }
    let time = time.ok_or(MediaError::InvalidWebm)?;
    let mut reader = Reader::new(body);
    while let Some(element) = reader.next(budget)? {
        match element.id {
            0xa3 => block(element.body, time, scale_ns, track, frames)?,
            0xa0 => {
                let mut group = Reader::new(element.body);
                let mut payload = None;
                while let Some(field) = group.next(budget)? {
                    if field.unknown { return Err(MediaError::InvalidWebm); }
                    if field.id == 0xa1 {
                        if payload.is_some() { return Err(MediaError::InvalidWebm); }
                        payload = Some(field.body);
                    }
                }
                block(payload.ok_or(MediaError::InvalidWebm)?, time, scale_ns, track, frames)?;
            },
            _ => {},
        }
    }
    Ok(())
}

struct Webm<'a> { track: VideoTrack, duration_ms: Option<f64>, frames: Vec<Frame<'a>> }

fn demux_webm(bytes: &[u8]) -> Result<Webm<'_>> {
    if bytes.is_empty() || bytes.len() > MAX_BYTES { return Err(MediaError::BufferLimit); }
    let mut budget = 100_000usize;
    let mut root = Reader::new(bytes);
    let header = root.next(&mut budget)?.filter(|element| element.id == 0x1a45dfa3 && !element.unknown)
        .ok_or(MediaError::InvalidWebm)?;
    let mut fields = Reader::new(header.body);
    let mut webm_type = false;
    while let Some(field) = fields.next(&mut budget)? {
        if field.unknown { return Err(MediaError::InvalidWebm); }
        if field.id == 0x4282 {
            if webm_type || field.body != b"webm" { return Err(MediaError::UnsupportedWebm); }
            webm_type = true;
        }
    }
    if !webm_type { return Err(MediaError::InvalidWebm); }
    let segment = root.next(&mut budget)?.filter(|element| element.id == 0x18538067)
        .ok_or(MediaError::InvalidWebm)?;
    if root.next(&mut budget)?.is_some() { return Err(MediaError::UnsupportedWebm); }
    let mut reader = Reader::new(segment.body);
    let (mut track, mut duration, mut scale_ns) = (None, None, 1_000_000u64);
    let mut info_seen = false;
    let mut clusters = Vec::new();
    while let Some(mut element) = reader.next(&mut budget)? {
        if element.unknown {
            if element.id != 0x1f43b675 { return Err(MediaError::UnsupportedWebm); }
            let end = cluster_end(element.body, &mut budget)?;
            reader.offset -= element.body.len() - end;
            element.body = &element.body[..end];
        }
        match element.id {
            0x1654ae6b if track.is_none() => track = Some(tracks(element.body, &mut budget)?),
            0x1654ae6b => return Err(MediaError::InvalidWebm),
            0x1549a966 => {
                if info_seen { return Err(MediaError::InvalidWebm); }
                info_seen = true;
                let mut fields = Reader::new(element.body);
                let mut scale_seen = false;
                while let Some(field) = fields.next(&mut budget)? {
                    if field.unknown { return Err(MediaError::InvalidWebm); }
                    match field.id {
                        0x2ad7b1 if !scale_seen => { scale_ns = number(field.body)?; scale_seen = true; },
                        0x4489 if duration.is_none() => duration = Some(float(field.body)?),
                        0x2ad7b1 | 0x4489 => return Err(MediaError::InvalidWebm),
                        _ => {},
                    }
                }
            },
            0x1f43b675 => {
                if clusters.len() >= MAX_FRAMES { return Err(MediaError::FrameLimit); }
                clusters.push(element.body);
            },
            _ => {},
        }
    }
    if scale_ns == 0 { return Err(MediaError::InvalidWebm); }
    let track = track.ok_or(MediaError::InvalidWebm)?;
    let duration_ms = duration.map(|value| value * scale_ns as f64 / 1_000_000.0);
    if duration_ms.is_some_and(|duration| !duration.is_finite() || duration > MAX_DURATION_MS) {
        return Err(MediaError::Duration);
    }
    let mut frames = Vec::new();
    for body in clusters { cluster(body, scale_ns, &track, &mut frames, &mut budget)?; }
    if frames.is_empty() { return Err(MediaError::Decode); }
    Ok(Webm { track, duration_ms, frames })
}

fn validate_vp8_frame(bytes: &[u8], track: &VideoTrack, first: bool) -> Result<()> {
    if bytes.len() < 3 { return Err(MediaError::Decode); }
    if bytes[0] & 1 == 0 {
        if bytes.len() < 10 || bytes[3..6] != [0x9d, 0x01, 0x2a] { return Err(MediaError::Decode); }
        let width = u32::from(u16::from_le_bytes([bytes[6], bytes[7]]) & 0x3fff);
        let height = u32::from(u16::from_le_bytes([bytes[8], bytes[9]]) & 0x3fff);
        frame_bytes(width, height)?;
        if width != track.width || height != track.height { return Err(MediaError::UnsupportedWebm); }
    } else if first { return Err(MediaError::Decode); }
    Ok(())
}

unsafe fn append_rgba(image: &vpx::vpx_image_t, output: &mut Vec<u8>, track: &VideoTrack) -> Result<()> {
    if image.fmt != vpx::vpx_img_fmt_t::VPX_IMG_FMT_I420 || image.bit_depth != 8
        || image.d_w != track.width || image.d_h != track.height
        || image.planes[..3].iter().any(|plane| plane.is_null())
        || image.stride[0] < track.width as i32
        || image.stride[1] < track.width.div_ceil(2) as i32
        || image.stride[2] < track.width.div_ceil(2) as i32 {
        return Err(MediaError::Decode);
    }
    reserve(output, frame_bytes(track.width, track.height)?)?;
    for y in 0..track.height as usize {
        for x in 0..track.width as usize {
            let luma = i32::from(*image.planes[0].add(y * image.stride[0] as usize + x)) - 16;
            let u = i32::from(*image.planes[1].add(y / 2 * image.stride[1] as usize + x / 2)) - 128;
            let v = i32::from(*image.planes[2].add(y / 2 * image.stride[2] as usize + x / 2)) - 128;
            output.extend_from_slice(&[channel((298 * luma + 409 * v + 128) >> 8),
                channel((298 * luma - 100 * u - 208 * v + 128) >> 8),
                channel((298 * luma + 516 * u + 128) >> 8), 255]);
        }
    }
    Ok(())
}

struct DecoderCodec {
    context: Box<vpx::vpx_codec_ctx_t>,
    // The public libvpx context retains config.dec. Keep its address stable
    // even if a codec implementation does not copy the configuration internally.
    _config: Box<vpx::vpx_codec_dec_cfg_t>,
    // libvpx owns this image until the next decode call. Keep its descriptor
    // for repeated reads because get_frame consumes VP8's available output.
    frame_image: Option<NonNull<vpx::vpx_image_t>>,
}

// SAFETY: this wrapper exclusively owns its context/configuration and never
// exposes an image pointer. It uses one codec thread, no application callbacks
// or external buffers, and every libvpx call requires &mut self. Moving it
// between blocking jobs cannot overlap codec access. It deliberately is not Sync.
// libvpx's vpx_decoder.c dispatches through the owned context, with no caller
// thread state; callers serialize initialization for non-multithread builds.
unsafe impl Send for DecoderCodec {}

impl DecoderCodec {
    fn new(track: &VideoTrack) -> Result<Self> {
        let config = Box::new(vpx::vpx_codec_dec_cfg_t { threads: 1, w: track.width, h: track.height });
        let mut context = Box::new(unsafe { std::mem::zeroed::<vpx::vpx_codec_ctx_t>() });
        let status = unsafe { vpx::vpx_codec_dec_init_ver(&mut *context, vpx::vpx_codec_vp8_dx(),
            &*config, 0, vpx::VPX_DECODER_ABI_VERSION as i32) };
        // libvpx cleans a failed initialization itself; only a successful
        // context enters this Drop implementation.
        if status != vpx::vpx_codec_err_t::VPX_CODEC_OK { return Err(MediaError::Decode); }
        Ok(Self { context, _config: config, frame_image: None })
    }
}

impl Drop for DecoderCodec {
    fn drop(&mut self) {
        unsafe { vpx::vpx_codec_destroy(&mut *self.context); }
    }
}

struct Packet {
    range: std::ops::Range<usize>,
    visible_index: Option<usize>,
    keyframe: bool,
}

/// Owns compressed input and the bounded libvpx reference-frame set. RGBA is
/// materialized only for the requested displayed frame, never for the whole clip.
pub(crate) struct MediaDecoder {
    input: Vec<u8>,
    track: VideoTrack,
    packets: Vec<Packet>,
    displayed_packets: Vec<usize>,
    timestamps: Vec<f64>,
    duration_ms: f64,
    codec: DecoderCodec,
    cursor: usize,
    current: Option<usize>,
}

impl MediaDecoder {
    pub(crate) fn open(input: Vec<u8>) -> Result<Self> {
        let webm = demux_webm(&input)?;
        let mut packets = Vec::new();
        let mut displayed_packets = Vec::new();
        let mut timestamps = Vec::new();
        packets.try_reserve_exact(webm.frames.len()).map_err(|_| MediaError::Allocation)?;
        displayed_packets.try_reserve_exact(webm.frames.len()).map_err(|_| MediaError::Allocation)?;
        timestamps.try_reserve_exact(webm.frames.len()).map_err(|_| MediaError::Allocation)?;
        for (index, frame) in webm.frames.iter().enumerate() {
            validate_vp8_frame(frame.bytes, &webm.track, index == 0)?;
            let start = (frame.bytes.as_ptr() as usize).checked_sub(input.as_ptr() as usize)
                .ok_or(MediaError::InvalidWebm)?;
            let end = start.checked_add(frame.bytes.len()).filter(|end| *end <= input.len())
                .ok_or(MediaError::InvalidWebm)?;
            // The VP8 show_frame bit and the container's invisible flag must
            // both allow display. Invisible packets still advance the codec.
            let visible_index = if frame.visible && frame.bytes[0] & 0x10 != 0 {
                if timestamps.last().is_some_and(|before| frame.timestamp_ms <= *before) {
                    return Err(MediaError::Timestamps);
                }
                let visible_index = timestamps.len();
                timestamps.push(frame.timestamp_ms);
                displayed_packets.push(index);
                Some(visible_index)
            } else { None };
            packets.push(Packet { range: start..end, visible_index, keyframe: frame.bytes[0] & 1 == 0 });
        }
        let last = timestamps.last().copied().ok_or(MediaError::Decode)?;
        let step = if timestamps.len() > 1 { last - timestamps[timestamps.len() - 2] } else { 1.0 };
        let inferred = last + webm.track.default_duration_ns
            .map(|duration| duration as f64 / 1_000_000.0).unwrap_or(step);
        let duration_ms = webm.duration_ms.unwrap_or(inferred);
        if !duration_ms.is_finite() || duration_ms <= last || duration_ms > MAX_DURATION_MS {
            return Err(MediaError::Duration);
        }
        let codec = DecoderCodec::new(&webm.track)?;
        drop(webm.frames);
        let track = webm.track;
        Ok(Self { input, track, packets, displayed_packets, timestamps, duration_ms,
            codec, cursor: 0, current: None })
    }

    pub(crate) fn width(&self) -> u32 { self.track.width }
    pub(crate) fn height(&self) -> u32 { self.track.height }
    pub(crate) fn timestamps(&self) -> &[f64] { &self.timestamps }
    pub(crate) fn duration_ms(&self) -> f64 { self.duration_ms }

    fn current_pixels(&mut self, cancel: &AtomicBool) -> Result<Vec<u8>> {
        if cancel.load(Ordering::Acquire) { return Err(MediaError::Cancelled); }
        let image = match self.codec.frame_image {
            Some(image) => image,
            None => {
                let mut iterator = ptr::null();
                let image = NonNull::new(unsafe {
                    vpx::vpx_codec_get_frame(&mut *self.codec.context, &mut iterator)
                }).ok_or(MediaError::Decode)?;
                self.codec.frame_image = Some(image);
                image
            },
        };
        let mut pixels = Vec::new();
        // No decode can overlap this conversion. The cached image is cleared
        // before every decode and is dropped with its owning codec on a seek.
        unsafe { append_rgba(image.as_ref(), &mut pixels, &self.track)?; }
        if cancel.load(Ordering::Acquire) { return Err(MediaError::Cancelled); }
        Ok(pixels)
    }

    /// Returns None after at most eight packet decodes if the requested frame
    /// has not been reached. The caller releases its shared codec permit before
    /// requesting another step. Sequential playback never restarts the codec.
    pub(crate) fn decode_to(&mut self, index: usize, cancel: &AtomicBool) -> Result<Option<Vec<u8>>> {
        if cancel.load(Ordering::Acquire) { return Err(MediaError::Cancelled); }
        let target = *self.displayed_packets.get(index).ok_or(MediaError::FrameIndex)?;
        if self.current == Some(index) { return self.current_pixels(cancel).map(Some); }
        if target < self.cursor {
            let keyframe = self.packets[..=target].iter().rposition(|packet| packet.keyframe)
                .ok_or(MediaError::Decode)?;
            self.codec = DecoderCodec::new(&self.track)?;
            self.cursor = keyframe;
            self.current = None;
        }
        for _ in 0..DECODE_PACKET_BUDGET {
            if cancel.load(Ordering::Acquire) { return Err(MediaError::Cancelled); }
            let packet = self.packets.get(self.cursor).ok_or(MediaError::Decode)?;
            let bytes = &self.input[packet.range.clone()];
            self.codec.frame_image = None;
            self.current = None;
            let status = unsafe { vpx::vpx_codec_decode(&mut *self.codec.context, bytes.as_ptr(),
                bytes.len() as u32, ptr::null_mut(), 0) };
            if status != vpx::vpx_codec_err_t::VPX_CODEC_OK { return Err(MediaError::Decode); }
            self.current = packet.visible_index;
            self.cursor += 1;
            if self.current == Some(index) { return self.current_pixels(cancel).map(Some); }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn colors() -> Vec<u8> {
        [[255, 0, 0, 255], [0, 0, 255, 255]].into_iter()
            .flat_map(|color| std::iter::repeat_n(color, 32 * 32)).flatten().collect()
    }

    #[test]
    fn vp8_webm_roundtrip_preserves_frames_timing_and_color() {
        let bytes = encode_webm(32, 32, &[0.0, 50.0], 100.0, true, &colors()).unwrap();
        assert!(bytes.windows(4).any(|window| window == [0x1f, 0x43, 0xb6, 0x75]));
        let mut decoder = MediaDecoder::open(bytes).unwrap();
        assert_eq!((decoder.width(), decoder.height()), (32, 32));
        assert_eq!(decoder.timestamps(), &[0.0, 50.0]);
        assert_eq!(decoder.duration_ms(), 100.0);
        let cancel = AtomicBool::new(false);
        let red = decoder.decode_to(0, &cancel).unwrap().unwrap();
        assert_eq!(red.len(), 32 * 32 * 4);
        assert!(red[0] > 220 && red[1] < 30 && red[2] < 30);
        let blue = decoder.decode_to(1, &cancel).unwrap().unwrap();
        assert!(blue[0] < 30 && blue[1] < 30 && blue[2] > 220);
        assert_eq!(decoder.decode_to(1, &cancel).unwrap().unwrap(), blue);
        assert_eq!(decoder.decode_to(0, &cancel).unwrap().unwrap(), red);
    }

    #[test]
    fn recorder_chunks_join_into_one_playable_webm_segment() {
        let pixels = colors();
        let mut bytes = encode_webm(32, 32, &[0.0], 0.0, true, &pixels[..4096]).unwrap();
        let later = encode_webm(32, 32, &[50.0], 0.0, false, &pixels[4096..]).unwrap();
        assert_eq!(&later[..4], &[0x1f, 0x43, 0xb6, 0x75]);
        bytes.extend_from_slice(&later);
        let mut decoder = MediaDecoder::open(bytes.clone()).unwrap();
        assert_eq!(decoder.timestamps(), &[0.0, 50.0]);
        assert_eq!(decoder.duration_ms(), 100.0);
        let cancel = AtomicBool::new(false);
        let last = decoder.decode_to(1, &cancel).unwrap().unwrap();
        let mut invisible = bytes.clone();
        let block = invisible.windows(4).position(|window| window == [0x81, 0, 0, 0x80]).unwrap();
        invisible[block + 3] |= 0x08;
        let mut hidden_decoder = MediaDecoder::open(invisible).unwrap();
        assert_eq!(hidden_decoder.timestamps(), &[50.0]);
        assert_eq!(hidden_decoder.decode_to(0, &cancel).unwrap().unwrap(), last);
        let cluster = bytes.windows(4).position(|window| window == [0x1f, 0x43, 0xb6, 0x75]).unwrap();
        let size = cluster + 4;
        let width = bytes[size].leading_zeros() as usize + 1;
        bytes[size] = 0xff >> (width - 1);
        bytes[size + 1..size + width].fill(0xff);
        let mut decoder = MediaDecoder::open(bytes).unwrap();
        assert_eq!(decoder.timestamps(), &[0.0, 50.0]);
        assert_eq!(decoder.decode_to(1, &cancel).unwrap().unwrap(), last);
    }

    #[test]
    fn decoder_yields_between_packet_windows_and_preserves_seek_pixels() {
        let timestamps: Vec<_> = (0..19).map(|index| index as f64 * 40.0).collect();
        let pixels: Vec<_> = (0..19).flat_map(|index| {
            let color = if index == 18 { [0, 0, 255, 255] } else { [255, 0, 0, 255] };
            std::iter::repeat_n(color, 32 * 32).flatten()
        }).collect();
        let bytes = encode_webm(32, 32, &timestamps, 760.0, true, &pixels).unwrap();
        let mut decoder = MediaDecoder::open(bytes).unwrap();
        let cancel = AtomicBool::new(false);
        assert_eq!(decoder.decode_to(usize::MAX, &cancel), Err(MediaError::FrameIndex));
        cancel.store(true, Ordering::Release);
        assert_eq!(decoder.decode_to(18, &cancel), Err(MediaError::Cancelled));
        cancel.store(false, Ordering::Release);
        assert_eq!(decoder.decode_to(18, &cancel).unwrap(), None);
        assert_eq!(decoder.decode_to(18, &cancel).unwrap(), None);
        let blue = decoder.decode_to(18, &cancel).unwrap().unwrap();
        assert!(blue[0] < 30 && blue[2] > 220);
        assert_eq!(decoder.decode_to(18, &cancel).unwrap().unwrap(), blue);
        let red = decoder.decode_to(0, &cancel).unwrap().unwrap();
        assert!(red[0] > 220 && red[2] < 30);
        // Every sequential frame is available in one bounded step after the seek.
        for index in 1..19 {
            let frame = decoder.decode_to(index, &cancel).unwrap().unwrap();
            assert_eq!(frame.len(), 32 * 32 * 4);
            if index == 18 { assert_eq!(frame, blue); }
        }
    }

    #[test]
    fn full_hd_clip_returns_one_frame_beyond_the_eager_decoded_byte_limit() {
        let mut red = Vec::new();
        red.extend(std::iter::repeat_n([255, 0, 0, 255], 1920 * 1080).flatten());
        let blue: Vec<_> = std::iter::repeat_n([0, 0, 255, 255], 1920 * 1080).flatten().collect();
        let mut bytes = Vec::new();
        for index in 0..17 {
            bytes.extend(encode_webm(1920, 1080, &[index as f64 * 40.0], 0.0,
                index == 0, if index == 16 { &blue } else { &red }).unwrap());
        }
        let mut decoder = MediaDecoder::open(bytes).unwrap();
        assert_eq!(decoder.timestamps().len(), 17);
        assert!(frame_bytes(decoder.width(), decoder.height()).unwrap() * 17 > MAX_BYTES);
        let cancel = AtomicBool::new(false);
        assert_eq!(decoder.decode_to(16, &cancel).unwrap(), None);
        assert_eq!(decoder.decode_to(16, &cancel).unwrap(), None);
        let pixels = decoder.decode_to(16, &cancel).unwrap().unwrap();
        assert_eq!(pixels.len(), 1920 * 1080 * 4);
        assert!(pixels[0] < 30 && pixels[2] > 220);
    }

    #[test]
    fn malformed_and_oversized_media_fails_before_codec_allocation() {
        let pixels = colors();
        for (width, height, times, duration, length, error) in [
            (0, 32, vec![0.0], 50.0, 4096, MediaError::Dimensions),
            (4097, 32, vec![0.0], 50.0, 4096, MediaError::Dimensions),
            (32, 32, vec![], 50.0, 0, MediaError::FrameLimit),
            (32, 32, vec![0.0; MAX_FRAMES + 1], 50.0, 0, MediaError::FrameLimit),
            (32, 32, vec![0.0, 0.0], 50.0, pixels.len(), MediaError::Timestamps),
            (32, 32, vec![f64::NAN], 50.0, 4096, MediaError::Timestamps),
            (32, 32, vec![0.5], 50.0, 4096, MediaError::Timestamps),
            (32, 32, vec![0.0], 0.0, 1, MediaError::BufferLimit),
            (32, 32, vec![0.0], f64::INFINITY, 4096, MediaError::Duration),
            (32, 32, vec![50.0], 50.0, 4096, MediaError::Duration),
        ] {
            assert_eq!(validate_encode(width, height, &times, duration, length), Err(error));
        }
        assert_eq!(MediaDecoder::open(Vec::new()).err(), Some(MediaError::BufferLimit));
        for bytes in [&b"not webm"[..], &[0x1a, 0x45, 0xdf, 0xa3, 0xff][..]] {
            assert!(MediaDecoder::open(bytes.to_vec()).is_err());
        }
        let valid = encode_webm(32, 32, &[0.0, 50.0], 100.0, true, &pixels).unwrap();
        for cut in [1, 4, 5, valid.len() - 1] { assert!(MediaDecoder::open(valid[..cut].to_vec()).is_err()); }
        let mut unsupported = valid.clone();
        let codec = unsupported.windows(5).position(|window| window == b"V_VP8").unwrap();
        unsupported[codec + 4] = b'9';
        assert_eq!(MediaDecoder::open(unsupported).err(), Some(MediaError::UnsupportedWebm));
        let mut laced = valid.clone();
        let block = laced.windows(4).position(|window| window == [0x81, 0, 0, 0x80]).unwrap();
        laced[block + 3] |= 0x02;
        assert_eq!(MediaDecoder::open(laced).err(), Some(MediaError::UnsupportedWebm));
        let mut corrupt_frame = valid.clone();
        let signature = corrupt_frame.windows(3).position(|window| window == [0x9d, 0x01, 0x2a]).unwrap();
        corrupt_frame[signature] = 0;
        assert_eq!(MediaDecoder::open(corrupt_frame).err(), Some(MediaError::Decode));
        let mut corrupted = valid.clone();
        let header = corrupted.windows(3).position(|window| window == [0x9d, 0x01, 0x2a]).unwrap();
        corrupted[header + 3..header + 5].copy_from_slice(&8192u16.to_le_bytes());
        assert_eq!(MediaDecoder::open(corrupted).err(), Some(MediaError::Dimensions));
    }
}
