{
  description = "Flake to manage bevy-ffmpeg dependencies";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = {
    self,
    nixpkgs,
    flake-utils,
    rust-overlay,
  }:
    flake-utils.lib.eachDefaultSystem
    (
      system: let
        overlays = [(import rust-overlay)];
        pkgs = import nixpkgs {
          inherit system overlays;
        };
        libs = with pkgs; [
          libGLU
          libGL
          openssl

          # needed for creating a window :D
          wayland
          wayland-scanner
          wayland-protocols
          egl-wayland

          # Bevy related
          # Audio (Linux only)
          alsa-lib
          # Cross Platform 3D Graphics API
          vulkan-loader
          # For debugging around vulkan
          vulkan-tools
          # Other dependencies
          libudev-zero
          xorg.libX11
          xorg.libXcursor
          xorg.libXi
          xorg.libXrandr
          libxkbcommon

          # Ffmpeg related
          (ffmpeg.override {
            ffmpegVariant = "headless";
          })
          libclang

          (rust-bin.beta.latest.default.override {
            extensions = ["rust-src" "rust-analyzer"];
          })
        ];
        nativeBuildInputs = with pkgs; [
          clang-tools
          pkg-config
          cargo
        ];
        build_project = optimize:
          pkgs.rustPlatform.buildRustPackage {
            pname = "bevy-ffmpeg";
            version = "1.0.0";

            src = ./.;

            cargoLock = {
              lockFile = ./Cargo.lock;
            };

            buildInputs = libs;
            nativeBuildInputs = nativeBuildInputs;
            LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath libs;
            CMAKE_PREFIX_PATH = pkgs.lib.makeLibraryPath libs;

            # These needed to be added for ffmpeg to compile
            LIBCLANG_PATH = "${pkgs.libclang.lib}/lib";
            BINDGEN_EXTRA_CLANG_ARGS = "-isystem ${pkgs.libclang.lib}/lib/clang/${pkgs.libclang.version}/include -isystem ${pkgs.glibc.dev}/include";

            buildType =
              if optimize
              then "release"
              else "debug";

            meta = with pkgs.lib; {
              description = "Apply quick pixel effects to images/videos.";
              license = licenses.mit;
              maintainers = with maintainers; [yunusey];
              platforms = platforms.linux;
            };
          };
      in {
        packages = rec {
          default = release;
          debug = build_project false;
          release = build_project true;
        };
        devShells.default = pkgs.mkShell {
          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath libs;
          CMAKE_PREFIX_PATH = pkgs.lib.makeLibraryPath libs;
          LIBCLANG_PATH = "${pkgs.libclang.lib}/lib";
          BINDGEN_EXTRA_CLANG_ARGS = "-isystem ${pkgs.libclang.lib}/lib/clang/${pkgs.libclang.version}/include -isystem ${pkgs.glibc.dev}/include";
          buildInputs = libs;
          nativeBuildInputs = nativeBuildInputs ++ libs;
        };
      }
    );
}
