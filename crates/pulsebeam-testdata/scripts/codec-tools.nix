{ pkgs ? import <nixpkgs> {} }:
pkgs.buildEnv {
  name = "pulsebeam-codec-authoring-tools";
  paths = with pkgs; [ (python3.withPackages (p: [ p.av ])) zstd ];
  pathsToLink = [ "/bin" ];
}
