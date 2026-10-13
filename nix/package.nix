{ lib, rustPlatform, makeWrapper, cacert }:
rustPlatform.buildRustPackage {
  pname = "context7-account-broker";
  version = (lib.importTOML ../Cargo.toml).package.version;
  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [ ../Cargo.toml ../Cargo.lock ../src ];
  };
  cargoLock.lockFile = ../Cargo.lock;
  nativeBuildInputs = [ makeWrapper ];
  SSL_CERT_FILE = "${cacert}/etc/ssl/certs/ca-bundle.crt";
  postInstall = ''
    wrapProgram "$out/bin/context7-account-broker" \
      --set-default SSL_CERT_FILE "${cacert}/etc/ssl/certs/ca-bundle.crt"
  '';
  meta = {
    description = "Context7 MCP with shared account routing, quota, and caching";
    homepage = "https://github.com/mirsella/context7-account-broker";
    license = lib.licenses.mit;
    platforms = [ "x86_64-linux" ];
    mainProgram = "context7-account-broker";
  };
}
