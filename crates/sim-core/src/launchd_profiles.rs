//! The launchd jobs a farm device runs without, as data (#199).
//!
//! A profile is a table: each [`Job`] names one launchd label in the device's `system` domain,
//! what it does and why a test device runs without it, what turning it off was measured to save,
//! and what stops working without it (the "safe" note). [`KEPT`] is the other side: jobs that
//! something simee or AutoMobile uses needs, which no profile may turn off. [`check`] rejects a
//! table entry that isn't an Apple launchd label, one listed twice, one on the keep list, and one
//! with no rationale or no safety note; it runs at compile time on [`SLIM`], so a bad entry never
//! builds.
//!
//! An entry is added with evidence only: a number from `scripts/bench/ios-slim.sh`, which boots a
//! clone with one candidate job off at a time against a stock baseline (settle time, CPU after
//! settling, footprint), and a check that the app flows a test needs still work. Candidates that
//! haven't been measured yet are that script's list, not this table. docs/ios.md describes the
//! method; docs/farm.md has the profile's measured effect as a whole.

/// One launchd job a profile turns off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Job {
    /// The launchd label (`com.apple.<name>`), in the device's `system` domain.
    pub label: &'static str,
    /// What the job does, and why a test device runs without it.
    pub why: &'static str,
    /// What turning it off was measured to save, with where; empty when it hasn't been measured
    /// on its own (the profile's effect as a whole is docs/farm.md's).
    pub saves: &'static str,
    /// What stops working without it: the "safe" note.
    pub breaks: &'static str,
}

/// A job no profile turns off, and what needs it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Kept {
    pub label: &'static str,
    /// The simee methods, AutoMobile tools or app flows that need it.
    pub needed_by: &'static str,
}

/// Why [`check`] refused a table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rejected {
    /// Not a launchd label simee knows: `com.apple.` and a name of letters, digits, `.`, `_`, `-`.
    Label(&'static str),
    /// Listed twice.
    Twice(&'static str),
    /// A profile entry that is on the keep list.
    Kept(&'static str),
    /// An entry with no rationale or no safety note (a kept job with no reason).
    Undocumented(&'static str),
}

impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Label(l) => write!(f, "{l:?} isn't a launchd label simee knows (com.apple.<name>: letters, digits, `.`, `_`, `-`)"),
            Self::Twice(l) => write!(f, "{l} is listed twice"),
            Self::Kept(l) => write!(f, "{l} is on the keep list: something simee or AutoMobile uses needs it"),
            Self::Undocumented(l) => write!(f, "{l} doesn't say why it is listed or what breaks without it"),
        }
    }
}

const PREFIX: &[u8] = b"com.apple.";

const fn is_label(label: &str) -> bool {
    let b = label.as_bytes();
    if b.len() <= PREFIX.len() {
        return false;
    }
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if i < PREFIX.len() {
            if c != PREFIX[i] {
                return false;
            }
        } else if !(c.is_ascii_alphanumeric() || c == b'.' || c == b'_' || c == b'-') {
            return false;
        }
        i += 1;
    }
    true
}

