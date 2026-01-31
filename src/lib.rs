use crossbeam_channel::{Receiver, RecvError, SendError, Sender, bounded};
use ffmpeg_next as ffmpeg;
use ffmpeg_sys_next as sys;
use num_rational::Ratio;
use std::ptr;
use std::thread;

/// RAII wrapper for SwsContext that automatically frees the context when dropped
struct SwsContextWrapper(*mut sys::SwsContext);

impl SwsContextWrapper {
    unsafe fn new(src_w: u32, src_h: u32, src_fmt: sys::AVPixelFormat) -> Option<Self> {
        let ctx = sys::sws_getContext(
            src_w as i32,
            src_h as i32,
            src_fmt,
            src_w as i32,
            src_h as i32,
            sys::AVPixelFormat::AV_PIX_FMT_RGBA,
            sys::SwsFlags::SWS_BILINEAR as i32,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
        );

        if ctx.is_null() {
            None
        } else {
            Some(SwsContextWrapper(ctx))
        }
    }

    fn as_ptr(&self) -> *mut sys::SwsContext {
        self.0
    }
}

impl Drop for SwsContextWrapper {
    fn drop(&mut self) {
        unsafe {
            sys::sws_freeContext(self.0);
        }
    }
}

unsafe fn scale_into_pool_buffer(
    sws: *mut sys::SwsContext,
    frame: *const sys::AVFrame,
    width: u32,
    height: u32,
    buffer: &mut [u8],
) {
    let mut dst_data: [*mut u8; 4] = [
        buffer.as_mut_ptr(),
        ptr::null_mut(),
        ptr::null_mut(),
        ptr::null_mut(),
    ];

    let mut dst_linesize: [i32; 4] = [(width as i32) * 4, 0, 0, 0];

    sys::sws_scale(
        sws,
        (*frame).data.as_ptr() as *const *const u8,
        (*frame).linesize.as_ptr(),
        0,
        height as i32,
        dst_data.as_mut_ptr(),
        dst_linesize.as_mut_ptr(),
    );
}

#[derive(Debug, Clone)]
pub struct FramePool {
    free_rx: Receiver<Vec<u8>>,
    free_tx: Sender<Vec<u8>>,
}

impl FramePool {
    /// `num_buffers` is the number of different buffers allocated. The more you have, the more
    /// memory you allocate.
    ///
    /// `frame_size` is the number of bytes each buffer contains. If your pixels are in the form of
    /// RGBA8 (it is for us), then this should be equal to `width * height * 4` since each pixel is
    /// 4 bytes long.
    pub fn new(num_buffers: usize, frame_size: usize) -> Self {
        let (tx, rx) = bounded(num_buffers);
        for _ in 0..num_buffers {
            tx.send(vec![0u8; frame_size])
                .expect("Couldn't setup buffers for ffmpeg");
        }
        Self {
            free_tx: tx,
            free_rx: rx,
        }
    }

    pub fn get(&self) -> Result<Vec<u8>, RecvError> {
        return self.free_rx.recv();
    }

    pub fn recycle(&self, buf: Vec<u8>) -> Result<(), SendError<Vec<u8>>> {
        return self.free_tx.send(buf);
    }
}

#[derive(Debug)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
    pub pts: Option<i64>,
}

#[derive(Debug)]
pub enum FfmpegMessage {
    Init {
        pool: FramePool,
        width: u32,
        height: u32,
        time_base: Ratio<i32>,
        fps: Ratio<i32>,
    },
    Frame(VideoFrame),
    EndOfFile,
    Error(String),
}

fn run_ffmpeg(tx: Sender<FfmpegMessage>, path: &str) -> Result<(), SendError<FfmpegMessage>> {
    let send_error = |err: ffmpeg::Error| -> Result<(), SendError<FfmpegMessage>> {
        tx.send(FfmpegMessage::Error(err.to_string()))
    };

    if let Err(err) = ffmpeg::init() {
        return send_error(err);
    }

    let mut input_format_ctx = match ffmpeg::format::input(&path) {
        Ok(ctx) => ctx,
        Err(err) => return send_error(err),
    };
    let input = match input_format_ctx.streams().best(ffmpeg::media::Type::Video) {
        Some(input) => input,
        None => {
            return tx.send(FfmpegMessage::Error(
                "The file exists, but it doesn't contain a video stream".to_string(),
            ));
        }
    };
    let stream_index = input.index();

    let context = match ffmpeg::codec::context::Context::from_parameters(input.parameters()) {
        Ok(ctx) => ctx,
        Err(err) => return send_error(err),
    };
    let mut decoder = match context.decoder().video() {
        Ok(dec) => dec,
        Err(err) => return send_error(err),
    };

    // NOTE: Technically, this is not a good approach, because the width and height *can* change in
    // weird formats throughout the video. However, for our project, we'll just assume that the
    // videos are uncorrupted :D
    let width = decoder.width();
    let height = decoder.height();
    let pool = FramePool::new(10, (width * height * 4) as usize);
    let time_base = input.time_base();
    let fps = input.avg_frame_rate();
    tx.send(FfmpegMessage::Init {
        pool: pool.clone(),
        width: width,
        height: height,
        time_base: Ratio::new(time_base.0, time_base.1),
        fps: Ratio::new(fps.0, fps.1),
    })?;

    let mut decoded = ffmpeg::util::frame::Video::empty();
    let sws_context =
        match unsafe { SwsContextWrapper::new(width, height, decoder.format().into()) } {
            Some(ctx) => ctx,
            None => {
                return tx.send(FfmpegMessage::Error(
                    "Couldn't create sws_context... quitting from ffmpeg thread".to_string(),
                ));
            }
        };

    let handle_frame =
        |decoded: &ffmpeg::util::frame::Video| -> Result<(), SendError<FfmpegMessage>> {
            loop {
                if let Ok(mut buffer) = pool.get() {
                    unsafe {
                        scale_into_pool_buffer(
                            sws_context.as_ptr(),
                            decoded.as_ptr(),
                            width,
                            height,
                            &mut buffer,
                        );
                    }

                    return tx.send(FfmpegMessage::Frame(VideoFrame {
                        width,
                        height,
                        data: buffer,
                        pts: decoded.pts(),
                    }));
                }
                // No buffers for us :( gotta wait some time
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        };

    for (stream, packet) in input_format_ctx.packets() {
        if stream.index() != stream_index {
            continue;
        }

        if let Err(err) = decoder.send_packet(&packet) {
            tx.send(FfmpegMessage::Error(err.to_string()))?;
            continue;
        }

        while decoder.receive_frame(&mut decoded).is_ok() {
            handle_frame(&decoded)?;
        }
    }

    // We flush and if there are anymore frames left, we decode them.
    decoder.send_eof().ok();
    while decoder.receive_frame(&mut decoded).is_ok() {
        handle_frame(&decoded)?;
    }

    tx.send(FfmpegMessage::EndOfFile)?;

    Ok(())
}

pub fn spawn_ffmpeg_thread(tx: Sender<FfmpegMessage>, path: &str) {
    let path = path.to_string();

    thread::spawn(move || {
        if let Err(e) = run_ffmpeg(tx, &path) {
            eprintln!("ffmpeg error: {e}");
        }
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {}
}
