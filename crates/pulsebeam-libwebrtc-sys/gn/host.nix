{ pkgs ? import <nixpkgs> {} }:
pkgs.buildEnv {
  name = "pulsebeam-webrtc-host-tools";
  paths = with pkgs; [ python3 pkg-config perl bash coreutils findutils gawk gnugrep gnused patch glib.dev ];
  pathsToLink = [ "/bin" ];
}
