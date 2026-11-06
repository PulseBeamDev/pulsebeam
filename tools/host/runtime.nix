{ pkgs ? import <nixpkgs> {} }:
pkgs.buildEnv {
  name = "pulsebeam-worker-runtime";
  paths = with pkgs; [ coreutils curl file gnutar iproute2 xz ];
  pathsToLink = [ "/bin" ];
  postBuild = ''
    mkdir -p "$out/etc/ssl/certs"
    ln -s ${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt "$out/etc/ssl/certs/ca-certificates.crt"
    printf '127.0.0.1 localhost\n::1 localhost\n' > "$out/etc/hosts"
    # Isolation changes must also invalidate remote action cache identities.
    printf '%s\n' '${builtins.hashFile "sha256" ../nativelink/server.py}' > "$out/sandbox-policy.sha256"
    printf '%s\n' "$out" > "$out/runtime-root"
  '';
}
