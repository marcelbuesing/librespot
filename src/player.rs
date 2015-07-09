use portaudio;
use vorbis;
use std::sync::{mpsc, Mutex, Arc, Condvar, MutexGuard};
use std::thread;

use metadata::TrackRef;
use session::Session;
use audio_decrypt::AudioDecrypt;
use util::{self, SpotifyId, Subfile};
use spirc::{SpircState, SpircDelegate, PlayStatus};

pub struct Player<'s> {
    state: Arc<(Mutex<PlayerState>, Condvar)>,

    commands: mpsc::Sender<PlayerCommand>,

    #[allow(dead_code)]
    thread: thread::JoinGuard<'s, ()>,
}

pub struct PlayerState {
    status: PlayStatus,
    position_ms: u32,
    position_measured_at: i64,
    update_time: i64
}

struct PlayerInternal<'s> {
    state: Arc<(Mutex<PlayerState>, Condvar)>,

    session: &'s Session,
    commands: mpsc::Receiver<PlayerCommand>,
}

enum PlayerCommand {
    Load(SpotifyId, bool, u32),
    Play,
    Pause,
    Stop,
    Seek(u32)
}

impl <'s> Player<'s> {
    pub fn new(session: &Session) -> Player {
        let (cmd_tx, cmd_rx) = mpsc::channel();

        let state = Arc::new((Mutex::new(PlayerState {
            status: PlayStatus::kPlayStatusStop,
            position_ms: 0,
            position_measured_at: 0,
            update_time: util::now_ms(),
        }), Condvar::new()));

        let internal = PlayerInternal {
            session: session,
            commands: cmd_rx,
            state: state.clone()
        };

        Player {
            commands: cmd_tx,
            state: state,
            thread: thread::scoped(move || {
                internal.run()
            })
        }
    }

    fn command(&self, cmd: PlayerCommand) {
        self.commands.send(cmd).unwrap();
    }
}

impl <'s> PlayerInternal<'s> {
    fn run(self) {
        portaudio::initialize().unwrap();

        let stream = portaudio::stream::Stream::<i16>::open_default(
                0,
                2,
                44100.0,
                portaudio::stream::FRAMES_PER_BUFFER_UNSPECIFIED,
                None
                ).unwrap();

        let mut decoder = None;

        loop {
            match self.commands.try_recv() {
                Ok(PlayerCommand::Load(id, play, position)) => {
                    println!("Load");
                    let mut h = self.state.0.lock().unwrap();
                    if h.status == PlayStatus::kPlayStatusPlay {
                        stream.stop().unwrap();
                    }
                    h.status = PlayStatus::kPlayStatusLoading;
                    h.position_ms = position;
                    h.position_measured_at = util::now_ms();
                    h.update_time = util::now_ms();
                    drop(h);

                    let track : TrackRef = self.session.metadata(id);
                    let file_id = *track.wait().unwrap().files.first().unwrap();
                    let key = self.session.audio_key(track.id(), file_id).into_inner();
                    decoder = Some(
                        vorbis::Decoder::new(
                        Subfile::new(
                        AudioDecrypt::new(key,
                        self.session.audio_file(file_id)), 0xa7)).unwrap());
                    decoder.as_mut().unwrap().time_seek(position as f64 / 1000f64).unwrap();

                    let mut h = self.state.0.lock().unwrap();
                    h.status = if play {
                        stream.start().unwrap();
                        PlayStatus::kPlayStatusPlay
                    } else {
                        PlayStatus::kPlayStatusPause
                    };
                    h.position_ms = position;
                    h.position_measured_at = util::now_ms();
                    h.update_time = util::now_ms();
                    println!("Load Done");
                }
                Ok(PlayerCommand::Seek(ms)) => {
                    let mut h = self.state.0.lock().unwrap();
                    decoder.as_mut().unwrap().time_seek(ms as f64 / 1000f64).unwrap();
                    h.position_ms = (decoder.as_mut().unwrap().time_tell().unwrap() * 1000f64) as u32;
                    h.position_measured_at = util::now_ms();
                    h.update_time = util::now_ms();
                },
                Ok(PlayerCommand::Play) => {
                    println!("Play");
                    let mut h = self.state.0.lock().unwrap();
                    h.status = PlayStatus::kPlayStatusPlay;
                    h.update_time = util::now_ms();

                    stream.start().unwrap();
                },
                Ok(PlayerCommand::Pause) => {
                    let mut h = self.state.0.lock().unwrap();
                    h.status = PlayStatus::kPlayStatusPause;
                    h.update_time = util::now_ms();

                    stream.stop().unwrap();
                },
                Ok(PlayerCommand::Stop) => {
                    let mut h = self.state.0.lock().unwrap();
                    if h.status == PlayStatus::kPlayStatusPlay {
                        stream.stop().unwrap();
                    }

                    h.status = PlayStatus::kPlayStatusPause;
                    h.update_time = util::now_ms();
                    decoder = None;
                },
                Err(..) => (),
            }

            if self.state.0.lock().unwrap().status == PlayStatus::kPlayStatusPlay {
                match decoder.as_mut().unwrap().packets().next().unwrap() {
                    Ok(packet) => {
                        match stream.write(&packet.data) {
                            Ok(_) => (),
                            Err(portaudio::PaError::OutputUnderflowed)
                                => eprintln!("Underflow"),
                            Err(e) => panic!("PA Error {}", e)
                        };
                    },
                    Err(vorbis::VorbisError::Hole) => (),
                    Err(e) => panic!("Vorbis error {:?}", e)
                }

                let mut h = self.state.0.lock().unwrap();
                h.position_ms = (decoder.as_mut().unwrap().time_tell().unwrap() * 1000f64) as u32;
                h.position_measured_at = util::now_ms();
            }
        }

        drop(stream);

        portaudio::terminate().unwrap();
    }
}

impl <'s> SpircDelegate for Player<'s> {
    type State = PlayerState;

    fn load(&self, track: SpotifyId,
            start_playing: bool, position_ms: u32) {
        self.command(PlayerCommand::Load(track, start_playing, position_ms));
    }

    fn play(&self) {
        self.command(PlayerCommand::Play)
    }

    fn pause(&self) {
        self.command(PlayerCommand::Pause)
    }

    fn stop(&self) {
        self.command(PlayerCommand::Stop)
    }

    fn seek(&self, position_ms: u32) {
        self.command(PlayerCommand::Seek(position_ms));
    }

    fn state(&self) -> MutexGuard<Self::State> {
        self.state.0.lock().unwrap()
    }

    fn wait_update<'a>(&'a self, guard: MutexGuard<'a, Self::State>)
        -> MutexGuard<'a, Self::State> {
        self.state.1.wait(guard).unwrap()
    }
}

impl SpircState for PlayerState {
    fn status(&self) -> PlayStatus {
        return self.status;
    }

    fn position(&self) -> (u32, i64) {
        return (self.position_ms, self.position_measured_at);
    }

    fn update_time(&self) -> i64 {
        return self.update_time;
    }
}

