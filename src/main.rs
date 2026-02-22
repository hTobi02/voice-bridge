use std::io::Seek;
use std::{ io::Read, mem::size_of, sync::Arc, time::Duration };
use byte_slice_cast::AsByteSlice;
use serde::Deserialize;
use serenity::prelude::GatewayIntents;
use tsclientlib::{ ClientId, Connection, DisconnectOptions, Identity, StreamItem };
use tsproto_packets::packets::{ AudioData, CodecType, OutAudio, OutPacket };
use audiopus::coder::Encoder;
use futures::prelude::*;
use slog::{ debug, o, Drain, Logger };
use tokio::task;
use tokio::sync::Mutex;
use anyhow::{ bail, Result };
use symphonia::core::io::MediaSource;

use std::collections::VecDeque;
use std::sync::Mutex as StdMutex;

mod discord;
mod discord_audiohandler;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct ConnectionId(u64);

use songbird::{ SerenityInit, Songbird };
use songbird::Config as DriverConfig;

use serenity::prelude::TypeMapKey;
use serenity::client::Client;

#[derive(Debug, Deserialize)]
struct Config {
    discord_token: String,
    teamspeak_server: String,
    teamspeak_identity: String,
    teamspeak_server_password: Option<String>,
    teamspeak_channel_id: Option<u64>,
    teamspeak_channel_name: Option<String>,
    teamspeak_channel_password: Option<String>,
    teamspeak_name: Option<String>,
    verbose: i32,
    volume: f32,
}

struct ListenerHolder;

type AudioBufferDiscord = Arc<Mutex<discord_audiohandler::AudioHandler<u32>>>;

type TsVoiceId = (ConnectionId, ClientId);
type TsAudioHandler = tsclientlib::audio::AudioHandler<TsVoiceId>;

#[derive(Clone)]
struct TsToDiscordPipeline {
    data: Arc<std::sync::Mutex<TsAudioHandler>>,
}

impl Seek for TsToDiscordPipeline {
    fn seek(&mut self, _: std::io::SeekFrom) -> std::io::Result<u64> {
        Err(std::io::Error::new(std::io::ErrorKind::Other, "source does not support seeking"))
    }
}

impl MediaSource for TsToDiscordPipeline {
    fn is_seekable(&self) -> bool {
        false
    }

    fn byte_len(&self) -> Option<u64> {
        None
    }
}

impl TsToDiscordPipeline {
    pub fn new(_logger: Logger) -> Self {
        Self {
            data: Arc::new(std::sync::Mutex::new(TsAudioHandler::new())),
        }
    }
}

impl Read for TsToDiscordPipeline {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let samples_requested = buf.len() / size_of::<f32>();
        let mut audio_buffer: Vec<f32> = vec![0.0; samples_requested];

        {
            let mut lock = self.data.lock().expect("Can't lock ts voice buffer!");
            lock.fill_buffer(&mut audio_buffer);
        }

        let max_sample = audio_buffer
            .iter()
            .map(|s| s.abs())
            .fold(0.0f32, f32::max);
        if max_sample > 0.001 {
            tracing::debug!(
                "TS→Discord: max sample: {:.4}, samples requested: {}",
                max_sample,
                samples_requested
            );
        }

        const GAIN: f32 = 3.0;
        for sample in &mut audio_buffer {
            *sample *= GAIN;
            *sample = sample.clamp(-1.0, 1.0);
        }

        let slice = audio_buffer.as_byte_slice();
        buf.copy_from_slice(slice);

        Ok(buf.len())
    }
}

impl TypeMapKey for ListenerHolder {
    type Value = (TsToDiscordPipeline, AudioBufferDiscord);
}

struct BufferedPipeline {
    inner: TsToDiscordPipeline,
    buffer: Arc<StdMutex<VecDeque<u8>>>,
}

impl BufferedPipeline {
    fn new(inner: TsToDiscordPipeline) -> Self {
        Self {
            inner,
            buffer: Arc::new(StdMutex::new(VecDeque::with_capacity(32768))),
        }
    }

    fn start_filler(&self) {
        let inner = self.inner.clone();
        let buffer = self.buffer.clone();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(20));
            loop {
                interval.tick().await;

                let mut temp_buf = vec![0u8; 1920 * 4];

                let n = {
                    let mut reader = inner.clone();
                    match std::io::Read::read(&mut reader, &mut temp_buf) {
                        Ok(n) => n,
                        Err(e) => {
                            tracing::warn!("Buffer filler read error: {}", e);
                            continue;
                        }
                    }
                };

                if n > 0 {
                    let mut buf_lock = buffer.lock().unwrap();
                    buf_lock.extend(&temp_buf[..n]);

                    while buf_lock.len() > 48000 * 2 * 4 {
                        buf_lock.drain(..1920 * 4);
                    }
                }
            }
        });
    }
}

