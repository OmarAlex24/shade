use crate::registry::{Registry, base64};
use serde_json::json;
use sha2::{Digest, Sha512};
use std::fs;
use std::path::Path;
use std::process::Command;

pub fn add_script_package(root: &Path, temp: &Path) -> Registry {
    let registry = Registry::new();
    let package = temp.join("tar/package");
    fs::create_dir_all(&package).unwrap();
    fs::write(package.join("package.json"), br#"{"name":"@shade/test-build","version":"1.0.0","scripts":{"postinstall":"node build.cjs"}}"#).unwrap();
    fs::write(
        package.join("build.cjs"),
        "require('node:fs').writeFileSync('built.txt',process.env.npm_package_version);\n",
    )
    .unwrap();
    let archive = temp.join("package.tgz");
    assert!(
        Command::new("/usr/bin/tar")
            .env("COPYFILE_DISABLE", "1")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(package.parent().unwrap())
            .arg("package")
            .status()
            .unwrap()
            .success()
    );
    let bytes = fs::read(archive).unwrap();
    let integrity = format!("sha512-{}", base64(&Sha512::digest(&bytes)));
    registry.route("/package.tgz", bytes);
    let dependencies = json!({"approved-alias":"npm:@shade/test-build@1.0.0"});
    let manifest = json!({"name":"shade-script-test","version":"1.0.0","private":true,"dependencies":dependencies,"scripts":{"postinstall":"node -e \"require('node:fs').writeFileSync('root-ran','bad')\""}});
    fs::write(
        root.join("package.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    fs::write(root.join("package-lock.json"), serde_json::to_vec(&json!({"name":"shade-script-test","version":"1.0.0","lockfileVersion":3,"requires":true,"packages":{
        "":{"name":"shade-script-test","version":"1.0.0","dependencies":dependencies},
        "node_modules/approved-alias":{"name":"@shade/test-build","version":"1.0.0","resolved":format!("{}/package.tgz",registry.url),"integrity":integrity,"hasInstallScript":true}
    }})).unwrap()).unwrap();
    fs::write(root.join(".npmrc"), format!("registry={}/\n", registry.url)).unwrap();
    registry
}
