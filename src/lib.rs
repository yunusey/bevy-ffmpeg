use crossbeam_channel::{Receiver, RecvError, SendError, Sender, bounded};
use ffmpeg_next as ffmpeg;
use num_rational::Ratio;
use std::ptr;
use std::thread;

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
    let mut scaler = match ffmpeg::software::scaling::Context::get(
        decoder.format(),
        decoder.width(),
        decoder.height(),
        ffmpeg::format::Pixel::RGBA,
        decoder.width(),
        decoder.height(),
        ffmpeg::software::scaling::Flags::BILINEAR,
    ) {
        Ok(scaler) => scaler,
        Err(err) => return send_error(err),
    };

    let mut handle_frame =
        |decoded: &ffmpeg::util::frame::Video| -> Result<(), SendError<FfmpegMessage>> {
            loop {
                if let Ok(mut buffer) = pool.get() {
                    let scaler_output_definition = scaler.output();

                    let mut rgb_frame = ffmpeg::util::frame::Video::empty();
                    rgb_frame.set_width(scaler_output_definition.width);
                    rgb_frame.set_height(scaler_output_definition.height);
                    rgb_frame.set_format(scaler_output_definition.format);

                    // NOTE: This unsafe code seems to be unavoidable unfortunately. ffmpeg-next is
                    // awesome and tries to keep things as safe as possible, but unfortunately, it
                    // also puts limit to the performance to some extent. There seems to be two
                    // different ways to use ffmpeg-next's pipeline:
                    // 1. Just create an empty video frame, pass it to the scaler, and let ffmpeg
                    //    *allocate* a new buffer for you, set the video's data to be this new
                    //    buffer, and you got yourself an allocated buffer that you have to, now,
                    //    copy back to the buffer (cost: allocation + copying)
                    // 2. Get your hands dirty, and set the data of the empty video frame to be
                    //    your buffer directly so that the scaler can directly write to it and you
                    //    do not have any allocation or copying.
                    // We'll go with 2 :D
                    unsafe {
                        let frame_ptr = rgb_frame.as_mut_ptr();

                        (*frame_ptr).data[0] = buffer.as_mut_ptr();
                        (*frame_ptr).data[1] = ptr::null_mut();
                        (*frame_ptr).data[2] = ptr::null_mut();
                        (*frame_ptr).data[3] = ptr::null_mut();

                        (*frame_ptr).linesize[0] = (width * 4) as i32;
                        (*frame_ptr).linesize[1] = 0;
                        (*frame_ptr).linesize[2] = 0;
                        (*frame_ptr).linesize[3] = 0;
                    }

                    if let Err(err) = scaler.run(decoded, &mut rgb_frame) {
                        send_error(err)?;
                    }

                    return tx.send(FfmpegMessage::Frame(VideoFrame {
                        width,
                        height,
                        data: buffer,
                        pts: decoded.pts(),
                    }));
                }
                // No buffers for us :( gotta wait some time; hardcoded to 16ms, but I think I'll
                // have it configurable through plugin settings.
                std::thread::sleep(std::time::Duration::from_millis(16));
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
