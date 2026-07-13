use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ringbuf::HeapRb;
use serde::Deserialize;
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use tungstenite::accept;

const FREQUENCIES: [f32; 10] = [22.0, 44.0, 240.0, 397.0, 735.0, 1360.0, 2520.0, 4670.0, 9100.0, 16000.0];

#[derive(Clone)]
struct Biquad {
    freq: f32,
    gain_db: f32,
    b0: f32, b1: f32, b2: f32,
    a1: f32, a2: f32,
    z1_l: f32, z2_l: f32,
    z1_r: f32, z2_r: f32,
}

impl Biquad {
    fn new(freq: f32) -> Self {
        let mut b = Self {
            freq, gain_db: 0.0,
            b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0,
            z1_l: 0.0, z2_l: 0.0, z1_r: 0.0, z2_r: 0.0,
        };
        b.calculate_coeffs(48000.0);
        b
    }

    fn calculate_coeffs(&mut self, sample_rate: f32) {
        let q = 1.4;
        let a = f32::powf(10.0, self.gain_db / 40.0);
        let w0 = 2.0 * std::f32::consts::PI * self.freq / sample_rate;
        let alpha = w0.sin() / (2.0 * q);

        let a0 = 1.0 + alpha / a;
        self.b0 = (1.0 + alpha * a) / a0;
        self.b1 = (-2.0 * w0.cos()) / a0;
        self.b2 = (1.0 - alpha * a) / a0;
        self.a1 = (-2.0 * w0.cos()) / a0;
        self.a2 = (1.0 - alpha / a) / a0;
    }

    fn process(&mut self, input: f32, is_left: bool) -> f32 {
        if is_left {
            let output = self.b0 * input + self.z1_l;
            self.z1_l = self.b1 * input - self.a1 * output + self.z2_l;
            self.z2_l = self.b2 * input - self.a2 * output;
            output
        } else {
            let output = self.b0 * input + self.z1_r;
            self.z1_r = self.b1 * input - self.a1 * output + self.z2_r;
            self.z2_r = self.b2 * input - self.a2 * output;
            output
        }
    }
}

struct DspState {
    eq_filters: [Biquad; 10],
    clarity_boost: Biquad,
    clarity_mud_cut: Biquad,
    volume_boost_linear: f32,
}

#[derive(Deserialize)]
struct ControlMessage {
    #[serde(default)]
    band: Option<usize>,
    #[serde(default)]
    gain: Option<f32>,
    #[serde(default)]
    boost: Option<f32>,
    #[serde(default)]
    clarity: Option<f32>,
}

