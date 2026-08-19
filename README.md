# bevy-ffmpeg

A Bevy plugin for playing video with FFmpeg.

The aim is a Bevy-native playback API, not a thin FFmpeg wrapper. You spawn a
`VideoPlayer` entity, the plugin decodes on a background thread, and frames
land in a Bevy `Image` you can use with sprites, UI, or materials. Playback
time is in seconds so it lines up with Bevy's `Time`. The decode engine itself
stays free of Bevy, in case you want it without an ECS.

**This project is a work in progress and the API is unstable.**

## Status

Working today:

- Video decode for anything FFmpeg can open
- Play, pause, seek, and looping
- Several videos at once, each on its own entity
- `VideoMessage` for ready, ended, and error

Not there yet (but one day :D):

- Audio
- Hardware-accelerated decode

## Usage

Add `FfmpegPlugin`, spawn a `VideoPlayer`, and attach a sprite (or anything else
that takes an `Image` handle) when the track is ready:

```rust
use bevy::prelude::*;
use bevy_ffmpeg::{FfmpegPlugin, VideoImage, VideoMessage, VideoPlayer};

fn main() {
    App::new()
        .add_plugins(DefaultPlugins)
        .add_plugins(FfmpegPlugin)
        .add_systems(Startup, setup)
        .add_systems(Update, on_video_ready)
        .run();
}

fn setup(mut commands: Commands) {
    commands.spawn(Camera2d::default());
    commands.spawn(VideoPlayer::new("video.mp4".into()).autoplay().looping());
}

fn on_video_ready(
    mut commands: Commands,
    mut messages: MessageReader<VideoMessage>,
    images: Query<&VideoImage>,
) {
    for message in messages.read() {
        let VideoMessage::Ready { entity, .. } = message else {
            continue;
        };
        let Ok(image) = images.get(*entity) else {
            continue;
        };
        commands.spawn(Sprite::from_image(image.0.clone()));
    }
}
```

Control playback by mutating the component: `play()`, `pause()`,
`toggle_playing()`, `seek_to(seconds)`, and `looping()`. Read `get_position()`
and `get_duration()` for the playhead.

You need FFmpeg development libraries on the system. This crate binds to them
through `ffmpeg-next`.

## Example

```bash
cargo run --example bevy_media_player --features examples -- path/to/video.mp4
```

## License

MIT
