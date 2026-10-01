{
  description = "PulseBeam Linux OS inputs; build tools and workflows remain Bazel-owned";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

  outputs = { nixpkgs, ... }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      identity = builtins.hashString "sha256"
        (builtins.readFile ./flake.nix + builtins.readFile ./flake.lock);
    in {
      packages = nixpkgs.lib.genAttrs systems (system:
        let
          pkgs = import nixpkgs { inherit system; };
          # libgcc_s.so's linker script also needs GCC's private static archives.
          gccRuntime = pkgs.buildEnv {
            name = "pulsebeam-gcc-runtime";
            paths = [ pkgs.stdenv.cc.cc pkgs.stdenv.cc.cc.lib ];
            pathsToLink = [ "/lib" ];
          };
        in {
          default = pkgs.buildFHSEnv {
            name = "pulsebeam-host";
            targetPkgs = p: with p; [
              (writeTextDir "etc/pulsebeam-host" "${identity}:${if system == "x86_64-linux" then "amd64" else "arm64"}")
              bash coreutils curl gitMinimal gnutar gzip xz unzip patch
              (docker-client.override { buildxSupport = false; composeSupport = false; })
              findutils gnugrep gnused cacert
              glibc glibc.dev linuxHeaders gccRuntime zlib zlib.dev
              ncurses libxml2 zstd
              # Compiler wrappers would inject Nix-specific search paths.
              mold-unwrapped
              alsa-lib at-spi2-core cairo cups dbus expat libdrm libgbm glib gtk3
              nspr nss pango systemdLibs
              libx11 libxcb libxcomposite libxdamage libxext libxfixes
              libxkbcommon libxrandr libxrender libxt libxtst
            ];
            multiPkgs = _: [];
            profile = ''
              export SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt
            '';
            runScript = "${pkgs.bash}/bin/bash";
          };
        });
    };
}
