# Android counterparts of push, notify, biometric and reset_keychain

Issue #419. iOS has four methods with no Android implementation: `push` (an APNs payload to an
app), `notify` (a Darwin notification), `biometric` (Touch ID and Face ID) and `reset_keychain`.
This page records what each one does on iOS, what Android offers, which of those is faithful, and
what simee does. Three are implemented (`crates/sim-android/src/notify.rs`,
`crates/sim-api/src/android_notify.rs`). The fourth, `reset_keychain`, is refused with a reason.

Status: the code is tested against the fake adb and fake console in `sim_android::testing`. It has
not yet been checked on a booted guest. Those checks are listed under "To verify on a device"
below.

## push

**iOS.** `push` hands an APNs payload (`{"aps": {"alert": ...}, ...}`) to the simulator's push
service. The app then gets it exactly as it would get a real remote notification: the system shows
the alert as the app's own, and the app's delegate is called.

**Android candidates.**

| Route | What the app sees | Faithful? |
| --- | --- | --- |
| `cmd notification post [-t title] <tag> <text>` | Nothing. The notification is posted by `com.android.shell` and shown under the Shell app, not the target app ([`NotificationShellCmd`][notif-shell] posts as the calling package). | No: it tests the system tray, not the app. |
| FCM through Google's servers | A real message. But it needs Play services on the image, a server key, and the app's registration token, which only the app itself can read. | Not host-side. |
| `am broadcast -n <pkg>/<FCM receiver> -a com.google.android.c2dm.intent.RECEIVE --es k v ...` as root | The Firebase SDK's `FirebaseInstanceIdReceiver` gets the same broadcast Play services sends for each message. `FirebaseMessagingService.onMessageReceived` runs with the data. A notification message is shown by the SDK as the app's own while the app is in the background. | Yes. |

The broadcast is the faithful route. Play services delivers every FCM message to an app as the
`com.google.android.c2dm.intent.RECEIVE` broadcast to the receiver that the Firebase SDK puts in the
app's manifest. That receiver is protected by `com.google.android.c2dm.permission.SEND`, which only
Play services holds, so the shell user (uid 2000) can't send to it. Root can:
[`ActivityManager.checkComponentPermission`][am-perm] grants every permission to uid 0 and to the
system uid.
On a google_apis image (the default image here) `su 0` is there, and adbd can be root.

The extras are FCM's wire format:

- Each `data` pair is a string extra under its own key. FCM data values are always strings, so a
  JSON number or boolean is sent as its text.
- Each `notification` field is sent as `gcm.n.<field>` (`title`, `body`, `image` and so on), the
  prefix the SDK reads ([`Constants.MessageNotificationKeys`][fcm-constants]: `NOTIFICATION_PREFIX`
  is `gcm.n.`; the older `gcm.notification.` is a fallback marked for removal). Fields take FCM
  v1's `AndroidNotification` names, and the one whose wire key differs is mapped: `channel_id` is
  sent as `gcm.n.android_channel_id` (`CHANNEL`), the key the SDK takes the channel from. The
  SDK's [`NotificationParams`][fcm-params] tries the old prefix only for the same suffix, so
  `gcm.notification.channel_id` would be dropped without an error. `gcm.n.e=1` is added too:
  `NotificationParams.isNotification` reads that marker as "this message carries a notification".
- The keys FCM's server refuses are refused here too: `from`, `message_type`, `collapse_key`, and
  any key that starts with `google.` or `gcm.`.

**What simee does.** On Android, `push` takes `bundle_id` and a `payload` of the form
`{data, notification}`. It finds the package's receiver for the action with
`cmd package query-receivers --brief --components`. It then broadcasts to that receiver, as adbd's
root or through `su 0`.

- An APNs payload (`aps`) is refused, and the error names FCM's shape.
- A package with no such receiver is refused: it doesn't use FCM.
- An image with no root is refused, and the error says how to get root.

The broadcast is sent without `FLAG_INCLUDE_STOPPED_PACKAGES`. A force-stopped app therefore gets
nothing, which is what real FCM does.

**Not covered.** Two things are left out:

- A local notification with no FCM in the app (`cmd notification post`), because it isn't the app's
  notification.
- Message priority and TTL. TTL only affects how Google's servers queue a message. Priority
  matters on the device too: Play services sends a high-priority message with
  `FLAG_RECEIVER_FOREGROUND` and a temporary power allowlist entry, and the SDK's
  [`FcmBroadcastProcessor`][fcm-processor] reads that flag (as does `google.delivered_priority`).
  Without it, an app on Android 8 or later gets the message by binding to its service, not by
  starting it, and can't start a foreground service from `onMessageReceived`. simee always
  delivers as a normal-priority message: no flag, no allowlist entry.

## notify

