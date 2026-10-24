{ pkgs ? import <nixpkgs> {} }:
pkgs.buildEnv {
  name = "pulsebeam-webrtc-test-tools";
  paths = with pkgs; [ openssl.bin coturn nettools ];
  pathsToLink = [ "/bin" ];
}
