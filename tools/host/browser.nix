{ name, version, url, sha256, archive }:
let
  pkgs = import <nixpkgs> {};
  libraries = with pkgs; [
    alsa-lib at-spi2-core cairo cups dbus expat libdrm libgbm glib gtk3
    nspr nss pango systemdLibs
    libx11 libxcb libxcomposite libxdamage libxext libxfixes
    libxkbcommon libxrandr libxrender libxt libxtst
    stdenv.cc.cc.lib
  ];
in pkgs.stdenv.mkDerivation ({
  pname = "pulsebeam-${name}";
  inherit version;
  src = pkgs.fetchurl { inherit url sha256; };
  nativeBuildInputs = [ pkgs.autoPatchelfHook pkgs.unzip ]
    ++ pkgs.lib.optional (name == "firefox") pkgs.patchelfUnstable;
  buildInputs = libraries;
  # Browsers dlopen libraries that are not visible in ELF's DT_NEEDED entries.
  runtimeDependencies = map pkgs.lib.getLib libraries;
  dontUnpack = true;
  dontConfigure = true;
  dontBuild = true;
  installPhase = ''
    mkdir -p "$out"
    ${if archive == "zip" then ''unzip "$src" -d "$out"'' else ''tar -xf "$src" -C "$out"''}
  '';
} // pkgs.lib.optionalAttrs (name == "firefox") {
  # Mozilla relrhack reads relocation data at fixed offsets in the original ELF.
  patchelfFlags = [ "--no-clobber-old-sections" ];
})
