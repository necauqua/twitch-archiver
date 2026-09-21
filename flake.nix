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
    overlays.default = final: _: {
      twitch-archiver = self.packages.${final.stdenv.hostPlatform.system}.twitch-archiver;
    };
    nixosModules.default = { config, lib, pkgs, ... }:
      with lib;
      let
        cfg = config.services.twitch-archiver;
        channels = concatStringsSep "," cfg.channels;

        subcmd =
          if cfg.elastic != null then
            let
              indices =
                if builtins.isList cfg.elastic.index then
                  concatStringsSep " " cfg.elastic.index
                else
                  cfg.elastic.index;
            in
            ''elastic ${cfg.elastic.url} "%d/apikey" ${indices}''
          else "irc /var/lib/twitch-archiver/twitch.log";

        load-credential =
          if cfg.elastic != null then {
            LoadCredential = "apikey:${cfg.elastic.apiKeyFile}";
          } else { };

        serviceConfig = {
          # the archiver reports readiness once it joined every channel,
          # which lets a new instance overlap with the one it replaces
          Type = "notify";
          NotifyAccess = "main";
          Restart = "on-failure";
          RestartSec = "1s";
          ExecStart = "${pkgs.twitch-archiver}/bin/twitch-archiver archive -c ${channels} --connections ${toString cfg.connections} ${subcmd}";
          DynamicUser = "yes";
        } // load-credential;

        # A tag that gives every generation of the archiver its own unit name,
        # so that two of them can run at the same time and the switcher below
        # can tell the instance it wants from the ones it must stop.
        #
        # It hashes the rendered unit, which is what a switch compares as well,
        # so it also covers what the unit gets from outside `serviceConfig`:
        # the environment of the service and any `restartTriggers`, such as the
        # content of the file that holds the credential.
        tag = builtins.substring 0 8 (builtins.hashString "sha256"
          config.systemd.units."twitch-archiver@.service".text);

        systemctl = "${config.systemd.package}/bin/systemctl";
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
          connections = mkOption {
            description = ''
              How many independent connections to Twitch to keep open.
              Every connection archives every channel and the messages are
              deduplicated, so that a RECONNECT request or a connection loss
              does not lose messages.
            '';
            type = types.ints.positive;
            default = 2;
          };
          overlap = mkOption {
            description = ''
              Overlap the instances of the archiver across a switch.

              A plain restart drops every message that arrives while the new
              process connects to Twitch and joins the channels. Instead one
              instance of a template unit runs per generation: a switch starts
              the new instance, waits for it to join all the channels, and only
              then stops the old one.

              Both instances hear the same messages for a moment, so this needs
              an output that deduplicates them, which the ElasticSearch one
              does on the message id.
            '';
            type = types.bool;
            default = cfg.elastic != null;
            defaultText = literalExpression "config.services.twitch-archiver.elastic != null";
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

          assertions = [{
            assertion = cfg.enable -> cfg.overlap -> cfg.elastic != null;
            message = ''
              services.twitch-archiver.overlap needs an output that
              deduplicates messages, so it needs services.twitch-archiver.elastic.
            '';
          }];

          systemd.services = mkIf cfg.enable (
            if cfg.overlap then {
              "twitch-archiver@" = {
                description = "Twitch chat archiver, generation %i";
                after = [ "network.target" ];

                # a switch must never touch a live instance, the switcher
                # below does the cutover
                restartIfChanged = false;

                inherit serviceConfig;
              };

              # The switcher holds no state and ends inactive, so every switch
              # starts it again through multi-user.target. It reconciles rather
              # than reacts: it makes the instance of this generation the one
              # that runs, which is a no-op when it already does, and it also
              # brings the archiver back if the instance died.
              twitch-archiver-switcher = {
                description = "Cut over to twitch-archiver generation ${tag}";
                wantedBy = [ "multi-user.target" ];
                after = [ "network.target" ];

                serviceConfig.Type = "oneshot";

                # `systemctl start` of a Type=notify unit returns once the
                # archiver reported that it joined every channel, so the old
                # instance is stopped only after the new one hears everything
                # the old one does
                script = ''
                  new=twitch-archiver@${tag}.service

                  echo "Starting new instance $new"
                  ${systemctl} start "$new"
                  echo "New instance $new is ready"

                  for u in $(${systemctl} list-units --plain --no-legend --state=active,activating 'twitch-archiver@*.service' | cut -d' ' -f1); do
                    if [ "$u" != "$new" ]; then
                      echo "Stopping previous instance $u"
                      ${systemctl} stop "$u"
                    fi
                  done
                '';
              };
            } else {
              twitch-archiver = {
                wantedBy = [ "multi-user.target" ];
                after = [ "network.target" ];
                serviceConfig = serviceConfig // {
                  StateDirectory = "twitch-archiver";
                  StateDirectoryMode = "0755";
                };
              };
            }
          );
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
          filter = path: _:
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