fn main() {
    println!("=== RUST SYSTEM-WIDE DSP ENGINE (BULLETPROOF V3) ===");

    let host = cpal::default_host();
    
    let input_device = host.input_devices().expect("No input devices").find(|d| {
        d.name().unwrap_or_default().to_lowercase().contains("cable")
    }).expect("VB-Audio Virtual Cable not found!");
    
    let mut output_device = host.output_devices().unwrap().find(|d| {
        let name = d.name().unwrap_or_default().to_lowercase();
        !name.contains("cable") && (name.contains("stereo") || name.contains("a2dp"))
    });

    if output_device.is_none() {
        output_device = host.output_devices().unwrap().find(|d| {
            let name = d.name().unwrap_or_default().to_lowercase();
            !name.contains("cable") && !name.contains("hands-free") && !name.contains("ag audio") && (name.contains("bluetooth") || name.contains("headphone"))
        });
    }

    if output_device.is_none() {
        output_device = host.output_devices().unwrap().find(|d| {
            let name = d.name().unwrap_or_default().to_lowercase();
            !name.contains("cable") && !name.contains("mapper") && !name.contains("hands-free")
        });
    }

    let output_device = output_device.expect("Could not find a valid output speaker!");

    println!("Intercepting Audio From: {}", input_device.name().unwrap());
    println!("Routing Processed Audio To: {}", output_device.name().unwrap());

    let out_supported_config = output_device.default_output_config().expect("Failed to get output config");
    let sample_rate = out_supported_config.sample_rate().0 as f32;
    let out_config: cpal::StreamConfig = out_supported_config.clone().into();

    let in_supported_config = input_device.supported_input_configs()
        .unwrap()
        .find(|c| c.min_sample_rate().0 <= out_supported_config.sample_rate().0 && c.max_sample_rate().0 >= out_supported_config.sample_rate().0)
        .map(|c| c.with_sample_rate(out_supported_config.sample_rate()))
        .unwrap_or_else(|| input_device.default_input_config().unwrap());
    let in_config: cpal::StreamConfig = in_supported_config.clone().into();

    println!("Input Sample Rate: {} Hz | Output Sample Rate: {} Hz", in_config.sample_rate.0, out_config.sample_rate.0);
    if in_config.sample_rate.0 != out_config.sample_rate.0 {
        println!("⚠️ WARNING: Sample rates mismatch! Please set both to matching rates in Windows Sound Properties.");
    }

    let dsp = Arc::new(Mutex::new(DspState {
        eq_filters: FREQUENCIES.map(|f| Biquad::new(f)),
        clarity_boost: Biquad::new(3800.0),
        clarity_mud_cut: Biquad::new(250.0),
        volume_boost_linear: 1.0,
    }));
    
    {
        let mut state = dsp.lock().unwrap();
        for filter in state.eq_filters.iter_mut() { filter.calculate_coeffs(sample_rate); }
        state.clarity_boost.calculate_coeffs(sample_rate);
        state.clarity_mud_cut.calculate_coeffs(sample_rate);
    }

    // UPGRADE: Atomic Stereo Buffer (f32, f32) prevents channel inversion forever!
    let buffer_size = (sample_rate * 0.25) as usize; 
    let rb = HeapRb::<(f32, f32)>::new(buffer_size);
    let (mut producer, mut consumer) = rb.split();

    // 4. Build Input Stream (Pushing atomic stereo tuples)
    let input_stream = match in_supported_config.sample_format() {
        cpal::SampleFormat::F32 => input_device.build_input_stream(
            &in_config,
            move |data: &[f32], _| { for chunk in data.chunks_exact(2) { let _ = producer.push((chunk[0], chunk[1])); } },
            |err| eprintln!("Input error: {}", err), None
        ),
        cpal::SampleFormat::I16 => input_device.build_input_stream(
            &in_config,
            move |data: &[i16], _| { for chunk in data.chunks_exact(2) { let _ = producer.push((chunk[0] as f32 / i16::MAX as f32, chunk[1] as f32 / i16::MAX as f32)); } },
            |err| eprintln!("Input error: {}", err), None
        ),
        cpal::SampleFormat::U16 => input_device.build_input_stream(
            &in_config,
            move |data: &[u16], _| { for chunk in data.chunks_exact(2) { let _ = producer.push(((chunk[0] as f32 - u16::MAX as f32 / 2.0) / (u16::MAX as f32 / 2.0), (chunk[1] as f32 - u16::MAX as f32 / 2.0) / (u16::MAX as f32 / 2.0))); } },
            |err| eprintln!("Input error: {}", err), None
        ),
        _ => panic!("Unsupported input sample format"),
    }.unwrap();

    // 5. Build Output Stream (Processing atomic stereo tuples)
    let dsp_clone = Arc::clone(&dsp);
    let output_stream = match out_supported_config.sample_format() {
        cpal::SampleFormat::F32 => output_device.build_output_stream(
            &out_config,
            move |data: &mut [f32], _| {
                let mut active_dsp = dsp_clone.try_lock(); 
                for chunk in data.chunks_exact_mut(2) {
                    let (mut left, mut right) = consumer.pop().unwrap_or((0.0, 0.0));
                    if let Ok(ref mut state) = active_dsp {
                        // -6dB Headroom safety net + gentle linear Volume Boost
                        let gain = state.volume_boost_linear * 0.5;
                        left *= gain; right *= gain;

                        for filter in state.eq_filters.iter_mut() {
                            left = filter.process(left, true); right = filter.process(right, false);
                        }
                        left = state.clarity_mud_cut.process(left, true); right = state.clarity_mud_cut.process(right, false);
                        left = state.clarity_boost.process(left, true); right = state.clarity_boost.process(right, false);
                    }
                    chunk[0] = f32::tanh(left); chunk[1] = f32::tanh(right);
                }
            }, |err| eprintln!("Output error: {}", err), None
        ),
        cpal::SampleFormat::I16 => output_device.build_output_stream(
            &out_config,
            move |data: &mut [i16], _| {
                let mut active_dsp = dsp_clone.try_lock(); 
                for chunk in data.chunks_exact_mut(2) {
                    let (mut left, mut right) = consumer.pop().unwrap_or((0.0, 0.0));
                    if let Ok(ref mut state) = active_dsp {
                        let gain = state.volume_boost_linear * 0.5;
                        left *= gain; right *= gain;

                        for filter in state.eq_filters.iter_mut() {
                            left = filter.process(left, true); right = filter.process(right, false);
                        }
                        left = state.clarity_mud_cut.process(left, true); right = state.clarity_mud_cut.process(right, false);
                        left = state.clarity_boost.process(left, true); right = state.clarity_boost.process(right, false);
                    }
                    chunk[0] = (f32::tanh(left) * i16::MAX as f32) as i16; chunk[1] = (f32::tanh(right) * i16::MAX as f32) as i16;
                }
            }, |err| eprintln!("Output error: {}", err), None
        ),
        cpal::SampleFormat::U16 => output_device.build_output_stream(
            &out_config,
            move |data: &mut [u16], _| {
                let mut active_dsp = dsp_clone.try_lock(); 
                for chunk in data.chunks_exact_mut(2) {
                    let (mut left, mut right) = consumer.pop().unwrap_or((0.0, 0.0));
                    if let Ok(ref mut state) = active_dsp {
                        let gain = state.volume_boost_linear * 0.5;
                        left *= gain; right *= gain;

                        for filter in state.eq_filters.iter_mut() {
                            left = filter.process(left, true); right = filter.process(right, false);
                        }
                        left = state.clarity_mud_cut.process(left, true); right = state.clarity_mud_cut.process(right, false);
                        left = state.clarity_boost.process(left, true); right = state.clarity_boost.process(right, false);
                    }
                    chunk[0] = ((f32::tanh(left) * 0.5 + 0.5) * u16::MAX as f32) as u16; chunk[1] = ((f32::tanh(right) * 0.5 + 0.5) * u16::MAX as f32) as u16;
                }
            }, |err| eprintln!("Output error: {}", err), None
        ),
        _ => panic!("Unsupported output sample format"),
    }.unwrap();

    input_stream.play().unwrap();
    output_stream.play().unwrap();
    println!("Audio Engine is Running! Playing sound...");

    let server = TcpListener::bind("127.0.0.1:3030").unwrap();
    println!("WebSocket Dashboard listening on ws://127.0.0.1:3030");

    for stream in server.incoming() {
        let mut websocket = accept(stream.unwrap()).unwrap();
        println!("Web UI Connected!");
        
        loop {
            let msg = match websocket.read() { Ok(msg) => msg, Err(_) => break };
            if msg.is_text() {
                let json = msg.to_text().unwrap();
                if let Ok(update) = serde_json::from_str::<ControlMessage>(json) {
                    let mut state = dsp.lock().unwrap();
                    
                    // Smooth, gentle 1x to 3x Volume Scaling (Prevents Square Wave Distortion!)
                    if let Some(boost_val) = update.boost {
                        state.volume_boost_linear = 1.0 + (boost_val * 0.2);
                        println!("Volume Boost set to: {:.2}x", state.volume_boost_linear);
                    }
                    
                    // Clarity: Gentle presence lift + mud reduction
                    if let Some(clarity_val) = update.clarity {
                        state.clarity_boost.gain_db = clarity_val * 0.6; 
                        state.clarity_mud_cut.gain_db = -(clarity_val * 0.3);
                        state.clarity_boost.calculate_coeffs(sample_rate);
                        state.clarity_mud_cut.calculate_coeffs(sample_rate);
                        println!("Clarity set to level: {}", clarity_val);
                    }

                    if let (Some(band), Some(gain)) = (update.band, update.gain) {
                        if band < 10 {
                            state.eq_filters[band].gain_db = gain;
                            state.eq_filters[band].calculate_coeffs(sample_rate);
                            println!("Updated Band {} ({} Hz) to {} dB", band, state.eq_filters[band].freq, gain);
                        }
                    }
                }
            }
        }
    }
}