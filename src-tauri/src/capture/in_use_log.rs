//! Which program streams a camera right now, from the Windows camera service's event log
//! (`Microsoft-Windows-MF-FrameServer/Camera_FrameServer`, readable without elevation). The second source of
//! [`super::in_use`], for PCs where the capability access manager does not record webcam users.
//!
//! What the log holds (measured on Windows 11 build 26200; this is what the code relies on):
//!
//! * Event 7 (`FsProxy Initialization Stop`), written **in the client's process** (its system `ProcessID` is the
//!   client's): the client has opened a camera source. Data: `SymbolicLink` (the camera), `Streams` (more than 0 on
//!   success) and `hr`. A client that then fails to start streaming ("camera in use") leaves only these events.
//! * Events 11/12 (`WatchdogTimer Start/Stop`), written **by the camera service host** (a `svchost.exe`): with
//!   `TimerScope` 2 and `WatchdogOperation` 2 a stream of `SymbolicLink` started, with 3 it stopped. `TimerScopeId`
//!   names the stream object. A client that is killed or crashes leaves a stop 20 to 30 ms after its death.
//!
//! The log has no event that says which client *owns* a stream. A stream is credited to the client whose source
//! finished opening on the same camera just before the stream started (the client opens, then starts: 100 ms
//! apart on this PC). Everything that cannot be told is reported as a doubt ([`Holders::maybe`]), never as a name.
//!
//! The rule, as code in [`holders`]: a name is given for a process that (a) opened a source of the camera just before
//! a stream of that camera started that no later stop ended, whose service host is still the process that wrote the
//! start, and (b) is still running under its process id with a start time earlier than that open. Better Cam itself,
//! the service host, and clients of a virtual camera are never named.
//!
//! The log is circular (about 1 MB, half an hour of activity on a PC that uses the virtual camera): a stream that
//! started before the oldest record is not seen, and the answer is then "no name" (the banner says "another app").
//!
//! The reading is bounded: newest first, at most [`MAX_EVENTS`] records from the last [`MAX_AGE_MS`], and it
//! returns what it has when [`BUDGET`] is spent. Every failure is an empty list.

use std::time::{Duration, Instant};

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, FILETIME};
use windows::Win32::System::EventLog::{
    EvtClose, EvtCreateRenderContext, EvtNext, EvtQuery, EvtQueryChannelPath, EvtQueryReverseDirection,
    EvtRender, EvtRenderEventValues, EvtRenderContextValues, EVT_HANDLE, EVT_VARIANT,
};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};

use super::{links_match, normalize_link};
use crate::config::arrival_matches;

/// The environment variable that replaces the event log with the lines of a text file (debug builds only, like
/// `BETTER_CAM_TEST_WINDOW`), re-read at every lookup, so a test can add and remove events while the app runs. Lines
/// (`#` starts a comment, times are milliseconds before now, so the processes in them can be real ones):
///
/// ```text
/// open  <ms_ago> <client_pid> <link>
/// start <ms_ago> <service_pid> <stream_id> <link>
/// stop  <ms_ago> <service_pid> <stream_id> <link>
/// ```
pub const TEST_LOG_ENV: &str = "BETTER_CAM_TEST_CAMERA_LOG";

/// Records read at most.
pub const MAX_EVENTS: usize = 2000;
/// How far back the log is read, in milliseconds.
pub const MAX_AGE_MS: u64 = 4 * 60 * 60 * 1000;
/// The time one lookup may take before it returns what it has.
pub const BUDGET: Duration = Duration::from_millis(50);

/// 100 ns units (FILETIME) in a millisecond.
const MS: u64 = 10_000;
/// A stream is credited to the sources opened up to this long before it started.
const OPEN_WINDOW: u64 = 2_000 * MS;
/// A virtual camera made through the Windows virtual camera API (Better Cam's is one) has this link prefix.
const VIRTUAL_PREFIX: &str = r"\\?\swd#vcamdevapi#";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A client process opened a source of the camera (event 7 with streams and no error); `pid` is the client.
    Opened,
    /// A stream of the camera started (event 11, operation 2); `pid` is the service host. Payload: the stream id.
    StreamStart(u64),
    /// A stream of the camera stopped (event 11, operation 3). Payload: the stream id.
    StreamStop(u64),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogEvent {
    /// FILETIME (100 ns since 1601), the event's `TimeCreated`.
    pub time: u64,
    /// The process that wrote the event.
    pub pid: u32,
    /// The camera's symbolic link as the event spells it.
    pub link: String,
    pub kind: Kind,
}

