use super::*;

const SYS_IMG: &str = include_str!("../testdata/sys-img2-3.xml");
const REPO: &str = include_str!("../testdata/repository2-3.xml");
const SYS_IMG_BASE: &str = "https://dl.google.com/android/repository/sys-img/google_apis/";
const HOST: Host = Host::MACOS_ARM64;

fn sys_img() -> Manifest {
    Manifest::parse(SYS_IMG, SYS_IMG_BASE).expect("parse sys-img fixture")
}

fn repo() -> Manifest {
    Manifest::parse(REPO, REPO_URL).expect("parse repository fixture")
}

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("emu-sdk-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("mkdir");
    d
}

#[test]
fn resolves_newest_stable_system_image() {
    let d = sys_img().resolve(&system_image_path("36", "google_apis", "arm64-v8a"), HOST).expect("resolve");
    // The beta r8 is newer but not stable.
    assert_eq!(d.revision, Revision { major: 7, ..Revision::default() });
    assert_eq!(d.url, format!("{SYS_IMG_BASE}arm64-v8a-36_r07.zip"));
    assert_eq!(d.size, 1_872_691_175);
    assert_eq!(d.sha1, "5a99183b6d924da606260e45fd41a3eb8eca6eb7");
    assert_eq!(d.api_level.as_deref(), Some("36"));
    assert_eq!(d.install_dir(), PathBuf::from("system-images/android-36/google_apis/arm64-v8a"));
    assert_eq!(d.file_name(), "arm64-v8a-36_r07.zip");

    // Duplicate stable entries: the higher revision wins regardless of order.
    let old = sys_img().resolve("system-images;android-24;google_apis;arm64-v8a", HOST).expect("resolve 24");
    assert_eq!(old.revision.major, 29);
}

#[test]
fn resolution_errors() {
    let m = sys_img();
    // Only a canary build exists.
    let e = m.resolve(&system_image_path("CANARY", "google_apis", "arm64-v8a"), HOST).expect_err("canary only");
    assert!(matches!(e, Error::NoArchive(..)), "{e}");
    assert!(e.to_string().contains("no stable archive") && e.to_string().contains("macosx/aarch64"), "{e}");
    let e = m.resolve(&system_image_path("99", "google_apis", "arm64-v8a"), HOST).expect_err("unknown");
    assert!(matches!(e, Error::UnknownPackage(..)), "{e}");
    // An x86-only archive is not for this host.
    assert!(matches!(repo().resolve("x86-only", HOST), Err(Error::NoArchive(..))));
}

#[test]
fn resolves_platform_tools_for_host() {
    let d = repo().resolve("platform-tools", HOST).expect("resolve");
    // The dev-channel 38.0.0-rc1 is skipped; the darwin archive is picked over linux/windows.
    assert_eq!(d.revision.to_string(), "37.0.3");
    assert_eq!(d.url, "https://dl.google.com/android/repository/platform-tools_r37.0.3-darwin.zip");
    assert_eq!(d.size, 16_597_626);
    assert_eq!(d.sha1, "d48920e2650321710b4d15d3acd9e277d59cd55b", "checksums are normalised to lowercase");
    assert_eq!(d.install_dir(), PathBuf::from("platform-tools"));

    // host-arch matters when present (and absolute archive URLs are kept as-is).
    let ndk = repo().resolve("ndk;23.0.7421159", HOST).expect("ndk");
    assert_eq!(ndk.url, "https://example.invalid/android-ndk-r23-darwin-aarch64.zip");
    let intel = Host { os: "macosx", arch: "x64" };
    assert!(repo().resolve("ndk;23.0.7421159", intel).expect("ndk x64").url.ends_with("darwin-x86_64.zip"));
    assert!(repo().resolve("platform-tools", Host { os: "linux", arch: "x64" }).expect("linux").url.ends_with("-linux.zip"));
}

#[test]
fn lists_system_images_for_host() {
    let m = sys_img();
    let all: Vec<String> = m.system_images(HOST, None, "arm64-v8a").into_iter().map(|d| format!("{} r{}", d.path, d.revision)).collect();
    assert_eq!(
        all,
        [
            "system-images;android-24;google_apis;arm64-v8a r29",
            "system-images;android-36;google_apis;arm64-v8a r7",
            "system-images;android-36;google_apis_ps16k;arm64-v8a r7",
        ]
    );
    let tagged = m.system_images(HOST, Some("google_apis"), "arm64-v8a");
    assert_eq!(tagged.len(), 2);
    let x86 = m.system_images(HOST, None, "x86_64");
    assert_eq!(x86.len(), 1);
    assert_eq!(x86[0].file_name(), "x86_64-36_r07.zip");

    // Platforms order numerically, extension and minor releases after their base.
    let mut platforms = ["android-CANARY", "android-37.0", "android-36.1", "android-36-ext19", "android-9", "android-36"];
    let key = |p: &&str| (SysImagePath { platform: p, tag: "t", abi: "a" }).api_key();
    platforms.sort_by_key(key);
    assert_eq!(platforms, ["android-9", "android-36", "android-36-ext19", "android-36.1", "android-37.0", "android-CANARY"]);
}

