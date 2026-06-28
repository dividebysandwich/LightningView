{
  description = "LightningView - a fast cross-platform image viewer";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
      in {
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "lightningview";
          version = "2.4.6";

          src = ./.;

          # Cargo.lock is committed; we let nixpkgs lock the crate sources.
          # Git dependencies need an explicit hash on first build — replace each
          # `lib.fakeHash` with the value Nix prints when it fails.
          cargoLock = {
            lockFile = ./Cargo.lock;
            outputHashes = {
              "imagepipe-0.5.0" = pkgs.lib.fakeHash;
              "rawler-0.7.1"    = pkgs.lib.fakeHash;
              "rawler-0.7.2"    = pkgs.lib.fakeHash;
            };
          };

          nativeBuildInputs = with pkgs; [
            pkg-config
            # cmake builds the vendored SDL3 (sdl3 "build-from-source") plus the
            # image/jxl-oxide native helpers and rawler.
            cmake
            # glslc compiles the SDL_GPU shaders to SPIR-V in build.rs.
            shaderc
          ];

          buildInputs = with pkgs; [
            libxkbcommon
            openssl
            wayland
            xorg.libX11
            xorg.libxcb
            xorg.libXcursor
            xorg.libXi
            xorg.libXrandr
            # SDL3's Vulkan GPU backend loads the loader at runtime.
            vulkan-loader
          ];

          # SDL3 dlopens its Wayland/X11/Vulkan backends at runtime.
          postFixup = ''
            patchelf \
              --add-rpath "${pkgs.lib.makeLibraryPath (with pkgs; [
                libxkbcommon
                wayland
                xorg.libX11
                xorg.libXcursor
                xorg.libXi
                xorg.libXrandr
                vulkan-loader
              ])}" \
              $out/bin/lightningview
          '';

          meta = with pkgs.lib; {
            description = "A fast cross-platform image viewer written in Rust";
            homepage = "https://github.com/dividebysandwich/LightningView";
            license = licenses.gpl2Only;
            mainProgram = "lightningview";
            platforms = platforms.linux;
          };
        };

        apps.default = flake-utils.lib.mkApp {
          drv = self.packages.${system}.default;
        };
      });
}
