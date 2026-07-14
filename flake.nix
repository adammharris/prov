{
  description = "colophon — a self-describing workspace metadata CLI and library";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    # colophon's `fig` and `twig-doc` dependencies are Zig-backed (their build.rs
    # scripts run `zig build`), so the Rust build needs the pinned Zig toolchain
    # (0.16.0) on PATH, matching those crates' CI.
    zig-overlay.url = "github:mitchellh/zig-overlay";
    zig-overlay.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = { self, nixpkgs, flake-utils, zig-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
        zig = zig-overlay.packages.${system}."0.16.0";

        # The workspace version (single source of truth in [workspace.package]).
        # Parse it so the flake reports the same number as `colophon --version`.
        version =
          let m = builtins.match ".*\n[[:blank:]]*version = \"([^\"]+)\".*"
                    (builtins.readFile ./Cargo.toml);
          in if m == null
             then throw "colophon flake: could not find workspace version in Cargo.toml"
             else builtins.head m;
      in {
        packages = rec {
          default = colophon;

          colophon = pkgs.rustPlatform.buildRustPackage {
            pname = "colophon";
            inherit version;
            src = ./.;

            cargoLock.lockFile = ./Cargo.lock;

            # zig for the fig/twig-doc build.rs steps. On Apple targets those
            # build scripts also repack Zig's static archive with `libtool`
            # (ld64 rejects Zig's alignment) — cctools provides that `libtool`,
            # which isn't otherwise on the sandbox PATH.
            nativeBuildInputs = [ zig ]
              ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [ pkgs.cctools ];

            # Those Zig builds want a writable HOME + cache dir, which the
            # read-only Nix store won't provide.
            preBuild = ''
              export HOME="$TMPDIR"
              export ZIG_GLOBAL_CACHE_DIR="$TMPDIR/zig-global-cache"
              export ZIG_LOCAL_CACHE_DIR="$TMPDIR/zig-local-cache"
            '';

            # The fig/twig-doc build scripts repack their Zig archives with
            # `libtool`/`ar`, which leaves an unreadable `__.SYMDEF` in each
            # build-script `out/repack` dir. buildRustPackage's install hook then
            # does a bulk `cp -r` of the release dir and fails on it. This runs
            # before that hook (the postBuild attr precedes postBuildHooks), so
            # make the tree readable first.
            postBuild = ''
              chmod -R u+rwX target
            '';

            # Build/test only the CLI crate; the library is a workspace member.
            cargoBuildFlags = [ "-p" "colophon-cli" ];
            cargoTestFlags = [ "-p" "colophon-cli" ];

            meta = {
              description = "Command-line companion for the colophon self-describing workspace library";
              homepage = "https://github.com/adammharris/colophon";
              license = with pkgs.lib.licenses; [ mit asl20 ];
              mainProgram = "colophon";
              platforms = pkgs.lib.platforms.unix;
            };
          };
        };

        apps.default = {
          type = "app";
          program = "${self.packages.${system}.colophon}/bin/colophon";
        };

        devShells.default = pkgs.mkShell {
          nativeBuildInputs = [ pkgs.cargo pkgs.rustc zig ];
        };
      });
}