/// What Windows says about a process id now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Proc {
    /// No such process (it ended).
    Gone,
    /// It exists but cannot be inspected.
    Unknown,
    /// Running: the creation time (FILETIME) and the file name of its image.
    Running { start: u64, name: String },
}

/// The answer of a lookup.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Holders {
    /// `(stream start FILETIME, process file name)`, most recent first, once per name (ignoring case).
    pub names: Vec<(u64, String)>,
    /// The names may include a program that does not hold the camera Better Cam wants: the camera could not be told
    /// (no target and several cameras stream), or the client of the stream could not be told from another one.
    pub maybe: bool,
}

fn is_physical(link: &str) -> bool {
    !link.to_ascii_lowercase().starts_with(VIRTUAL_PREFIX)
}

fn is_service_host(name: &str) -> bool {
    name.eq_ignore_ascii_case("svchost.exe")
}

/// A stream that started and has not been seen to stop.
struct Open {
    time: u64,
    service: u32,
    link: String,
}

/// The programs that stream the camera `target` (the symbolic link Better Cam wants; `None`: the first camera, which
/// is not known here) according to `events` (any order). `own_pid` is Better Cam's process; `proc_of` tells what a
/// process id is now. See the module comment for the rule.
pub fn holders(events: &[LogEvent], target: Option<&str>, own_pid: u32, proc_of: &dyn Fn(u32) -> Proc) -> Holders {
    let target = target.filter(|t| !t.is_empty());
    let physical: Vec<&LogEvent> = events.iter().filter(|e| is_physical(&e.link)).collect();
    // The target's own link first; the same model on another port only when the link itself never shows up.
    let mut ev: Vec<&LogEvent> = match target {
        Some(t) => {
            let exact: Vec<&LogEvent> = physical.iter().copied().filter(|e| links_match(&e.link, t)).collect();
            if exact.is_empty() {
                physical.iter().copied().filter(|e| arrival_matches(Some(t), &e.link)).collect()
            } else {
                exact
            }
        }
        None => physical,
    };
    ev.sort_by_key(|e| e.time);

    // The streams that started and were not stopped later (a stream id is a pointer: a new start replaces an old one).
    let mut open: Vec<(u64, Open)> = Vec::new();
    for e in &ev {
        match e.kind {
            Kind::StreamStart(id) => {
                open.retain(|(i, o)| !(*i == id && links_match(&o.link, &e.link)));
                open.push((id, Open { time: e.time, service: e.pid, link: e.link.clone() }));
            }
            Kind::StreamStop(id) => open.retain(|(i, o)| !(*i == id && links_match(&o.link, &e.link))),
            Kind::Opened => {}
        }
    }
    // A stream whose service host is gone (the service restarted) is gone with it.
    open.retain(|(_, o)| {
        o.service == 0
            || match proc_of(o.service) {
                Proc::Gone => false,
                Proc::Running { start, .. } => start < o.time,
                Proc::Unknown => true,
            }
    });
    open.sort_by_key(|(_, o)| std::cmp::Reverse(o.time));

    let mut links: Vec<String> = open.iter().map(|(_, o)| normalize_link(&o.link)).collect();
    links.sort();
    links.dedup();
    // Without a target, streams on several cameras cannot be told apart.
    let mut maybe = target.is_none() && links.len() > 1;

    let mut named: Vec<(u64, String)> = Vec::new();
    for (_, stream) in &open {
        // The clients that opened this camera before the stream: those just before it, else the latest one.
        let mut opened: Vec<&&LogEvent> = ev
            .iter()
            .filter(|e| e.kind == Kind::Opened && e.time <= stream.time && links_match(&e.link, &stream.link))
            .collect();
        opened.sort_by_key(|e| std::cmp::Reverse(e.time));
        // A failed try of Better Cam's own (the camera is held by a program that bypasses the camera service) also
        // starts a stream that nothing ever stops: it is ours when our open is the newest one before it.
        if opened.first().is_some_and(|e| e.pid == own_pid) {
            continue;
        }
        let near: Vec<&&LogEvent> = opened.iter().copied().filter(|e| stream.time - e.time <= OPEN_WINDOW).collect();
        // Just before the stream, else the latest open before it (a client that opened the camera and streamed later).
        let candidates: Vec<&&LogEvent> = if near.is_empty() { opened.iter().copied().take(1).collect() } else { near.clone() };
        // Only a process that can be the client counts, once, by its newest open: not Better Cam (which is the one
        // asking), not the service host, not one that ended or whose id was reused after the open.
        let mut eligible: Vec<(u32, String)> = Vec::new();
        for c in candidates {
            if c.pid == own_pid || eligible.iter().any(|(p, _)| *p == c.pid) {
                continue;
            }
            let Proc::Running { start, name } = proc_of(c.pid) else { continue };
            if start >= c.time || is_service_host(&name) {
                continue;
            }
            eligible.push((c.pid, name));
        }
        // Several possible clients, or one that opened long before the stream: it is a doubt, not a certainty.
        maybe |= !eligible.is_empty() && (near.is_empty() || eligible.len() > 1);
        for (_, name) in eligible {
            if !named.iter().any(|(_, n)| n.eq_ignore_ascii_case(&name)) {
                named.push((stream.time, name));
            }
        }
    }
    if named.is_empty() {
        maybe = false;
    }
    Holders { names: named, maybe }
}

