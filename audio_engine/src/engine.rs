


use cpal::{
	SampleRate,
	StreamError,
	traits::{ DeviceTrait, HostTrait, StreamTrait }
};

use std::sync::{ Arc, Mutex };

use crate::mixer;
use crate::mixer::{ Mixer, Sound, SoundSource };
use crate::converter::{ ChannelConverter, SampleRateConverter };



use backend::Backend;

mod backend {


	use super::create_device;
	use crate::mixer::Mixer;
	use std::sync::{ Arc, Mutex };


	struct StreamEventLoop {
		mixer: Arc<Mutex<Mixer>>,
		stream: Option<cpal::platform::Stream>
	}

	impl StreamEventLoop {

		fn run (
			&mut self,
			event_channel: std::sync::mpsc::Sender<StreamEvent>,
			stream_evemt_reciever: std::sync::mpsc::Receiver<StreamEvent>
		) {

			// trigger first device creation
			event_channel.send(StreamEvent::RecreateStream).unwrap();

			let mut handled = false;
			let error_callback = move |err| {
				log::error!("stream error: {}", err);
				if !handled {
					// https://github.com/Rodrigodd/audio-engine/blob/3d0da3711b5cc78e7192d616ebb1d4069920707d/src/engine.rs#L35
					// the stream could have send multiple errors which has been confirmed on android
					// (an error before the stream closes, and an error after it closes)
					handled = true;
					event_channel.send(StreamEvent::RecreateStream).unwrap()
				}
			};

			while let Ok(event) = stream_evemt_reciever.recv() {
				match event {
					StreamEvent::RecreateStream => {
						log::debug!("recreating audio device");

						// https://github.com/Rodrigodd/audio-engine/blob/3d0da3711b5cc78e7192d616ebb1d4069920707d/src/engine.rs#L47
						// Droping the stream is unsound in android, see:
						// https://github.com/katyo/oboe-rs/issues/41
						#[cfg(target_os = "android")]
						std::mem::forget(self.stream.take());

						#[cfg(not(target_os = "android"))]
						drop(self.stream.take());

						let stream = create_device(&self.mixer, error_callback.clone());
						let stream = match stream {
							Ok(x) => x,
							Err(x) => {
								log::error!("creating audio device failed: {}", x);
								return;
							}
						};
						self.stream = Some(stream);
					},
					StreamEvent::Drop => return
				}
			}

		}

	}



	enum StreamEvent {
		RecreateStream,
		Drop
	}



	pub struct Backend {

		join: Option<std::thread::JoinHandle<()>>,
		sender: std::sync::mpsc::Sender<StreamEvent>

	}

	impl Backend {

		pub (super) fn start (mixer: Arc<Mutex<Mixer>>) -> Result<Self, &'static str> {

			let (sender, receiver) = std::sync::mpsc::channel::<StreamEvent>();

			let join = {
				let sender = sender.clone();
				std::thread::spawn( move || {
					log::debug!("starting thread");
					StreamEventLoop { mixer, stream: None }.run(sender, receiver)
				})
			};

			Ok(Self {
				join: Some(join),
				sender
			})

		}

	}

	impl Drop for Backend {

		fn drop (&mut self) {

			self.sender.send(StreamEvent::Drop).unwrap();
			self.join.take().unwrap().join().unwrap();

		}

	}


}



/// The main struct of the crate
///
/// This holds all existing Wav Sources and `cpal::platform::Stream`
pub struct AudioEngine {

	mixer: Arc<Mutex<Mixer>>,
	_backend: Backend

}

impl AudioEngine {


	/// tries to create a new Audio Engine
	///
	/// `cpal` will spawn a new thread where the sound samples will
	/// be sampled, mixed and outputed to the output stream
	pub fn new () -> Result<Self, &'static str> {
		let mixer = Arc::new(Mutex::new(Mixer::new(2, mixer::SampleRate(48000)))); // 48k sample rate
		let backend = Backend::start(mixer.clone())?;

