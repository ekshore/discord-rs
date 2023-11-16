#[cfg(feature = "voice")]
use std::collections::HashMap;
use std::sync::mpsc;

use futures_util::stream::StreamExt;
use rand::Rng;
use websocket::client::{Client, Receiver, Sender};
use websocket::stream::WebSocketStream;

use serde_json;

use tokio_tungstenite::connect_async;

use crate::internal::Status;
use crate::model::*;
use crate::sleep_ms;
#[cfg(feature = "voice")]
use crate::voice::VoiceConnection;
use crate::Timer;
use crate::WebSocketRX;
use crate::{AsyncRecieverExt, AsyncSenderExt, Error, ReceiverExt, Result, SenderExt};

const GATEWAY_VERSION: u64 = 6;

#[cfg(feature = "voice")]
macro_rules! finish_connection {
	($($name1:ident: $val1:expr),*; $($name2:ident: $val2:expr,)*) => { Connection {
		$($name1: $val1,)*
		$($name2: $val2,)*
	}}
}
#[cfg(not(feature = "voice"))]
macro_rules! finish_connection {
	($($name1:ident: $val1:expr),*; $($name2:ident: $val2:expr,)*) => { Connection {
		$($name1: $val1,)*
	}}
}

#[derive(Clone)]
pub struct ConnectionBuilder<'a> {
	base_url: String,
	token: &'a str,

	//large_threshold: Option<u32>,
	shard: Option<[u8; 2]>,
	intents: Option<Intents>,
	// TODO: presence
}

impl<'a> ConnectionBuilder<'a> {
	pub(crate) fn new(base_url: String, token: &'a str) -> Self {
		ConnectionBuilder {
			base_url,
			token,
			//large_threshold: None,
			shard: None,
			intents: None,
		}
	}

	/// Connect to only a specific shard.
	///
	/// The `shard_id` is indexed at 0 while `total_shards` is indexed at 1.
	pub fn with_shard(&mut self, shard_id: u8, total_shards: u8) -> &mut Self {
		self.shard = Some([shard_id, total_shards]);
		self
	}

	pub fn with_intents(&mut self, intents: Intents) -> &mut Self {
		self.intents = Some(intents);
		self
	}

	/// Establish a websocket connection over which events can be received.
	///
	/// Also returns the `ReadyEvent` sent by Discord upon establishing the
	/// connection, which contains the initial state as seen by the client.
	pub fn connect(&self) -> Result<(Connection, ReadyEvent)> {
		let identify = self.build_idenity();
		Connection::__connect(&self.base_url, self.token, identify)
	}

	pub async fn connect_async(&self) -> Result<(AsyncConnection, ReadyEvent)> {
		let identify = self.build_idenity();
		AsyncConnection::__connect(&self.base_url, self.token, identify).await
	}

	fn build_idenity(&self) -> serde_json::Value {
		let mut d = json! {{
			"token": self.token,
			"properties": {
				"$os": ::std::env::consts::OS,
				"$browser": "Discord library for Rust",
				"$device": "discord-rs",
				"$referring_domain": "",
				"$referrer": "",
			},
			"large_threshold": 250,
			"compress": true,
			"v": GATEWAY_VERSION,
		}};
		if let Some(info) = self.shard {
			d["shard"] = json![[info[0], info[1]]];
		}
		if let Some(intents) = self.intents {
			d["intents"] = intents.bits().into();
		}
		let identify = json! {{
			"op": 2,
			"d": d
		}};
		identify
	}
}

/// Asynchronous Websocket connection
#[allow(dead_code)]
pub struct AsyncConnection {
	keepalive_channel: tokio::sync::mpsc::Sender<Status>,
	receiver: WebSocketRX,
	#[cfg(feature = "voice")]
	voice_handles: HashMap<Option<ServerId>, VoiceConnection>,
	#[cfg(feature = "voice")]
	user_id: UserId,
	gateway_resume_url: String,
	ws_url: String,
	token: String,
	session_id: Option<String>,
	last_sequence: u64,
	identify: serde_json::Value,
}

