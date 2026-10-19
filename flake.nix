{
  description = "queuewatch — a read-only status page for your *arr stack";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
  # The RustSec advisory database, pinned like any other input. The `audit`
  # check reads it offline; `nix flake update advisory-db` brings news in.
  inputs.advisory-db = {
    url = "github:rustsec/advisory-db";
    flake = false;
  };

  outputs =
    { self, nixpkgs, advisory-db }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAll = f: nixpkgs.lib.genAttrs systems (s: f nixpkgs.legacyPackages.${s});
    in
    {
      packages = forAll (pkgs: {
        default = pkgs.rustPlatform.buildRustPackage {
          pname = "queuewatch";
          version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
          src = self;
          cargoLock.lockFile = ./Cargo.lock;
          meta = {
            description = "A read-only, OIDC-protected status page for your *arr stack";
            license = pkgs.lib.licenses.agpl3Only;
            mainProgram = "queuewatch";
          };
        };
      });

      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [ cargo rustc rustfmt clippy rust-analyzer ];
        };
      });

      nixosModules.default = ./nix/module.nix;

      checks = forAll (
        pkgs:
        let
          package = self.packages.${pkgs.system}.default;
        in
        {
          inherit package;
          # Known advisories against Cargo.lock, read offline from the pinned
          # database.
          #
          # ONE advisory is ignored, by name: RUSTSEC-2023-0071 (Marvin, a
          # timing side channel in `rsa`). `rsa` comes in through
          # `openidconnect` and IS linked - but queuewatch holds no RSA private
          # key; it only verifies the provider's ID token signatures with
          # public keys, and the attack needs an oracle for private-key
          # operations. No fixed version exists. Anything else fails the check.
          audit = pkgs.runCommand "queuewatch-audit" { nativeBuildInputs = [ pkgs.cargo-audit ]; } ''
            HOME=$TMPDIR cargo-audit audit --no-fetch --db ${advisory-db} \
              --ignore RUSTSEC-2023-0071 --file ${./Cargo.lock}
            touch $out
          '';
          # Bans, sources and licenses of the dependency tree (deny.toml).
          # Inside the package's build environment: the vendored crates are
          # what `cargo metadata` reads there, so nothing is fetched.
          deny = package.overrideAttrs (old: {
            pname = "queuewatch-deny";
            nativeBuildInputs = old.nativeBuildInputs ++ [ pkgs.cargo-deny ];
            buildPhase = "cargo deny --offline check bans sources licenses";
            doCheck = false;
            installPhase = "touch $out";
          });
          clippy = package.overrideAttrs (old: {
            pname = "queuewatch-clippy";
            nativeBuildInputs = old.nativeBuildInputs ++ [ pkgs.clippy ];
            buildPhase = "cargo clippy --offline --all-targets -- -D warnings";
            doCheck = false;
            installPhase = "touch $out";
          });
          fmt = pkgs.runCommand "queuewatch-fmt" { nativeBuildInputs = [ pkgs.cargo pkgs.rustfmt ]; } ''
            cd ${self} && cargo fmt --check
            touch $out
          '';
        }
        // nixpkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          vm = import ./nix/test.nix {
            inherit pkgs;
            module = self.nixosModules.default;
            package = self.packages.${pkgs.system}.default;
          };
        }
      );
    };
}
