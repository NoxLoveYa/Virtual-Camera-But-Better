//! The record of the data folders that the helper moved aside (`%ProgramData%\BetterCam.old-*`).
//!
//! The helper moves `BetterCam` out of the way when it is a link, has the wrong owner, and so on, and deletes the old
//! copy at once. A copy that cannot be deleted then (a file in it is open) is deleted by a later run, which acts on
//! **exactly the entries on this record and on nothing else**: a standard user can make entries in `%ProgramData%`
//! under any name, so a name pattern proves nothing, and neither does an owner (an administrator-owned folder that a
//! user may rename into `%ProgramData%` has a trusted owner).
//!
//! # Format
//!
//! `HKLM\SOFTWARE\BetterCam\Vcam`, value `MovedAside`, a `REG_MULTI_SZ` (only administrators can write the key). One
//! string per folder:
//!
//! ```text
//! <name>|<volume serial number: 16 hex digits>|<file id: 32 hex digits>
//! ```
//!
//! The name is a single name in `%ProgramData%` (see [`is_aside_name`]); the two numbers are the identity of the
//! folder, read on the handle of the folder that was renamed to that name (`FILE_ID_INFO`: the file id is unique on
//! the volume and carries a sequence number, so a folder that is deleted and another one renamed into the name has
//! another one). The sweep deletes a recorded folder only when its name, its identity, "not a link" and "owner
//! trusted" all hold on the handle it deletes through; anything else is left exactly as it is and dropped.
//!
//! # What is never done with a record that is not understood
//!
//! * A value that is **not there** is an empty record. A value that is there and **cannot be read** (another type,
//!   too long, more than [`MAX_ENTRIES`] strings, access denied) is [`Record::Unreadable`]: nothing is acted on and
//!   it is never overwritten or deleted (it is logged and left).
//! * A string that is not an entry (wrong number of parts, a number that is not hex or has the wrong length, a name
//!   or a string longer than [`MAX_ENTRY_CHARS`], an all-zero file id) is dropped. It is gone from the record the
//!   next time the record is written, and no folder is touched because of it.
//!
//! # Order
//!
//! A run looks at the first [`crate::datadir::MAX_ASIDES_EXAMINED`] entries only. The ones that were looked at and
//! stay (a file in the folder is in use, or the folder could not be looked at) go to the **end** of the record
//! ([`finish_sweep`]), so that over successive runs every entry is reached.

use std::cell::RefCell;

use vcam_protocol::secure::FileIdentity;

use crate::datadir::AsideSweep;
use crate::log::Log;
use crate::machine::{AsideStore, Machine, Stored};
use crate::util::Res;

/// Most entries a record can hold. A record with more strings is not something this helper wrote: it is
/// [`Record::Unreadable`], and nothing is added to it.
pub const MAX_ENTRIES: usize = 256;

/// Longest string of an entry, in characters (the name is at most [`MAX_NAME_CHARS`]).
pub const MAX_ENTRY_CHARS: usize = 200;

/// Longest name of an entry.
pub const MAX_NAME_CHARS: usize = 120;

/// A data folder that was moved aside: its name in `%ProgramData%` and who it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AsideEntry {
    pub name: String,
    pub identity: FileIdentity,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[allow(clippy::manual_is_multiple_of)] // MSRV 1.87: unsigned `is_multiple_of` is stable only from 1.90
fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

impl AsideEntry {
    /// The string stored for this entry.
    pub fn encode(&self) -> String {
        format!(
            "{}|{:016x}|{}",
            self.name,
            self.identity.volume,
            hex(&self.identity.id)
        )
    }

    /// The entry a stored string says, or `None` for anything that is not exactly an entry. The name is not judged
    /// here beyond its length ([`is_aside_name`] is, by the sweep, which then forgets it).
    pub fn parse(s: &str) -> Option<AsideEntry> {
        if s.is_empty() || s.chars().count() > MAX_ENTRY_CHARS {
            return None;
        }
        let mut parts = s.split('|');
        let (name, volume, id) = (parts.next()?, parts.next()?, parts.next()?);
        if parts.next().is_some() || name.is_empty() || name.chars().count() > MAX_NAME_CHARS {
            return None;
        }
        if volume.len() != 16 || id.len() != 32 {
            return None;
        }
        let volume = u64::from_str_radix(volume, 16).ok()?;
        let id: [u8; 16] = unhex(id)?.try_into().ok()?;
        if id == [0u8; 16] {
            return None;
        }
        Some(AsideEntry {
            name: name.to_string(),
            identity: FileIdentity { volume, id },
        })
    }
}

