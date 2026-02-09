use super::frame_pool::FramePool;
use super::session::{
    MediaSession, Packet, ProcessOutput, VideoFrame, flush, load_media_session, process_packet,
    read_packet,
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
    Seek(f64),
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
}

pub fn spawn_worker_thread() -> WorkerHandle {
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
    let (msg_tx, msg_rx) = crossbeam_channel::unbounded();

    std::thread::spawn(move || {
        worker_loop(cmd_rx, msg_tx);
    });

    WorkerHandle { cmd_tx, msg_rx }
}

pub fn worker_loop(cmd_rx: Receiver<WorkerCommand>, msg_tx: Sender<WorkerMessage>) {
    let mut session: Option<MediaSession> = None;
    let mut frame_pool: Option<FramePool> = None;

    let mut playing = false;

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
                    }
                    Err(e) => msg_tx
                        .send(WorkerMessage::Error(e.to_string()))
                        .ok()
                        .unwrap(),
                },

                WorkerCommand::Play => playing = true,
                WorkerCommand::Pause => playing = false,

                // The most difficult one probably :D
                WorkerCommand::Seek(val) => _ = val,
            }
        }

        if playing {
            if let Some(s) = session.as_mut()
                && let Some(pool) = &frame_pool
            {
                match read_packet(s) {
                    Ok(Packet::Packet(packet)) => {
                        if let Ok(outputs) = process_packet(s, &packet, &pool) {
                            for output in outputs {
                                match output {
                                    ProcessOutput::Video(frame) => {
                                        msg_tx.send(WorkerMessage::VideoFrame(frame)).ok();
                                    }
                                }
                            }
                        }
                    }

                    Ok(Packet::Eof) => {
                        if let Ok(outputs) = flush(s, &pool) {
                            for output in outputs {
                                match output {
                                    ProcessOutput::Video(frame) => {
                                        msg_tx.send(WorkerMessage::VideoFrame(frame)).ok();
                                    }
                                }
                            }
                        }

                        msg_tx.send(WorkerMessage::EndOfStream).ok();
                        playing = false;
                    }

                    Err(e) => {
                        msg_tx.send(WorkerMessage::Error(e.to_string())).ok();
                    }
                }
            }
        }
    }
}
