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
            # Needed by image/jxl-oxide native helpers and rawler.
            cmake
          ];

          buildInputs = with pkgs; [
            gtk3
            libxkbcommon
            openssl
            wayland
            xorg.libX11
            xorg.libxcb
            xorg.libXcursor
            xorg.libXi
            xorg.libXrandr
          ];

          # eframe loads Wayland/X11 libs at runtime via dlopen.
          postFixup = ''
            patchelf \
              --add-rpath "${pkgs.lib.makeLibraryPath (with pkgs; [
                libxkbcommon
                wayland
                xorg.libX11
                xorg.libXcursor
                xorg.libXi
                xorg.libXrandr
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