/// A name that can be one of the helper's old data folders: `BetterCam.old-...`, one path component made of letters,
/// digits, `.`, `-` and `_` only, not ending in `.` (Windows would drop it: the name would open another entry), at
/// most [`MAX_NAME_CHARS`] long. The record is only written by administrators; this is for a record that is damaged.
pub fn is_aside_name(name: &str) -> bool {
    let prefix = format!("{}.old-", vcam_protocol::DATA_DIR_NAME).to_lowercase();
    name.to_lowercase().starts_with(&prefix)
        && name.chars().count() <= MAX_NAME_CHARS
        && !name.ends_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// The record as it is now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Record {
    /// The entries, in order (duplicates of a name removed), and how many stored strings were not entries.
    Entries {
        entries: Vec<AsideEntry>,
        malformed: usize,
    },
    /// The value exists and is not understood (the text says why): not acted on, not overwritten.
    Unreadable(String),
}

/// The record from what the store holds.
pub fn from_stored(stored: Stored) -> Record {
    match stored {
        Stored::Missing => Record::Entries {
            entries: Vec::new(),
            malformed: 0,
        },
        Stored::Unreadable(why) => Record::Unreadable(why),
        Stored::Strings(strings) => {
            if strings.len() > MAX_ENTRIES {
                return Record::Unreadable(format!(
                    "it holds {} strings, more than the {MAX_ENTRIES} the helper ever writes",
                    strings.len()
                ));
            }
            let mut entries: Vec<AsideEntry> = Vec::new();
            let mut malformed = 0;
            for s in &strings {
                match AsideEntry::parse(s) {
                    Some(e) if entries.iter().any(|x| x.name == e.name) => {
                        // the same name twice: the first one is the entry (the next write drops the other)
                        malformed += 1;
                    }
                    Some(e) => entries.push(e),
                    None => malformed += 1,
                }
            }
            Record::Entries { entries, malformed }
        }
    }
}

/// Read the record.
pub fn load<S: AsideStore + ?Sized>(store: &mut S) -> Record {
    from_stored(store.aside_record())
}

/// The record for a sweep: its entries, with what was found said in the log. A record that cannot be read is left as
/// it is and gives `None` (nothing is deleted this time, and nothing is written to it afterwards).
pub fn load_logged<S: AsideStore + ?Sized>(
    store: &mut S,
    log: &mut dyn Log,
) -> Option<Vec<AsideEntry>> {
    match load(store) {
        Record::Entries { entries, malformed } => {
            if malformed > 0 {
                log.line(&format!(
                    "{malformed} string(s) of the record of old data folders are not entries: ignored, and dropped from the record the next time it is written"
                ));
            }
            Some(entries)
        }
        Record::Unreadable(why) => {
            log.line(&format!(
                "the record of old data folders cannot be read ({why}): left as it is, and no old data folder is deleted this time"
            ));
            None
        }
    }
}

fn write<S: AsideStore + ?Sized>(store: &mut S, entries: &[AsideEntry]) -> Res<()> {
    let strings: Vec<String> = entries.iter().map(AsideEntry::encode).collect();
    store.write_aside_record(&strings)
}

fn unreadable(why: &str) -> String {
    format!("the record of old data folders cannot be read ({why}); it is left as it is and not overwritten")
}

/// Put `entry` on the record (a name that is there already is replaced). An unreadable record is not overwritten: the
/// error says so and nothing is written.
pub fn add<S: AsideStore + ?Sized>(store: &mut S, entry: &AsideEntry) -> Res<()> {
    match load(store) {
        Record::Unreadable(why) => Err(unreadable(&why)),
        Record::Entries { mut entries, .. } => {
            entries.retain(|e| e.name != entry.name);
            if entries.len() >= MAX_ENTRIES {
                return Err(format!(
                    "the record of old data folders is full ({MAX_ENTRIES} entries)"
                ));
            }
            entries.push(entry.clone());
            write(store, &entries)
        }
    }
}

