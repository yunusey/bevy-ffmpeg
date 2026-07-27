use crate::engine::{MediaEngine, TrackId, TrackState};
use crate::session::VideoFrame;
use bevy::asset::RenderAssetUsages;
use bevy::math::UVec2;
use bevy::prelude::{
    Added, App, Assets, Commands, Component, Entity, Handle, Image, IntoScheduleConfigs, Message,
    MessageWriter, Plugin, Query, RemovedComponents, Res, ResMut, Resource, Time, Update,
};
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};
use std::collections::HashMap;
use std::ops::{Deref, DerefMut};

pub struct FfmpegPlugin;

/// I could just have MediaEngine to be a Resource on its own, but I want to keep the engine code
/// free of bevy so that it can, should there be a need, be used without any surrounding bevy context.
#[derive(Resource, Default)]
pub struct MediaEngineResource(MediaEngine);

/// When our `cleanup_destroyed_tracks` system runs, the data part of `VideoPlayer` and likely any
/// other `Component` data the entity had will have been destroyed. So, there is no way for us to
/// get the track id of the entity there.
///
/// When I was trying to figure out if I did something wrong, which I still may have, I found that
/// [`RemovedComponents` docs](https://docs.rs/bevy/0.14.0/bevy/ecs/prelude/struct.RemovedComponents.html)
/// > Note that this does not allow you to see which data existed before removal.
/// > If you need this, you will need to track the component data value on your own,
/// > using a regularly scheduled system that requests `Query<(Entity, &T), Changed<T>>`
/// > and stores the data somewhere safe to later cross-reference.
/// This is what this resource is for. It helps us destroy tracks once all of its data is removed.
///
/// NOTE: We know that our track id won't change after creation, so we don't really need to use a
/// separate system for achieving this.
#[derive(Resource, Default)]
pub struct EntityTrackMap(HashMap<Entity, TrackId>);

