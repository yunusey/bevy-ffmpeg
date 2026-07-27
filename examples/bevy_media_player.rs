use bevy::prelude::*;
use bevy_egui::{EguiContexts, EguiPlugin, EguiPrimaryContextPass, egui};
use bevy_ffmpeg::{FfmpegPlugin, VideoImage, VideoMessage, VideoPlayer};

/// Unfortunately, we need to store the path in the main function directly, because if we try to
/// use `setup` to read the path from the command line and then insert is as a resource (and if
/// this fails), then even if we can try to exit the app by writing an `AppExit` message, the
/// `video_update_system` will still run at least once and panic when it tries to access the
/// missing resource.
#[derive(Resource)]
struct VideoPath(String);

#[derive(Resource)]
struct UIState {
    slider_pos: f64,
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
        .add_plugins(FfmpegPlugin)
        .insert_resource(VideoPath(track_path))
        .add_systems(Startup, setup)
        .add_systems(EguiPrimaryContextPass, overlay_ui)
        .add_systems(Update, on_video_ready)
        .run();
}

fn setup(mut commands: Commands, video_path: Res<VideoPath>) {
    commands.spawn(Camera2d::default());
    commands.spawn(VideoPlayer::new(video_path.0.clone()).looping());
    commands.insert_resource(UIState { slider_pos: 0f64 });
}

fn on_video_ready(
    mut commands: Commands,
    mut messages: MessageReader<VideoMessage>,
    images: Query<&VideoImage>,
) {
    for message in messages.read() {
        match message {
            VideoMessage::Ready { entity, size: _ } => {
                let Ok(video_image) = images.get(*entity) else {
                    continue;
                };
                commands.spawn(Sprite::from_image(video_image.0.clone()));
            }
            VideoMessage::Ended { entity: _ } => {}
            VideoMessage::Error { entity, message } => {
                println!("Encountered error {message} during playback");
                commands.entity(*entity).despawn();
            }
        }
    }
}

fn overlay_ui(
    mut contexts: EguiContexts,
    mut ui_state: ResMut<UIState>,
    mut query: Query<&mut VideoPlayer>,
) {
    let Ok(mut player) = query.single_mut() else {
        return;
    };

    let Ok(context) = contexts.ctx_mut() else {
        eprintln!("Couldn't get the context in egui");
        return;
    };
    egui::Area::new(egui::Id::new("controls"))
        .anchor(egui::Align2::CENTER_BOTTOM, [0.0, -20.0])
        .show(context, |ui| {
            ui.horizontal(|ui| {
                if ui
                    .button(match player.is_playing() {
                        true => "Pause",
                        false => "Play",
                    })
                    .clicked()
                {
                    player.toggle_playing();
                }

                let position = player.get_position();
                let duration = player.get_duration().unwrap_or(100f64); // don't know what to do...
                ui_state.slider_pos = position;
                if ui
                    .add(
                        egui::Slider::new(&mut ui_state.slider_pos, 0f64..=duration)
                            .show_value(false),
                    )
                    .changed()
                {
                    player.seek_to(ui_state.slider_pos);
                }
                ui.label(format!("{:.1}s", position));
            });
        });
}
