// Lets the binary find the SDK's gfxstream renderer and its @rpath dependencies at runtime:
// next to the binary (an SDK overlay installs emu as emulator/emulator), and in the SDK the
// build machine has.
fn main() {
    println!("cargo:rerun-if-env-changed=ANDROID_HOME");
    println!("cargo:rerun-if-env-changed=ANDROID_SDK_ROOT");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    println!("cargo:rustc-link-arg-bins=-Wl,-rpath,@executable_path/lib64");
    let sdk = std::env::var("ANDROID_SDK_ROOT")
        .or_else(|_| std::env::var("ANDROID_HOME"))
        .unwrap_or_else(|_| format!("{}/Library/Android/sdk", std::env::var("HOME").unwrap_or_default()));
    println!("cargo:rustc-link-arg-bins=-Wl,-rpath,{sdk}/emulator/lib64");
}
