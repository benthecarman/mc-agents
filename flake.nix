{
  description = "AI-controlled Minecraft bots (azalea + goose GDK)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    { nixpkgs, rust-overlay, ... }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs {
        inherit system;
        overlays = [ (import rust-overlay) ];
      };
      # Azalea needs nightly. rust-toolchain.toml holds the pin, so rustup
      # and this shell agree on the toolchain.
      rust = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
    in
    {
      devShells.${system}.default = pkgs.mkShell {
        packages = with pkgs; [
          rust
          jdk21_headless
          pkg-config
          openssl
          git
          just
          jq
          curl
          tmux
          netcat
        ];
      };
    };
}
