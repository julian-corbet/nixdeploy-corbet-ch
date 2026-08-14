# Runtime regression for the first system-manager switch on an independent foreign Nix
# store. The signed target begins absent. A fake pinned Nix installation substitutes it only
# when local and remote builders are explicitly disabled; the newly available target then
# registers and activates successfully inside an unprivileged synthetic root.
{ pkgs, lib, nixdeployModule, backendAdapters }:

let
  fixture = pkgs.runCommand "nixdeploy-remote-system-manager-fixture" { } ''
    mkdir -p "$out/bin"

    cat > "$out/bin/register-profile" <<'SH'
    #!${pkgs.runtimeShell}
    set -eu
    target="$(${pkgs.coreutils}/bin/dirname "$(${pkgs.coreutils}/bin/dirname "$0")")"
    profile_dir=/nix/var/nix/profiles/system-manager-profiles
    gcroot_dir=/nix/var/nix/gcroots
    ${pkgs.coreutils}/bin/mkdir -p "$profile_dir" "$gcroot_dir"
    ${pkgs.coreutils}/bin/ln -sfn "$target" "$profile_dir/system-manager"
    ${pkgs.coreutils}/bin/ln -sfn "$target" "$gcroot_dir/system-manager-current"
    : > /tmp/register-ran
    SH

    cat > "$out/bin/activate" <<'SH'
    #!${pkgs.runtimeShell}
    set -eu
    : > /tmp/activate-ran
    SH

    chmod +x "$out/bin/register-profile" "$out/bin/activate"
  '';

  fakeNix = pkgs.runCommand "nixdeploy-pinned-foreign-nix" { } ''
    mkdir -p "$out/bin"

    cat > "$out/bin/nix" <<'SH'
    #!${pkgs.runtimeShell}
    exit 0
    SH

    cat > "$out/bin/nix-store" <<'SH'
    #!${pkgs.runtimeShell}
    set -eu
    test "$#" -eq 8
    test "$1" = --option
    test "$2" = builders
    test -z "$3"
    test "$4" = --option
    test "$5" = max-jobs
    test "$6" = 0
    test "$7" = --realise
    target="$8"
    test ! -e "$target"
    ${pkgs.coreutils}/bin/mkdir -p "$target"
    ${pkgs.coreutils}/bin/cp -R ${fixture}/. "$target/"
    printf '%s\n' "$target"
    SH

    cat > "$out/bin/nix-env" <<'SH'
    #!${pkgs.runtimeShell}
    echo "unexpected nix-env call in forward realization test" >&2
    exit 1
    SH

    chmod +x "$out/bin/nix" "$out/bin/nix-store" "$out/bin/nix-env"
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
      backendAdapters.system-manager
      platformStub
      {
        nixdeploy = {
          backend = "system-manager";
          provider = "foreign-test";
          receiver = {
            enable = true;
            nixBinary = "${fakeNix}/bin/nix";
            manifest = {
              url = "https://cache.example.org/manifest.json";
              publicKey = "cache.example.org-1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
            };
          };
        };
      }
    ];
  }).config;

  activate = evaluated.nixdeploy.receiver.activation.activate;
in
pkgs.runCommand "nixdeploy-system-manager-remote-realization"
  {
    inherit activate;
    nativeBuildInputs = [ pkgs.coreutils pkgs.proot ];
  }
  ''
    root="$TMPDIR/root"
    ${pkgs.coreutils}/bin/mkdir -p \
      "$root/dev" \
      "$root/nix/store" \
      "$root/nix/var/nix/profiles/system-manager-profiles" \
      "$root/nix/var/nix/gcroots" \
      "$root/tmp"

    test ! -e "$root/tmp/unrealized-target"
    ${pkgs.proot}/bin/proot \
      -r "$root" \
      -b /nix/store:/nix/store \
      -b /dev/null:/dev/null \
      -w /tmp \
      ${pkgs.coreutils}/bin/env -i \
        PATH=/path-intentionally-without-nix \
        "$activate" /tmp/unrealized-target

    test -x "$root/tmp/unrealized-target/bin/register-profile"
    test -x "$root/tmp/unrealized-target/bin/activate"
    test -f "$root/tmp/register-ran"
    test -f "$root/tmp/activate-ran"
    touch "$out"
  ''
