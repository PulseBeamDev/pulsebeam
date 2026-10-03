{
  description = "PulseBeam Linux SDK; Bazel owns language tools and application workflows";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

  outputs = { nixpkgs, ... }: {
    packages = nixpkgs.lib.genAttrs [ "x86_64-linux" "aarch64-linux" ] (system: {
      default = import ./sdk.nix { pkgs = import nixpkgs { inherit system; }; };
    });
  };
}
