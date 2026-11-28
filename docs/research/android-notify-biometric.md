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
| `cmd notification post [-t title] <tag> <text>` | Nothing. The notification is posted by `com.android.shell` and shown under the Shell app, not the target app. | No: it tests the system tray, not the app. |
| FCM through Google's servers | A real message. But it needs Play services on the image, a server key, and the app's registration token, which only the app itself can read. | Not host-side. |
| `am broadcast -n <pkg>/<FCM receiver> -a com.google.android.c2dm.intent.RECEIVE --es k v ...` as root | The Firebase SDK's `FirebaseInstanceIdReceiver` gets the same broadcast Play services sends for each message. `FirebaseMessagingService.onMessageReceived` runs with the data. A notification message is shown by the SDK as the app's own while the app is in the background. | Yes. |

The broadcast is the faithful route. Play services delivers every FCM message to an app as the
`com.google.android.c2dm.intent.RECEIVE` broadcast to the receiver that the Firebase SDK puts in the
app's manifest. That receiver is protected by `com.google.android.c2dm.permission.SEND`, which only
Play services holds, so the shell user (uid 2000) can't send to it. Root can:
`ActivityManager.checkComponentPermission` grants every permission to uid 0 and to the system uid.
On a google_apis image (the default image here) `su 0` is there, and adbd can be root.

The extras are FCM's wire format:

- Each `data` pair is a string extra under its own key. FCM data values are always strings, so a
  JSON number or boolean is sent as its text.
- Each `notification` field is sent as `gcm.notification.<field>` (`title`, `body`,
  `channel_id`, `image` and so on). `gcm.n.e=1` is added too: the SDK's `NotificationParams` reads
  that marker as "this message carries a notification".
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
- Message priority and TTL, which affect only how Google's servers queue a message, not what the
  app receives.

## notify

**iOS.** `notify` posts a Darwin notification (`notify_post`) by name, system-wide, optionally after
setting its 64-bit state (`notify_set_state`). Any process that registered for the name is woken.
The notification carries no payload beyond that state.

**Android.** The closest analogue is a broadcast intent: `am broadcast -a <action>`. The
differences between the two:

- A broadcast carries typed extras instead of one integer state. `state` is refused on Android, and
  extras take its place.
- Since Android 8, a receiver declared in an app's manifest hears an implicit broadcast only when
  the broadcast names its package (`-p`). Receivers registered at runtime hear it either way. So
  `bundle_id` matters on Android in a way it has no counterpart for on iOS.
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
  `biometric` `match` with the finger's id. No host-side switch flips the enrolled state.
- `face: true` is refused, because the emulator has no face sensor.

## reset_keychain

**iOS.** `reset_keychain` deletes every keychain item on the device: one system-wide credential
store.

**Android.** Android has no device-wide keychain. The candidates, and why none of them is
`reset_keychain`:

- **App KeyStore keys.** Android Keystore keys belong to one app's uid. Only that app can delete
  them, or uninstalling it does. Deleting keystore2's database rows from the host, as root, would
  corrupt the daemon's state. It is not an operation the platform offers.
- **`locksettings clear --old <credential>`.** This removes the screen lock. It also invalidates
  keys bound to user authentication, but it doesn't delete keys, and it needs the current
  credential.
- **`pm clear <package>` (`clear_container` with `pm_clear: true`).** This erases one app's data,
  not a credential store.

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
