{ pkgs ? import <nixpkgs> {} }:
pkgs.buildEnv {
  name = "pulsebeam-worker-runtime";
  paths = with pkgs; [ coreutils curl file gnutar xz ];
  pathsToLink = [ "/bin" ];
  postBuild = ''
    mkdir -p "$out/etc/ssl/certs"
    ln -s ${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt "$out/etc/ssl/certs/ca-certificates.crt"
    printf '127.0.0.1 localhost\n::1 localhost\n' > "$out/etc/hosts"
    printf '%s\n' "$out" > "$out/runtime-root"
  '';
}