const fn same(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// Whether `jobs` is a table a profile may use against the keep list `kept`: each label an Apple
/// launchd label, none twice, none kept, each with a rationale and a safety note; and `kept` well
/// formed itself. The first problem found is the error.
pub const fn check(jobs: &[Job], kept: &[Kept]) -> Result<(), Rejected> {
    let mut i = 0;
    while i < kept.len() {
        let k = kept[i];
        if !is_label(k.label) {
            return Err(Rejected::Label(k.label));
        }
        if k.needed_by.is_empty() {
            return Err(Rejected::Undocumented(k.label));
        }
        let mut j = 0;
        while j < i {
            if same(kept[j].label, k.label) {
                return Err(Rejected::Twice(k.label));
            }
            j += 1;
        }
        i += 1;
    }
    let mut i = 0;
    while i < jobs.len() {
        let job = jobs[i];
        if !is_label(job.label) {
            return Err(Rejected::Label(job.label));
        }
        if job.why.is_empty() || job.breaks.is_empty() {
            return Err(Rejected::Undocumented(job.label));
        }
        let mut j = 0;
        while j < i {
            if same(jobs[j].label, job.label) {
                return Err(Rejected::Twice(job.label));
            }
            j += 1;
        }
        let mut j = 0;
        while j < kept.len() {
            if same(kept[j].label, job.label) {
                return Err(Rejected::Kept(job.label));
            }
            j += 1;
        }
        i += 1;
    }
    Ok(())
}

/// The labels of `jobs`, in order: the list a profile disables. `N` is `jobs.len()`.
pub const fn labels<const N: usize>(jobs: &[Job]) -> [&'static str; N] {
    assert!(jobs.len() == N, "N is the table's length");
    let mut out = [""; N];
    let mut i = 0;
    while i < N {
        out[i] = jobs[i].label;
        i += 1;
    }
    out
}

const _: () = assert!(check(SLIM, KEPT).is_ok(), "the slim table fails its check: cargo test -p simee-core launchd_profiles says why");

const fn job(label: &'static str, why: &'static str, breaks: &'static str) -> Job {
    Job { label, why, saves: "", breaks }
}

const fn measured(label: &'static str, why: &'static str, saves: &'static str, breaks: &'static str) -> Job {
    Job { label, why, saves, breaks }
}

// Rationales and safety notes a group of jobs shares (docs/farm.md, "The `slim` profile").
const SIRI: &str = "Siri, dictation and suggestions: nothing a test drives";
const NO_SIRI: &str = "Siri, dictation, Siri Suggestions and Spotlight's suggestions";
const ANALYSIS: &str = "photo and media analysis (faces, memories, search indexing)";
const NO_ANALYSIS: &str = "Photos' people, memories and search; the photo library and the photos picker still work (assetsd is kept)";
const HEALTH: &str = "Health, Home and fitness: no accessories or workouts on a test device";
const STORE: &str = "Store, iCloud sync, mail, news, weather, wallet, games, finance: no Apple Account on a test device";
const NO_ACCOUNT: &str = "the Store's and Apple Music's account features, which need an Apple Account the test device doesn't have";
const SOCIAL: &str = "location history, maps analytics, people and social suggestions";
const NO_SOCIAL: &str = "significant locations, Maps analytics and people suggestions; `location` still works (locationd is kept)";
const SCREEN_TIME: &str = "Screen Time";
const NO_SCREEN_TIME: &str = "Screen Time reports and limits, and the FamilyControls API";
const WATCH: &str = "paired-watch and Find My companions: no watch is paired with a simulator";
const NANO: &str = "nanoregistrylaunchd enables the 17 paired-watch daemons each time it runs, one to three minutes after a boot, \
                    which started these on every boot (docs/farm.md)";
const NO_WATCH: &str = "paired-watch features, which a simulator has no watch for";
const PLATFORM: &str = "experiments, ML hosting and asset prefetch that Apple's servers drive";
const NO_PLATFORM: &str = "server-driven experiments and model downloads; an app's own Core ML models still load";
const FITNESS: &str = "Fitness: activity sharing, awards, Fitness+ (healthd is off already)";
const LATE12: &str = "with the 11 others added with it: a clone settled in 20.1 s against 33.2 s, 82 against 112 CPU-s to settle \
                      (iOS 27.2, foreground; docs/farm.md)";

/// The `slim` profile: daemons that burn CPU or memory on an idle booted device and that no
/// AutoMobile tool or simee method needs (measured on iOS 27.2 and 18.6, iPhone 17; docs/farm.md).
pub const SLIM: &[Job] = &[
    job(
        "com.apple.PosterBoard",
        "lock and home screen wallpapers: PosterBoard and the ~15 poster extensions it keeps alive (~30-45 MB footprint each)",
        "wallpaper and lock-screen editing; the home screen still draws",
    ),
    job("com.apple.chronod", "widgets' timelines", "home and lock screen widgets don't refresh"),
    job("com.apple.assistantd", SIRI, NO_SIRI),
    job("com.apple.assistant_cdmd", SIRI, NO_SIRI),
    job("com.apple.siriactionsd", SIRI, NO_SIRI),
    job("com.apple.siriinferenced", SIRI, NO_SIRI),
    job("com.apple.siriknowledged", SIRI, NO_SIRI),
    job("com.apple.sirittsd", SIRI, NO_SIRI),
    job("com.apple.siri.context.service", SIRI, NO_SIRI),
    job("com.apple.corespeechd", SIRI, NO_SIRI),
    job("com.apple.voicebankingd", SIRI, "Personal Voice"),
    job("com.apple.suggestd", SIRI, NO_SIRI),
    job("com.apple.textunderstandingd", SIRI, NO_SIRI),
    job("com.apple.generativeexperiencesd", SIRI, "Apple Intelligence features"),
    job("com.apple.spotlightknowledged.updater", SIRI, NO_SIRI),
    job("com.apple.parsecd", SIRI, NO_SIRI),
    job("com.apple.photoanalysisd", ANALYSIS, NO_ANALYSIS),
    job("com.apple.mediaanalysisd", ANALYSIS, NO_ANALYSIS),
    // Not sleepd: see KEPT.
    job("com.apple.healthd", HEALTH, "HealthKit: an app's HealthKit queries get no data"),
    job("com.apple.homed", HEALTH, "HomeKit"),
    job("com.apple.homeeventsd", HEALTH, "HomeKit"),
    job("com.apple.fitnesscoachingd", HEALTH, "Fitness coaching"),
    job("com.apple.fitnessintelligenced", HEALTH, "Fitness coaching"),
    job("com.apple.appstored", STORE, "App Store downloads and updates; `install` uses installd, which is kept"),
    job("com.apple.itunesstored", STORE, NO_ACCOUNT),
    job("com.apple.itunescloudd", STORE, NO_ACCOUNT),
    job("com.apple.amsengagementd", STORE, NO_ACCOUNT),
    job("com.apple.ap.adprivacyd", STORE, "App Store ads attribution"),
    job("com.apple.ap.promotedcontentd", STORE, "App Store ads"),
    job("com.apple.cloudd", STORE, "CloudKit sync"),
    job("com.apple.email.maild", STORE, "the Mail app's accounts"),
    job("com.apple.icloudmailagent", STORE, "iCloud Mail"),
    job("com.apple.newsd", STORE, "News"),
    job("com.apple.weatherd", STORE, "Weather and WeatherKit"),
    job("com.apple.passd", STORE, "Wallet and PassKit"),
    job("com.apple.financed", STORE, "FinanceKit"),
    job("com.apple.gamed", STORE, "Game Center"),
    job("com.apple.SafariBookmarksSyncAgent", STORE, "Safari bookmarks sync; Safari and `open_url` work (webbookmarksd is kept)"),
    job("com.apple.followupd", STORE, "Settings' account follow-ups"),
    job("com.apple.remindd", STORE, "Reminders sync"),
    job("com.apple.routined", SOCIAL, NO_SOCIAL),
    job("com.apple.geoanalyticsd", SOCIAL, NO_SOCIAL),
    job("com.apple.navd", SOCIAL, NO_SOCIAL),
    job("com.apple.peopled", SOCIAL, NO_SOCIAL),
    job("com.apple.sociallayerd", SOCIAL, "Shared with You"),
    job("com.apple.ScreenTimeAgent", SCREEN_TIME, NO_SCREEN_TIME),
    job("com.apple.UsageTrackingAgent", SCREEN_TIME, NO_SCREEN_TIME),
    job("com.apple.FamilyControlsAgent", SCREEN_TIME, NO_SCREEN_TIME),
    job("com.apple.nanoregistrylaunchd", NANO, NO_WATCH),
    job("com.apple.addressbooksyncd", NANO, NO_WATCH),
    job("com.apple.bulletindistributord", NANO, NO_WATCH),
    job("com.apple.pairedsyncd", NANO, NO_WATCH),
    job("com.apple.NPKCompanionAgent", WATCH, NO_WATCH),
    job("com.apple.companionappd", WATCH, NO_WATCH),
    job("com.apple.wcd", WATCH, "WatchConnectivity (an app's WCSession never activates)"),
    job("com.apple.brook.brookcompaniond", WATCH, NO_WATCH),
    job("com.apple.findmy.findmylocated", WATCH, "Find My location sharing"),
    job("com.apple.triald", PLATFORM, NO_PLATFORM),
    job("com.apple.mlhostd", PLATFORM, NO_PLATFORM),
    job("com.apple.jetpackassetd", PLATFORM, NO_PLATFORM),
    job("com.apple.replicatord", PLATFORM, NO_PLATFORM),
    job("com.apple.diagnosticextensionsd", PLATFORM, "diagnostic extensions for feedback reports; `logs` reads diagnosticd, which is kept"),
    measured(
        "com.apple.linkd",
        "App Intents: it runs donotdisturbd's Focus-filter first-launch task, which starts the Calendar, Safari and Messages intents \
         extensions and Calendar itself 13-38 s after the boot",
        "~8 CPU-s, the burst that held the settle window open (iOS 27.2, docs/farm.md)",
        "App Intents and App Shortcuts, Focus filters",
    ),
    measured(
        "com.apple.nanotimekitcompaniond",
        "watch faces, with no watch paired",
        "5.0 CPU-s in a slim clone's first 120 s on iOS 27.2, 1.6 on 18.6 (docs/farm.md)",
        NO_WATCH,
    ),
    job("com.apple.nanoprefsyncd.2", "watch settings sync, with no watch paired", NO_WATCH),
    measured(
        "com.apple.dataaccess.dataaccessd",
        "Exchange, CalDAV and CardDAV account sync, with no such accounts",
        "4.7 CPU-s in a slim clone's first 120 s on iOS 27.2, 0.4 on 18.6 (docs/farm.md)",
        "Exchange, CalDAV and CardDAV accounts",
    ),
    measured(
        "com.apple.Safari.passwordbreachd",
        "Safari's breached-password lookups",
        "1.42 CPU-s in a stock boot's burst 5-7 minutes after settling (iOS 27.2, #199)",
        "Safari's compromised-password warnings",
    ),
    job("com.apple.activitysharingd", FITNESS, "Fitness activity sharing"),
    job("com.apple.activityawardsd", FITNESS, "Fitness awards"),
    job("com.apple.fitcore", FITNESS, "Fitness+"),
    measured(
        "com.apple.StatusKitAgent",
        "Focus status sharing",
        "2.8 CPU-s in a slim clone's first 120 s on iOS 18.6, most of it after 20 s (docs/farm.md)",
        "Focus status sharing; Focus itself stays (donotdisturbd is kept)",
    ),
    measured("com.apple.avatarsd", "Memoji", LATE12, "Memoji"),
    measured("com.apple.modelcatalogd", "Apple Intelligence's model catalog", LATE12, "Apple Intelligence model downloads"),
    measured("com.apple.familynotification", "Family Sharing alerts", LATE12, "Family Sharing alerts"),
];

/// Jobs no profile turns off: what simee's methods reach in the device (pinned per method in
/// `sim-api`'s tests), what AutoMobile reads the screen through, and what common app flows need
/// though a slim clone still spends CPU on them (docs/farm.md).
pub const KEPT: &[Kept] = &[
    Kept {
        label: "com.apple.SpringBoard",
        needed_by: "the home screen: `launch`, `push`, `open_url`, `status_bar`, `appearance`, `carplay`",
    },
    Kept { label: "com.apple.backboardd", needed_by: "input and the display: `type`, `hardware_keyboard`, `appearance`" },
    Kept { label: "com.apple.InputUI", needed_by: "the keyboard: `type`, `hardware_keyboard`, `keyboard_language`" },
    Kept { label: "com.apple.TextInput.kbd", needed_by: "the keyboard: `type`, `keyboard_language`" },
    Kept { label: "com.apple.AccessibilityUIServer", needed_by: "AutoMobile reads the screen through accessibility" },
    Kept { label: "com.apple.mobile.installd", needed_by: "`install`, `apps`" },
    Kept { label: "com.apple.lsd", needed_by: "`install`, `apps`, `open_url`" },
    Kept { label: "com.apple.runningboardd", needed_by: "`launch`" },
    Kept { label: "com.apple.tccd", needed_by: "`privacy`" },
    Kept { label: "com.apple.usernotificationsd", needed_by: "`push`" },
    Kept { label: "com.apple.apsd", needed_by: "`push`" },
    Kept { label: "com.apple.locationd", needed_by: "`location`" },
    Kept { label: "com.apple.sharingd", needed_by: "kept on purpose with the app-flow daemons (docs/farm.md)" },
    Kept { label: "com.apple.WebBookmarks.webbookmarksd", needed_by: "Safari: `open_url`" },
    Kept { label: "com.apple.assetsd", needed_by: "the photo library and the photos picker" },
    Kept { label: "com.apple.syslogd", needed_by: "`logs`" },
    Kept { label: "com.apple.logd", needed_by: "`logs`" },
    Kept { label: "com.apple.diagnosticd", needed_by: "the log stream `logs` reads" },
    Kept { label: "com.apple.configd_sim", needed_by: "networking: `network_shape`" },
    Kept { label: "com.apple.nsurlsessiond", needed_by: "networking: `network_shape`" },
    Kept { label: "com.apple.carkitd", needed_by: "`carplay`" },
    Kept { label: "com.apple.CarPlayApp", needed_by: "`carplay`" },
    Kept { label: "com.apple.CoreAuthentication.daemon", needed_by: "`biometric`" },
    Kept { label: "com.apple.systemstatusd", needed_by: "`status_bar`, `clear_status_bar`" },
    Kept { label: "com.apple.notifyd", needed_by: "`notify`, `memory_warning`, `biometric`" },
    Kept { label: "com.apple.securityd", needed_by: "`reset_keychain`" },
    Kept {
        label: "com.apple.cfprefsd.xpc.daemon",
        needed_by: "`defaults_read`, `defaults_write`, `keyboard_language`, `appearance`, `content_size`",
    },
    Kept { label: "com.apple.audio.systemsoundserver-simd", needed_by: "`audio_volume`" },
    Kept {
        label: "com.apple.sleepd",
        needed_by: "settling: without it SpringBoard and mobiletimerd retry its mach service in a tight loop on every later boot \
                    (~350 % of a core) and the device never settles (iOS 27.2, docs/farm.md)",
    },
    Kept { label: "com.apple.donotdisturbd", needed_by: "notification delivery and Focus" },
    Kept { label: "com.apple.liveactivitiesd", needed_by: "Live Activities" },
    Kept {
        label: "com.apple.AuthenticationServicesCore.AuthenticationServicesAgent",
        needed_by: "password AutoFill and passkeys (#223's keep-list idea would let a farm drop it)",
    },
    Kept { label: "com.apple.telephonyutilities.callservicesd", needed_by: "CallKit" },
    Kept { label: "com.apple.calaccessd", needed_by: "EventKit" },
    Kept { label: "com.apple.mobileassetd", needed_by: "keyboard and font assets" },
    Kept { label: "com.apple.identityservicesd", needed_by: "kept on purpose with the app-flow daemons (docs/farm.md)" },
];

#[cfg(test)]
mod tests {
    use super::*;

    fn job(label: &'static str) -> Job {
        Job { label, why: "why", saves: "", breaks: "nothing a test uses" }
    }

    #[test]
    fn the_slim_table_passes_its_own_check() {
        assert_eq!(check(SLIM, KEPT), Ok(()));
        assert_eq!(SLIM.len(), 74);
        // Every entry says what it does and what goes without it; a measured saving is optional
        // (scripts/bench/ios-slim.sh measures one job at a time).
        assert!(SLIM.iter().all(|j| !j.why.is_empty() && !j.breaks.is_empty()));
        assert!(KEPT.iter().all(|k| !k.needed_by.is_empty()));
    }

    #[test]
    fn the_labels_are_the_tables_in_order() {
        let labels: [&str; SLIM.len()] = labels(SLIM);
        assert!(labels.iter().zip(SLIM).all(|(l, j)| *l == j.label));
        assert_eq!((labels[0], labels[73]), ("com.apple.PosterBoard", "com.apple.familynotification"));
    }

    #[test]
    fn an_unknown_or_unsafe_entry_is_rejected() {
        let kept = [Kept { label: "com.apple.sleepd", needed_by: "settling" }];
        assert_eq!(check(&[job("com.apple.a"), job("com.apple.b")], &kept), Ok(()));
        // Not an Apple launchd label: a typo, another vendor's, a bare prefix, a space or a quote.
        for bad in ["com.aple.a", "org.example.a", "com.apple.", "com.apple.a b", "com.apple.a\"", "", "a"] {
            assert_eq!(check(&[job(bad)], &kept), Err(Rejected::Label(bad)), "{bad:?}");
        }
        assert_eq!(check(&[job("com.apple.a"), job("com.apple.a")], &kept), Err(Rejected::Twice("com.apple.a")));
        assert_eq!(check(&[job("com.apple.sleepd")], &kept), Err(Rejected::Kept("com.apple.sleepd")));
        let silent = Job { why: "", ..job("com.apple.a") };
        assert_eq!(check(&[silent], &kept), Err(Rejected::Undocumented("com.apple.a")));
        let unsafe_note = Job { breaks: "", ..job("com.apple.a") };
        assert_eq!(check(&[unsafe_note], &kept), Err(Rejected::Undocumented("com.apple.a")));
        // The keep list is checked too.
        let kept_twice = [kept[0], kept[0]];
        assert_eq!(check(&[], &kept_twice), Err(Rejected::Twice("com.apple.sleepd")));
        assert_eq!(check(&[], &[Kept { label: "sleepd", needed_by: "x" }]), Err(Rejected::Label("sleepd")));
        assert_eq!(check(&[], &[Kept { label: "com.apple.sleepd", needed_by: "" }]), Err(Rejected::Undocumented("com.apple.sleepd")));
        assert_eq!(
            Rejected::Kept("com.apple.sleepd").to_string(),
            "com.apple.sleepd is on the keep list: something simee or AutoMobile uses needs it"
        );
    }

    #[test]
    fn the_jobs_this_issue_keeps_stay_kept() {
        // Heavy, but app flows reach them (docs/farm.md); sleepd's absence spins SpringBoard.
        for label in [
            "com.apple.sleepd",
            "com.apple.AuthenticationServicesCore.AuthenticationServicesAgent",
            "com.apple.diagnosticd",
            "com.apple.donotdisturbd",
            "com.apple.carkitd",
        ] {
            assert!(KEPT.iter().any(|k| k.label == label), "{label}");
            assert!(SLIM.iter().all(|j| j.label != label), "{label}");
        }
    }
}