impl AsyncConnection {
	/// Establish a connection to the Discord websocket servers.
	///
	/// Returns both the `Connection` and the `ReadyEvent` which is always the
	/// first event received and contains initial state information.
	///
	/// Usually called internally by `Discord::connect`, which provides both
	/// the token and URL and an optional user-given shard ID and total shard
	/// count.
	pub async fn new(
		base_url: &str,
		token: &str,
		shard: Option<[u8; 2]>,
	) -> Result<(AsyncConnection, ReadyEvent)> {
		ConnectionBuilder {
			shard,
			..ConnectionBuilder::new(base_url.to_owned(), token)
		}
		.connect_async()
		.await
	}

	async fn __connect(
		base_url: &str,
		token: &str,
		identify: serde_json::Value,
	) -> Result<(AsyncConnection, ReadyEvent)> {
		trace!("Gateway: {}", base_url);
		// establish the websocket connection
		let url = build_gateway_url_v2(base_url)?;

		let (socket, _res) = connect_async(url).await?;
		let (mut socket_tx, mut socket_rx) = socket.split();

		let heartbeat_interval = match socket_rx.recv_json(GatewayEvent::decode).await? {
			GatewayEvent::Hello(interval) => Ok(interval),
			other => {
				debug!("Unexpected event: {:?}", other);
				Err(Error::Protocol("Expected Hello during handshake"))
			}
		}?;

		socket_tx.send_json(&identify).await?;
		let (keepalive_channel, rx) = tokio::sync::mpsc::channel(10);
		tokio::spawn(keepalive_async(heartbeat_interval, socket_tx, rx));

		let sequence;
		let ready;
		match socket_rx.recv_json(GatewayEvent::decode).await? {
			GatewayEvent::Dispatch(seq, Event::Ready(event)) => {
				sequence = seq;
				ready = event;
			}
			GatewayEvent::InvalidateSession => {
				debug!("Session invalidated, reidentifying");
				let _ = keepalive_channel
					.send(Status::SendMessage(identify.clone()))
					.await
					.map_err(|e| {
						debug!("Error sending Message down keepalive channel: {:?}", e);
						Error::Other("Error sending message down keepalive channel")
					})?;
				match socket_rx.recv_json(GatewayEvent::decode).await? {
                    GatewayEvent::Dispatch(seq, Event::Ready(event)) => {
                        sequence = seq;
                        ready = event;
                    }
                    GatewayEvent::InvalidateSession => {
                        return Err(Error::Protocol(
                                "Invalid session during handshake. \
                                Double-check your token or consider waiting 5 seconds between starting shards.",
                                ))
                    }
                    other => {
                        debug!("Unexpected event: {:?}", other);
                        return Err(Error::Protocol("Expected Ready during handshake"));
                    }
                }
			}
			other => {
				debug!("Unexpected event: {:?}", other);
				return Err(Error::Protocol(
					"Expected Ready or InvalidateSession during handshake",
				));
			}
		}
		if ready.version != GATEWAY_VERSION {
			warn!(
				"Got protocol version {} instead of {}",
				ready.version, GATEWAY_VERSION
			);
		}
		let session_id = ready.session_id.clone();
		let resume_url = ready.resume_url.clone();
		Ok((
			AsyncConnection {
				keepalive_channel,
				receiver: socket_rx,
				gateway_resume_url: resume_url,
				ws_url: base_url.to_owned(),
				token: token.to_owned(),
				session_id: Some(session_id),
				last_sequence: sequence,
				identify,
				// voice only
				#[cfg(feature = "voice")]
				user_id: ready.user.id,
				#[cfg(feature = "voice")]
				voice_handles: HashMap::new(),
			},
			ready,
		))
	}

	/// Change the game information that this client reports as playing.
	pub async fn set_game(&self, game: Option<Game>) {
		self.set_presence(game, OnlineStatus::Online, false).await;
	}

	/// Set the client to be playing this game, with defaults used for any
	/// extended information.
	pub async fn set_game_name(&self, name: String) {
		self.set_presence(Some(Game::playing(name)), OnlineStatus::Online, false)
			.await;
	}