// ---------------------------------------------------------------------------------------------------------------
// The real log

/// The file name of a process image path.
fn file_name(path: &str) -> Option<String> {
    let name = path.rsplit(['\\', '/']).next()?.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// What Windows says about `pid` now (a limited query handle; never reads the process).
pub fn process_info(pid: u32) -> Proc {
    // SAFETY: plain calls; the handle is closed before the function returns.
    unsafe {
        let h = match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(h) => h,
            Err(e) => {
                return if e.code() == ERROR_INVALID_PARAMETER.to_hresult() { Proc::Gone } else { Proc::Unknown };
            }
        };
        let (mut created, mut exited, mut kernel, mut user) =
            (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        let times = GetProcessTimes(h, &mut created, &mut exited, &mut kernel, &mut user);
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let image = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, windows::core::PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(h);
        if times.is_err() || image.is_err() {
            return Proc::Unknown;
        }
        if exited.dwLowDateTime != 0 || exited.dwHighDateTime != 0 {
            return Proc::Gone; // a handle somewhere keeps the ended process object alive
        }
        let start = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
        let path = String::from_utf16_lossy(&buf[..(len as usize).min(buf.len())]);
        match file_name(&path) {
            Some(name) => Proc::Running { start, name },
            None => Proc::Unknown,
        }
    }
}

/// Now as a FILETIME.
fn now_filetime() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    // 100 ns units between 1601-01-01 and 1970-01-01.
    const EPOCH_DIFF: u64 = 116_444_736_000_000_000;
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    EPOCH_DIFF + (d.as_nanos() / 100) as u64
}

/// The events of the test file ([`TEST_LOG_ENV`]); lines that do not parse are skipped.
pub fn parse_test_log(text: &str, now: u64) -> Vec<LogEvent> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut f = line.split_whitespace();
        let (Some(what), Some(ago)) = (f.next(), f.next().and_then(|a| a.parse::<u64>().ok())) else { continue };
        let time = now.saturating_sub(ago * MS);
        let ev = match what {
            "open" => {
                let (Some(pid), Some(link)) = (f.next().and_then(|p| p.parse().ok()), f.next()) else { continue };
                LogEvent { time, pid, link: link.to_string(), kind: Kind::Opened }
            }
            "start" | "stop" => {
                let (Some(pid), Some(id), Some(link)) =
                    (f.next().and_then(|p| p.parse().ok()), f.next().and_then(|i| i.parse().ok()), f.next())
                else {
                    continue;
                };
                let kind = if what == "start" { Kind::StreamStart(id) } else { Kind::StreamStop(id) };
                LogEvent { time, pid, link: link.to_string(), kind }
            }
            _ => continue,
        };
        out.push(ev);
    }
    out
}

fn test_log() -> Option<String> {
    if !cfg!(debug_assertions) {
        return None;
    }
    std::env::var(TEST_LOG_ENV).ok().filter(|s| !s.is_empty())
}

/// Event Log queries: only what [`read_events`] understands, newest first, in the time window.
fn query_text() -> String {
    format!(
        "*[System[(EventID=7 or EventID=11) and TimeCreated[timediff(@SystemTime) <= {MAX_AGE_MS}]] and \
         (System[EventID=7] or EventData[Data[@Name='TimerScope']=2 and \
         (Data[@Name='WatchdogOperation']=2 or Data[@Name='WatchdogOperation']=3)])]"
    )
}

