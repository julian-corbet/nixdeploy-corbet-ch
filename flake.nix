{
  description = "The mechanism for getting a PREBUILT Nix closure onto a machine that did not build it, and knowing afterwards whether it arrived: a publisher that signs closures into a cache and names them in a manifest, a receiver that sizes the change against its OWN store from narinfo metadata and refuses what would not survive activation, per-backend activation adapters and per-provider reimage adapters, and typed outcomes in which 'did nothing' and 'succeeded' are different values. Not a builder (it never evaluates Nix), not a cloud provisioner, not a CI system, not a monitoring stack, not any one operator's deployment policy -- see README.md.";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    # Deliberately NO sibling inputs. nixdeploy reads host FACTS (backend, provider,
    # capability class) defensively by NAME from whatever namespace an operator uses to
    # declare them -- the "read a sibling by name, never as a flake input" convention this
    # family uses between PEER repos. Taking the fact-provider as an input would make this
    # module unloadable for anyone who spells their facts differently, and would invert the
    # dependency: facts are lower than delivery, not beside it.
  };

  outputs = { self, nixpkgs }:
    let
      lib = nixpkgs.lib;
      supportedSystems = [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" "x86_64-darwin" ];
      forAllSystems = lib.genAttrs supportedSystems;
      pkgsFor = system: import nixpkgs { inherit system; };
    in
    {
      # THREE backends, one module. Which one composed it is stated by the caller (see the
      # module's own `backend` option) rather than detected -- a module cannot probe for a
      # backend-specific primitive without becoming unloadable under the other two.
      nixosModules.nixdeploy = ./modules/default.nix;
      nixosModules.default = self.nixosModules.nixdeploy;
      systemManagerModules.nixdeploy = ./modules/default.nix;
      darwinModules.nixdeploy = ./modules/default.nix;

      # The delta arithmetic, exposed standalone for anyone who wants to size a closure
      # change without adopting the module system.
      lib.narinfoDelta = import ./lib/delta.nix { inherit lib; };

      # The manifest schema is the ONE contract between publisher and receiver. Exposed so a
      # third party can produce a manifest without using this repo's publisher at all.
      lib.manifestSchema = import ./lib/manifest.nix { inherit lib; };

      packages = forAllSystems (system:
        let pkgs = pkgsFor system; in
        rec {
          nixdeploy = pkgs.callPackage ./package.nix { };
          default = nixdeploy;
        });

      checks = forAllSystems (system:
        import ./checks {
          pkgs = pkgsFor system;
          inherit lib nixpkgs system;
          nixdeployModule = self.nixosModules.nixdeploy;
        });

      formatter = forAllSystems (system: (pkgsFor system).nixpkgs-fmt);
    };
}
