use super::frame_pool::FramePool;
use super::session::VideoFrame;
use super::worker::{WorkerCommand, WorkerHandle, WorkerMessage, spawn_worker_thread};
use ffmpeg::rescale::Rescale;
use ffmpeg_next as ffmpeg;
use std::collections::{HashMap, VecDeque};

pub struct MediaEngine {
    next_id: u32,
    tracks: HashMap<TrackId, MediaTrack>,
}

#[derive(Eq, PartialEq, Hash, Clone, Copy)]
pub struct TrackId(u32);

#[derive(Eq, PartialEq, Clone, Debug)]
pub enum TrackState {
    Loading,
    Ready,
    Playing,
    Paused,
    Ended,
    Error(String),
}

struct MediaTrack {
    desired_state: TrackState,
    worker_state: TrackState,
    worker: WorkerHandle,
    loop_enabled: bool,
    time_base: Option<ffmpeg::Rational>,
    start_pts: Option<i64>,
    frame_pool: Option<FramePool>,
    size: Option<(u32, u32)>,
    video_queue: VecDeque<VideoFrame>,
}

impl MediaEngine {
    pub fn new() -> Self {
        Self {
            next_id: 0,
            tracks: HashMap::new(),
        }
    }

    pub fn create_track(&mut self, path: &str) -> TrackId {
        let worker = spawn_worker_thread();

        worker
            .cmd_tx
            .send(WorkerCommand::Load(path.to_string()))
            .ok();

        let id = TrackId(self.next_id);
        self.next_id += 1;

        self.tracks.insert(
            id,
            MediaTrack {
                desired_state: TrackState::Ready,
                worker_state: TrackState::Loading,
                worker: worker,
                frame_pool: None,
                loop_enabled: false,
                size: None,
                time_base: None,
                start_pts: None,
                video_queue: VecDeque::new(),
            },
        );

        id
    }

    pub fn destroy_track(&mut self, id: TrackId) {
        self.tracks.remove(&id);
    }

    /// This function is handed over to the user so that they can handle different states properly.
    /// For instance, they should initialize their textures once the track is `Ready`, they should
    /// probably early return if `Loading` display some stuff if `Playing` or `Paused`.
    pub fn get_state(&self, id: TrackId) -> Option<TrackState> {
        Some(self.tracks.get(&id)?.worker_state.clone())
    }

    pub fn play(&mut self, id: TrackId) {
        match self.tracks.get_mut(&id) {
            Some(ref mut track) => track.desired_state = TrackState::Playing,
            None => {}
        };
    }

    pub fn pause(&mut self, id: TrackId) {
        match self.tracks.get_mut(&id) {
            Some(ref mut track) => track.desired_state = TrackState::Paused,
            None => {}
        };
    }

    pub fn set_loop(&mut self, id: TrackId, enabled: bool) {
        match self.tracks.get_mut(&id) {
            Some(ref mut track) => track.loop_enabled = enabled,
            None => {}
        };
    }

    pub fn seek(&mut self, id: TrackId, seconds: f64) {
        match self.tracks.get_mut(&id) {
            Some(ref mut track) => {
                track.desired_state = TrackState::Playing;
                track.worker.cmd_tx.send(WorkerCommand::Seek(seconds)).ok();
            }
            None => {}
        };
    }

    pub fn try_get_video_frame(&mut self, id: TrackId) -> Option<VideoFrame> {
        match self.tracks.get_mut(&id) {
            Some(ref mut track) => track.video_queue.pop_back(),
            None => None,
        }
    }

    pub fn peek_video_frame(&self, id: TrackId) -> Option<&VideoFrame> {
        match self.tracks.get(&id) {
            Some(track) => track.video_queue.back(),
            None => None,
        }
    }

    pub fn reycle_video_frame_buffer(&self, id: TrackId, buffer: Vec<u8>) {
        match self.tracks.get(&id) {
            Some(track) => {
                let Some(pool) = &track.frame_pool else {
                    return;
                };
                pool.recycle(buffer).ok();
            }
            None => {}
        }
    }

    pub fn pts_in_seconds(&self, id: TrackId, pts: i64) -> Option<f64> {
        match self.tracks.get(&id) {
            Some(track) => {
                let relative_pts = pts - track.start_pts?;
                let microseconds =
                    relative_pts.rescale(track.time_base?, ffmpeg::mathematics::rescale::TIME_BASE);
                Some(microseconds as f64 / 1_000_000.0)
            }
            None => None,
        }
    }

    pub fn get_size(&self, id: TrackId) -> Option<(u32, u32)> {
        self.tracks.get(&id)?.size
    }

    pub fn update(&mut self) {
        for track in self.tracks.values_mut() {
            while let Ok(msg) = track.worker.msg_rx.try_recv() {
                match msg {
                    WorkerMessage::Initialized {
                        pool,
                        width,
                        height,
                        time_base,
                        start_pts,
                    } => {
                        track.worker_state = TrackState::Ready;
                        track.frame_pool = Some(pool);
                        track.size = Some((width, height));
                        track.time_base = Some(time_base);
                        track.start_pts = Some(start_pts);
                    }
                    WorkerMessage::VideoFrame(frame) => {
                        track.video_queue.push_front(frame);
                    }
                    WorkerMessage::Error(e) => track.worker_state = TrackState::Error(e),
                    WorkerMessage::EndOfStream => {
                        if track.loop_enabled {
                            track.worker.cmd_tx.send(WorkerCommand::Seek(0.0)).ok();
                            track.worker_state = TrackState::Playing;
                        } else {
                            track.worker_state = TrackState::Ended;
                        }
                    }
                }
            }
            if track.worker_state != track.desired_state {
                match track.desired_state {
                    TrackState::Playing => {
                        track.worker.cmd_tx.send(WorkerCommand::Play).ok();
                        track.worker_state = TrackState::Playing;
                    }
                    TrackState::Paused => {
                        track.worker.cmd_tx.send(WorkerCommand::Pause).ok();
                        track.worker_state = TrackState::Paused;
                    }
                    // If the desired state is not one of them, we ignore them as it doesn't quite
                    // make sense
                    _ => {}
                };
            }
        }
    }
}
