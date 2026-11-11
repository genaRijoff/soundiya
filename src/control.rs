//! The local command socket the `soundiya` CLI talks to.
use crate::{
    bypass::Handle,
    config,
    mixer::{DeviceMixer, from_percent, percent},
    player::VoiceHandle,
    soundcloud::{Client, Track, clean_token},
    voice::{self, Event, Intent},
};
use std::{
    fs, io,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::{Arc, Mutex, atomic::Ordering},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::mpsc,
    time::Instant,
};

pub struct Context {
    pub client: Arc<Client>,
    pub status: Mutex<String>,
    /// The `STATE` answer: the same as `status`, plus the queue, as one line of JSON.
    pub state: Mutex<String>,
    pub mixer: Arc<DeviceMixer>,
    /// The running bypass proxy, whose strategy can be switched on the fly.
    pub bypass: Handle,
    pub state_dir: PathBuf,
    /// Voice messages go straight to the audio thread (`VOICE <path>`).
    pub voice: VoiceHandle,
    /// "передай …" phrases waiting for the Telegram bot (`MESSAGES`).
    pub mailbox: Arc<crate::mailbox::Mailbox>,
    /// Where Alice reads `SAY` text aloud; None when speech is off.
    pub speaker: Option<std::net::SocketAddr>,
    pub guard: Arc<crate::audio_guard::AudioGuard>,
}

/// One track of a listing (`FIND`, `LIKES`, `SIMILAR`, `CHART`): what the bot's panel shows.
#[derive(serde::Serialize)]
struct Listed<'a> {
    id: u64,
    artist: &'a str,
    title: &'a str,
    duration: u64,
    art: &'a str,
    url: &'a str,
}

/// Tracks as one line of JSON; nothing is played.
fn listing(tracks: &[Track]) -> String {
    let listed: Vec<Listed> = tracks
        .iter()
        .map(|t| Listed {
            id: t.id,
            artist: &t.artist,
            title: &t.title,
            duration: t.duration_ms / 1000,
            art: &t.art,
            url: &t.url,
        })
        .collect();
    serde_json::to_string(&listed).unwrap_or_else(|_| "[]".to_owned())
}

/// The most `PLAYIDS` takes at once.
const MAX_IDS: usize = 100;

/// `PLAYIDS 11,22,33`: SoundCloud track ids, in the order to play them.
fn track_ids(argument: &str) -> Result<Vec<u64>, String> {
    let ids: Vec<u64> = argument
        .split(',')
        .map(|id| id.trim().parse::<u64>())
        .collect::<Result<_, _>>()
        .map_err(|_| "playids takes track ids: 11,22,33".to_owned())?;
    if ids.is_empty() || ids.len() > MAX_IDS {
        return Err(format!("playids takes 1 to {MAX_IDS} track ids"));
    }
    Ok(ids)
}

fn signed_in(context: &Context) -> Result<(), String> {
    if context.client.logged_in() {
        Ok(())
    } else {
        Err("not signed in: run 'soundiya login'".into())
    }
}

/// `SAY <text>`: Alice reads a short answer aloud, after her own answer was cancelled.
fn say(context: &Context, text: &str) -> Result<String, String> {
    let address = context.speaker.ok_or("speech is off")?;
    if text.chars().count() > 300 {
        return Err("say takes up to 300 characters".into());
    }
    let (text, guard) = (text.to_owned(), context.guard.clone());
    // Keeps the music guard from pausing the station's player (and her voice) meanwhile.
    guard.hold(std::time::Duration::from_secs(20));
    tokio::spawn(async move {
        tokio::time::sleep(voice::CANCELS_DONE).await;
        match voice::say(address, &text).await {
            Ok(()) => eprintln!("say: '{text}'"),
            Err(e) => eprintln!("say: {e}"),
        }
        guard.release();
    });
    Ok("say".into())
}

pub async fn serve(sender: mpsc::Sender<Event>, context: Arc<Context>) -> io::Result<()> {
    let _ = fs::remove_file(voice::CONTROL);
    let listener = UnixListener::bind(voice::CONTROL)?;
    fs::set_permissions(voice::CONTROL, fs::Permissions::from_mode(0o600))?;
    loop {
        let (socket, _) = listener.accept().await?;
        let (sender, context) = (sender.clone(), context.clone());
        tokio::spawn(async move {
            let _ = session(socket, sender, context).await;
        });
    }
}