impl Read for BufferedPipeline {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut buffer_lock = self.buffer.lock().unwrap();
        let available = buffer_lock.len().min(buf.len());

        for i in 0..available {
            buf[i] = buffer_lock.pop_front().unwrap();
        }

        if available == 0 {
            buf.fill(0);
            return Ok(buf.len());
        }

        Ok(available)
    }
}

impl Seek for BufferedPipeline {
    fn seek(&mut self, _: std::io::SeekFrom) -> std::io::Result<u64> {
        Err(std::io::Error::new(std::io::ErrorKind::Other, "source does not support seeking"))
    }
}

impl MediaSource for BufferedPipeline {
    fn is_seekable(&self) -> bool {
        false
    }

    fn byte_len(&self) -> Option<u64> {
        None
    }
}

impl Clone for BufferedPipeline {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            buffer: self.buffer.clone(),
        }
    }
}

const TICK_TIME: u64 = 20;
const FRAME_SIZE_MS: usize = 20;
const SAMPLE_RATE: usize = 48000;
const STEREO_20MS: usize = (SAMPLE_RATE * 2 * FRAME_SIZE_MS) / 1000;
const MAX_OPUS_FRAME_SIZE: usize = 1275;

const RUST_LOG: &'static str = "RUST_LOG";

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring
        ::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    if std::env::var(RUST_LOG).is_err() {
        std::env::set_var(
            RUST_LOG,
            #[cfg(debug_assertions)] "info,voice_bridge=debug",
            #[cfg(not(debug_assertions))] "error,tsclientlib=error,songbird=error,voice_bridge=info"
        );
    }
    tracing_subscriber::fmt::init();

    let config: Config = toml
        ::from_str(&std::fs::read_to_string(".credentials.toml").expect("No config file!"))
        .expect("Invalid config");

    let logger = {
        let decorator = slog_term::TermDecorator::new().build();
        let drain = slog_term::CompactFormat::new(decorator).build().fuse();
        let drain = slog_envlogger::new(drain).fuse();
        let drain = slog_async::Async::new(drain).build().fuse();
        Logger::root(drain, o!())
    };

    // Create Poise framework
    let framework = poise::Framework
        ::builder()
        .options(poise::FrameworkOptions {
            commands: vec![
                discord::join(),
                discord::leave(),
                discord::deafen(),
                discord::undeafen(),
                discord::mute(),
                discord::unmute(),
                discord::ping(),
                discord::volume(),
                discord::volume_check(),
                discord::reset_audio()
            ],
            ..Default::default()
        })
        .setup(|ctx, _ready, framework| {
            Box::pin(async move {
                poise::builtins::register_globally(ctx, &framework.options().commands).await?;
                Ok(discord::Data {})
            })
        })
        .build();

    let songbird = Songbird::serenity();
    songbird.set_config(DriverConfig::default().decode_mode(songbird::driver::DecodeMode::Decode));

    // Store songbird manager for graceful shutdown
    let songbird_manager_shutdown = songbird.clone();

    let intents =
        GatewayIntents::GUILD_MESSAGES |
        GatewayIntents::MESSAGE_CONTENT |
        GatewayIntents::GUILD_VOICE_STATES;

    let mut client = Client::builder(&config.discord_token, intents)
        .event_handler(discord::Handler)
        .framework(framework)
        .register_songbird_with(songbird.into()).await
        .expect("Err creating client");

    let ts_voice_logger = logger.new(o!("pipeline" => "voice-ts"));
    let teamspeak_voice_handler = TsToDiscordPipeline::new(ts_voice_logger);

    let discord_voice_logger = logger.new(o!("pipeline" => "voice-discord"));
    let mut handler = discord_audiohandler::AudioHandler::new(discord_voice_logger);
    handler.set_global_volume(config.volume);
    let discord_voice_buffer: AudioBufferDiscord = Arc::new(Mutex::new(handler));

    {
        let mut data = client.data.write().await;
        data.insert::<ListenerHolder>((
            teamspeak_voice_handler.clone(),
            discord_voice_buffer.clone(),
        ));
    }

    let client_handle = tokio::spawn(async move {
        let _ = client.start().await.map_err(|why| println!("Client ended: {:?}", why));
    });

    let con_id = ConnectionId(0);

    let mut con_config = Connection::build(config.teamspeak_server)
        .log_commands(config.verbose >= 1)
        .log_packets(config.verbose >= 2)
        .log_udp_packets(config.verbose >= 3);

    if let Some(name) = config.teamspeak_name {
        con_config = con_config.name(name);
    }
    if let Some(channel) = config.teamspeak_channel_id {
        con_config = con_config.channel_id(tsclientlib::ChannelId(channel));
    }
    if let Some(channel) = config.teamspeak_channel_name {
        con_config = con_config.channel(channel);
    }
    if let Some(password) = config.teamspeak_server_password {
        con_config = con_config.password(password);
    }
    if let Some(password) = config.teamspeak_channel_password {
        con_config = con_config.channel_password(password);
    }

    let id = Identity::new_from_str(&config.teamspeak_identity).expect("Can't load identity!");
    let con_config = con_config.identity(id);

    let mut con = con_config.connect()?;

    let r = con
        .events()
        .try_filter(|e| future::ready(matches!(e, StreamItem::BookEvents(_))))
        .next().await;
    if let Some(r) = r {
        r?;
    }

    let encoder = audiopus::coder::Encoder
        ::new(
            audiopus::SampleRate::Hz48000,
            audiopus::Channels::Stereo,
            audiopus::Application::Voip
        )
        .expect("Can't construct encoder!");
    let encoder = Arc::new(Mutex::new(encoder));

    let mut interval = tokio::time::interval(Duration::from_millis(TICK_TIME));

    loop {
        let events = con.events().try_for_each(|e| async {
            if let StreamItem::Audio(packet) = e {
                let from = ClientId(match packet.data().data() {
                    AudioData::S2C { from, .. } => *from,
                    AudioData::S2CWhisper { from, .. } => *from,
                    _ => panic!("Can only handle S2C packets but got a C2S packet"),
                });

                let mut ts_voice = teamspeak_voice_handler.data
                    .lock()
                    .expect("Can't lock ts audio buffer!");
                if let Err(e) = ts_voice.handle_packet((con_id, from), packet) {
                    debug!(logger, "Failed to handle TS_Voice packet"; "error" => %e);
                }
            }
            Ok(())
        });

        tokio::select! {
            _send = interval.tick() => {
                let start = std::time::Instant::now();
                if let Some(processed) = process_discord_audio(&discord_voice_buffer,&encoder).await {
                    con.send_audio(processed)?;
                    let dur = start.elapsed();
                    if dur >= Duration::from_millis(1) {
                        tracing::debug!("Audio pipeline took {}ms",dur.as_millis());
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => { 
                println!("Received shutdown signal...");
                break; 
            }
            r = events => {
                r?;
                bail!("Disconnected");
            }
        }
    }

    // Graceful shutdown
    println!("Disconnecting from Discord voice channels...");
    let guild_ids: Vec<_> = songbird_manager_shutdown
        .iter()
        .map(|(guild_id, _)| guild_id)
        .collect();

    for guild_id in guild_ids {
        println!("  Leaving guild {}...", guild_id);
        if let Err(e) = songbird_manager_shutdown.remove(guild_id).await {
            eprintln!("  Error leaving guild {}: {:?}", guild_id, e);
        }
    }

    // Give a moment for Discord to process the leave
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Abort the client task
    client_handle.abort();
    println!("Discord client stopped");

    println!("Disconnecting from TeamSpeak...");
    con.disconnect(DisconnectOptions::new())?;
    con.events().for_each(|_| future::ready(())).await;
    println!("Shutdown complete!");
    Ok(())
}

async fn process_discord_audio(
    voice_buffer: &AudioBufferDiscord,
    encoder: &Arc<Mutex<Encoder>>
) -> Option<OutPacket> {
    let mut data = [0.0; STEREO_20MS];
    {
        let mut lock = voice_buffer.lock().await;
        lock.fill_buffer(&mut data);
    }
    let mut encoded = [0; MAX_OPUS_FRAME_SIZE];
    let encoder_c = encoder.clone();

    let res = task
        ::spawn_blocking(move || {
            let start = std::time::Instant::now();
            let lock = encoder_c.try_lock().expect("Can't reach encoder!");
            let length = match lock.encode_float(&data, &mut encoded) {
                Err(e) => {
                    tracing::error!("Failed to encode voice: {}", e);
                    return None;
                }
                Ok(size) => size,
            };

            let duration = start.elapsed().as_millis();
            if duration > 2 {
                tracing::warn!("Took too {}ms for processing audio!", duration);
            }

            Some(
                OutAudio::new(
                    &(AudioData::C2S {
                        id: 0,
                        codec: CodecType::OpusMusic,
                        data: &encoded[..length],
                    })
                )
            )
        }).await
        .expect("Join error for audio processing thread!");
    res
}
