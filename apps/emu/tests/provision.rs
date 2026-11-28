//! `emu`'s provisioning end to end, offline: a `file://` mirror of the SDK repository
//! (`SDK_TEST_BASE_URL`, as sdkmanager honours) with platform-tools and an Android TV image,
//! provisioned into an empty SDK, made into an AVD and planned, as a user would on any host.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use emu_sdk::testing::stored_zip;

struct Mirror {
    root: PathBuf,
}

impl Drop for Mirror {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn archive(name: &str, zip: &[u8], host: Option<&str>) -> String {
    let host = host.map(|h| format!("<host-os>{h}</host-os>")).unwrap_or_default();
    format!(
        "<archives><archive><complete><size>{}</size><checksum type=\"sha1\">{}</checksum><url>{name}</url></complete>{host}</archive></archives>",
        zip.len(),
        emu_sdk::sha1_hex(zip)
    )
}

fn mirror(tag: &str) -> Mirror {
    let root = std::env::temp_dir().join(format!("emu-mirror-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let repo = root.join("repo");
    std::fs::create_dir_all(repo.join("sys-img/android-tv")).expect("mirror dirs");

    let tools = stored_zip(&[("platform-tools/source.properties", b"Pkg.Revision=37.0.3\n"), ("platform-tools/adb", b"#!/bin/sh\n")]);
    let archives: String = ["linux", "macosx", "windows"]
        .iter()
        .map(|h| {
            std::fs::write(repo.join(format!("platform-tools-{h}.zip")), &tools).expect("tools zip");
            archive(&format!("platform-tools-{h}.zip"), &tools, Some(h))
        })
        .collect::<Vec<_>>()
        .join("")
        .replace("</archives><archives>", "");
    std::fs::write(
        repo.join("repository2-3.xml"),
        format!(
            r#"<sdk:sdk-repository xmlns:sdk="http://schemas.android.com/sdk/android/repo/repository2/03" xmlns:generic="http://schemas.android.com/repository/android/generic/02" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <channel id="channel-0">stable</channel>
  <remotePackage path="platform-tools"><type-details xsi:type="generic:genericDetailsType"/><revision><major>37</major><minor>0</minor><micro>3</micro></revision><display-name>Platform-Tools</display-name><channelRef ref="channel-0"/>{archives}</remotePackage>
</sdk:sdk-repository>"#
        ),
    )
    .expect("repository manifest");

    let mut system = vec![0u8; 4 << 20];
    system[8192..12288].fill(1);
    let image = stored_zip(&[
        ("arm64-v8a/source.properties", b"Pkg.Revision=3\nSystemImage.TagId=android-tv\nAndroidVersion.ApiLevel=34\n"),
        ("arm64-v8a/advancedFeatures.ini", b"VirtioSndCard = on\n"),
        ("arm64-v8a/kernel-ranchu", b"k"),
        ("arm64-v8a/ramdisk.img", b"r"),
        ("arm64-v8a/system.img", &system),
        ("arm64-v8a/vendor.img", b"v"),
    ]);
    std::fs::write(repo.join("sys-img/android-tv/tv-34.zip"), &image).expect("image zip");
    std::fs::write(
        repo.join("sys-img/android-tv/sys-img2-3.xml"),
        format!(
            r#"<sys-img:sdk-sys-img xmlns:sys-img="http://schemas.android.com/sdk/android/repo/sys-img2/03" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <channel id="channel-0">stable</channel>
  <remotePackage path="system-images;android-34;android-tv;arm64-v8a"><type-details xsi:type="sys-img:sysImgDetailsType"><api-level>34</api-level><tag><id>android-tv</id><display>Android TV</display></tag><abi>arm64-v8a</abi></type-details><revision><major>3</major></revision><display-name>Android TV ARM 64 v8a System Image</display-name><channelRef ref="channel-0"/>{}</remotePackage>
</sys-img:sdk-sys-img>"#,
            archive("tv-34.zip", &image, None)
        ),
    )
    .expect("sys-img manifest");
    std::fs::write(
        repo.join("addons_list-5.xml"),
        r#"<common:site-list xmlns:sdk="http://schemas.android.com/sdk/android/addons-list/5" xmlns:common="http://schemas.android.com/repository/android/sites-common/1" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <site xsi:type="sdk:sysImgSiteType"><displayName>Android TV</displayName><url>sys-img/android-tv/sys-img2-3.xml</url></site>
</common:site-list>"#,
    )
    .expect("site list");
    std::fs::create_dir_all(root.join("home")).expect("home");
    Mirror { root }
}

impl Mirror {
    fn emu(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_emu"))
            .args(args)
            .env("SDK_TEST_BASE_URL", format!("file://{}/repo", self.root.display()))
            .env("ANDROID_HOME", self.root.join("sdk"))
            .env("ANDROID_AVD_HOME", self.root.join("avd"))
            .env("HOME", self.root.join("home"))
            .env_remove("ANDROID_SDK_ROOT")
            .output()
            .expect("emu runs")
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.emu(args);
        assert!(out.status.success(), "emu {args:?}:\n{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn err(&self, args: &[&str]) -> String {
        let out = self.emu(args);
        assert!(!out.status.success(), "emu {args:?} should fail");
        String::from_utf8_lossy(&out.stderr).into_owned()
    }

    fn sdk(&self, rel: &str) -> PathBuf {
        self.root.join("sdk").join(rel)
    }
}

// Unix only: provisioning unpacks the archives with `unzip`, which a Windows host doesn't have
// (docs/development.md "Windows").
#[cfg(unix)]
#[test]
fn provisions_an_image_into_an_empty_sdk_and_plans_its_avd() {
    let m = mirror("tv");
    let listed = m.ok(&["-list-images", "-tag", "android-tv", "-abi", "arm64-v8a"]);
    assert!(listed.lines().any(|l| l.starts_with("34 ") && l.contains("android-tv")), "{listed}");

    let dry = m.ok(&["-provision", "34", "-tag", "android-tv", "-abi", "arm64-v8a", "-no-renderer", "-dry-run"]);
    assert!(dry.contains("tv-34.zip") && !m.sdk("system-images").exists(), "a dry run downloads nothing:\n{dry}");

    let out = m.ok(&["-provision", "34", "-tag", "android-tv", "-abi", "arm64-v8a", "-no-renderer", "-create-avd", "TV"]);
    assert!(out.contains("created TV"), "{out}");
    assert!(m.sdk("platform-tools/adb").is_file());
    let sysdir = m.sdk("system-images/android-34/android-tv/arm64-v8a");
    let system = std::fs::read(sysdir.join("system.img")).expect("system.img");
    assert_eq!((system.len(), system[8192], system[0]), (4 << 20, 1, 0), "the image, byte for byte");
    assert!(!m.sdk(".downloads").read_dir().expect("downloads").flatten().any(|e| e.path().extension().is_some_and(|x| x == "part")));

    assert_eq!(m.ok(&["-list-avds"]).trim(), "TV");
    let config = std::fs::read_to_string(m.root.join("avd/TV.avd/config.ini")).expect("config");
    for key in ["hw.lcd.width=1920", "hw.dPad=yes", "hw.screen=no-touch", "hw.device.name=tv_1080p"] {
        assert!(config.contains(key), "{key} missing from\n{config}");
    }
    let plan = m.ok(&["-avd", "TV", "-dry-run"]);
    assert!(plan.contains("avd       TV (Android TV · TV)") && !plan.contains("multi-touch"), "{plan}");

    let again = m.ok(&["-provision", "34", "-tag", "android-tv", "-abi", "arm64-v8a", "-no-renderer"]);
    assert!(again.contains("have system-images;android-34;android-tv;arm64-v8a") && again.contains("have platform-tools"), "{again}");
    assert!(
        m.err(&["-provision", "34", "-tag", "android-tv", "-abi", "arm64-v8a", "-create-avd", "TV"]).contains("AVD 'TV' already exists")
    );
    let missing = m.err(&["-provision", "99", "-tag", "android-tv", "-abi", "arm64-v8a", "-no-renderer"]);
    assert!(
        missing.contains("system-images;android-99;android-tv;arm64-v8a") && missing.contains("emu -list-images -tag android-tv"),
        "{missing}"
    );
}

#[test]
fn a_dry_run_names_what_a_boot_would_miss() {
    let m = mirror("missing");
    m.ok(&["-provision", "34", "-tag", "android-tv", "-abi", "arm64-v8a", "-no-renderer", "-create-avd", "TV"]);
    std::fs::remove_file(m.sdk("system-images/android-34/android-tv/arm64-v8a/vendor.img")).expect("rm");
    let out = m.emu(&["-avd", "TV", "-dry-run"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("missing") && String::from_utf8_lossy(&out.stderr).contains("vendor.img"));
    assert!(m.err(&["-avd", "nope", "-dry-run"]).contains("nope"));
    assert!(Path::new(&m.root).exists());
}
