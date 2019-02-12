#![cfg_attr(windows, windows_subsystem = "windows")]

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{prelude::*, BufRead, BufReader, Error, ErrorKind, Result};
use std::path::PathBuf;
use std::process;
use std::sync::mpsc::{sync_channel, SyncSender};
use std::thread;

use log::*;
use rand::prelude::*;
use serde;
use serde::Serialize;
use serde_json::Value;

struct Playlist(PathBuf);

impl std::ops::Deref for Playlist {
    type Target = PathBuf;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Playlist {
    fn walk(list: &mut Vec<PathBuf>, path: PathBuf) {
        if path.is_file() && !path.is_dir() {
            list.push(path);
            return;
        }

        for (ft, path) in path
            .read_dir()
            .unwrap()
            .filter_map(|s| s.ok())
            .filter_map(|s| s.file_type().ok().map(|ft| (ft, s.path())))
        {
            match (ft.is_dir(), ft.is_file()) {
                (true, false) => Self::walk(list, path),
                (false, true) => list.push(path),
                _ => unreachable!(),
            }
        }
    }

    fn make_temp(files: &[PathBuf]) -> Result<Self> {
        let data = files
            .iter()
            .filter_map(|s| {
                s.extension()
                    .and_then(|e| e.to_str())
                    .and_then(|e| Some((s, e)))
            })
            .filter(|(_, e)| match e.to_lowercase().as_str() {
                "jpg" | "jpeg" | "png" | "gif" | "heif" | "webp" | "tga" | "bpg" => false,
                _ => true,
            })
            .filter_map(|(s, _)| s.to_str())
            .collect::<Vec<_>>();

        if data.is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "file list contains no valid files",
            ));
        }

        let dir = std::env::temp_dir()
            .join(random_name(7))
            .with_extension("mpv-playlist");

        std::fs::write(&dir, data.join("\n"))?;
        Ok(Playlist(dir))
    }
}

impl Drop for Playlist {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[derive(Debug, Copy, Clone, PartialEq)]
enum Event {
    FileLoaded,
    Unpause,
}

impl Event {
    fn try_from_value(val: &Value) -> Option<Self> {
        match val.get("event")?.as_str()? {
            "file-loaded" => Some(Event::FileLoaded),
            "unpause" => Some(Event::Unpause),
            _ => None,
        }
    }
}

enum Command<S> {
    SetProperty(S, Value),
    GetProperty(S),
}

impl<S: Into<Value>> Command<S> {
    fn get(prop: S) -> Self {
        Command::GetProperty(prop)
    }

    fn set<V: Into<Value>>(prop: S, value: V) -> Self {
        Command::SetProperty(prop, value.into())
    }