	/// Sets the active presence of the client, including game and/or status
	/// information.
	///
	/// `afk` will help Discord determine where to send notifications.
	pub async fn set_presence(&self, game: Option<Game>, status: OnlineStatus, afk: bool) {
		let status = match status {
			OnlineStatus::Offline => OnlineStatus::Invisible,
			other => other,
		};
		let game = match game {
			Some(Game {
				kind: GameType::Streaming,
				url: Some(url),
				name,
			}) => json! {{ "type": GameType::Streaming, "url": url, "name": name }},
			Some(game) => json! {{ "name": game.name, "type": GameType::Playing }},
			None => json!(null),
		};
		let msg = json! {{
			"op": 3,
			"d": {
				"afk": afk,
				"since": 0,
				"status": status,
				"game": game,
			}
		}};
		let _ = self.keepalive_channel.send(Status::SendMessage(msg)).await;
	}

	/// Get a handle to the voice connection for a server.
	///
	/// Pass `None` to get the handle for group and one-on-one calls.
	#[cfg(feature = "voice")]
	pub fn voice(&mut self, server_id: Option<ServerId>) -> &mut VoiceConnection {
		let AsyncConnection {
			ref mut voice_handles,
			user_id,
			ref keepalive_channel,
			..
		} = *self;
		voice_handles.entry(server_id).or_insert_with(|| {
			unimplemented!()
			//VoiceConnection::__new(server_id, user_id, keepalive_channel.clone())
		})
	}

	/// Drop the voice connection for a server, forgetting all settings.
	///
	/// Calling `.voice(server_id).disconnect()` will disconnect from voice but retain the mute
	/// and deaf status, audio source, and audio receiver.
	///
	/// Pass `None` to drop the connection for group and one-on-one calls.
	#[cfg(feature = "voice")]
	pub fn drop_voice(&mut self, server_id: Option<ServerId>) {
		self.voice_handles.remove(&server_id);
	}

	/// Receive an event over the websocket, blocking until one is available.
	pub async fn recv_event(&mut self) -> Result<Event> {
		loop {
			match self.receiver.recv_json(GatewayEvent::decode).await {
				Err(Error::Tungstenite(err)) => {
					warn!("Websocket error, reconnecting: {:?}", err);
					// Try resuming if we haven't received an InvalidateSession
					if let Some(session_id) = self.session_id.clone() {
						match self.resume(session_id).await {
							Ok(event) => return Ok(event),
							Err(e) => debug!("Failed to resume: {:?}", e),
						}
					}
					// If resuming didn't work, reconnect
					return self.reconnect().await.map(Event::Ready);
				}
				Err(Error::Closed(num, message)) => {
					debug!("Closure, reconnecting: {:?}: {}", num, message);
					// Try resuming if we haven't received a 4006 or an InvalidateSession
					if num != Some(4006) {
						if let Some(session_id) = self.session_id.clone() {
							match self.resume(session_id).await {
								Ok(event) => return Ok(event),
								Err(e) => debug!("Failed to resume: {:?}", e),
							}
						}
					}
					// If resuming didn't work, reconnect
					return self.reconnect().await.map(Event::Ready);
				}
				Err(error) => return Err(error),
				Ok(GatewayEvent::Hello(interval)) => {
					debug!("Mysterious late-game hello: {}", interval);
				}
				Ok(GatewayEvent::Dispatch(sequence, event)) => {
					self.last_sequence = sequence;
					let _ = self.keepalive_channel.send(Status::Sequence(sequence));
					#[cfg(feature = "voice")]
					{
						if let Event::VoiceStateUpdate(server_id, ref voice_state) = event {
							self.voice(server_id).__update_state(voice_state);
						}
						if let Event::VoiceServerUpdate {
							server_id,
							ref endpoint,
							ref token,
							..
						} = event
						{
							self.voice(server_id).__update_server(endpoint, token);
						}
					}
					return Ok(event);
				}
				Ok(GatewayEvent::Heartbeat(sequence)) => {
					debug!("Heartbeat received with seq {}", sequence);
					let map = json! {{
						"op": 1,
						"d": sequence,
					}};
					let _ = self.keepalive_channel.send(Status::SendMessage(map)).await;
				}
				Ok(GatewayEvent::HeartbeatAck) => {}
				Ok(GatewayEvent::Reconnect) => {
					return self.reconnect().await.map(Event::Ready);
				}
				Ok(GatewayEvent::InvalidateSession) => {
					debug!("Session invalidated, reidentifying");
					self.session_id = None;
					let _ = self
						.keepalive_channel
						.send(Status::SendMessage(self.identify.clone()))
						.await;
				}
			}
		}
	}