#[test]
fn revisions_order_and_print() {
    let r = |major, minor, micro, preview| Revision { major, minor, micro, preview };
    assert!(r(38, 0, 0, Some(1)) < r(38, 0, 0, None));
    assert!(r(37, 0, 3, None) < r(38, 0, 0, Some(1)));
    assert_eq!(r(7, 0, 0, None).to_string(), "7");
    assert_eq!(r(38, 0, 0, Some(2)).to_string(), "38.0.0-rc2");
}

#[test]
fn site_list_and_urls() {
    let xml = r#"<common:site-list xmlns:sdk="http://schemas.android.com/sdk/android/addons-list/5" xmlns:common="http://schemas.android.com/repository/android/sites-common/1" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <site xsi:type="sdk:sysImgSiteType"><displayName>Android System Images</displayName><url>sys-img/android/sys-img2-3.xml</url></site>
  <site xsi:type="sdk:addonSiteType"><displayName>Some add-on</displayName><url>addon2-3.xml</url></site>
  <site xsi:type="sdk:sysImgSiteType"><displayName>Google APIs</displayName><url>sys-img/google_apis/sys-img2-3.xml</url></site>
</common:site-list>"#;
    assert_eq!(
        parse_site_list(xml, REPO_URL).expect("sites"),
        [format!("{REPO_URL}sys-img/android/sys-img2-3.xml"), format!("{REPO_URL}sys-img/google_apis/sys-img2-3.xml")]
    );
    assert_eq!(sys_img_url("default"), format!("{REPO_URL}sys-img/android/sys-img2-3.xml"));
    assert_eq!(sys_img_url("google_apis_ps16k"), format!("{REPO_URL}sys-img/google_apis/sys-img2-3.xml"));
    assert_eq!(sys_img_url("android-tv"), format!("{REPO_URL}sys-img/android-tv/sys-img2-3.xml"));
    assert_eq!(system_image_path("android-36.1", "google_apis", "arm64-v8a"), "system-images;android-36.1;google_apis;arm64-v8a");
    assert!(Manifest::parse("<not xml", REPO_URL).is_err());
}

#[test]
fn sha1_known_values() {
    assert_eq!(sha1_hex(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
    let dir = scratch("sha1");
    let f = dir.join("big");
    // Spans several read buffers.
    let data: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&f, &data).expect("write");
    assert_eq!(sha1_file(&f).expect("sha1"), sha1_hex(&data));
    let _ = std::fs::remove_dir_all(dir);
}

/// The whole install path, offline: curl fetches a `file://` URL, the archive is verified,
/// unpacked and moved into place. Needs macOS's `/usr/bin/zip` to build the archive.
#[test]
fn installs_from_a_local_archive() {
    if !Path::new("/usr/bin/zip").is_file() || !Path::new("/usr/bin/curl").is_file() || !Path::new("/usr/bin/unzip").is_file() {
        return;
    }
    let work = scratch("install");
    let src = work.join("src/platform-tools");
    std::fs::create_dir_all(&src).expect("mkdir");
    std::fs::write(src.join("source.properties"), "Pkg.Revision=37.0.3\n").expect("write");
    std::fs::write(src.join("adb"), "#!/bin/sh\necho fake adb\n").expect("write");
    let zip = work.join("platform-tools_r37.0.3-darwin.zip");
    let ok = Command::new("/usr/bin/zip").current_dir(work.join("src")).arg("-qr").arg(&zip).arg("platform-tools").status().expect("zip");
    assert!(ok.success());
    let bytes = std::fs::read(&zip).expect("read zip");

    let mut sdk = Sdk::new(work.join("sdk")).expect("sdk");
    sdk.quiet = true;
    let good = Download {
        path: "platform-tools".into(),
        revision: Revision { major: 37, minor: 0, micro: 3, preview: None },
        api_level: None,
        url: format!("file://{}", zip.display()),
        size: bytes.len() as u64,
        sha1: sha1_hex(&bytes),
    };

    // A bad checksum is refused and the partial download is discarded.
    let bad = Download { sha1: "0".repeat(40), ..good.clone() };
    let e = sdk.install(&bad).expect_err("sha1 mismatch");
    assert!(matches!(e, Error::Verify { what: "sha1", .. }), "{e}");
    assert!(!sdk.downloads().join("platform-tools_r37.0.3-darwin.zip.part").exists());
    assert!(!sdk.is_installed(Path::new("platform-tools")));

    let dest = sdk.install(&good).expect("install");
    assert_eq!(dest, sdk.root().join("platform-tools"));
    assert!(sdk.is_installed(Path::new("platform-tools")));
    assert!(dest.join("adb").is_file());
    assert!(!sdk.downloads().join("platform-tools_r37.0.3-darwin.zip.part").exists(), "archive removed after install");
    let leftovers: Vec<_> = std::fs::read_dir(sdk.downloads()).expect("ls").flatten().collect();
    assert!(leftovers.is_empty(), "no temp dirs left: {leftovers:?}");

    // Manifests are fetched once, then served from the cache.
    let mf = work.join("repository2-3.xml");
    std::fs::write(&mf, REPO).expect("write manifest");
    let url = format!("file://{}", mf.display());
    let m = sdk.manifest(&url).expect("manifest");
    assert_eq!(m.base_url, format!("file://{}/", work.display()));
    std::fs::remove_file(&mf).expect("rm");
    assert_eq!(sdk.manifest(&url).expect("cached").packages.len(), m.packages.len());
    let _ = std::fs::remove_dir_all(work);
}
