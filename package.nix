{
  lib,
  rustPlatform,
  pkg-config,
  libglvnd,
  libx11,
  libxcb,
  libxcursor,
  libxi,
  libxkbcommon,
  libxrandr,
  vulkan-loader,
  wayland,
}:
let
  manifest = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package;
  graphics = [
    libglvnd
    libx11
    libxcb
    libxcursor
    libxi
    libxkbcommon
    libxrandr
    vulkan-loader
    wayland
  ];
in
rustPlatform.buildRustPackage {
  pname = manifest.name;
  inherit (manifest) version;
  src = lib.fileset.toSource {
    root = ./.;
    fileset = lib.fileset.unions [
      ./Cargo.lock
      ./Cargo.toml
      ./LICENSE
      ./README.md
      ./src
      ./tests
    ];
  };
  cargoLock.lockFile = ./Cargo.lock;

  nativeBuildInputs = [ pkg-config ];
  buildInputs = graphics;
  # The window and GPU backends open these libraries at runtime.
  postFixup = ''
    patchelf --add-rpath "${lib.makeLibraryPath graphics}" "$out/bin/gcv"
  '';

  meta = {
    inherit (manifest) description;
    homepage = "https://github.com/apkd/gcv";
    license = lib.licenses.gpl3Only;
    mainProgram = "gcv";
    platforms = lib.platforms.linux;
  };
}