	/// Reconnect after receiving an OP7 RECONNECT
	async fn reconnect(&mut self) -> Result<ReadyEvent> {
		sleep_ms(1000);
		self.keepalive_channel
			.send(Status::Aborted)
			.await
			.expect("Could not stop the keepalive thread, there will be a thread leak.");
		trace!("Reconnecting...");
		// Make two attempts on the current known gateway URL
		for _ in 0..2 {
			if let Ok((conn, ready)) =
				AsyncConnection::__connect(&self.ws_url, &self.token, self.identify.clone()).await
			{
				::std::mem::replace(self, conn).raw_shutdown();
				self.session_id = Some(ready.session_id.clone());
				return Ok(ready);
			}
			sleep_ms(1000);
		}

		// If those fail, hit REST for a new endpoint
		let url = crate::Discord::from_token_raw(self.token.to_owned()).get_gateway_url()?;
		let (conn, ready) =
			AsyncConnection::__connect(&url, &self.token, self.identify.clone()).await?;
		::std::mem::replace(self, conn).raw_shutdown();
		self.session_id = Some(ready.session_id.clone());
		Ok(ready)
	}

	/// Resume using our existing session
	async fn resume(&mut self, session_id: String) -> Result<Event> {
		sleep_ms(1000);
		trace!("Resuming...");

		let url = build_gateway_url_v2(&self.gateway_resume_url)?;
		let (socket, _res) = connect_async(url).await?;
		let (mut socket_tx, mut socket_rx) = socket.split();

		// send the resume request
		let resume = json! {{
			"op": 6,
			"d": {
				"seq": self.last_sequence,
				"token": self.token,
				"session_id": session_id,
			}
		}};
		let _ = socket_tx.send_json(&resume).await;

		// TODO: when Discord has implemented it, observe the RESUMING event here
		let first_event;
		loop {
			match socket_rx.recv_json(GatewayEvent::decode).await? {
				GatewayEvent::Hello(interval) => {
					let _ = self
						.keepalive_channel
						.send(Status::ChangeInterval(interval));
				}
				GatewayEvent::Dispatch(seq, event) => {
					if let Event::Resumed { .. } = event {
						trace!("Resumed successfully");
					}
					if let Event::Ready(ReadyEvent { ref session_id, .. }) = event {
						self.session_id = Some(session_id.clone());
					}
					self.last_sequence = seq;
					first_event = event;
					break;
				}
				GatewayEvent::InvalidateSession => {
					debug!("Session invalidated in resume, reidentifying");
					socket_tx.send_json(&self.identify).await?;
				}
				other => {
					debug!("Unexpected event: {:?}", other);
					return Err(Error::Protocol("Unexpected event during resume"));
				}
			}
		}

		// switch everything to the new connection
		self.receiver = socket_rx;
		let _ = self
			.keepalive_channel
			.send(Status::ChangeSenderV2(socket_tx))
			.await;
		Ok(first_event)
	}

	/// Cleanly shut down the websocket connection. Optional.
	pub async fn shutdown(mut self) -> Result<()> {
		self.inner_shutdown().await?;
		::std::mem::forget(self); // don't call a second time
		Ok(())
	}

	// called from shutdown() and drop()
	async fn inner_shutdown(&mut self) -> Result<()> {
		self.keepalive_channel
			.send(Status::Aborted)
			.await
			.expect("Could not stop the keepalive thread, there will be a thread leak.");
		Ok(())
	}

	// called when we want to drop the connection with no fanfare
	fn raw_shutdown(mut self) {
		::std::mem::forget(self); // don't call inner_shutdown()
	}

