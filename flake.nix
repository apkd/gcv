{
  description = "3D G-code toolpath viewer with interactive GUI and CLI PNG export";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { nixpkgs, ... }:
    {
      packages = nixpkgs.lib.genAttrs [ "x86_64-linux" "aarch64-linux" ] (system: rec {
        gcv = nixpkgs.legacyPackages.${system}.callPackage ./package.nix { };
        default = gcv;
      });
    };
}
