use bevy::prelude::*;
use bevy::render::render_resource::*;
use bevy::shader::ShaderRef;
use bevy::{
    asset::RenderAssetUsages,
    sprite_render::{AlphaMode2d, Material2d, Material2dPlugin, MeshMaterial2d},
    window::WindowResized,
};
use bevy_egui::{EguiContexts, EguiPlugin, EguiPrimaryContextPass, egui};
use bevy_ffmpeg::{FfmpegMessage, FramePool, VideoFrame, spawn_ffmpeg_thread};
use crossbeam_channel::{Receiver, unbounded};
use num_rational::Ratio;
use std::collections::VecDeque;

#[derive(Resource)]
struct FfmpegMessageResource {
    rx: Receiver<FfmpegMessage>,
    frame_pool: Option<FramePool>,
}

#[derive(Resource, Default)]
struct VideoTexture {
    handle: Option<Handle<Image>>,
}

#[derive(Resource)]
struct VideoPlayback {
    start_time: Option<f64>,
    base_time: Option<Ratio<i32>>,
    pending_frames: VecDeque<VideoFrame>,
}

fn main() {
    let (ffmpeg_tx, ffmpeg_rx) = unbounded();
    spawn_ffmpeg_thread(
        ffmpeg_tx,
        &std::env::args()
            .nth(1)
            .expect("No video path specified.\nUsage: pixfx <path/to/video/file>"),
    );
    App::new()
        .insert_resource(FfmpegMessageResource {
            rx: ffmpeg_rx,
            frame_pool: None,
        })
        .add_plugins(DefaultPlugins)
        .add_plugins(EguiPlugin::default())
        .add_systems(Startup, setup)
        .add_systems(EguiPrimaryContextPass, overlay_ui)
        .add_systems(Update, video_update_system)
        .run();
}

fn setup(mut commands: Commands) {
    commands.spawn(Camera2d::default());
    commands.insert_resource(VideoTexture { handle: None });
}

fn video_update_system(
    time: Res<Time>,
    mut commands: Commands,
    mut images: ResMut<Assets<Image>>,
    mut video_texture: ResMut<VideoTexture>,
    mut video_playback: Option<ResMut<VideoPlayback>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut ffmpeg_message_res: ResMut<FfmpegMessageResource>,
) {
    let current_time = time.elapsed_secs_f64();
    // Handle all messages at once. Not necessarily what we want though :D
    while let Ok(msg) = ffmpeg_message_res.rx.try_recv() {
        match msg {
            FfmpegMessage::Init {
                pool,
                width,
                height,
                time_base,
                fps,
            } => {
                ffmpeg_message_res.frame_pool = Some(pool);

                let video_playback = VideoPlayback {
                    base_time: Some(time_base),
                    start_time: Some(current_time),
                    pending_frames: VecDeque::new(),
                };
                commands.insert_resource(video_playback);

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
            }
            FfmpegMessage::Frame(frame) => {
                let Some(ref mut video_playback) = video_playback else {
                    return;
                };
                video_playback.pending_frames.push_front(frame);
            }
            FfmpegMessage::EndOfFile => {
                println!("Ffmpeg: End of file reached");
            }
            FfmpegMessage::Error(err) => {
                println!("Ffmpeg: Error `{}`", err);
            }
        }
    }

    let Some(mut video_playback) = video_playback else {
        return;
    };

    let (Some(start_time), Some(time_base)) = (video_playback.start_time, video_playback.base_time)
    else {
        return;
    };
    // I know it is confusing :D will find better variable names in the future!
    let base_time = *time_base.numer() as f64 / *time_base.denom() as f64;
    let playback_time = current_time - start_time;
    while let Some(frame) = video_playback.pending_frames.back() {
        // We don't support invalid pts for now.
        let Some(pts) = frame.pts else {
            continue;
        };
        let pts_in_seconds = (pts as f64) * base_time;
        println!("{pts_in_seconds}: {playback_time}");
        let frame_time = pts_in_seconds - start_time;
        if frame_time <= playback_time {
            // This is just our frame :D
            let frame = video_playback.pending_frames.pop_back().unwrap();
            let Some(handle) = &video_texture.handle else {
                continue;
            };
            let Some(image) = images.get_mut(handle) else {
                continue;
            };
            let old_data = image.data.replace(frame.data);
            if let (Some(old_buf), Some(pool)) = (old_data, &ffmpeg_message_res.frame_pool) {
                pool.recycle(old_buf).ok();
            }
        }
        // We will assume that the next frame is in the future for now, but this may not be a good
        // idea.
        else {
            break;
        }
    }
}

fn overlay_ui(mut contexts: EguiContexts) {
    let Ok(context) = contexts.ctx_mut() else {
        eprintln!("Couldn't get the context in egui");
        return;
    };
    egui::Area::new(egui::Id::new("controls"))
        .anchor(egui::Align2::CENTER_BOTTOM, [0.0, -20.0])
        .show(context, |ui| {
            ui.horizontal(|ui| {
                if ui.button("Play/Pause").clicked() {}

                let duration = 100.0;
                let mut position = 0.0;
                ui.add(egui::Slider::new(&mut position, 0.0..=duration).show_value(false));

                ui.label(format!("{:.1}s", position));
            });
        });
}