	/// Requests a download of online member lists.
	///
	/// It is recommended to avoid calling this method until the online member list
	/// is actually needed, especially for large servers, in order to save bandwidth
	/// and memory.
	///
	/// Can be used with `State::all_servers`.
	pub async fn sync_servers(&self, servers: &[ServerId]) {
		let msg = json! {{
			"op": 12,
			"d": servers,
		}};
		let _ = self.keepalive_channel.send(Status::SendMessage(msg)).await;
	}

	/// Request a synchronize of active calls for the specified channels.
	///
	/// Can be used with `State::all_private_channels`.
	pub async fn sync_calls(&self, channels: &[ChannelId]) {
		for &channel in channels {
			let msg = json! {{
				"op": 13,
				"d": { "channel_id": channel }
			}};
			let _ = self.keepalive_channel.send(Status::SendMessage(msg)).await;
		}
	}

	/// Requests a download of all member information for large servers.
	///
	/// The members lists are cleared on call, and then refilled as chunks are received. When
	/// `unknown_members()` returns 0, the download has completed.
	pub async fn download_all_members(&mut self, state: &mut crate::State) {
		if state.unknown_members() == 0 {
			return;
		}
		let servers = state.__download_members();
		let msg = json! {{
			"op": 8,
			"d": {
				"guild_id": servers,
				"query": "",
				"limit": 0,
			}
		}};
		let _ = self.keepalive_channel.send(Status::SendMessage(msg)).await;
	}
}

impl Drop for AsyncConnection {
	fn drop(&mut self) {
		// Swallow errors
		let _ = self.inner_shutdown();
	}
}

/// Websocket connection to the Discord servers.
pub struct Connection {
	runtime: tokio::runtime::Runtime,
	async_connection: AsyncConnection,
}

impl Connection {
	/// Establish a connection to the Discord websocket servers.
	///
	/// Returns both the `Connection` and the `ReadyEvent` which is always the
	/// first event received and contains initial state information.
	///
	/// Usually called internally by `Discord::connect`, which provides both
	/// the token and URL and an optional user-given shard ID and total shard
	/// count.
	pub fn new(
		base_url: &str,
		token: &str,
		shard: Option<[u8; 2]>,
	) -> Result<(Connection, ReadyEvent)> {
		ConnectionBuilder {
			shard,
			..ConnectionBuilder::new(base_url.to_owned(), token)
		}
		.connect()
	}

	fn __connect(
		base_url: &str,
		token: &str,
		identify: serde_json::Value,
	) -> Result<(Connection, ReadyEvent)> {
		let rt = tokio::runtime::Builder::new_current_thread()
			.enable_all()
			.build()
			.unwrap();
		let (connection, ready) = rt.block_on(AsyncConnection::new(&base_url, token, None))?;
		// return the connection
		Ok((
			Connection {
				runtime: rt,
				async_connection: connection,
			},
			ready,
		))
	}

	/// Change the game information that this client reports as playing.
	pub fn set_game(&self, game: Option<Game>) {
		self.runtime.block_on(self.async_connection.set_game(game))
	}

	/// Set the client to be playing this game, with defaults used for any
	/// extended information.
	pub fn set_game_name(&self, name: String) {
		self.runtime.block_on(self.async_connection.set_game_name(name))
	}

	/// Sets the active presence of the client, including game and/or status
	/// information.
	///
	/// `afk` will help Discord determine where to send notifications.
	pub fn set_presence(&self, game: Option<Game>, status: OnlineStatus, afk: bool) {
		self.runtime.block_on(self.async_connection.set_presence(game, status, afk))
	}

	/// Get a handle to the voice connection for a server.
	///
	/// Pass `None` to get the handle for group and one-on-one calls.
	#[cfg(feature = "voice")]
	pub fn voice(&mut self, server_id: Option<ServerId>) -> &mut VoiceConnection {
		self.async_connection.voice(server_id)
	}

	/// Drop the voice connection for a server, forgetting all settings.
	///
	/// Calling `.voice(server_id).disconnect()` will disconnect from voice but retain the mute
	/// and deaf status, audio source, and audio receiver.
	///
	/// Pass `None` to drop the connection for group and one-on-one calls.
	#[cfg(feature = "voice")]
	pub fn drop_voice(&mut self, server_id: Option<ServerId>) {
		self.async_connection.drop_voice(server_id)
	}

