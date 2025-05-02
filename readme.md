## twitch-archiver

This is a simple Rust program that I wrote in a couple of afternoons that
connects to the Twitch chat IRC and archives all the messages forever.

The Nix flake also contains a simple NixOS module which defines a systemd
service that starts it and keeps it running.

Correction: I have since then added an elasticsearch exporter to it. It still can do what it did, but can also do that, as well as converting
the IRC logs to ES bulk ndjson. The JSON format also contains some metadata specific to my command format (the one I invented for Twitch Plays Noita).

### License
Like most of my work, this is licensed under MIT, meaning you can do basically
whatever you want with this code as long as you keep the original LICENSE file,
which has my name on top of it
