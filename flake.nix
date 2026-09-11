{
  description = "Lua-configurable Rust agent harness with CLI and Ratatui interfaces";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    naersk = {
      url = "github:nix-community/naersk";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    treefmt-nix = {
      url = "github:numtide/treefmt-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      naersk,
      treefmt-nix,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        naerskLib = pkgs.callPackage naersk { };
        treefmt = treefmt-nix.lib.evalModule pkgs {
          projectRootFile = "flake.nix";
          programs.nixfmt.enable = true;
          programs.rustfmt.enable = true;
        };
      in
      {
        checks.formatting = treefmt.config.build.check self;

        packages.default = naerskLib.buildPackage {
          pname = "ri-agent";
          version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version;
          src = pkgs.lib.cleanSource ./.;
          cargoBuildOptions =
            options:
            options
            ++ [
              "-p"
              "ri-agent-cli"
            ];
          nativeBuildInputs = [
            pkgs.pkg-config
            pkgs.makeWrapper
          ];
          buildInputs = [ pkgs.openssl ];
          postInstall = ''
            wrapProgram "$out/bin/ri" \
              --prefix PATH : ${
                pkgs.lib.makeBinPath [
                  pkgs.bash
                  pkgs.ripgrep
                  pkgs.fd
                ]
              }
          '';
        };

        apps.default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/ri";
          meta.description = "OpenRouter agent harness with a Ratatui interface";
        };

        devShells.default = pkgs.mkShell {
          inputsFrom = [ self.packages.${system}.default ];
          packages = with pkgs; [
            cargo
            rustc
            rustfmt
            clippy
            rust-analyzer
            ripgrep
            fd
          ];
          RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
        };

        formatter = treefmt.config.build.wrapper;
      }
    );
}
