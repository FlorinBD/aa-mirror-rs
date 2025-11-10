use std::env;
use std::io;
use std::io::Write;
use std::process::Command;

fn main() {
    // Use write! as a workaround to avoid https://github.com/rust-lang/rust/issues/46016
    // when piping output to an external program
    let mut stdout = io::stdout();

    let mut output = Command::new("git")
        .args(&["log", "-n1", "--pretty=format:%h", "HEAD"])
        .output()
        .unwrap();
    let mut result = String::from_utf8(output.stdout).unwrap();
    if result.is_empty() {
        result = env::var("BUILDROOT_COMMIT").unwrap_or_default();
        result.truncate(7);
        result = format!("br#{}", result); // add buildroot prefix
    } else if !Command::new("git")
        .args(&["diff", "--quiet"])
        .status()
        .expect("failed to execute process")
        .success()
    {
        result += "-dirty";
    }
    _ = write!(&mut stdout, "cargo:rustc-env=GIT_HASH={}\n", result);

    output = Command::new("git")
        .args(&["log", "-n1", "--pretty=format:%cd", "--date=short", "HEAD"])
        .output()
        .unwrap();
    result = String::from_utf8(output.stdout).unwrap().replace("-", "");
    if result.is_empty() {
        result = env::var("AA_MIRROR_COMMIT").unwrap_or_default();
        result.truncate(7);
    }
    _ = write!(&mut stdout, "cargo:rustc-env=GIT_DATE={}\n", result);

    output = Command::new("date")
        .args(&["+%Y%m%d_%H%M%S"])
        .output()
        .unwrap();
    result = String::from_utf8(output.stdout).unwrap();
    _ = write!(&mut stdout, "cargo:rustc-env=BUILD_DATE={}\n", result);

    // creating protobuf
    protobuf_codegen::Codegen::new()
        // Use `protoc` parser, optional.
        .protoc()
        // Use `protoc-bin-vendored` bundled protoc command, optional.
        .protoc_path(&protoc_bin_vendored::protoc_bin_path().unwrap())
        // All inputs and imports from the inputs must reside in `includes` directories.
        .includes(&["src/protos"])
        // Inputs must reside in some of include paths.
        .input("src/protos/WifiStartRequest.proto")
        .input("src/protos/WifiInfoResponse.proto")
        .input("src/protos/protos.proto")
        .input("src/protos/ev.proto")
        // Specify output directory relative to Cargo output directory.
        .cargo_out_dir("protos")
        .run_from_script();
}