async fn session(
    socket: UnixStream,
    sender: mpsc::Sender<Event>,
    context: Arc<Context>,
) -> io::Result<()> {
    let (reader, mut writer) = socket.into_split();
    let mut line = String::new();
    // A pasted browser header or cURL command is long, but never more than this.
    BufReader::new(reader.take(32 * 1024)).read_line(&mut line).await?;
    let reply = match handle(line.trim(), &sender, &context).await {
        Ok(text) => format!("OK {text}\n"),
        Err(text) => format!("ERROR {text}\n"),
    };
    writer.write_all(reply.as_bytes()).await
}

fn send(sender: &mpsc::Sender<Event>, intent: Intent) -> Result<String, String> {
    sender
        .try_send(Event {
            intent,
            received: Instant::now(),
            from_voice: false,
        })
        .map(|_| "sent".to_owned())
        .map_err(|_| "the receiver is busy".to_owned())
}

async fn handle(
    line: &str,
    sender: &mpsc::Sender<Event>,
    context: &Context,
) -> Result<String, String> {
    let (command, argument) = line.split_once(' ').unwrap_or((line, ""));
    let argument = argument.trim();
    match command.to_ascii_uppercase().as_str() {
        "STATUS" => Ok(context.status.lock().unwrap().clone()),
        "STATE" => Ok(context.state.lock().unwrap().clone()),
        "STRATEGY" if argument.is_empty() => Ok(context.bypass.strategy()),
        "STRATEGY" => {
            // Switch first (it validates the name), then remember the choice for the next start.
            context.bypass.set_strategy(argument)?;
            config::save_strategy(&context.state_dir, argument)
                .map_err(|e| format!("switched, but could not save it to config.json ({e})"))?;
            Ok(format!("strategy {argument}"))
        }
        "PLAY" if !argument.is_empty() => send(sender, Intent::Play(argument.to_owned())),
        "PLAY" => Err("missing search query".into()),
        "FAV" => send(sender, Intent::Favorites),
        "PLAYLIST" if !argument.is_empty() => send(sender, Intent::Playlist(argument.to_owned())),
        "PLAYLIST" => Err("missing playlist name".into()),
        "TOP" | "TRENDING" => {
            let genre = voice::genre_of(&argument.to_lowercase()).unwrap_or("all-music");
            send(sender, Intent::Chart { trending: command.eq_ignore_ascii_case("TRENDING"), genre })
        }
        "RELATED" => send(sender, Intent::Related),
        // Listings for the bot's panel: they only answer, nothing starts playing.
        "FIND" if !argument.is_empty() => {
            let tracks = if crate::is_link(argument) {
                context.client.link(argument, 100).await?.1
            } else {
                context.client.search(argument, 30).await?
            };
            Ok(listing(&tracks))
        }
        "FIND" => Err("missing search query".into()),
        "LIKES" => {
            signed_in(context)?;
            Ok(listing(&context.client.liked_tracks(100).await?))
        }
        "SIMILAR" => {
            let id: u64 = argument
                .parse()
                .map_err(|_| "similar takes a track id".to_owned())?;
            Ok(listing(&context.client.related(id, 30).await?))
        }
        "CHART" => {
            let (kind, genre) = argument.split_once(' ').unwrap_or((argument, ""));
            let trending = match kind.to_ascii_lowercase().as_str() {
                "" | "trending" => true,
                "top" => false,
                _ => return Err("chart takes top or trending, then a genre".into()),
            };
            let genre = voice::genre_of(&genre.to_lowercase()).unwrap_or("all-music");
            Ok(listing(&context.client.chart(trending, genre, 30).await?))
        }
        "PLAYLISTS" => {
            signed_in(context)?;
            let playlists = context.client.my_playlists(50).await?;
            Ok(serde_json::to_string(&playlists).unwrap_or_else(|_| "[]".to_owned()))
        }
        "PLAYIDS" => send(sender, Intent::Tracks(track_ids(argument)?)),
        "OPENPL" => {
            let id: u64 = argument
                .parse()
                .map_err(|_| "openpl takes a playlist id".to_owned())?;
            send(sender, Intent::PlaylistId(id))
        }
        "NOW" => send(sender, Intent::NowPlaying),
        "PAUSE" => send(sender, Intent::Pause),
        "RESUME" => send(sender, Intent::Resume),
        "NEXT" => send(sender, Intent::Next),
        "PREV" => send(sender, Intent::Previous),
        "SEEK" => {
            let seconds: i32 = argument.parse().map_err(|_| "seek takes seconds: 20 or -10".to_owned())?;
            if seconds == 0 || seconds.unsigned_abs() > 3600 {
                return Err("seek takes seconds: 20 or -10 (up to an hour)".into());
            }
            send(sender, Intent::Seek(seconds))
        }
        "RESTART" => send(sender, Intent::Restart),
        "VOICE" if !argument.is_empty() => context.voice.play_file(argument),
        "VOICE" => Err("voice takes a file path".into()),
        "MESSAGES" => Ok(context.mailbox.collect()),
        "SAY" if !argument.is_empty() => say(context, argument),
        "SAY" => Err("say takes the text".into()),
        "JUMP" => match argument.parse::<usize>() {
            Ok(n) if n >= 1 => send(sender, Intent::Jump(n - 1)),
            _ => Err("jump takes a queue position: 1, 2, ...".into()),
        },
        "SLEEP" => match argument.to_ascii_lowercase().as_str() {
            "off" | "cancel" => send(sender, Intent::CancelSleep),
            minutes => match minutes.parse::<u32>() {
                Ok(m) if (1..=720).contains(&m) => send(sender, Intent::SleepIn(m * 60)),
                _ => Err("sleep takes minutes (1-720) or off".into()),
            },
        },
        "REPEAT" => match argument.to_ascii_lowercase().as_str() {
            "" | "on" => send(sender, Intent::Loop(true)),
            "off" => send(sender, Intent::Loop(false)),
            _ => Err("repeat takes on or off".into()),
        },
        "VOLUME" => {
            let current = percent(context.mixer.desired.load(Ordering::Acquire));
            let percent = volume_target(argument, current)
                .ok_or("volume is 0-100, or a step such as +10 or -10")?;
            context.mixer.set(from_percent(percent));
            Ok(format!("volume {percent}%"))
        }
        "LOGIN" => {
            let token = clean_token(argument)
                .ok_or("no SoundCloud token found in the pasted text")?;
            context.client.set_token(Some(token))?;
            match context.client.username().await {
                Ok(name) => Ok(format!("signed in as {name}")),
                Err(e) => {
                    let _ = context.client.set_token(None);
                    Err(format!("SoundCloud did not accept the token ({e})"))
                }
            }
        }
        "LOGOUT" => {
            context.client.set_token(None)?;
            Ok("signed out".into())
        }
        _ => Err("unknown command".into()),
    }
}

