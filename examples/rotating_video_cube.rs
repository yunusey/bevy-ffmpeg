use bevy::prelude::*;
use bevy_ffmpeg::{FfmpegPlugin, VideoImage, VideoMessage, VideoPlayer};

/// Unfortunately, we need to store the path in the main function directly, because if we try to
/// use `setup` to read the path from the command line and then insert is as a resource (and if
/// this fails), then even if we can try to exit the app by writing an `AppExit` message, the
/// `video_update_system` will still run at least once and panic when it tries to access the
/// missing resource.
#[derive(Resource)]
struct VideoPath(String);

#[derive(Component)]
struct VideoCube;

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
        .add_plugins(FfmpegPlugin)
        .insert_resource(VideoPath(track_path))
        .add_systems(Startup, setup)
        .add_systems(
            Update,
            (on_video_ready, rotate_cube, refresh_video_materials),
        )
        .run();
}

fn setup(mut commands: Commands, video_path: Res<VideoPath>) {
    commands.spawn((
        Camera3d::default(),
        Transform::from_xyz(0.0, 1.5, 4.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));
    commands.spawn(VideoPlayer::new(video_path.0.clone()).autoplay().looping());
}

fn on_video_ready(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut messages: MessageReader<VideoMessage>,
    images: Query<&VideoImage>,
) {
    for message in messages.read() {
        match message {
            VideoMessage::Ready { entity, size: _ } => {
                let Ok(video_image) = images.get(*entity) else {
                    continue;
                };
                commands.spawn((
                    VideoCube,
                    Mesh3d(meshes.add(Cuboid::from_length(1.5))),
                    MeshMaterial3d(materials.add(StandardMaterial {
                        base_color_texture: Some(video_image.0.clone()),
                        unlit: true,
                        ..default()
                    })),
                ));
            }
            VideoMessage::Ended { entity: _ } => {}
            VideoMessage::Error { entity, message } => {
                println!("Encountered error {message} during playback");
                commands.entity(*entity).despawn();
            }
        }
    }
}

fn rotate_cube(time: Res<Time>, mut cubes: Query<&mut Transform, With<VideoCube>>) {
    let dt = time.delta_secs();
    for mut transform in &mut cubes {
        transform.rotate_y(0.6 * dt);
        transform.rotate_x(0.35 * dt);
    }
}

fn refresh_video_materials(
    mut image_events: MessageReader<AssetEvent<Image>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let updated_images: Vec<_> = image_events
        .read()
        .filter_map(|event| match event {
            AssetEvent::Modified { id } => Some(*id),
            _ => None,
        })
        .collect();

    if updated_images.is_empty() {
        return;
    }

    // Touch any material using one of these updated images so the bind group is rebuilt.
    for (_, material) in materials.iter_mut() {
        let Some(texture) = &material.base_color_texture else {
            continue;
        };
        if updated_images.contains(&texture.id()) {
            material.base_color_texture = Some(texture.clone());
        }
    }
}
