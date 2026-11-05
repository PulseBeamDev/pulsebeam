{ pkgs ? import <nixpkgs> {} }:
let
  # libgcc_s.so's linker script needs the compiler's private static archives too.
  gccRuntime = pkgs.buildEnv {
    name = "pulsebeam-gcc-runtime";
    paths = [ pkgs.stdenv.cc.cc pkgs.stdenv.cc.cc.lib ];
    pathsToLink = [ "/lib" ];
  };
in pkgs.buildEnv {
  name = "pulsebeam-linux-sdk";
  # Select concrete outputs, not glibc meta.outputsToInstall (which omits crt objects).
  paths = map toString [
    pkgs.glibc pkgs.glibc.dev pkgs.linuxHeaders gccRuntime
    pkgs.zlib pkgs.zlib.dev
  ];
  pathsToLink = [ "/include" "/lib" ];
  postBuild = ''
    # Remote inputs dereference symlinks, so linker scripts must resolve inside
    # the staged sysroot rather than relative to their original Nix store paths.
    for name in libc.so libm.so; do
      source=$(readlink -f "$out/lib/$name")
      rm "$out/lib/$name"
      ${pkgs.gnused}/bin/sed -E 's|/nix/store/[^/]+/lib/|=/lib/|g' "$source" > "$out/lib/$name"
    done
    # LLVM's Linux sysroot search follows the conventional usr/include layout.
    mkdir -p "$out/usr" "$out/bin"
    ln -s ../include "$out/usr/include"
    ln -s ../lib "$out/usr/lib"
    ln -s ${pkgs.mold-unwrapped}/bin/mold "$out/bin/mold"
    echo '${pkgs.mold-unwrapped}/bin/mold' > "$out/mold-path"
  '';
}
