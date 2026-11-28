{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { nixpkgs, rust-overlay, ... }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f (import nixpkgs {
        inherit system;
        overlays = [ (import rust-overlay) ];
      }));
    in
    {
      devShells = forAll (pkgs:
        let
          # stein-shell links wayland-client and xkbcommon, stein-gpu loads libvulkan.
          runtimeLibs = with pkgs; [ wayland libxkbcommon vulkan-loader ];
        in
        {
          default = pkgs.mkShell {
            packages = [
              (pkgs.rust-bin.nightly.latest.minimal.override {
                extensions = [ "clippy" "rustfmt" ];
              })
              pkgs.cargo-fuzz
              pkgs.cargo-nextest
              pkgs.clang
              pkgs.wild
              pkgs.pkg-config
              # stein-gpu shader sources, its tests compare them to the checked in SPIR-V.
              pkgs.glslang
              pkgs.vulkan-validation-layers
            ];
            buildInputs = runtimeLibs;
            shellHook = ''
              # The GPU driver comes from the host through /run/opengl-driver/lib.
              export LD_LIBRARY_PATH="${pkgs.lib.makeLibraryPath runtimeLibs}:/run/opengl-driver/lib''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
              export VK_LAYER_PATH="${pkgs.vulkan-validation-layers}/share/vulkan/explicit_layer.d"
              # stein-text tests pin a font.
              export STEIN_TEST_FONT="${pkgs.dejavu_fonts}/share/fonts/truetype/DejaVuSansMono.ttf"
            '';
          };
        });
    };
}