impl Deref for MediaEngineResource {
    type Target = MediaEngine;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl DerefMut for MediaEngineResource {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

/// After some thinking, I decided to use f64, representing seconds, instead of raw PTS in the plugin API.
/// I think, in the long run, it makes more sense for most users as it is much easier to integrate with Bevy's
/// time API. So, the plugin will be responsible for converting from seconds to raw PTS.
#[derive(Component)]
pub struct VideoPlayer {
    source: String,
    playing: bool,
    looping: bool,
    seek_to: Option<f64>,

    // The system will set these for us. We won't expose the clock directly.
    position: f64,
    duration: Option<f64>,
}

impl VideoPlayer {
    pub fn new(source: String) -> Self {
        Self {
            source,
            playing: false,
            looping: false,
            seek_to: None,
            position: 0.0,
            duration: None,
        }
    }
    pub fn autoplay(mut self) -> Self {
        self.playing = true;
        self
    }
    pub fn looping(mut self) -> Self {
        self.looping = true;
        self
    }
    pub fn is_playing(&self) -> bool {
        self.playing
    }
    pub fn play(&mut self) {
        self.playing = true;
    }
    pub fn pause(&mut self) {
        self.playing = false;
    }
    pub fn toggle_playing(&mut self) {
        self.playing = !self.playing;
    }
    pub fn seek_to(&mut self, time: f64) {
        self.seek_to = Some(time);
    }
    pub fn get_position(&self) -> f64 {
        self.position
    }
    pub fn get_duration(&self) -> Option<f64> {
        self.duration
    }
}

/// Once a `VideoPlayer` is added to the engine, it is assigned a track id, which we need to call
/// internal functions.
///
/// Similar to `MediaEngine`, this can directly be a `Component` on its own, but for now, we'll
/// just have a wrapper.
#[derive(Component, Copy, Clone)]
struct VideoTrack(TrackId);

impl Deref for VideoTrack {
    type Target = TrackId;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl DerefMut for VideoTrack {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

/// When the user seeks, pauses, or does anything that changes the overall playback, we also change
/// the properties of `VideoClock`. So, `base_sec` and `base_pts` refers to the last time we
/// synchronized the clock with the playback--when we restarted playing and which pts we started
/// the playback from.
#[derive(Component, Clone, Copy)]
struct VideoClock {
    base_sec: f64,
    base_pts: i64,
    frame_pts: i64,
}

/// When the video is `Ready`, we will create an image texture for it. Having this struct as a
/// separate `Component` is useful for us to query, for instance, the videos that are currently
/// playing or the ones that are ready, and take specific actions for them.
#[derive(Component)]
pub struct VideoImage(pub Handle<Image>);

/// We'll use `Event`s to communicate with the users. When switching from `Loading` to `Ready` or
/// when the worker encountered an error, we should communicate so that they can take the necessary
/// steps in their programs.
#[derive(Message)]
pub enum VideoMessage {
    Ready { entity: Entity, size: UVec2 },
    Ended { entity: Entity },
    Error { entity: Entity, message: String },
}

impl Plugin for FfmpegPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<MediaEngineResource>()
            .init_resource::<EntityTrackMap>()
            .add_message::<VideoMessage>()
            .add_systems(
                Update,
                (
                    create_new_tracks,
                    sync_engine_state,
                    update_engine,
                    sync_frames_with_images,
                    cleanup_destroyed_tracks,
                )
                    .chain(),
            );
    }
}

/// Creates tracks for each new `VideoPlayer` spawned in the engine.
fn create_new_tracks(
    mut commands: Commands,
    mut media_engine: ResMut<MediaEngineResource>,
    mut entity_track_map: ResMut<EntityTrackMap>,
    query: Query<(Entity, &VideoPlayer), Added<VideoPlayer>>,
) {
    for (entity_id, video_player) in query.iter() {
        let track_id = media_engine.create_track(&video_player.source);
        commands.entity(entity_id).insert(VideoTrack(track_id));
        entity_track_map.0.insert(entity_id, track_id);
    }
}

fn sync_engine_state(
    time: Res<Time>,
    mut commands: Commands,
    mut media_engine: ResMut<MediaEngineResource>,
    mut images: ResMut<Assets<Image>>,
    mut video_messages: MessageWriter<VideoMessage>,
    mut query: Query<(
        Entity,
        &mut VideoPlayer,
        &VideoTrack,
        Option<&mut VideoClock>,
    )>,
) {
    for (entity_id, mut video_player, &track_id, clock) in query.iter_mut() {
        let Some(state) = media_engine.get_state(*track_id) else {
            continue;
        };
        match (
            state,
            video_player.playing,
            video_player.looping,
            video_player.seek_to,
        ) {
            (TrackState::Playing, false, _, None) => {
                let Some(mut clock) = clock else {
                    continue;
                };
                media_engine.pause(*track_id);
                clock.base_pts = clock.frame_pts;
            }
            (TrackState::Paused, true, _, None) => {
                let Some(mut clock) = clock else {
                    continue;
                };
                media_engine.play(*track_id);
                clock.base_sec = time.elapsed_secs_f64();
                clock.base_pts = clock.frame_pts;
            }
            (TrackState::Ended, _, true, None) => {
                let Some(mut clock) = clock else {
                    continue;
                };
                media_engine.seek_beginning(*track_id);
                clock.base_sec = time.elapsed_secs_f64();
                clock.base_pts = media_engine.get_start_pts(*track_id).unwrap_or(0);
            }
            (TrackState::Ended, _, false, None) => {
                video_messages.write(VideoMessage::Ended { entity: entity_id });
            }
            (TrackState::Playing | TrackState::Paused | TrackState::Ended, _, _, Some(seek)) => {
                let Some(pts) = media_engine.seconds_in_pts(*track_id, seek) else {
                    continue;
                };
                let Some(mut clock) = clock else {
                    continue;
                };
                media_engine.seek(*track_id, pts);
                video_player.seek_to = None;
                clock.base_sec = time.elapsed_secs_f64();
                clock.base_pts = pts;
                clock.frame_pts = pts;
            }
            (TrackState::Ready, _, _, _) => {
                let (width, height) = media_engine.get_size(*track_id).unwrap();
                let image = Image::new_uninit(
                    Extent3d {
                        width,
                        height,
                        depth_or_array_layers: 1,
                    },
                    TextureDimension::D2,
                    TextureFormat::Rgba8UnormSrgb,
                    RenderAssetUsages::MAIN_WORLD | RenderAssetUsages::RENDER_WORLD,
                );
                let handle = images.add(image);
                commands.entity(entity_id).insert(VideoImage(handle));

                let pts = media_engine.get_start_pts(*track_id).unwrap_or(0);
                let clock = VideoClock {
                    base_sec: time.elapsed_secs_f64(),
                    base_pts: pts,
                    frame_pts: pts,
                };
                commands.entity(entity_id).insert(clock);
                if video_player.playing {
                    media_engine.play(*track_id);
                }

                video_player.position = media_engine.pts_in_seconds(*track_id, pts).unwrap_or(0f64);
                video_player.duration = media_engine
                    .get_duration(*track_id)
                    .and_then(|d| media_engine.pts_in_seconds(*track_id, d));

                video_messages.write(VideoMessage::Ready {
                    entity: entity_id,
                    size: UVec2 {
                        x: width,
                        y: height,
                    },
                });
            }
            (TrackState::Error(message), _, _, _) => {
                video_messages.write(VideoMessage::Error {
                    entity: entity_id,
                    message,
                });
            }
            _ => {}
        };
    }
}

fn update_engine(mut media_engine: ResMut<MediaEngineResource>) {
    media_engine.update();
}

fn sync_frames_with_images(
    time: Res<Time>,
    mut media_engine: ResMut<MediaEngineResource>,
    mut images: ResMut<Assets<Image>>,
    mut query: Query<(
        Entity,
        &mut VideoPlayer,
        &VideoTrack,
        &mut VideoClock,
        &VideoImage,
    )>,
) {
    let current_time = time.elapsed_secs_f64();
    for (entity_id, mut video_player, &track_id, mut clock, image_handler) in query.iter_mut() {
        let Some(state) = media_engine.get_state(*track_id) else {
            continue;
        };
        match state {
            TrackState::Playing => {
                let Some(track_sec) = media_engine.pts_in_seconds(*track_id, clock.base_pts) else {
                    continue;
                };
                let target_sec = current_time - clock.base_sec + track_sec;
                let mut best_frame: Option<VideoFrame> = None;
                while let Some(frame) = media_engine.peek_video_frame(*track_id) {
                    // We don't support invalid pts for now. I don't really know much about how the
                    // video files with frames without pts info work. So, I guess, it makes sense
                    // to just continue peeking for a new frame?
                    let Some(frame_pts) = frame.pts else {
                        let frame = media_engine.try_get_video_frame(*track_id).unwrap();
                        media_engine.reycle_video_frame_buffer(*track_id, frame.data);
                        continue;
                    };

                    let Some(frame_sec) = media_engine.pts_in_seconds(*track_id, frame_pts) else {
                        continue;
                    };

                    if frame_sec <= target_sec {
                        let frame = media_engine.try_get_video_frame(*track_id).unwrap();
                        if let Some(old_best_frame) = best_frame.take() {
                            media_engine.reycle_video_frame_buffer(*track_id, old_best_frame.data);
                        }
                        best_frame = Some(frame);
                    }
                    // We will assume that the next frame is in the future, so we break here.
                    else {
                        break;
                    }
                }
                // We couldn't find a good frame... just stick to the old one.
                let Some(frame) = best_frame else {
                    continue;
                };
                let Some(image) = images.get_mut(&image_handler.0) else {
                    continue;
                };
                if let Some(old_buffer) = image.data.replace(frame.data) {
                    media_engine.reycle_video_frame_buffer(*track_id, old_buffer);
                }
                // We know that it is `Some`.
                (*clock).frame_pts = frame.pts.unwrap();
                if let Some(position_sec) =
                    media_engine.pts_in_seconds(*track_id, (*clock).frame_pts)
                {
                    video_player.position = position_sec;
                }
            }
            TrackState::Paused => {
                // While paused (e.g. after a seek), show the newest queued frame once.
                let mut best_frame: Option<VideoFrame> = None;
                while let Some(frame) = media_engine.try_get_video_frame(*track_id) {
                    if frame.pts.is_none() {
                        media_engine.reycle_video_frame_buffer(*track_id, frame.data);
                        continue;
                    }
                    if let Some(old) = best_frame.take() {
                        media_engine.reycle_video_frame_buffer(*track_id, old.data);
                    }
                    best_frame = Some(frame);
                }
                let Some(frame) = best_frame else {
                    continue;
                };
                let Some(image) = images.get_mut(&image_handler.0) else {
                    continue;
                };
                if let Some(old_buffer) = image.data.replace(frame.data) {
                    media_engine.reycle_video_frame_buffer(*track_id, old_buffer);
                }
                // We know that it is `Some`.
                let pts = frame.pts.unwrap();

                // Keep clock aligned so Play resumes from this scrub position.
                clock.frame_pts = pts;
                clock.base_pts = pts;
                clock.base_sec = current_time;
                if let Some(position_sec) = media_engine.pts_in_seconds(*track_id, pts) {
                    video_player.position = position_sec;
                }
            }
            _ => {}
        }
    }
}

fn cleanup_destroyed_tracks(
    mut media_engine: ResMut<MediaEngineResource>,
    mut entity_track_map: ResMut<EntityTrackMap>,
    mut removals: RemovedComponents<VideoPlayer>,
) {
    for entity in removals.read() {
        match entity_track_map.0.remove_entry(&entity) {
            Some((_, track_id)) => media_engine.destroy_track(track_id),
            None => {}
        };
    }
}