	/// Receive an event over the websocket, blocking until one is available.
	pub fn recv_event(&mut self) -> Result<Event> {
		self.runtime.block_on(self.async_connection.recv_event())
	}

	/// Cleanly shut down the websocket connection. Optional.
	pub fn shutdown(mut self) -> Result<()> {
		self.runtime.block_on(self.async_connection.shutdown());
		::std::mem::forget(self); // don't call a second time
		Ok(())
	}

	/// Requests a download of online member lists.
	///
	/// It is recommended to avoid calling this method until the online member list
	/// is actually needed, especially for large servers, in order to save bandwidth
	/// and memory.
	///
	/// Can be used with `State::all_servers`.
	pub fn sync_servers(&self, servers: &[ServerId]) {
		self.runtime.block_on(self.async_connection.sync_servers(servers))
	}

	/// Request a synchronize of active calls for the specified channels.
	///
	/// Can be used with `State::all_private_channels`.
	pub fn sync_calls(&self, channels: &[ChannelId]) {
		self.runtime.block_on(self.async_connection.sync_calls(channels))
	}

	/// Requests a download of all member information for large servers.
	///
	/// The members lists are cleared on call, and then refilled as chunks are received. When
	/// `unknown_members()` returns 0, the download has completed.
	pub fn download_all_members(&mut self, state: &mut crate::State) {
		self.runtime.block_on(self.async_connection.download_all_members(state))
	}
}

impl Drop for Connection {
	fn drop(&mut self) {
		// Swallow errors
		let _ = self.inner_shutdown();
	}
}

#[inline]
fn build_gateway_url(base: &str) -> Result<::websocket::client::request::Url> {
	::websocket::client::request::Url::parse(&format!("{}?v={}", base, GATEWAY_VERSION))
		.map_err(|_| Error::Other("Invalid gateway URL"))
}

#[inline]
fn build_gateway_url_v2(base: &str) -> Result<url::Url> {
	url::Url::parse(&format!("{}?v={}", base, GATEWAY_VERSION))
		.map_err(|_| Error::Other("Invalid gateway URL"))
}

async fn keepalive_async(
	interval: u64,
	mut sender: crate::WebSocketTX,
	mut channel: tokio::sync::mpsc::Receiver<Status>,
) {
	use futures_util::SinkExt;
	let jitter = rand::thread_rng().gen_range(0.0..1.0);
	sleep_ms((interval as f64 * jitter) as u64);

	match sender.send_json(&json! {{ "op": 1, "d": null }}).await {
		Ok(()) => {}
		Err(e) => warn!(
			"Error sending first heartbeat, Interval: {}, Error: {:?}",
			interval, e
		),
	}

	let mut timer = Timer::new(interval);
	let mut last_sequence = 0;

	'outer: loop {
		sleep_ms(100);

		loop {
			match channel.try_recv() {
				Ok(Status::SendMessage(val)) => match sender.send_json(&val).await {
					Ok(()) => {}
					Err(e) => warn!("Error sending gateway message: {:?}", e),
				},
				Ok(Status::Sequence(seq)) => {
					last_sequence = seq;
				}
				Ok(Status::ChangeInterval(interval)) => {
					timer = Timer::new(interval);
				}
				Ok(Status::ChangeSender(_)) => unimplemented!(),
				Ok(Status::ChangeSenderV2(new_sender)) => {
					sender = new_sender;
				}
				Ok(Status::Aborted) => break 'outer,
				Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
				Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break 'outer,
			}
		}

		if timer.check_tick() {
			let map = json! {{
				"op" : 1,
				"d" : last_sequence
			}};
			match sender.send_json(&map).await {
				Ok(()) => {}
				Err(e) => warn!("Error sending gateway keepalive: {:?}", e),
			}
		}
	}
	debug!("Why the hell are we here");
	let _ = sender
		.send(tokio_tungstenite::tungstenite::Message::Close(None))
		.await;
}
