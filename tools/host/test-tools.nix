{ pkgs ? import <nixpkgs> {} }:
pkgs.runCommand "pulsebeam-contract-test-tools" {} ''
  mkdir -p "$out/bin"
  ln -s ${pkgs.gitMinimal}/bin/git "$out/bin/git"
  ln -s ${pkgs.bash}/bin/bash "$out/bin/bash"
  ln -s ${pkgs.util-linux}/bin/flock "$out/bin/flock"
  for tool in readlink stat sha256sum uname mkdir dirname; do
    ln -s ${pkgs.coreutils}/bin/$tool "$out/bin/$tool"
  done
''
