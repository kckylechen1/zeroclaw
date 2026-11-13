# Apply to the real flake package with `nix eval --json ... --apply`.
# These assertions inspect its derivations; they do not build Rust packages.
package:

let
  optionValues = option: args:
    if builtins.length args < 2 then [ ] else
    (if builtins.head args == option then [ (builtins.elemAt args 1) ] else [ ])
    ++ optionValues option (builtins.tail args);

  require = name: condition: message:
    if condition then true else throw "${name}: ${message}";

  check = name: drv: expectedFeatures: wantAcp:
    let
      flags = drv.cargoBuildFlags;
      selectedPackages = optionValues "-p" flags;
      featureArgs = optionValues "--features" flags;
      featureNames = builtins.concatLists (builtins.map
        (value: builtins.filter builtins.isString (builtins.split "," value))
        featureArgs);
      installLines = builtins.filter builtins.isString (builtins.split "\n" drv.postInstall);
      checksBinary = binary: builtins.elem ''test -x "$out/bin/${binary}"'' installLines;
      expectedPackages = [ "zeroclawlabs" ]
        ++ (if wantAcp then [ "zeroclaw-bridge-acp" ] else [ ]);
    in
    assert require name (drv.type == "derivation") "expected a real package derivation";
    assert require name (selectedPackages == expectedPackages) "unexpected Cargo packages";
    assert require name (builtins.elem "--no-default-features" flags) "default features must be explicit";
    assert require name
      (if expectedFeatures == null then builtins.length featureArgs == 1 else featureArgs == expectedFeatures)
      "unexpected root feature arguments";
    assert require name
      (builtins.all (feature: builtins.match "zeroclawlabs/.+" feature != null) featureNames)
      "root features must be package-qualified";
    assert require name (checksBinary "zeroclaw") "missing main executable postInstall check";
    assert require name ((checksBinary "zeroclaw-acp-bridge") == wantAcp)
      "ACP postInstall check does not match the selected features";
    {
      cargoBuildFlags = flags;
      postInstall = drv.postInstall;
    };
in
{
  default = check "default" package null true;
  minimal = check "features=[]" (package.override { features = [ ]; }) [ ] false;
  schemaExport = check "schema-export" (package.override { features = [ "schema-export" ]; })
    [ "zeroclawlabs/schema-export" ] false;
  agentRuntime = check "agent-runtime" (package.override { features = [ "agent-runtime" ]; })
    [ "zeroclawlabs/agent-runtime" ] true;
  slimControl = check "slim-control" (package.override { features = [ "slim-control" ]; })
    [ "zeroclawlabs/slim-control" ] true;
}