/// `VOLUME 40` sets 40 %; `VOLUME +10` and `VOLUME -10` step from `current`, within 0-100.
fn volume_target(argument: &str, current: u16) -> Option<u16> {
    let step = |n: &str| n.parse::<u16>().ok().filter(|&n| n <= 100);
    if let Some(n) = argument.strip_prefix('+') {
        Some((current + step(n)?).min(100))
    } else if let Some(n) = argument.strip_prefix('-') {
        Some(current.saturating_sub(step(n)?))
    } else {
        step(argument)
    }
}

#[cfg(test)]
mod tests {
    use super::{track_ids, volume_target};
    #[test]
    fn playids_takes_a_bounded_list_of_numbers() {
        assert_eq!(track_ids("11, 22,33"), Ok(vec![11, 22, 33]));
        assert_eq!(track_ids("7"), Ok(vec![7]));
        for bad in ["", "1,,2", "1,x", "-1", "1 2"] {
            assert!(track_ids(bad).is_err(), "{bad}");
        }
        let many: Vec<String> = (1..=101).map(|n| n.to_string()).collect();
        assert!(track_ids(&many.join(",")).is_err());
    }
    #[test]
    fn volume_takes_a_level_or_a_step() {
        assert_eq!(volume_target("40", 70), Some(40));
        assert_eq!(volume_target("+10", 70), Some(80));
        assert_eq!(volume_target("+10", 95), Some(100));
        assert_eq!(volume_target("-10", 5), Some(0));
        assert_eq!(volume_target("-10", 70), Some(60));
        for bad in ["", "101", "+", "-x", "+200", "ten"] {
            assert_eq!(volume_target(bad, 50), None, "{bad}");
        }
    }
}
