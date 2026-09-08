{
  description = "Lua-configurable Rust agent harness with CLI and Ratatui interfaces";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    naersk = {
      url = "github:nix-community/naersk";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      naersk,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        naerskLib = pkgs.callPackage naersk { };
      in
      {
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
            wrapProgram "$out/bin/codecrafters-claude-code" \
              --prefix PATH : ${pkgs.lib.makeBinPath [ pkgs.bash ]}
          '';
        };

        apps.default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/codecrafters-claude-code";
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
          ];
          MODEL = "minimax/minimax-m3:free";
          RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
        };

        formatter = pkgs.nixfmt;
      }
    );
}