**iOS.** `notify` posts a Darwin notification (`notify_post`) by name, system-wide, optionally after
setting its 64-bit state (`notify_set_state`). Any process that registered for the name is woken.
The notification carries no payload beyond that state.

**Android.** The closest analogue is a broadcast intent: `am broadcast -a <action>`. The
differences between the two:

- A broadcast carries typed extras instead of one integer state. `state` is refused on Android, and
  extras take its place.
- Since Android 8, a receiver declared in an app's manifest hears an implicit broadcast only when
  the broadcast names its package (`-p`) ([background execution limits][bg-broadcasts]). Receivers
  registered at runtime hear it either way, subject to the export rule below. So `bundle_id`
  matters on Android in a way it has no counterpart for on iOS.
- The shell user (uid 2000) is another uid to every app, so only an exported receiver hears the
  broadcast. A manifest receiver with `android:exported="false"` never hears `notify`
  ([`<receiver>`][receiver-exported]). An app targeting Android 14 (API 34) or later must
  register a runtime receiver with `RECEIVER_EXPORTED` or `RECEIVER_NOT_EXPORTED`
  ([runtime-registered receivers][a14-receivers]), and one registered with
  `RECEIVER_NOT_EXPORTED` doesn't hear it either. In both cases `am` still prints
  `Broadcast completed`, so `notify` succeeds with nothing received: simee can't tell the two
  apart from the host.
- The shell user may not send protected system broadcasts (`BOOT_COMPLETED`, battery and so on).
  The activity manager refuses them with a `SecurityException`, and that error is returned.

**What simee does.** On Android, `notify` takes `name` (the action), an optional `bundle_id` (`-p`)
and optional `extras`. Each extra's `am` flag follows its JSON type:

| JSON type | `am` flag |
| --- | --- |
| string | `--es` |
| null | `--esn` |
| boolean | `--ez` |
| integer within 32 bits | `--ei` |
| other integer | `--el` |
| any other number | `--ef` |

The broadcast is sent as the shell user, the way an app's own broadcast would be. Success is read
from `am`'s `Broadcast completed` line, because `am`'s exit status differs between releases.

## biometric

**iOS.** `biometric` takes one of four actions:

- `match` and `nomatch` simulate a matching or failing Touch ID or Face ID attempt.
- `enroll` and `unenroll` flip whether the device has biometrics enrolled.

**Android.** The emulator console has `finger touch <id>` and `finger remove`. simee's emu and the
stock emulator both have them. They reach the goldfish fingerprint HAL over
`qemud:fingerprintlisten` (`crates/emu-vmm/src/pipe/fingerprint.rs`) as "a finger with this id is
on the sensor" and "lifted". What the HAL does with a touch:

- While enrolling, the touch enrolls that id.
- While authenticating, an enrolled id is a match.
- Any other id is a failed attempt.

This is the stock emulator's "Fingerprint" control. Its UI offers fingers 1 to 10.

#160 also lists fingerprint. It is implemented here, once, and #160 can use `biometric`.

**What simee does.** On Android, `biometric` behaves as follows:

- `match` sends `finger touch <finger_id>` (default 1) followed by `finger remove`.
- `nomatch` sends the same with id 99 by default, an id the emulator's controls never enroll.
- `enroll` and `unenroll` are refused. Android enrolls a fingerprint only through Settings'
  enrollment flow (Security, after a screen lock is set). Each touch that flow asks for is a
  `biometric` `match` with the finger's id. The screen lock itself can be set host-side with
  `locksettings set-pin <pin>` ([`LockSettingsShellCommand`][locksettings]), so only the
  enrollment taps need Settings. No host-side switch flips goldfish's enrolled state.
- Considered and rejected: AOSP's virtual fingerprint HAL enrolls without UI
  (`settings put secure biometric_virtual_enabled 1`, then
  `setprop persist.vendor.fingerprint.virtual.enrollments 1` and `cmd fingerprint sync`;
  [its README][virtual-hal]). It doesn't fit here: it ships as a debug-only vendor APEX
  (`PRODUCT_PACKAGES_DEBUG`), so SDK images don't carry it; it is ignored while a real HAL is
  installed, as goldfish's is; and it still wants a screen lock set.
- `face: true` is refused, because the emulator has no face sensor.

## reset_keychain

**iOS.** `reset_keychain` deletes every keychain item on the device: one system-wide credential
store.

**Android.** Android has no device-wide keychain. The candidates, and why none of them is
`reset_keychain`:

