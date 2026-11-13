{
  description = "Buzz CLI for Linux and NixOS";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    crane.url = "github:ipetkov/crane";
  };

  outputs = { nixpkgs, crane, ... }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forEachSystem = nixpkgs.lib.genAttrs systems;
      packagesFor = system:
        let
          pkgs = import nixpkgs { inherit system; };
          craneLib = crane.mkLib pkgs;
          src = pkgs.lib.cleanSourceWith {
            src = ./.;
            filter = path: type:
              let
                rel = pkgs.lib.removePrefix (toString ./. + "/") (toString path);
                under = dir: rel == dir || pkgs.lib.hasPrefix (dir + "/") rel;
                above = dir: pkgs.lib.hasPrefix (rel + "/") dir;
                kept = [
                  "Cargo.toml" "Cargo.lock" ".cargo" "crates"
                  "examples/countdown-bot" "test-fixtures"
                  "scripts/model-capabilities.json" "voltage/remote-agents.md"
                ];
              in
              builtins.any (dir: under dir || above dir) kept
              && !(builtins.elem (baseNameOf path) [ "target" "node_modules" ".hermit" ]);
          };
          args = {
            pname = "buzz-cli";
            version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version;
            inherit src;
            cargoExtraArgs = "--locked -p buzz-cli";
            # Unit tests run in the native release workflow. This derivation
            # verifies the installed binary and its setup commands below.
            doCheck = false;
            nativeBuildInputs = [ pkgs.pkg-config pkgs.makeWrapper ];
          };
          buzz = craneLib.buildPackage (args // {
            cargoArtifacts = craneLib.buildDepsOnly args;
            postInstall = ''
              # Setup invokes these tools to identify the Git account and
              # create/register the agents' commit-signing key.
              wrapProgram "$out/bin/buzz" \
                --prefix PATH : ${pkgs.lib.makeBinPath [ pkgs.git pkgs.openssh pkgs.gh ]}
            '';
            doInstallCheck = true;
            installCheckPhase = ''
              runHook preInstallCheck
              "$out/bin/buzz" --help >/dev/null
              "$out/bin/buzz" paseo setup --help >/dev/null
              "$out/bin/buzz" paseo pairing --help >/dev/null
              runHook postInstallCheck
            '';
            meta = {
              description = "Agent-first Buzz CLI, including Paseo setup and pairing";
              homepage = "https://github.com/voltagecloud/buzz";
              license = pkgs.lib.licenses.asl20;
              mainProgram = "buzz";
              platforms = systems;
            };
          });
        in
        { inherit buzz; default = buzz; };
    in
    {
      packages = forEachSystem packagesFor;
      apps = forEachSystem (system:
        let
          app = {
            type = "app";
            program = "${(packagesFor system).buzz}/bin/buzz";
            meta.description = "Buzz command-line client";
          };
        in
        { buzz = app; default = app; });
      checks = forEachSystem (system: { buzz = (packagesFor system).buzz; });
    };
}