    fn into_values(self) -> Vec<Value> {
        match self {
            Command::SetProperty(prop, val) => vec!["set_property".into(), prop.into(), val],
            Command::GetProperty(prop) => vec!["get_property".into(), prop.into()],
        }
    }
}

#[derive(Serialize)]
struct Request {
    command: Vec<Value>,
    request_id: u32,
}

impl Request {
    fn new<S: Into<Value>>(cmd: Command<S>) -> Self {
        Self {
            command: cmd.into_values(),
            request_id: thread_rng().next_u32(),
        }
    }
}

struct Mpv {
    child: process::Child,
    _playlist: Playlist,
}

impl Mpv {
    fn new(playlist: Playlist) -> Result<Self> {
        let name = crate::random_name(5);

        let child = process::Command::new("mpv")
            .arg(&format!("--input-ipc-server=tmp/{}", name))
            .arg(&format!("--playlist={}", playlist.to_str().unwrap()))
            .arg("--pause")
            .spawn()?;

        let file = Self::try_connect(&format!(r#"\\.\\pipe\tmp\{}"#, name))?;
        thread::spawn(move || {
            State::run(file, |info| {
                #[derive(Debug, Serialize)]
                #[serde(rename_all = "lowercase")]
                enum ItemKind {
                    Local {
                        artist: String,
                        title: String,
                        album: String,
                    },
                }

                impl From<SongInfo> for ItemKind {
                    fn from(
                        SongInfo {
                            artist,
                            title,
                            album,
                        }: SongInfo,
                    ) -> Self {
                        ItemKind::Local {
                            artist,
                            title,
                            album,
                        }
                    }
                }
                #[derive(Debug, Serialize)]
                #[serde(rename_all = "lowercase")]
                struct Item {
                    pub kind: ItemKind,
                    pub ts: i64,
                    pub version: u32,
                }

                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::SystemTime::UNIX_EPOCH)
                    .unwrap();

                let data = serde_json::to_string(&Item {
                    kind: info.into(),
                    ts: (ts.as_secs() * 1000 + u64::from(ts.subsec_nanos()) / 1_000_000) as i64,
                    version: 1,
                })
                .map_err(|err| Error::new(ErrorKind::InvalidData, err))?;

                match ureq::post("http://localhost:50006/local")
                    .send_string(&data)
                    .synthetic_error()
                {
                    Some(err) => Err(Error::new(
                        ErrorKind::ConnectionRefused,
                        format!("{}: {}", err.status(), err.status_text()),
                    )),
                    None => {
                        debug!("sent song info");
                        Ok(())
                    }
                }
            })
        });

        Ok(Mpv {
            child,
            _playlist: playlist,
        })
    }

    fn try_connect(fd: &str) -> Result<File> {
        if cfg!(not(windows)) {
            return File::open(&fd);
        }

        let mut count = 0;
        loop {
            match miow::pipe::connect(fd) {
                Ok(file) => {
                    trace!("connected to pipe");
                    return Ok(file);
                }
                Err(..) if count < 5 => {
                    debug!(
                        "(attempt #{}) waiting a bit for the ipc pipe to be ready",
                        count + 1
                    );
                    thread::sleep(std::time::Duration::from_millis(100));
                    count += 1;
                }
                Err(err) => {
                    error!("cannot connect to the mpv ipc named pipe: {}", err);
                    return Err(err);
                }
            }
        }
    }
}

impl Drop for Mpv {
    fn drop(&mut self) {
        debug!("dropping child: {}", self.child.id());
        let _ = self.child.kill();
    }
}

struct State {
    file: File,
    // this should really have a LRU eviction policy,
    // technically it currently has unbounded growth
    waiting: HashSet<u32>,
}

impl State {
    fn run<F>(file: File, func: F) -> Result<()>
    where
        F: Fn(SongInfo) -> Result<()>,
    {
        let mut this = Self {
            file: file.try_clone().unwrap(),
            waiting: HashSet::new(),
        };

        let req = this.write_command(Command::set("pause", false))?;
        this.waiting.insert(req);

        let mut last = None;
        for msg in BufReader::new(file)
            .lines()
            .filter_map(|line| line.ok())
            .filter_map(|line| Message::parse(line).ok())
        {
            match msg {
                Message::Unpause | Message::FileLoaded => {
                    if let Err(err) = this.get_song_info() {
                        error!("cannot get song info: {}", err)
                    }
                }
                Message::Response(id, val) => {
                    if let Response::Song(info) = match this.handle_resp(id, val) {
                        Err(err) => {
                            error!("cannot handle response: {}", err);
                            continue;
                        }
                        Ok(req) => req,
                    } {
                        // TODO: this will stop any songs from repeating back to back (single song playlists..)
                        if last.as_ref() == Some(&info) {
                            continue;
                        }
                        last.replace(info.clone());
                        if let Err(err) = func(info) {
                            error!("cannot send song info: {}", err)
                        }
                    }
                }
                Message::Unknown => (),
            }
        }
        Ok(())
    }

    fn get_song_info(&mut self) -> Result<()> {
        let req = self.write_command(Command::get("filtered-metadata"))?;
        self.waiting.insert(req);
        Ok(())
    }

    fn handle_resp(&mut self, id: u32, mut val: Value) -> Result<Response> {
        macro_rules! maybe {
            ($e:expr) => {
                match $e {
                    Some(d) => d,
                    None => return Ok(Response::Empty),
                }
            };
        };

        if !self.waiting.remove(&id) {
            return Ok(Response::Empty);
        }

        let data = maybe!(val.get_mut("data"));
        let mut map: HashMap<String, String> = maybe!(serde_json::from_value(data.take()).ok());

        Ok(Response::Song(SongInfo {
            artist: maybe!(map.remove("Artist")),
            album: maybe!(map.remove("Album")),
            title: maybe!(map.remove("Title")),
        }))
    }