- **App KeyStore keys.** Android Keystore keys belong to one app's uid (its keystore2 namespace).
  They go when the app deletes them, when it is uninstalled, or when its data is cleared:
  `pm clear` clears the namespace too (`AppDataHelper.clearKeystoreData` calls
  `AndroidKeyStoreMaintenance.clearNamespace`; keystore2's [`maintenance.rs`][ks-maintenance]).
  That is one app's keys, not a device-wide store. Deleting keystore2's database rows from the
  host, as root, would corrupt the daemon's state. It is not an operation the platform offers.
- **`cmd keystore`.** There is no such shell command: keystore2 registers none. The only command
  line client is `keystore_cli_v2` ([search][ks-cli]), on userdebug images, and its `delete-all`
  deletes the caller's own namespace (the shell's or root's), not every app's.
- **`locksettings clear --old <credential>`.** This removes the screen lock, and it needs the
  current credential. Since Android 14, LockSettingsService then calls keystore2's
  `onUserLskfRemoved`, which deletes the user's auth-bound keys
  (`unbind_auth_bound_keys_for_user` in [`maintenance.rs`][ks-maintenance]). Keys not bound to
  authentication stay. It is a side effect of removing the lock, not a reset of credentials.
- **`pm clear <package>` (`clear_container` with `pm_clear: true`).** This erases one app's data,
  its keystore namespace included (above), not a device-wide credential store.

**What simee does.** `reset_keychain` stays iOS-only. On an Android device it fails with
"isn't available on Android devices (it works on iOS)" followed by the reason above
(`android_notify::UNSUPPORTED_WHY`).

## To verify on a device

Run these on a booted google_apis AVD (Apple silicon, or software mode):

- **`push`.** Push `{"data": {"k": "v"}}` to a debug app that uses Firebase Messaging, and check
  that `onMessageReceived` logs `k=v`. With the app in the background, a push with
  `{"notification": {"title": "t", "body": "b"}}` should show in the tray under the app.
- **`push` without root.** On a google_apis_playstore image (no `su`), `push` should report that it
  needs root.
- **`notify`.** `notify {"name": "<action>", "bundle_id": "<pkg>"}` should reach a manifest
  receiver. `notify` of `android.intent.action.BOOT_COMPLETED` should be refused.
- **`biometric`.** Enroll finger 1 in Settings, using `biometric match` for each touch. Then
  `biometric match` should unlock a fingerprint-locked screen, and `biometric nomatch` should show
  "Not recognized". Repeat on the stock emulator.

## Sources

[am-perm]: https://cs.android.com/android/platform/superproject/main/+/main:frameworks/base/core/java/android/app/ActivityManager.java
[notif-shell]: https://cs.android.com/android/platform/superproject/main/+/main:frameworks/base/services/core/java/com/android/server/notification/NotificationShellCmd.java
[fcm-constants]: https://github.com/firebase/firebase-android-sdk/blob/main/firebase-messaging/src/main/java/com/google/firebase/messaging/Constants.java
[fcm-params]: https://github.com/firebase/firebase-android-sdk/blob/main/firebase-messaging/src/main/java/com/google/firebase/messaging/NotificationParams.java
[fcm-processor]: https://github.com/firebase/firebase-android-sdk/blob/main/firebase-messaging/src/main/java/com/google/firebase/messaging/FcmBroadcastProcessor.java
[bg-broadcasts]: https://developer.android.com/about/versions/oreo/background#broadcasts
[receiver-exported]: https://developer.android.com/guide/topics/manifest/receiver-element#exported
[a14-receivers]: https://developer.android.com/about/versions/14/behavior-changes-14#runtime-receivers-exported
[locksettings]: https://cs.android.com/android/platform/superproject/main/+/main:frameworks/base/services/core/java/com/android/server/locksettings/LockSettingsShellCommand.java
[virtual-hal]: https://cs.android.com/android/platform/superproject/main/+/main:hardware/interfaces/biometrics/fingerprint/aidl/default/README.md
[ks-maintenance]: https://cs.android.com/android/platform/superproject/main/+/main:system/security/keystore2/src/maintenance.rs
[ks-cli]: https://cs.android.com/search?q=keystore_cli_v2&ss=android%2Fplatform%2Fsuperproject%2Fmain

- AOSP `ActivityManager.checkComponentPermission`: [ActivityManager.java][am-perm].
- `cmd notification post`: [NotificationShellCmd.java][notif-shell].
- FCM wire keys and the notification marker: firebase-android-sdk [Constants.java][fcm-constants],
  [NotificationParams.java][fcm-params], [FcmBroadcastProcessor.java][fcm-processor].
- Broadcast delivery rules: [background broadcast limits (8.0)][bg-broadcasts],
  [`<receiver android:exported>`][receiver-exported],
  [runtime-registered receivers must say whether they are exported (14)][a14-receivers].
- Screen lock from the shell: [LockSettingsShellCommand.java][locksettings].
- Virtual fingerprint HAL: [README.md][virtual-hal].
- keystore2: [maintenance.rs][ks-maintenance]; `keystore_cli_v2`: [code search][ks-cli].