/// Take the entry `name` off the record (nothing is written when it is not there). An unreadable record is not
/// touched.
pub fn remove<S: AsideStore + ?Sized>(store: &mut S, name: &str) -> Res<()> {
    match load(store) {
        Record::Unreadable(why) => Err(unreadable(&why)),
        Record::Entries { entries, malformed } => {
            if !entries.iter().any(|e| e.name == name) {
                return Ok(());
            }
            let rest: Vec<AsideEntry> = entries.into_iter().filter(|e| e.name != name).collect();
            let _ = malformed; // dropped by this write
            write(store, &rest)
        }
    }
}

/// Apply what a sweep found: the entries it is done with are taken off, and the entries it looked at and that stay go
/// to the end (so that the next sweep, which looks at the first few only, reaches the others). The record is read
/// again first; one that cannot be read is not touched. Nothing is written when nothing changes.
pub fn finish_sweep<S: AsideStore + ?Sized>(store: &mut S, sweep: &AsideSweep) -> Res<()> {
    let (entries, malformed) = match load(store) {
        Record::Unreadable(why) => return Err(unreadable(&why)),
        Record::Entries { entries, malformed } => (entries, malformed),
    };
    let gone = |e: &&AsideEntry| sweep.forget.contains(&e.name);
    let moves = |e: &&AsideEntry| sweep.kept.contains(&e.name);
    let mut next: Vec<AsideEntry> = entries
        .iter()
        .filter(|e| !gone(e) && !moves(e))
        .cloned()
        .collect();
    next.extend(entries.iter().filter(|e| !gone(e) && moves(e)).cloned());
    if next == entries && malformed == 0 {
        return Ok(());
    }
    write(store, &next)
}

/// What the data folder preparation needs from the record: put an entry on it before a folder is moved, and take it
/// off again when the move did not happen.
pub trait Ledger {
    fn add(&mut self, entry: &AsideEntry) -> Res<()>;
    fn remove(&mut self, name: &str) -> Res<()>;
}

/// A ledger that writes nothing (a preparation that nobody records).
pub struct NoLedger;

impl Ledger for NoLedger {
    fn add(&mut self, _: &AsideEntry) -> Res<()> {
        Ok(())
    }
    fn remove(&mut self, _: &str) -> Res<()> {
        Ok(())
    }
}

/// The ledger of a machine that is also used for other things meanwhile (the camera service is stopped through the
/// same machine while the data folder is prepared), hence the `RefCell`: the calls never overlap.
pub struct MachineLedger<'a, 'b>(pub &'a RefCell<&'b mut dyn Machine>);

