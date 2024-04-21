{
  # mostly copied from jujutsu (https://github.com/martinvonz/jj)
  description = "A little app that connects to the Twitch IRC and archives everything it hears";

  inputs = {
    nixpkgs.url = "nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.flake-utils.follows = "flake-utils";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }: {
    overlays.default = (final: prev: {
      twitch-archiver = self.packages.${final.system}.twitch-archiver;
    });
  } //
  (flake-utils.lib.eachDefaultSystem (system:
    let
      pkgs = import nixpkgs {
        inherit system;
        overlays = [
          rust-overlay.overlays.default
        ];
      };

      filterSrc = src: regexes:
        pkgs.lib.cleanSourceWith {
          inherit src;
          filter = path: type:
            let
              relPath = pkgs.lib.removePrefix (toString src + "/") (toString path);
            in
            pkgs.lib.all (re: builtins.match re relPath == null) regexes;
        };

      rust-version = pkgs.rust-bin.stable."1.77.2".default;

      ourRustPlatform = pkgs.makeRustPlatform {
        rustc = rust-version;
        cargo = rust-version;
      };

      # these are needed in both devShell and buildInputs
      darwinDeps = with pkgs; lib.optionals stdenv.isDarwin [ ];
    in
    {
      packages = {
        twitch-archiver = ourRustPlatform.buildRustPackage {
          pname = "twitch-archiver";
          version = "unstable-${self.shortRev or "dirty"}";
          src = filterSrc ./. [
            ".*\\.nix$"
            "^.jj/"
            "^flake\\.lock$"
            "^target/"
          ];

          cargoLock.lockFile = ./Cargo.lock;
          useNextest = true;

          nativeBuildInputs = [ ];
          buildInputs = [ ] ++ darwinDeps;

          # makes no sense in a nix package
          CARGO_INCREMENTAL = "0";

          preCheck = "export RUST_BACKTRACE=1";

          # for clap apps
          # postInstall = ''
          #   $out/bin/twitch-archiver util mangen > ./twitch-archiver.1
          #   installManPage ./twitch-archiver.1
          #
          #   installShellCompletion --cmd twitch-archiver \
          #     --bash <($out/bin/twitch-archiver util completion --bash) \
          #     --fish <($out/bin/twitch-archiver util completion --fish) \
          #     --zsh  <($out/bin/twitch-archiver util completion --zsh)
          # '';
        };
        default = self.packages.${system}.twitch-archiver;
      };
      apps.default = {
        type = "app";
        program = "${self.packages.${system}.twitch-archiver}/bin/twitch-archiver";
      };
      formatter = pkgs.nixpkgs-fmt;
      devShells.default = pkgs.mkShell {
        buildInputs = with pkgs; [
          # Should be before rust?.
          (rust-bin.selectLatestNightlyWith (toolchain: toolchain.rustfmt))

          # Using the minimal profile with explicit "clippy" extension to avoid
          # two versions of rustfmt
          (rust-version.override {
            extensions = [
              "rust-src" # for rust-analyzer
              "clippy"
            ];
          })

          # Make sure rust-analyzer is present
          rust-analyzer

          cargo-nextest
          # cargo-insta
          # cargo-deny
        ] ++ darwinDeps;

        shellHook = ''
          export RUST_BACKTRACE=1
        '';
      };
    }));
}
