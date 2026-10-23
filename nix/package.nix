{ gh, git, lib, makeWrapper, rustPlatform, systems }:

let
  cargoToml = builtins.fromTOML (builtins.readFile ../Cargo.toml);
in
rustPlatform.buildRustPackage {
  pname = cargoToml.package.name;
  version = cargoToml.package.version;
  src = ../.;
  cargoLock.lockFile = ../Cargo.lock;
  nativeBuildInputs = [ makeWrapper ];
  # The Changes panel's tests make throwaway repos and worktrees.
  nativeCheckInputs = [ git ];

  # $out doubles as the plugin root: the manifest runs bin/herdr-sim beside it.
  postInstall = ''
    install -Dm644 herdr-plugin.toml "$out/herdr-plugin.toml"
    wrapProgram "$out/bin/herdr-sim" --suffix PATH : ${lib.makeBinPath [ gh ]}
  '';

  meta = {
    description = cargoToml.package.description;
    homepage = cargoToml.package.homepage;
    license = with lib.licenses; [ mit asl20 ];
    mainProgram = "herdr-sim";
    platforms = systems;
  };
}
