# Runtime regression for a foreign Linux host whose Nix installation is deliberately absent
# from the user service manager's PATH. Home Manager's generated activation script calls
# nix-build by bare name, so the adapter must expose the bin directory containing the exact
# receiver.nixBinary before it enters that script.
{ pkgs, lib, nixdeployModule, backendAdapters }:

let
  fakeNix = pkgs.runCommand "nixdeploy-foreign-nix" { } ''
    mkdir -p "$out/bin"

    cat > "$out/bin/nix" <<'SH'
    #!${pkgs.runtimeShell}
    exit 0
    SH

    cat > "$out/bin/nix-build" <<'SH'
    #!${pkgs.runtimeShell}
    set -eu
    test "$#" -eq 1
    test "$1" = --foreign-path-probe
    SH

    chmod +x "$out/bin/nix" "$out/bin/nix-build"
  '';

  fakeGeneration = pkgs.runCommand "nixdeploy-foreign-home-manager-generation" { } ''
    mkdir -p "$out"
    cat > "$out/activate" <<'SH'
    #!${pkgs.runtimeShell}
    set -eu
    test "$#" -eq 2
    test "$1" = --driver-version
    test "$2" = 1
    nix-build --foreign-path-probe
    : > "$NIXDEPLOY_PATH_PROBE"
    # Leave current-home untouched so the adapter takes its ordinary verification-failure
    # path after the PATH probe. This test is about entering Home Manager with the right Nix
    # tools, not duplicating Home Manager's own GC-root behavior.
    exit 17
    SH
    chmod +x "$out/activate"
  '';

  platformStub = { ... }: {
    options = {
      systemd = lib.mkOption { type = lib.types.attrs; default = { }; };
      launchd = lib.mkOption { type = lib.types.attrs; default = { }; };
      nix = lib.mkOption { type = lib.types.attrs; default = { }; };
      users = lib.mkOption { type = lib.types.attrs; default = { }; };
      home = lib.mkOption { type = lib.types.attrs; default = { }; };
      xdg = lib.mkOption { type = lib.types.attrs; default = { }; };
      assertions = lib.mkOption { type = lib.types.listOf lib.types.unspecified; default = [ ]; };
    };
  };

  evaluated = (lib.evalModules {
    specialArgs = { inherit pkgs; };
    modules = [
      nixdeployModule
      backendAdapters.home-manager
      platformStub
      {
        nixdeploy = {
          backend = "home-manager";
          provider = "foreign-test";
          receiver = {
            enable = true;
            nixBinary = "${fakeNix}/bin/nix";
            plane.identity = "alice";
            manifest = {
              url = "https://cache.example.org/manifest.json";
              publicKey = "cache.example.org-1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
            };
          };
        };
        nix.package = null;
        home = {
          username = "alice";
          homeDirectory = "/build/nixdeploy-home-manager-path/alice";
          activationGenerateGcRoot = true;
        };
        xdg = {
          stateHome = "/build/nixdeploy-home-manager-path/alice/.local/state";
          cacheHome = "/build/nixdeploy-home-manager-path/alice/.cache";
        };
      }
    ];
  }).config;

  outerActivate = evaluated.nixdeploy.receiver.activation.activate;
in
pkgs.runCommand "nixdeploy-home-manager-foreign-nix-path"
  {
    inherit outerActivate fakeGeneration;
    nativeBuildInputs = [ pkgs.coreutils pkgs.gnugrep ];
  }
  ''
    mkdir -p "$out"
    inner="$(${pkgs.gnugrep}/bin/grep -o '/nix/store/[^ ]*-nixdeploy-home-manager-apply-and-verify' "$outerActivate" | head -n 1)"
    test -n "$inner"
    test -x "$inner"

    set +e
    ${pkgs.coreutils}/bin/env -i \
      PATH=/path-intentionally-without-nix \
      NIXDEPLOY_PATH_PROBE="$out/path-probe" \
      "$inner" "$fakeGeneration"
    status="$?"
    set -e

    test "$status" -ne 0
    test -f "$out/path-probe"
  ''
