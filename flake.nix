{
  inputs = {
    flake-utils.url = "github:numtide/flake-utils";
    nixpkgs.url = "nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs, flake-utils, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
        };

        desktopItem = pkgs.makeDesktopItem {
          name = "victoria";
          desktopName = "Victoria";
          exec = "victoria %u";
          terminal = true;
          categories = [ "Network" "FileTransfer" ];
          mimeTypes = [ "x-scheme-handler/magnet" ];
        };

        victoria = pkgs.rustPlatform.buildRustPackage {
          pname = "victoria";
          version = "0.1.0";

          src = ./.;
          RUSTFLAGS = "--cfg tokio_unstable";
          cargoLock = {
            lockFile = ./Cargo.lock;
          };
          nativeBuildInputs = [
            pkgs.pkg-config
          ];
          buildInputs = [
            pkgs.openssl_3
          ];
          postInstall = ''
            install -Dm644 ${desktopItem}/share/applications/victoria.desktop \
            $out/share/applications/victoria.desktop
          '';
        };
      in {
        packages.default = victoria;

        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            gdb
            rust-analyzer
            cargo
            rustc
            pkg-config
            tokio-console
          ];

          buildInputs = with pkgs; [
            openssl_3
          ];
        };
      });
}