/// The values read from each record, in this order (`EvtRender` with value paths).
const VALUE_PATHS: [PCWSTR; 9] = [
    w!("Event/System/EventID"),
    w!("Event/System/TimeCreated/@SystemTime"),
    w!("Event/System/Execution/@ProcessID"),
    w!("Event/EventData/Data[@Name='SymbolicLink']"),
    w!("Event/EventData/Data[@Name='Streams']"),
    w!("Event/EventData/Data[@Name='hr']"),
    w!("Event/EventData/Data[@Name='TimerScope']"),
    w!("Event/EventData/Data[@Name='TimerScopeId']"),
    w!("Event/EventData/Data[@Name='WatchdogOperation']"),
];

/// An `EVT_VARIANT` as a number, `None` for a missing value or a type that is not a number.
fn number(v: &EVT_VARIANT) -> Option<u64> {
    // SAFETY: the union member read is the one the type tag says is set.
    unsafe {
        match v.Type & 0x7f {
            4 => Some(u64::from(v.Anonymous.ByteVal)),
            6 => Some(u64::from(v.Anonymous.UInt16Val)),
            7 | 20 => Some(u64::from(v.Anonymous.Int32Val as u32)),
            8 => Some(u64::from(v.Anonymous.UInt32Val)),
            9 | 10 | 21 => Some(v.Anonymous.UInt64Val),
            17 => Some(v.Anonymous.FileTimeVal),
            _ => None,
        }
    }
}

/// An `EVT_VARIANT` as a string, `None` for a missing value or another type.
fn text(v: &EVT_VARIANT) -> Option<String> {
    // SAFETY: a string variant points at a NUL-terminated UTF-16 string inside the render buffer, which is alive.
    unsafe {
        if v.Type & 0x7f != 1 || v.Anonymous.StringVal.is_null() {
            return None;
        }
        v.Anonymous.StringVal.to_string().ok()
    }
}

/// One record as an event, `None` for one that is not an open or a stream start/stop.
fn event_of(values: &[EVT_VARIANT]) -> Option<LogEvent> {
    let id = number(values.first()?)?;
    let time = number(values.get(1)?)?;
    let pid = u32::try_from(number(values.get(2)?)?).ok()?;
    let link = text(values.get(3)?).filter(|l| !l.is_empty())?;
    let kind = match id {
        7 => {
            let streams = number(values.get(4)?)?;
            let hr = number(values.get(5)?)?;
            if streams == 0 || hr != 0 {
                return None;
            }
            Kind::Opened
        }
        11 => {
            if number(values.get(6)?)? != 2 {
                return None;
            }
            let stream = number(values.get(7)?)?;
            match number(values.get(8)?)? {
                2 => Kind::StreamStart(stream),
                3 => Kind::StreamStop(stream),
                _ => return None,
            }
        }
        _ => return None,
    };
    Some(LogEvent { time, pid, link, kind })
}

/// Closes an event log handle on drop.
struct Handle(EVT_HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle came from the Event Log API and is closed once.
        unsafe {
            let _ = EvtClose(self.0);
        }
    }
}

/// The events of the camera service's log, newest first, at most [`MAX_EVENTS`] from the last [`MAX_AGE_MS`],
/// stopping when `deadline` passes. Empty on any failure (channel missing or disabled, access denied, another layout).
pub fn read_events(deadline: Instant) -> Vec<LogEvent> {
    let mut out = Vec::new();
    // SAFETY: plain Event Log calls; every handle is closed (the render buffer outlives the values read from it).
    unsafe {
        let query = HSTRING::from(query_text());
        let Ok(results) = EvtQuery(
            None,
            w!("Microsoft-Windows-MF-FrameServer/Camera_FrameServer"),
            &query,
            EvtQueryChannelPath.0 | EvtQueryReverseDirection.0,
        ) else {
            return out;
        };
        let results = Handle(results);
        let Ok(context) = EvtCreateRenderContext(Some(&VALUE_PATHS), EvtRenderContextValues.0) else {
            return out;
        };
        let context = Handle(context);
        // u64 elements: the variants inside need 8-byte alignment.
        let mut buf = vec![0u64; 1024];
        let mut seen = 0usize;
        while seen < MAX_EVENTS && Instant::now() < deadline {
            let mut batch = [0isize; 32];
            let mut got = 0u32;
            if EvtNext(results.0, &mut batch, 20, 0, &mut got).is_err() {
                break; // no more records, or a failure: what was read is the answer
            }
            for raw in batch.iter().take(got as usize) {
                let record = Handle(EVT_HANDLE(*raw));
                seen += 1;
                let (mut used, mut count) = (0u32, 0u32);
                let ok = EvtRender(
                    Some(context.0),
                    record.0,
                    EvtRenderEventValues.0,
                    (buf.len() * 8) as u32,
                    Some(buf.as_mut_ptr() as *mut _),
                    &mut used,
                    &mut count,
                );
                if ok.is_err() || (count as usize) < VALUE_PATHS.len() {
                    continue;
                }
                let values = std::slice::from_raw_parts(buf.as_ptr() as *const EVT_VARIANT, count as usize);
                out.extend(event_of(values));
            }
        }
    }
    out
}

