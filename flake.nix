{
  description = "Logos bitcoin_wallet_backend: the Bitcoin wallet coordinator (roles, jobs, sends, events, the node module's settings). Holds no wallet keys.";

  inputs = {
    logos-module-builder.url = "github:logos-co/logos-module-builder";
    # The wallet core and the node module are reached through their contracts under
    # contracts/, named by dependency_overrides in metadata.json.
    # logos.wallets/1 (wallets.lidl): this module's keys from the signer manager, bound at runtime.
    logos-signer-interface = {
      url = "github:logos-co/logos-signer-interface/d8cfff84327c9b2b5cdb9702d1c4217cad567cee";
      flake = false;
    };
  };

  outputs = inputs@{ self, logos-module-builder, ... }:
    let
      nixpkgs = logos-module-builder.inputs.nixpkgs;
      systems = [ "aarch64-darwin" "x86_64-darwin" "aarch64-linux" "x86_64-linux" ];
      # x86_64-windows is a cross build from x86_64-linux.
      targets = systems ++ [ "x86_64-windows" ];
      forAllSystems = f: nixpkgs.lib.genAttrs targets f;
    in
    {
      packages = forAllSystems (system:
        (logos-module-builder.lib.mkLogosModule {
          src = ./.;
          configFile = ./metadata.json;
          flakeInputs = inputs;
        }).packages.${system});
    };
}