		Ok(Self {
			mixer,
			_backend: backend
		})
	}


	/// the sample rate that is currently being outputed to the device
	pub fn sample_rate(&self) -> u32 {
		self.mixer.lock().unwrap().sample_rate()
	}


	/// the sample rate of the current output device
	///
	/// may change when device changes
	pub fn channels (&self) -> u16 {
		self.mixer.lock().unwrap().channels()
	}


	/// create a new sound
	///
	/// Return a `Err` if the number of channels doesn't match the
	/// output number of channels. If the output number of channels
	/// of `source` is 1, `source` will be automatic wrapped in a
	/// [`ChannelConverter`]
	///
	/// if the `sample_rate` of `source` mismatch the output
	/// `sample_rate`, `source` will be wrapped in a
	/// [`SampleRateConverter`]
	pub fn new_sound <T: SoundSource + Send + 'static> (
		&self,
		source: T,
		effect: impl FnMut(f32) -> f32 + 'static + std::marker::Send
	) -> Result<Sound, &'static str> {
		let mut mixer = self.mixer.lock().unwrap();

		let sound: Box<dyn SoundSource + Send> = if source.sample_rate() != mixer.sample_rate.0 {
			if source.channels() == mixer.channels {
				Box::new(SampleRateConverter::new(source, mixer.sample_rate.0))
			} else if mixer.channels == 1 || source.channels() == 1 {
				Box::new(ChannelConverter::new(
					SampleRateConverter::new(source, mixer.sample_rate.0),
					mixer.channels
				))
			} else {
				return Err("Number of channels do not match the output, and neither are 1");
			}
		} else if source.channels() == mixer.channels {
			Box::new(source)
		} else if mixer.channels == 1 || source.channels() == 1 {
			Box::new(ChannelConverter::new(source, mixer.channels))
		} else {
			return Err("Number of channels do not match the output, and is not 1");
		};

		let id = mixer.add_sound(sound, effect);
		drop(mixer);

		Ok(Sound {
			mixer: self.mixer.clone(),
			id
		})
	}


}



fn create_device (
	mixer: &Arc<Mutex<Mixer>>,
	error_callback: impl FnMut(StreamError) + Send + Clone + 'static
) -> Result<cpal::Stream, &'static str> {

	let host = cpal::default_host();
	let device = host
					.default_output_device()
					.ok_or("no output device available")?;
	let mut supported_configs_range = device
										.supported_output_configs()
										.map_err(|_| "error while querying formats")?
										.map(|x| {
											let sample_rate = SampleRate(48000);
											if x.min_sample_rate() <= sample_rate && sample_rate <= x.max_sample_rate() {
												return x.with_sample_rate(sample_rate);
											}

											let sample_rate = SampleRate(44100);
											if x.min_sample_rate() <= sample_rate && sample_rate <= x.max_sample_rate() {
												return x.with_sample_rate(sample_rate);
											}

											x.with_max_sample_rate()
										})
										.collect::<Vec<_>>();

	supported_configs_range.sort_unstable_by(|a, b| {
		let key = |x: &cpal::SupportedStreamConfig| {
			(
				x.sample_rate().0 == 48000,
				x.sample_rate().0 == 441000,
				x.channels() == 2,
				x.channels() == 1,
				x.sample_format() == cpal::SampleFormat::I16,
				x.sample_rate().0
			)
		};
		key(a).cmp(&key(b))
	});

	if log::max_level() >= log::LevelFilter::Trace {
		for config in &supported_configs_range {
			log::trace!("config {:?}", config);
		}
	}

	let stream = loop {
		let config = if let Some(config) = supported_configs_range.pop() {
			config
		} else {
			return Err("no supported config");
		};
		let sample_format = config.sample_format();
		let config = config.config();
		mixer
			.lock()
			.unwrap()
			.set_config(config.channels, mixer::SampleRate(config.sample_rate.0));

		let stream = {
			use cpal::SampleFormat::*;
			match sample_format {
				I16 => stream::<i16, _>(mixer, error_callback.clone(), &device, &config),
				U16 => stream::<u16, _>(mixer, error_callback.clone(), &device, &config),
				F32 => stream::<f32, _>(mixer, error_callback.clone(), &device, &config)
			}
		};

		let stream = match stream {
			Ok(x) => {
				log::info!("created {:?} stream with config {:?}", sample_format, config);
				x
			},
			Err(e) => {
				log::error!("failed to create stream with config {:?}: {:?}", config, e);
				continue;
			}
		};

		stream.play().unwrap();
		break stream;
	};

	Ok(stream)

}



fn stream <T: cpal::Sample, E: FnMut(StreamError) + Send + 'static> (
	mixer: &Arc<Mutex<Mixer>>,
	error_callback: E,
	device: &cpal::Device,
	config: &cpal::StreamConfig
) -> Result<cpal::Stream, cpal::BuildStreamError> {

	let mixer = mixer.clone();
	let mut input_buffer = Vec::new();
	device.build_output_stream(
		config,
		move |output_buffer: &mut [T], _| {
			input_buffer.clear();
			input_buffer.resize(output_buffer.len(), 0);
			mixer.lock().unwrap().write_samples(&mut input_buffer);
			// write sample to output buffer
			output_buffer
				.iter_mut()
				.zip(input_buffer.iter())
				.for_each(|(a, b)| *a = T::from(b));
		},
		error_callback
	)

}