/// The programs that stream the camera `target` now, from the real log (or the test file in a debug build).
pub fn lookup(target: Option<&str>) -> Holders {
    let events = match test_log() {
        Some(path) => parse_test_log(&std::fs::read_to_string(path).unwrap_or_default(), now_filetime()),
        None => read_events(Instant::now() + BUDGET),
    };
    holders(&events, target, std::process::id(), &process_info)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const CAM: &str = r"\\?\usb#vid_0c45&pid_636d&mi_00#9&3171a958&0&0000#{e5323777-f976-4f5b-9b55-b94699c46e44}\global";
    /// How the service host spells the same camera.
    const CAM_UPPER: &str = r"\\?\USB#VID_0C45&PID_636D&MI_00#9&3171A958&0&0000#{E5323777-F976-4F5B-9B55-B94699C46E44}\GLOBAL";
    const OTHER: &str = r"\\?\usb#vid_046d&pid_0825&mi_00#7&1a2b3c4d&0&0000#{e5323777-f976-4f5b-9b55-b94699c46e44}\global";
    const VCAM: &str = r"\\?\swd#vcamdevapi#d8f8cb27#{65e8773d-8f56-11d0-a3b9-00a0c9223196}\{fcebba03-9d13-4c13-9940-cc84fcd132d1}";

    const OWN: u32 = 100;
    const SVC: u32 = 900;
    const T: u64 = 1_000_000_000_000; // an arbitrary "now" in FILETIME

    fn t(ms: u64) -> u64 {
        T + ms * MS
    }
    fn open(ms: u64, pid: u32, link: &str) -> LogEvent {
        LogEvent { time: t(ms), pid, link: link.into(), kind: Kind::Opened }
    }
    fn start(ms: u64, id: u64, link: &str) -> LogEvent {
        LogEvent { time: t(ms), pid: SVC, link: link.into(), kind: Kind::StreamStart(id) }
    }
    fn stop(ms: u64, id: u64, link: &str) -> LogEvent {
        LogEvent { time: t(ms), pid: SVC, link: link.into(), kind: Kind::StreamStop(id) }
    }

    /// A process table: pid -> (start in ms on the test clock, name). Pids not in it are gone; pid 666 cannot be opened.
    struct Table(HashMap<u32, (u64, &'static str)>);
    impl Table {
        fn new(rows: &[(u32, u64, &'static str)]) -> Table {
            let mut m: HashMap<u32, (u64, &'static str)> = rows.iter().map(|(p, s, n)| (*p, (*s, *n))).collect();
            m.entry(SVC).or_insert((0, "svchost.exe"));
            m.entry(OWN).or_insert((0, "better-cam.exe"));
            Table(m)
        }
        fn of(&self, pid: u32) -> Proc {
            if pid == 666 {
                return Proc::Unknown;
            }
            match self.0.get(&pid) {
                Some((s, n)) => Proc::Running { start: t(*s), name: n.to_string() },
                None => Proc::Gone,
            }
        }
    }

    fn run(events: &[LogEvent], target: Option<&str>, table: &Table) -> (Vec<String>, bool) {
        let h = holders(events, target, OWN, &|p| table.of(p));
        (h.names.into_iter().map(|(_, n)| n).collect(), h.maybe)
    }
    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_client_that_opened_and_started_a_stream_is_named() {
        let table = Table::new(&[(500, 0, "Discord.exe")]);
        let events = [open(1000, 500, CAM), start(1100, 7, CAM_UPPER)];
        assert_eq!(run(&events, Some(CAM), &table), (s(&["Discord.exe"]), false));
    }

    #[test]
    fn a_stream_that_stopped_names_nobody() {
        let table = Table::new(&[(500, 0, "Discord.exe")]);
        let events = [open(1000, 500, CAM), start(1100, 7, CAM_UPPER), stop(5000, 7, CAM_UPPER)];
        assert_eq!(run(&events, Some(CAM), &table), (vec![], false));
    }

    #[test]
    fn a_stop_ends_only_its_own_stream() {
        let table = Table::new(&[(500, 0, "a.exe"), (501, 0, "b.exe")]);
        let events = [
            open(1000, 500, CAM),
            start(1100, 7, CAM),
            open(3000, 501, CAM),
            start(3100, 8, CAM),
            stop(6000, 7, CAM),
        ];
        assert_eq!(run(&events, Some(CAM), &table), (s(&["b.exe"]), false), "a.exe's stream stopped, b.exe's did not");
    }

    #[test]
    fn two_streams_name_both_clients_most_recent_first() {
        let table = Table::new(&[(500, 0, "a.exe"), (501, 0, "b.exe")]);
        let events = [open(1000, 500, CAM), start(1100, 7, CAM), open(3000, 501, CAM), start(3100, 8, CAM)];
        assert_eq!(run(&events, Some(CAM), &table), (s(&["b.exe", "a.exe"]), false));
    }

    #[test]
    fn events_out_of_order_give_the_same_answer() {
        let table = Table::new(&[(500, 0, "a.exe"), (501, 0, "b.exe")]);
        let events = [start(3100, 8, CAM), stop(6000, 7, CAM), open(3000, 501, CAM), start(1100, 7, CAM), open(1000, 500, CAM)];
        assert_eq!(run(&events, Some(CAM), &table), (s(&["b.exe"]), false));
        // a stop that sorts before its start is not a stop of it
        let events = [stop(900, 7, CAM), open(1000, 500, CAM), start(1100, 7, CAM)];
        assert_eq!(run(&events, Some(CAM), &table), (s(&["a.exe"]), false));
    }

    #[test]
    fn a_recycled_pid_is_not_named() {
        // pid 500 now belongs to a program that started after the open event
        let table = Table::new(&[(500, 2000, "newcomer.exe")]);
        let events = [open(1000, 500, CAM), start(1100, 7, CAM)];
        assert_eq!(run(&events, Some(CAM), &table), (vec![], false));
    }

    #[test]
    fn a_dead_client_is_not_named() {
        let table = Table::new(&[]);
        let events = [open(1000, 500, CAM), start(1100, 7, CAM)];
        assert_eq!(run(&events, Some(CAM), &table), (vec![], false));
    }

    #[test]
    fn a_process_that_cannot_be_opened_is_skipped() {
        let table = Table::new(&[]);
        let events = [open(1000, 666, CAM), start(1100, 7, CAM)];
        assert_eq!(run(&events, Some(CAM), &table), (vec![], false));
    }

    #[test]
    fn better_cam_and_the_service_host_are_never_named() {
        let table = Table::new(&[]);
        let events = [open(1000, OWN, CAM), start(1100, 7, CAM)];
        assert_eq!(run(&events, Some(CAM), &table), (vec![], false), "own process");
        let events = [open(1000, SVC, CAM), start(1100, 7, CAM)];
        assert_eq!(run(&events, Some(CAM), &table), (vec![], false), "the service host");
        // another svchost (any pid) is the same
        let table = Table::new(&[(777, 0, "SVCHOST.EXE")]);
        assert_eq!(run(&[open(1000, 777, CAM), start(1100, 7, CAM)], Some(CAM), &table), (vec![], false));
    }

    #[test]
    fn a_client_of_the_virtual_camera_only_is_not_named() {
        let table = Table::new(&[(500, 0, "Zoom.exe")]);
        let events = [open(1000, 500, VCAM), start(1100, 7, VCAM)];
        assert_eq!(run(&events, Some(CAM), &table), (vec![], false));
        assert_eq!(run(&events, None, &table), (vec![], false), "also without a target");
    }

    #[test]
    fn a_stream_of_another_camera_is_not_the_targets() {
        let table = Table::new(&[(500, 0, "Zoom.exe")]);
        let events = [open(1000, 500, OTHER), start(1100, 7, OTHER)];
        assert_eq!(run(&events, Some(CAM), &table), (vec![], false));
    }

    #[test]
    fn the_same_camera_on_another_port_is_the_target_when_its_own_link_is_silent() {
        let moved = r"\\?\usb#vid_0c45&pid_636d&mi_00#9&aaaaaaaa&0&0001#{e5323777-f976-4f5b-9b55-b94699c46e44}\global";
        let table = Table::new(&[(500, 0, "Zoom.exe")]);
        let events = [open(1000, 500, moved), start(1100, 7, moved)];
        assert_eq!(run(&events, Some(CAM), &table), (s(&["Zoom.exe"]), false));
        // but with the target's own link in the log, the other port is another camera
        let events = [open(1000, 500, moved), start(1100, 7, moved), open(9000, 501, CAM), stop(9500, 9, CAM)];
        assert_eq!(run(&events, Some(CAM), &table), (vec![], false));
    }

    #[test]
    fn without_a_target_one_streaming_camera_is_it_and_two_are_a_doubt() {
        let table = Table::new(&[(500, 0, "a.exe"), (501, 0, "b.exe")]);
        let one = [open(1000, 500, CAM), start(1100, 7, CAM)];
        assert_eq!(run(&one, None, &table), (s(&["a.exe"]), false));
        let two = [open(1000, 500, CAM), start(1100, 7, CAM), open(2000, 501, OTHER), start(2100, 8, OTHER)];
        assert_eq!(run(&two, None, &table), (s(&["b.exe", "a.exe"]), true));
        assert_eq!(run(&two, Some(CAM), &table), (s(&["a.exe"]), false), "a target tells them apart");
    }

    #[test]
    fn a_client_that_failed_to_start_streaming_is_not_named() {
        // b.exe opened the camera after a.exe's stream started and never got a stream (camera in use)
        let table = Table::new(&[(500, 0, "a.exe"), (501, 0, "b.exe")]);
        let events = [open(1000, 500, CAM), start(1100, 7, CAM), open(4000, 501, CAM)];
        assert_eq!(run(&events, Some(CAM), &table), (s(&["a.exe"]), false));
    }

    #[test]
    fn two_clients_opening_together_are_both_named_with_a_doubt() {
        let table = Table::new(&[(500, 0, "a.exe"), (501, 0, "b.exe")]);
        let events = [open(1000, 500, CAM), open(1050, 501, CAM), start(1100, 7, CAM)];
        assert_eq!(run(&events, Some(CAM), &table), (s(&["b.exe", "a.exe"]), true), "the later open first");
        // a dead one cannot be the client of a stream that is still open, and Better Cam is the one asking
        let table = Table::new(&[(501, 0, "b.exe")]);
        assert_eq!(run(&events, Some(CAM), &table), (s(&["b.exe"]), false));
        let table = Table::new(&[(500, 0, "a.exe")]);
        let events = [open(1000, OWN, CAM), open(1050, 500, CAM), start(1100, 7, CAM)];
        assert_eq!(run(&events, Some(CAM), &table), (s(&["a.exe"]), false), "Better Cam's own open is no second candidate");
    }

    #[test]
    fn a_stream_that_better_cam_opened_last_is_its_own_failed_try_and_names_nobody() {
        // a program that does not use the camera service holds the camera: our try opens it, starts a stream that
        // fails, and nothing stops it. Another program that happened to open the camera just before is not its client.
        let table = Table::new(&[(500, 0, "a.exe")]);
        let events = [open(1000, 500, CAM), open(1050, OWN, CAM), start(1100, 7, CAM)];
        assert_eq!(run(&events, Some(CAM), &table), (vec![], false));
        // ... and our try alone is no one
        assert_eq!(run(&[open(1050, OWN, CAM), start(1100, 7, CAM)], Some(CAM), &table), (vec![], false));
        // a stream another program started, with our open before it, still goes to that program
        let events = [open(1000, OWN, CAM), open(1050, 500, CAM), start(1100, 7, CAM)];
        assert_eq!(run(&events, Some(CAM), &table), (s(&["a.exe"]), false));
    }

    #[test]
    fn a_stream_long_after_the_open_is_a_doubt_not_a_certainty() {
        let table = Table::new(&[(500, 0, "a.exe")]);
        let events = [open(1000, 500, CAM), start(60_000, 7, CAM)];
        assert_eq!(run(&events, Some(CAM), &table), (s(&["a.exe"]), true));
        // nobody opened it in the part of the log that is there: no name at all
        assert_eq!(run(&[start(60_000, 7, CAM)], Some(CAM), &table), (vec![], false));
    }

    #[test]
    fn a_stream_of_a_service_that_is_gone_is_gone() {
        let table = Table::new(&[(500, 0, "a.exe")]);
        let mut events = [open(1000, 500, CAM), start(1100, 7, CAM)];
        events[1].pid = 4242; // the host that wrote the start no longer runs
        assert_eq!(run(&events, Some(CAM), &table), (vec![], false));
        // a host that started after the stream is another process with the same id
        let mut table = Table::new(&[(500, 0, "a.exe"), (4243, 5000, "svchost.exe")]);
        events[1].pid = 4243;
        assert_eq!(run(&events, Some(CAM), &table), (vec![], false));
        // a host that cannot be inspected does not veto
        events[1].pid = 666;
        table.0.insert(500, (0, "a.exe"));
        assert_eq!(run(&events, Some(CAM), &table), (s(&["a.exe"]), false));
    }

    #[test]
    fn a_program_is_named_once_by_name() {
        let table = Table::new(&[(500, 0, "Chrome.exe"), (501, 0, "chrome.exe")]);
        let events = [open(1000, 500, CAM), start(1100, 7, CAM), open(3000, 501, CAM), start(3100, 8, CAM)];
        assert_eq!(run(&events, Some(CAM), &table), (s(&["chrome.exe"]), false));
    }

    #[test]
    fn an_empty_log_names_nobody() {
        let table = Table::new(&[]);
        assert_eq!(run(&[], Some(CAM), &table), (vec![], false));
        assert_eq!(run(&[], None, &table), (vec![], false));
    }

    #[test]
    fn the_test_file_has_the_events_in_it() {
        let text = "# a comment\n\nopen 300 500 \\\\?\\usb#x#{g}\\global\nstart 200 900 7 \\\\?\\usb#x#{g}\\global\n\
                    stop 100 900 7 \\\\?\\usb#x#{g}\\global\nbogus line\nopen x 1 y\nstart 1 2 three link\n";
        let ev = parse_test_log(text, t(1000));
        assert_eq!(ev.len(), 3);
        assert_eq!((ev[0].pid, ev[0].time, &ev[0].kind), (500, t(700), &Kind::Opened));
        assert_eq!((ev[1].pid, ev[1].time, &ev[1].kind), (900, t(800), &Kind::StreamStart(7)));
        assert_eq!((ev[2].pid, ev[2].time, &ev[2].kind), (900, t(900), &Kind::StreamStop(7)));
        assert_eq!(ev[0].link, r"\\?\usb#x#{g}\global");
    }

    #[test]
    fn a_process_image_path_is_its_file_name() {
        assert_eq!(file_name(r"C:\Program Files\Google\Chrome\Application\chrome.exe").as_deref(), Some("chrome.exe"));
        assert_eq!(file_name("tool.exe").as_deref(), Some("tool.exe"));
        assert_eq!(file_name(r"C:\dir\"), None);
    }

    #[test]
    fn this_process_is_running_and_a_dead_pid_is_gone() {
        match process_info(std::process::id()) {
            Proc::Running { start, name } => {
                assert!(start > 0 && start < now_filetime());
                assert!(name.to_ascii_lowercase().ends_with(".exe"), "{name}");
            }
            other => panic!("{other:?}"),
        }
        // a process that ended: start one, wait for it, ask
        let mut child = std::process::Command::new("cmd").args(["/c", "exit", "0"]).spawn().expect("cmd");
        let pid = child.id();
        child.wait().unwrap();
        assert!(matches!(process_info(pid), Proc::Gone | Proc::Unknown), "an ended process is not running");
        assert_eq!(process_info(0), Proc::Gone);
    }

    #[test]
    fn this_machine_reads_the_real_log_within_the_budget() {
        // What is in the log differs per machine (and the channel may be off); the read itself must work and be quick.
        let mut times = Vec::new();
        let mut last = Vec::new();
        for _ in 0..20 {
            let t0 = Instant::now();
            last = read_events(t0 + BUDGET);
            times.push(t0.elapsed());
        }
        times.sort();
        let held = holders(&last, None, std::process::id(), &process_info);
        eprintln!(
            "camera log: {} events; median {:?}, worst {:?}; holders now: {:?}",
            last.len(),
            times[times.len() / 2],
            times[times.len() - 1],
            held
        );
        assert!(*times.last().unwrap() < BUDGET + Duration::from_millis(100), "{times:?}");
    }
}
