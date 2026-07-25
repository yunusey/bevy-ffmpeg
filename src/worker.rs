use super::frame_pool::FramePool;
use super::session::{
    DecodeEvent, MediaSession, Packet, VideoFrame, load_media_session, read_packet, seek_pts,
    try_receive_frame,
};
use crossbeam_channel::{Receiver, Sender};
use ffmpeg_next as ffmpeg;

pub struct WorkerHandle {
    pub cmd_tx: Sender<WorkerCommand>,
    pub msg_rx: Receiver<WorkerMessage>,
}

pub enum WorkerCommand {
    Load(String),
    Play,
    Pause,
    Seek(i64),
}

pub enum WorkerMessage {
    Initialized {
        width: u32,
        height: u32,
        duration: i64,
        pool: FramePool,
        time_base: ffmpeg::Rational,
        start_pts: i64,
    },
    VideoFrame(VideoFrame),
    EndOfStream,
    Error(String),
    SeekingCompleted(i64),
}

pub fn spawn_worker_thread() -> WorkerHandle {
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
    let (msg_tx, msg_rx) = crossbeam_channel::unbounded();

    std::thread::spawn(move || {
        worker_loop(cmd_rx, msg_tx);
    });

    WorkerHandle { cmd_tx, msg_rx }
}

fn worker_loop(cmd_rx: Receiver<WorkerCommand>, msg_tx: Sender<WorkerMessage>) {
    let mut session: Option<MediaSession> = None;
    let mut frame_pool: Option<FramePool> = None;

    let mut playing = false;
    let mut pending_seek: Option<i64> = None;
    let mut flushing = false;
    let mut discard_before: Option<i64> = None;

    // The loop will first drain all the commands pushed to the queue, and then will decode exactly
    // one frame (if playing) and will check if there has been a new command pushed to the queue
    // that needs to be processed. That is why we use the non-blocking `try_recv` over `recv`.
    //
    // TODO: If not playing, we should probably block until a new command is received. We are busy
    // waiting right now.
    loop {
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                WorkerCommand::Load(path) => match load_media_session(&path) {
                    Ok(s) => {
                        if let Some(video) = &s.video {
                            let pool =
                                FramePool::new(10, (video.width * video.height * 4) as usize);
                            let time_base = video.time_base;
                            let start_pts = video.start_pts;
                            msg_tx
                                .send(WorkerMessage::Initialized {
                                    width: video.width,
                                    height: video.height,
                                    duration: video.duration,
                                    pool: pool.clone(),
                                    time_base,
                                    start_pts,
                                })
                                .ok();
                            frame_pool = Some(pool);
                        };
                        session = Some(s);
                        flushing = false;
                    }
                    Err(e) => msg_tx
                        .send(WorkerMessage::Error(e.to_string()))
                        .ok()
                        .unwrap(),
                },

                WorkerCommand::Play => playing = true,
                WorkerCommand::Pause => playing = false,

                WorkerCommand::Seek(val) => {
                    pending_seek = Some(val);
                }
            }
        }

        if let Some(val) = pending_seek.take() {
            if let Some(s) = &mut session {
                match seek_pts(s, val) {
                    Err(e) => {
                        msg_tx.send(WorkerMessage::Error(e.to_string())).ok();
                    }
                    _ => {
                        discard_before = Some(val);
                    }
                }
            }
            flushing = false;
            continue;
        }

        // If we are playing or we need to keep decoding until some PTS (during seek), keep decoding.
        if (playing || discard_before.is_some())
            && let Some(s) = session.as_mut()
            && let Some(pool) = &frame_pool
        {
            match try_receive_frame(s, pool, discard_before) {
                // There is a readily available frame. Send it.
                Ok(DecodeEvent::Frame(frame)) => {
                    // We were seeking and we finally have a frame during/after `discard_before` so
                    // we completed seeking.
                    if let Some(min_pts) = discard_before {
                        if let Some(pts) = frame.pts {
                            assert!(pts >= min_pts);
                            msg_tx.send(WorkerMessage::SeekingCompleted(pts)).ok();
                            discard_before = None;
                        } else {
                            unreachable!();
                        }
                    }
                    msg_tx.send(WorkerMessage::VideoFrame(frame)).ok();
                }
                // We have received a frame that is before `discard_before` so just keep looping.
                Ok(DecodeEvent::Discarded) => {}
                // Reached the end.
                Ok(DecodeEvent::Eof) => {
                    msg_tx.send(WorkerMessage::EndOfStream).ok();
                    playing = false;
                    flushing = false;
                    discard_before = None;
                }
                // We need more packets to continue processing. Read packet, and send the packets
                // to the decoder.
                Ok(DecodeEvent::NeedData) => {
                    if flushing {
                        msg_tx.send(WorkerMessage::EndOfStream).ok();
                        playing = false;
                        flushing = false;
                        discard_before = None;
                    } else {
                        match read_packet(s) {
                            Ok(Packet::Packet(packet)) => {
                                if let Some(video) = &mut s.video {
                                    if packet.stream() == video.stream_index {
                                        if let Err(e) = video.decoder.send_packet(&packet) {
                                            msg_tx.send(WorkerMessage::Error(e.to_string())).ok();
                                            playing = false;
                                            discard_before = None;
                                        }
                                    }
                                }
                            }
                            Ok(Packet::Eof) => {
                                if let Some(video) = &mut s.video {
                                    video.decoder.send_eof().ok();
                                }
                                flushing = true;
                            }
                            Err(e) => {
                                msg_tx.send(WorkerMessage::Error(e.to_string())).ok();
                                playing = false;
                                discard_before = None;
                            }
                        }
                    }
                }
                Err(e) => {
                    msg_tx.send(WorkerMessage::Error(e.to_string())).ok();
                    playing = false;
                    discard_before = None;
                }
            }
        }
    }
}
