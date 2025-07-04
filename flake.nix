{
  # mostly copied from jujutsu (https://github.com/martinvonz/jj)
  description = "A little app that connects to the Twitch IRC and archives everything it hears";

  inputs = {
    nixpkgs.url = "nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }: {
    overlays.default = (final: prev: {
      twitch-archiver = self.packages.${final.system}.twitch-archiver;
    });
    nixosModules.default = { config, lib, pkgs, ... }:
      with lib;
      let
        cfg = config.services.twitch-archiver;
        channels = concatStringsSep " " cfg.channels;
      in
      {
        options.services.twitch-archiver = {
          enable = mkEnableOption {
            description = "Enable the twitch-archiver, a small service to archive Twitch chat logs";
          };
          channels = mkOption {
            description = "A list of channels to connect to and archive";
            type = types.listOf types.str;
          };
          rotationLimit = mkOption {
            description = "";
            type = types.number;
            default = 16777216;
          };
          elastic = mkOption {
            description = "ElasticSearch configuration";
            type = types.nullOr (types.submodule {
              options = {
                url = mkOption {
                  description = "The ElasticSearch url";
                  type = types.str;
                  default = "http://localhost:9200";
                };
                index = mkOption {
                  description = "The ElasticSearch index or indices to send messages to";
                  type = types.either types.str (types.listOf types.str);
                  default = "twitch-chat-*";
                };
                apiKeyFile = mkOption {
                  description = "Path to the file containing the ElasticSearch API key";
                  type = types.path;
                };
              };
            });
          };
        };

        config = {
          nixpkgs.overlays = [ self.overlays.default ];
          systemd.services.twitch-archiver = mkIf cfg.enable {
            wantedBy = [ "multi-user.target" ];
            after = [ "network.target" ];
            serviceConfig = {
              Restart = "on-failure";
              RestartSec = "1s";
              ExecStart =
                let
                  subcmd =
                    if cfg.elastic != null then
                      let
                        indices =
                          if builtins.isList cfg.elastic.index then
                            concatStringsSep " " cfg.elastic.index
                          else
                            cfg.elastic.index;
                      in
                        ''elastic ${cfg.elastic.url} "${cfg.elastic.apiKeyFile}" ${indices}''
                    else "irc /var/lib/twitch-archiver/twitch.log";
                in
                "${pkgs.twitch-archiver}/bin/twitch-archiver archive -c ${channels} ${subcmd}";
              DynamicUser = "yes";
              StateDirectory = "twitch-archiver";
              StateDirectoryMode = "0755";
            };
          };
        };
      };
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

      rust-version = pkgs.rust-bin.stable.latest.default;

      ourRustPlatform = pkgs.makeRustPlatform {
        rustc = rust-version;
        cargo = rust-version;
      };
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

          nativeBuildInputs = [ pkgs.pkg-config ];
          buildInputs = [ pkgs.openssl ];

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
        inputsFrom = [ self.packages.${system}.twitch-archiver ];

        nativeBuildInputs = with pkgs; [
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
        ];

        LD_LIBRARY_PATH = with pkgs; lib.makeLibraryPath [ openssl ];

        RUSTDOCFLAGS = "-D warnings";
        RUST_BACKTRACE = "full";
      };
    }));
}
