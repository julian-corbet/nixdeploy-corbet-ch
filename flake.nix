{
  description = "The mechanism for getting a PREBUILT Nix closure onto a machine that did not build it, and knowing afterwards whether it arrived: a publisher that signs a manifest naming cache-available closures, a receiver that sizes that change against its OWN store from narinfo metadata and refuses what would not survive activation, per-backend activation adapters and per-provider reimage adapters, and typed outcomes in which 'did nothing' and 'succeeded' are different values. Not a builder (it never evaluates Nix), not a cache uploader, not a cloud provisioner, not a CI system, not a monitoring stack, not any one operator's deployment policy -- see README.md.";

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
      #
      # Every namespace below exports TWO things, and the naming says which registry each
      # one belongs to: `nixdeploy` is the option surface (the same file in all three), and
      # `backendAdapter` is that namespace's entry in the BACKEND ADAPTER registry -- the
      # registry keyed by `nixdeploy.backend`, answering `activate`, `currentPath`,
      # `rollback`, `schedule` and `nixSettings`. A machine composes exactly one of each.
      # (The second registry, PROVISIONING, is keyed by `nixdeploy.provider` and is not a
      # module at all: it is an operator-populated attrset, so it is exported below as
      # `lib.provisioning`, a factory, rather than as a fixed set of adapters this repo
      # could not know the names of.)
      nixosModules = {
        nixdeploy = ./modules/default.nix;
        default = ./modules/default.nix;
        backendAdapter = ./modules/adapters/nixos.nix;
      };
      systemManagerModules = {
        nixdeploy = ./modules/default.nix;
        default = ./modules/default.nix;
        backendAdapter = ./modules/adapters/system-manager.nix;
      };
      darwinModules = {
        nixdeploy = ./modules/default.nix;
        default = ./modules/default.nix;
        backendAdapter = ./modules/adapters/nix-darwin.nix;
      };

      # The same three adapter files again, keyed by the exact string a machine sets
      # `nixdeploy.backend` to. Not a duplicate export for its own sake: a publisher (or
      # anything else) building configurations for a MIXED fleet has that string in hand as
      # data, and indexing an attrset by it is the difference between `backendAdapters.
      # ${host.backend}` and a three-way conditional written once per consumer. The
      # per-namespace exports above are for a human writing one machine's flake; this one is
      # for code writing many.
      backendAdapters = {
        nixos = ./modules/adapters/nixos.nix;
        system-manager = ./modules/adapters/system-manager.nix;
        nix-darwin = ./modules/adapters/nix-darwin.nix;
      };

      # The delta arithmetic, exposed standalone for anyone who wants to size a closure
      # change without adopting the module system.
      lib.narinfoDelta = import ./lib/delta.nix { inherit lib; };

      # The manifest schema is the ONE contract between publisher and receiver. Exposed so a
      # third party can produce a manifest without using this repo's publisher at all.
      lib.manifestSchema = import ./lib/manifest.nix { inherit lib; };

      # The PROVISIONING registry's factory: `mkAdapter` turns one shell command into a value
      # shaped like `provisioningAdapter`, ready to assign into
      # `nixdeploy.publisher.provisioning.<providerName>` -- an attrset nothing in this repo
      # reads yet (see docs/reimage.md's "What is implemented"). A function of `pkgs` rather
      # than a `forAllSystems` package set, because it builds scripts and therefore needs the
      # pkgs of whichever machine will actually run `reimage`, which this repo has no way to
      # guess.
      lib.provisioning = pkgs: import ./modules/adapters/provisioning-generic.nix {
        inherit pkgs;
        inherit (pkgs) lib;
      };

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
          inherit (self) backendAdapters;
        });

      formatter = forAllSystems (system: (pkgsFor system).nixpkgs-fmt);
    };
}
