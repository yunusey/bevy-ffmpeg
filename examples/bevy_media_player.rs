use bevy::asset::RenderAssetUsages;
use bevy::prelude::*;
use bevy::render::render_resource::*;
use bevy_egui::{EguiContexts, EguiPlugin, EguiPrimaryContextPass, egui};
use bevy_ffmpeg::{MediaEngine, TrackId, TrackState, VideoFrame};

/// Unfortunately, we need to store the path in the main function directly, because if we try to
/// use `setup` to read the path from the command line and then insert is as a resource (and if
/// this fails), then even if we can try to exit the app by writing an `AppExit` message, the
/// `video_update_system` will still run at least once and panic when it tries to access the
/// missing resource.
#[derive(Resource)]
struct VideoPath(String);

#[derive(Resource)]
struct FfmpegData {
    media_engine: MediaEngine,
    track_id: TrackId,
}

#[derive(Resource, Default)]
struct VideoTexture {
    handle: Option<Handle<Image>>,
}

#[derive(Resource)]
struct VideoPlayback {
    playback_init_time: f64,
    playback_init_pts: i64,
    playback_frame_pts: i64,
}

fn main() {
    let track_path = match std::env::args().nth(1) {
        Some(path) => path,
        None => {
            eprintln!("Please provide a path to a video/image file as the first argument");
            return;
        }
    };

    App::new()
        .add_plugins(DefaultPlugins)
        .add_plugins(EguiPlugin::default())
        .add_systems(Startup, setup)
        .add_systems(EguiPrimaryContextPass, overlay_ui)
        .add_systems(Update, video_update_system)
        .insert_resource(VideoPath(track_path))
        .run();
}

fn setup(mut commands: Commands, video_path: Res<VideoPath>) {
    commands.spawn(Camera2d::default());
    commands.insert_resource(VideoTexture { handle: None });

    let mut engine = MediaEngine::new();
    let track_id = engine.create_track(&video_path.0);
    commands.insert_resource(FfmpegData {
        media_engine: engine,
        track_id,
    })
}

fn video_update_system(
    time: Res<Time>,
    mut commands: Commands,
    mut images: ResMut<Assets<Image>>,
    mut video_texture: ResMut<VideoTexture>,
    video_playback: Option<ResMut<VideoPlayback>>,
    mut ffmpeg_data: ResMut<FfmpegData>,
) {
    let current_time = time.elapsed_secs_f64();
    let track_id = ffmpeg_data.track_id;
    let engine: &mut MediaEngine = &mut ffmpeg_data.media_engine;

    engine.update();

    match engine.get_state(track_id).unwrap() {
        TrackState::Loading => return,
        TrackState::Ready => {
            let (width, height) = engine.get_size(track_id).unwrap();
            // We don't need to initialize the image--it will be overridden by a frame message
            // right away anyway.
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
            video_texture.handle = Some(handle.clone());
            commands.spawn(Sprite::from_image(handle.clone()));

            let video_playback = VideoPlayback {
                playback_init_time: current_time,
                playback_init_pts: 0,
                playback_frame_pts: 0,
            };
            commands.insert_resource(video_playback);

            // Now, we need to ask the engine to play our video
            engine.play(track_id);
            return;
        }
        // If paused, we just early return. However, of course, we still need to let our engine
        // update itself so that it can handle messages from the worker thread and stuff. But we
        // don't want to update the texture or anything similar.
        TrackState::Paused => {
            return;
        }
        _ => {}
    }

    let Some(mut video_playback) = video_playback else {
        return;
    };

    // This loop will traverse the deque of frames and choose the one that is just before our
    // current playback time. All frames that are to the left of the best frame have pts lower than
    // it, so we recycle them along the way. Uploading to GPU is expensive, so we try not to do
    // that here :D
    let playback_time = current_time - video_playback.playback_init_time
        + engine
            .pts_in_seconds(track_id, video_playback.playback_init_pts)
            .unwrap();
    let mut best_frame: Option<VideoFrame> = None;
    while let Some(frame) = engine.peek_video_frame(track_id) {
        // We don't support invalid pts for now.
        let Some(pts) = frame.pts else {
            let frame = engine.try_get_video_frame(track_id).unwrap();
            engine.reycle_video_frame_buffer(track_id, frame.data);
            continue;
        };

        let Some(pts_in_seconds) = engine.pts_in_seconds(track_id, pts) else {
            continue;
        };

        if pts_in_seconds <= playback_time {
            let frame = engine.try_get_video_frame(track_id).unwrap();
            if let Some(old_best_frame) = best_frame.take() {
                engine.reycle_video_frame_buffer(track_id, old_best_frame.data);
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
        return;
    };

    // We have a good frame, so we upload it to GPU. We also recycle the old buffer if there is one.
    let Some(handle) = &video_texture.handle else {
        return;
    };
    let Some(image) = images.get_mut(handle) else {
        return;
    };
    if let Some(old_buffer) = image.data.replace(frame.data) {
        engine.reycle_video_frame_buffer(track_id, old_buffer);
    }
    video_playback.playback_frame_pts = frame.pts.unwrap();
}

fn overlay_ui(
    time: Res<Time>,
    mut contexts: EguiContexts,
    mut ffmpeg_data: ResMut<FfmpegData>,
    video_playback: Option<ResMut<VideoPlayback>>,
) {
    let track_id: TrackId = ffmpeg_data.track_id;
    let engine: &mut MediaEngine = &mut ffmpeg_data.media_engine;

    let Ok(context) = contexts.ctx_mut() else {
        eprintln!("Couldn't get the context in egui");
        return;
    };
    egui::Area::new(egui::Id::new("controls"))
        .anchor(egui::Align2::CENTER_BOTTOM, [0.0, -20.0])
        .show(context, |ui| {
            ui.horizontal(|ui| {
                let Some(mut video_playback) = video_playback else {
                    return;
                };
                if ui.button("Play/Pause").clicked() {
                    match engine.get_state(track_id).unwrap() {
                        TrackState::Playing => {
                            engine.pause(track_id);
                        }
                        TrackState::Paused => {
                            // We continue from the current pts when we resume.
                            video_playback.playback_init_time = time.elapsed_secs_f64();
                            video_playback.playback_init_pts = video_playback.playback_frame_pts;
                            engine.play(track_id);
                        }
                        _ => {}
                    };
                }

                let duration = engine.get_duration(track_id).unwrap_or(0);
                let mut position = video_playback.playback_frame_pts;
                ui.add(egui::Slider::new(&mut position, 0..=duration).show_value(false));

                let position_in_secs = engine.pts_in_seconds(track_id, position).unwrap_or(0.0);
                ui.label(format!("{:.1}s", position_in_secs));
            });
        });
}