impl Ledger for MachineLedger<'_, '_> {
    fn add(&mut self, entry: &AsideEntry) -> Res<()> {
        add(&mut **self.0.borrow_mut(), entry)
    }
    fn remove(&mut self, name: &str) -> Res<()> {
        remove(&mut **self.0.borrow_mut(), name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::VecLog;
    use crate::testkit::MemStore as Mem;

    fn id(n: u8) -> FileIdentity {
        FileIdentity {
            volume: 0xabcd_0000_0000_0001,
            id: [n; 16],
        }
    }

    fn entry(name: &str, n: u8) -> AsideEntry {
        AsideEntry {
            name: name.to_string(),
            identity: id(n),
        }
    }

    #[test]
    fn an_entry_round_trips_and_has_the_documented_shape() {
        let e = entry("BetterCam.old-1700000000000-0", 0xab);
        let s = e.encode();
        assert_eq!(
            s,
            "BetterCam.old-1700000000000-0|abcd000000000001|abababababababababababababababab"
        );
        assert_eq!(AsideEntry::parse(&s), Some(e));
        // upper case digits are digits too
        assert!(
            AsideEntry::parse(&s.to_uppercase().replace("BETTERCAM.OLD", "BetterCam.old"))
                .is_some()
        );
    }

    #[test]
    fn what_is_not_exactly_an_entry_is_dropped_and_never_acted_on() {
        let good = entry("BetterCam.old-1-0", 7).encode();
        let vol = "abcd000000000001";
        let id = "07070707070707070707070707070707";
        let long_name = format!("BetterCam.old-{}", "x".repeat(MAX_NAME_CHARS));
        for bad in [
            String::new(),
            "BetterCam.old-1-0".to_string(),    // no identity
            format!("BetterCam.old-1-0|{vol}"), // no file id
            format!("BetterCam.old-1-0|{vol}|{id}|extra"), // four parts
            format!("|{vol}|{id}"),             // no name
            format!("BetterCam.old-1-0|{}|{id}", &vol[1..]), // short volume
            format!("BetterCam.old-1-0|{vol}|{}", &id[2..]), // short id
            format!("BetterCam.old-1-0|{vol}0|{id}"), // long volume
            format!("BetterCam.old-1-0|{}g|{id}", &vol[1..]), // not hex
            format!("BetterCam.old-1-0|{vol}|{}", "z".repeat(32)), // not hex
            format!("BetterCam.old-1-0|{vol}|{}", "0".repeat(32)), // an all-zero id is no id
            format!("{long_name}|{vol}|{id}"),  // name too long
            format!("{good}{}", "x".repeat(MAX_ENTRY_CHARS)), // string too long
            // a record of the old format (names only) is not an entry either
            "BetterCam.old-5-0".to_string(),
        ] {
            assert_eq!(AsideEntry::parse(&bad), None, "{bad:?}");
        }
        // the same, in a stored list: the good entries are kept, the others counted
        let stored = Stored::Strings(vec![
            "garbage".into(),
            good.clone(),
            format!("BetterCam.old-1-0|{vol}|{id}|extra"),
            good.clone(), // a name twice: counted, once is kept
        ]);
        let Record::Entries { entries, malformed } = from_stored(stored) else {
            panic!("a list with some good entries is readable")
        };
        assert_eq!(entries, [entry("BetterCam.old-1-0", 7)]);
        assert_eq!(malformed, 3);
    }

    #[test]
    fn names_are_one_plain_component_and_nothing_that_windows_would_change() {
        for ok in [
            "BetterCam.old-1700000000000-0",
            "bettercam.OLD-1-7",
            "BetterCam.old-a_b.c",
        ] {
            assert!(is_aside_name(ok), "{ok}");
        }
        for bad in [
            "BetterCam",
            "Other.old-1-0",
            "BetterCam.old",
            r"BetterCam.old-\..\..\victim.txt",
            "BetterCam.old-a/b",
            "BetterCam.old-C:x",
            "BetterCam.old-1-0.", // Windows drops the dot: it would open BetterCam.old-1-0
            "BetterCam.old-1-0 ", // and the space
            "BetterCam.old-1*",
            "BetterCam.old-1?",
            "BetterCam.old-\u{0}",
            "BetterCam.old-é",
            &format!("BetterCam.old-{}", "x".repeat(MAX_NAME_CHARS)),
        ] {
            assert!(!is_aside_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn a_missing_record_is_empty_and_an_unreadable_one_is_never_overwritten() {
        // missing: add works and writes the one entry
        let mut m = Mem::default();
        add(&mut m, &entry("BetterCam.old-1-0", 1)).unwrap();
        assert_eq!(m.writes.len(), 1);
        // every kind of "exists and cannot be read": nothing is written, nothing is acted on
        let too_many = Stored::Strings(
            (0..=MAX_ENTRIES)
                .map(|i| entry(&format!("BetterCam.old-{i}-0"), 1).encode())
                .collect(),
        );
        for (what, stored) in [
            (
                "wrong type",
                Stored::Unreadable("error 1630 (another type)".into()),
            ),
            ("more data", Stored::Unreadable("error 234".into())),
            ("too long", Stored::Unreadable("70000 bytes".into())),
            ("too many strings", too_many),
        ] {
            let mut m = Mem {
                stored: Some(stored.clone()),
                ..Mem::default()
            };
            let e = add(&mut m, &entry("BetterCam.old-9-0", 9)).unwrap_err();
            assert!(e.contains("not overwritten"), "{what}: {e}");
            assert!(remove(&mut m, "BetterCam.old-9-0").is_err(), "{what}");
            let sweep = AsideSweep {
                forget: vec!["BetterCam.old-9-0".into()],
                ..AsideSweep::default()
            };
            assert!(finish_sweep(&mut m, &sweep).is_err(), "{what}");
            assert!(m.writes.is_empty(), "{what}: the record was written");
            assert_eq!(m.stored, Some(stored), "{what}: the record changed");
            // and a sweep gets no entries from it, with the reason in the log
            let mut log = VecLog::default();
            assert_eq!(load_logged(&mut m, &mut log), None, "{what}");
            assert!(
                log.0
                    .iter()
                    .any(|l| l.contains("cannot be read") && l.contains("left as it is")),
                "{what}: {:?}",
                log.0
            );
        }
    }

    #[test]
    fn adding_replaces_a_name_and_a_full_record_takes_no_more() {
        let mut m = Mem::default();
        add(&mut m, &entry("BetterCam.old-1-0", 1)).unwrap();
        add(&mut m, &entry("BetterCam.old-2-0", 2)).unwrap();
        add(&mut m, &entry("BetterCam.old-1-0", 3)).unwrap();
        let Record::Entries { entries, .. } = load(&mut m) else {
            panic!()
        };
        assert_eq!(
            entries,
            [entry("BetterCam.old-2-0", 2), entry("BetterCam.old-1-0", 3)]
        );
        // a malformed string goes with the next write
        m.stored = Some(Stored::Strings(vec![
            "junk".into(),
            entry("BetterCam.old-4-0", 4).encode(),
        ]));
        add(&mut m, &entry("BetterCam.old-5-0", 5)).unwrap();
        assert_eq!(
            m.stored,
            Some(Stored::Strings(vec![
                entry("BetterCam.old-4-0", 4).encode(),
                entry("BetterCam.old-5-0", 5).encode(),
            ]))
        );
        // full
        let mut full = Mem {
            stored: Some(Stored::Strings(
                (0..MAX_ENTRIES)
                    .map(|i| entry(&format!("BetterCam.old-{i}-0"), 1).encode())
                    .collect(),
            )),
            ..Mem::default()
        };
        assert!(add(&mut full, &entry("BetterCam.old-new-0", 2))
            .unwrap_err()
            .contains("full"));
        assert!(full.writes.is_empty());
        // removing a name that is not there writes nothing; the last one removes the value
        let mut m = Mem::default();
        add(&mut m, &entry("BetterCam.old-1-0", 1)).unwrap();
        let writes = m.writes.len();
        remove(&mut m, "BetterCam.old-zzz").unwrap();
        assert_eq!(m.writes.len(), writes);
        remove(&mut m, "BetterCam.old-1-0").unwrap();
        assert_eq!(m.writes.last(), Some(&Vec::new()));
        assert_eq!(m.aside_record(), Stored::Missing);
    }

    /// The entries a sweep is done with go; the ones it looked at and that stay go to the end, the rest keep their
    /// place in front of them, and a sweep that changes nothing writes nothing.
    #[test]
    fn a_finished_sweep_drops_what_is_done_and_moves_what_stays_to_the_end() {
        let names: Vec<String> = (1..=6).map(|i| format!("BetterCam.old-{i}-0")).collect();
        let mut m = Mem {
            stored: Some(Stored::Strings(
                names
                    .iter()
                    .enumerate()
                    .map(|(i, n)| entry(n, i as u8 + 1).encode())
                    .collect(),
            )),
            ..Mem::default()
        };
        let sweep = AsideSweep {
            left: 1,
            forget: vec![names[1].clone()],
            kept: vec![names[0].clone(), names[2].clone()],
        };
        finish_sweep(&mut m, &sweep).unwrap();
        let Record::Entries { entries, .. } = load(&mut m) else {
            panic!()
        };
        let order: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            order,
            [
                names[3].as_str(),
                names[4].as_str(),
                names[5].as_str(),
                names[0].as_str(),
                names[2].as_str()
            ]
        );
        // nothing to change, nothing written
        let writes = m.writes.len();
        finish_sweep(&mut m, &AsideSweep::default()).unwrap();
        assert_eq!(m.writes.len(), writes);
        // a failing write is an error for the caller to log, and the record is as it was
        let before = m.stored.clone();
        m.fail_writes = true;
        let sweep = AsideSweep {
            forget: vec![names[3].clone()],
            ..AsideSweep::default()
        };
        assert!(finish_sweep(&mut m, &sweep).is_err());
        assert_eq!(m.stored, before);
    }
}