    fn write_command<S: Into<Value>>(&mut self, cmd: Command<S>) -> Result<u32> {
        let req = Request::new(cmd);
        let json = serde_json::to_string(&req).map_err(|err| {
            Error::new(
                ErrorKind::InvalidData,
                format!("failed to serialize json: {}", err),
            )
        })?;

        trace!("writing: {}", json);
        self.file
            .write(json.as_bytes())
            .and_then(|_| self.file.write_all(b"\n"))
            .and_then(|_| self.file.flush())
            .and_then(|_| Ok(req.request_id))
    }
}

#[derive(Debug)]
enum Response {
    Song(SongInfo),
    Empty,
}

#[derive(Debug)]
enum Message {
    FileLoaded,
    Unpause,
    Unknown,
    Response(u32, Value),
}

impl Message {
    fn parse(line: impl AsRef<str>) -> Result<Self> {
        let line = line.as_ref();
        let val: Value = serde_json::from_str(&line) //
            .map_err(|err| Error::new(ErrorKind::InvalidData, err))?;

        if let Some(id) = val
            .get("request_id")
            .and_then(|r| r.as_u64())
            .map(|r| r as u32)
        {
            return Ok(Message::Response(id, val));
        }

        let ok = Event::try_from_value(&val)
            .and_then(|ev| {
                Some(match ev {
                    Event::Unpause => Message::Unpause,
                    Event::FileLoaded => Message::FileLoaded,
                })
            })
            .unwrap_or(Message::Unknown);
        Ok(ok)
    }
}

#[derive(Serialize, PartialEq, Debug, Clone)]
struct SongInfo {
    artist: String,
    title: String,
    album: String,
}

fn random_name(len: usize) -> String {
    std::iter::repeat_with(|| thread_rng().sample(rand::distributions::Alphanumeric))
        .take(len)
        .collect()
}

fn create_drop_target(tx: SyncSender<PathBuf>) {
    use winit::ControlFlow;
    use winit::Event::WindowEvent;
    use winit::WindowEvent::{CloseRequested, DroppedFile};

    let mut events_loop = winit::EventsLoop::new();
    let _window = winit::WindowBuilder::new()
        .with_title("playlist generator for mpv")
        .with_dimensions((300., 200.).into())
        .build(&events_loop)
        .unwrap();

    events_loop.run_forever(|event| match event {
        WindowEvent {
            event: DroppedFile(path),
            ..
        } => {
            tx.send(path).unwrap();
            ControlFlow::Continue
        }
        WindowEvent {
            event: CloseRequested,
            ..
        } => ControlFlow::Break,
        _ => ControlFlow::Continue,
    });
    trace!("end of drop target")
}

fn main() {
    env_logger::Builder::from_default_env()
        .default_format_timestamp(false)
        .default_format_module_path(false)
        .init();

    let (tx, rx) = sync_channel::<PathBuf>(2);
    let handle = thread::spawn(move || {
        let mut mpv = None;
        while let Ok(path) = rx.recv() {
            mpv.take(); // drop early

            let mut list = vec![];
            Playlist::walk(&mut list, path);
            alphanumeric_sort::sort_path_slice(&mut list);

            match Playlist::make_temp(&list) {
                Ok(playlist) => {
                    mpv.replace(Mpv::new(playlist));
                }
                Err(err) => {
                    error!("cannot make playlist: {}", err);
                    continue;
                }
            }
        }
        trace!("ending recv loop");
    });

    create_drop_target(tx);

    let _ = handle.join();
}

// fn playlist(&mut self) -> Option<Vec<String>> {
//     let count: u64 = self.get("playlist/count")?;
//     let list = (0..count)
//         .map(|i| format!("playlist/{}/filename", i))
//         .filter_map(|f| self.get(&f))
//         .collect();
//     Some(list)
// }
