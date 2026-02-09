use super::frame_pool::FramePool;
use ffmpeg::rescale::{Rescale, TIME_BASE};
use ffmpeg_next as ffmpeg;
use std::ptr;

#[derive(Debug)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
    pub pts: Option<i64>,
}

pub struct VideoState {
    pub stream_index: usize,
    pub decoder: ffmpeg::decoder::Video,
    pub scaler: ffmpeg::software::scaling::Context,
    pub decoded: ffmpeg::util::frame::Video,

    pub width: u32,
    pub height: u32,
    pub duration: i64,

    pub time_base: ffmpeg::Rational,
    pub start_pts: i64,
}

pub struct MediaSession {
    pub input_format_ctx: ffmpeg::format::context::Input,
    pub video: Option<VideoState>,
}

pub enum ProcessOutput {
    Video(VideoFrame),
}

pub enum Packet {
    Packet(ffmpeg::Packet),
    Eof,
}

/// This unsafe code seems to be unavoidable unfortunately. ffmpeg-next is
/// awesome and tries to keep things as safe as possible, but unfortunately, it
/// also puts limit to the performance to some extent. There seems to be two
/// different ways to use ffmpeg-next's pipeline:
/// 1. Just create an empty video frame, pass it to the scaler, and let ffmpeg
///    *allocate* a new buffer for you, set the video's data to be this new
///    buffer, and you got yourself an allocated buffer that you have to, now,
///    copy back to the buffer (cost: allocation + copying)
/// 2. Get your hands dirty, and set the data of the empty video frame to be
///    your buffer directly so that the scaler can directly write to it and you
///    do not have any allocation or copying.
/// We'll go with 2 :D
fn create_video_frame_from_buffer(
    width: u32,
    height: u32,
    format: ffmpeg::format::Pixel,
    buffer: &mut Vec<u8>,
) -> ffmpeg::util::frame::Video {
    let mut frame = ffmpeg::util::frame::Video::empty();
    frame.set_width(width);
    frame.set_height(height);
    frame.set_format(format);

    unsafe {
        let frame_ptr = frame.as_mut_ptr();

        (*frame_ptr).data[0] = buffer.as_mut_ptr() as *mut u8;
        (*frame_ptr).data[1] = ptr::null_mut();
        (*frame_ptr).data[2] = ptr::null_mut();
        (*frame_ptr).data[3] = ptr::null_mut();

        (*frame_ptr).linesize[0] = (width * 4) as i32;
        (*frame_ptr).linesize[1] = 0;
        (*frame_ptr).linesize[2] = 0;
        (*frame_ptr).linesize[3] = 0;
    }

    frame
}

pub fn load_media_session(source: &str) -> Result<MediaSession, ffmpeg::Error> {
    ffmpeg::init()?;
    let input_format_ctx = ffmpeg::format::input(source)?;
    let video = if let Some(stream) = input_format_ctx.streams().best(ffmpeg::media::Type::Video) {
        let stream_index = stream.index();

        let context = ffmpeg::codec::context::Context::from_parameters(stream.parameters())?;

        let decoder = context.decoder().video()?;
        let width = decoder.width();
        let height = decoder.height();
        let duration = stream.duration();

        let scaler = ffmpeg::software::scaling::Context::get(
            decoder.format(),
            width,
            height,
            ffmpeg::format::Pixel::RGBA,
            width,
            height,
            ffmpeg::software::scaling::Flags::BILINEAR,
        )?;

        let time_base = stream.time_base();
        let start_pts = stream.start_time();

        Some(VideoState {
            stream_index,
            decoder,
            scaler,
            decoded: ffmpeg::util::frame::Video::empty(),

            width,
            height,
            duration,

            time_base,
            start_pts,
        })
    } else {
        None
    };

    Ok(MediaSession {
        input_format_ctx,
        video: video,
    })
}

pub fn read_packet(session: &mut MediaSession) -> Result<Packet, ffmpeg::Error> {
    let mut packet = ffmpeg::Packet::empty();
    match packet.read(&mut session.input_format_ctx) {
        Ok(_) => Ok(Packet::Packet(packet)),
        Err(ffmpeg::error::Error::Eof) => Ok(Packet::Eof),
        Err(e) => Err(e),
    }
}

pub fn process_packet(
    session: &mut MediaSession,
    packet: &ffmpeg::Packet,
    pool: &FramePool,
) -> Result<Vec<ProcessOutput>, ffmpeg::Error> {
    let mut outputs = Vec::new();

    if let Some(video) = &mut session.video {
        if packet.stream() == video.stream_index {
            video.decoder.send_packet(packet)?;

            while video.decoder.receive_frame(&mut video.decoded).is_ok() {
                if let Ok(mut buffer) = pool.get() {
                    let mut rgb_frame = create_video_frame_from_buffer(
                        video.width,
                        video.height,
                        ffmpeg::format::Pixel::RGBA,
                        &mut buffer,
                    );
                    video.scaler.run(&video.decoded, &mut rgb_frame)?;
                    outputs.push(ProcessOutput::Video(VideoFrame {
                        width: video.width,
                        height: video.height,
                        data: buffer,
                        pts: video.decoded.pts(),
                    }));
                }
            }
        }
    }

    Ok(outputs)
}

pub fn flush(
    session: &mut MediaSession,
    pool: &FramePool,
) -> Result<Vec<ProcessOutput>, ffmpeg::Error> {
    let mut outputs = Vec::new();

    if let Some(video) = &mut session.video {
        video.decoder.send_eof().ok();

        while video.decoder.receive_frame(&mut video.decoded).is_ok() {
            if let Ok(mut buffer) = pool.get() {
                let mut rgb_frame = create_video_frame_from_buffer(
                    video.width,
                    video.height,
                    ffmpeg::format::Pixel::RGBA,
                    &mut buffer,
                );
                video.scaler.run(&video.decoded, &mut rgb_frame)?;
                outputs.push(ProcessOutput::Video(VideoFrame {
                    width: video.width,
                    height: video.height,
                    data: buffer,
                    pts: video.decoded.pts(),
                }));
            }
        }
    }

    Ok(outputs)
}

pub fn seek_pts(session: &mut MediaSession, pts: i64) -> Result<(), ffmpeg::Error> {
    if let Some(video) = &mut session.video {
        let position = pts.rescale(video.time_base, TIME_BASE);
        session.input_format_ctx.seek(position, ..position + 1)?;
        video.decoder.flush();
    }
    Ok(())
}
