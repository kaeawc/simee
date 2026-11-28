#!/usr/bin/env bash
# Builds simee's iOS camera shim and its fixture app for the iOS simulator (universal: arm64 and x86_64), with the
# Xcode toolchain only (xcrun clang; nothing is downloaded), ad-hoc signed:
#
#   shims/ios-camera/build.sh [OUT_DIR]     # default: target/ios-camera
#
#   OUT_DIR/libsimee-camera.dylib        the library to insert (DYLD_INSERT_LIBRARIES); links only
#                                        libSystem and loads the hooks into user apps
#   OUT_DIR/libsimee-camera-hooks.dylib  the AVFoundation hooks (must stay beside the first)
#   OUT_DIR/CameraFixture.app            the fixture app (bundle id dev.simee.camerafixture)
set -euo pipefail

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
out=${1:-$here/../../target/ios-camera}
mkdir -p -- "$out/CameraFixture.app"
out=$(cd -- "$out" && pwd)
tmp=$(mktemp -d)
trap 'rm -rf -- "$tmp"' EXIT
frameworks=(-framework Foundation -framework UIKit -framework AVFoundation -framework CoreImage
  -framework CoreMedia -framework CoreVideo -framework CoreGraphics -framework QuartzCore)

# Each product is built per architecture (arm64 simulators; x86_64 for Rosetta apps, which would
# refuse an inserted arm64-only dylib) and joined with lipo.
for arch in arm64 x86_64; do
  cc=(xcrun --sdk iphonesimulator clang -target "$arch-apple-ios17.0-simulator" -O2 -Wall -Werror)
  "${cc[@]}" -dynamiclib -install_name @rpath/libsimee-camera.dylib \
    "$here/loader.c" -o "$tmp/libsimee-camera.$arch.dylib"
  "${cc[@]}" -dynamiclib -fobjc-arc -install_name @rpath/libsimee-camera-hooks.dylib "${frameworks[@]}" \
    "$here/hooks.m" -o "$tmp/libsimee-camera-hooks.$arch.dylib"
  "${cc[@]}" -fobjc-arc "${frameworks[@]}" "$here/fixture/main.m" -o "$tmp/CameraFixture.$arch"
done
lipo -create -output "$out/libsimee-camera.dylib" "$tmp"/libsimee-camera.*.dylib
lipo -create -output "$out/libsimee-camera-hooks.dylib" "$tmp"/libsimee-camera-hooks.*.dylib
lipo -create -output "$out/CameraFixture.app/CameraFixture" "$tmp"/CameraFixture.*
cp -- "$here/fixture/Info.plist" "$out/CameraFixture.app/Info.plist"

codesign -f -s - "$out/libsimee-camera.dylib" "$out/libsimee-camera-hooks.dylib" 2>/dev/null
codesign -f -s - "$out/CameraFixture.app" 2>/dev/null
echo "$out"
