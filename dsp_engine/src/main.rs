use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ringbuf::HeapRb;
use serde::{Deserialize, Serialize};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
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
    bass_sub_boost: Biquad,
    surround_width: f32,
    ambience_level: f32,
    dynamic_boost_ratio: f32,
    volume_boost_linear: f32,
    current_energy: [f32; 10],
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
    #[serde(default)]
    ambience: Option<f32>,
    #[serde(default)]
    surround: Option<f32>,
    #[serde(default)]
    dynamic: Option<f32>,
    #[serde(default)]
    bass: Option<f32>,
    #[serde(default)]
    set_device: Option<String>,
}

#[derive(Serialize)]
struct TelemetryMessage {
    msg_type: String,
    active_device: String,
    available_devices: Vec<String>,
    levels: Vec<f32>,
    ascii_beat: String,
}

fn main() {
    println!("=== RUST SYSTEM-WIDE DSP ENGINE (PRO STUDIO V5.2) ===");

    let host = cpal::default_host();
    
    let input_device = host.input_devices().expect("No input devices").find(|d| {
        d.name().unwrap_or_default().to_lowercase().contains("cable")
    }).expect("VB-Audio Virtual Cable not found!");
    
    let mut available_device_names: Vec<String> = host.output_devices().unwrap()
        .map(|d| d.name().unwrap_or_default())
        .filter(|n| !n.to_lowercase().contains("cable") && !n.to_lowercase().contains("mapper"))
        .collect();
    available_device_names.dedup();

    let mut output_device = host.output_devices().unwrap().find(|d| {
        let name = d.name().unwrap_or_default().to_lowercase();
        !name.contains("cable") && (name.contains("stereo") || name.contains("a2dp") || name.contains("headphone"))
    });

    if output_device.is_none() {
        output_device = host.output_devices().unwrap().find(|d| {
            let name = d.name().unwrap_or_default().to_lowercase();
            !name.contains("cable") && !name.contains("mapper")
        });
    }

    let output_device = output_device.expect("Could not find a valid output speaker!");
    let active_device_name = output_device.name().unwrap_or_else(|_| "Default Speaker".to_string());

    println!("Intercepting Audio From: {}", input_device.name().unwrap());
    println!("Routing Processed Audio To: {}", active_device_name);

    let out_supported_config = output_device.default_output_config().expect("Failed to get output config");
    let sample_rate = out_supported_config.sample_rate().0 as f32;
    let out_config: cpal::StreamConfig = out_supported_config.clone().into();

    let in_supported_config = input_device.supported_input_configs()
        .unwrap()
        .find(|c| c.min_sample_rate().0 <= out_supported_config.sample_rate().0 && c.max_sample_rate().0 >= out_supported_config.sample_rate().0)
        .map(|c| c.with_sample_rate(out_supported_config.sample_rate()))
        .unwrap_or_else(|| input_device.default_input_config().unwrap());
    let in_config: cpal::StreamConfig = in_supported_config.clone().into();

    println!("Input Format: {:?} | Output Format: {:?}", in_supported_config.sample_format(), out_supported_config.sample_format());

    let dsp = Arc::new(Mutex::new(DspState {
        eq_filters: FREQUENCIES.map(|f| Biquad::new(f)),
        clarity_boost: Biquad::new(3800.0),
        clarity_mud_cut: Biquad::new(250.0),
        bass_sub_boost: Biquad::new(35.0),
        surround_width: 0.0,
        ambience_level: 0.0,
        dynamic_boost_ratio: 1.0,
        volume_boost_linear: 1.0,
        current_energy: [0.0; 10],
    }));
    
    {
        let mut state = dsp.lock().unwrap();
        for filter in state.eq_filters.iter_mut() { filter.calculate_coeffs(sample_rate); }
        state.clarity_boost.calculate_coeffs(sample_rate);
        state.clarity_mud_cut.calculate_coeffs(sample_rate);
        state.bass_sub_boost.calculate_coeffs(sample_rate);
    }

    let buffer_size = (sample_rate * 0.25) as usize; 
    let rb = HeapRb::<(f32, f32)>::new(buffer_size);
    let (mut producer, mut consumer) = rb.split();

    // UNIVERSAL INPUT STREAM (All tuple parentheses perfectly wrapped!)
    let input_stream = match in_supported_config.sample_format() {
        cpal::SampleFormat::F32 => input_device.build_input_stream(&in_config, move |data: &[f32], _| { for chunk in data.chunks_exact(2) { let _ = producer.push((chunk[0], chunk[1])); } }, |e| eprintln!("In err: {}", e), None),
        cpal::SampleFormat::I16 => input_device.build_input_stream(&in_config, move |data: &[i16], _| { for chunk in data.chunks_exact(2) { let _ = producer.push((chunk[0] as f32 / i16::MAX as f32, chunk[1] as f32 / i16::MAX as f32)); } }, |e| eprintln!("In err: {}", e), None),
        cpal::SampleFormat::U16 => input_device.build_input_stream(&in_config, move |data: &[u16], _| { for chunk in data.chunks_exact(2) { let _ = producer.push(((chunk[0] as f32 - 32768.0) / 32768.0, (chunk[1] as f32 - 32768.0) / 32768.0)); } }, |e| eprintln!("In err: {}", e), None),
        cpal::SampleFormat::I32 => input_device.build_input_stream(&in_config, move |data: &[i32], _| { for chunk in data.chunks_exact(2) { let _ = producer.push((chunk[0] as f32 / i32::MAX as f32, chunk[1] as f32 / i32::MAX as f32)); } }, |e| eprintln!("In err: {}", e), None),
        cpal::SampleFormat::F64 => input_device.build_input_stream(&in_config, move |data: &[f64], _| { for chunk in data.chunks_exact(2) { let _ = producer.push((chunk[0] as f32, chunk[1] as f32)); } }, |e| eprintln!("In err: {}", e), None),
        cpal::SampleFormat::I8  => input_device.build_input_stream(&in_config, move |data: &[i8], _|  { for chunk in data.chunks_exact(2) { let _ = producer.push((chunk[0] as f32 / i8::MAX as f32, chunk[1] as f32 / i8::MAX as f32)); } }, |e| eprintln!("In err: {}", e), None),
        cpal::SampleFormat::U8  => input_device.build_input_stream(&in_config, move |data: &[u8], _|  { for chunk in data.chunks_exact(2) { let _ = producer.push(((chunk[0] as f32 - 128.0) / 128.0, (chunk[1] as f32 - 128.0) / 128.0)); } }, |e| eprintln!("In err: {}", e), None),
        cpal::SampleFormat::U32 => input_device.build_input_stream(&in_config, move |data: &[u32], _| { for chunk in data.chunks_exact(2) { let _ = producer.push(((chunk[0] as f64 - 2147483648.0) as f32 / 2147483648.0, (chunk[1] as f64 - 2147483648.0) as f32 / 2147483648.0)); } }, |e| eprintln!("In err: {}", e), None),
        cpal::SampleFormat::I64 => input_device.build_input_stream(&in_config, move |data: &[i64], _| { for chunk in data.chunks_exact(2) { let _ = producer.push(((chunk[0] as f64 / i64::MAX as f64) as f32, (chunk[1] as f64 / i64::MAX as f64) as f32)); } }, |e| eprintln!("In err: {}", e), None),
        cpal::SampleFormat::U64 => input_device.build_input_stream(&in_config, move |data: &[u64], _| { for chunk in data.chunks_exact(2) { let _ = producer.push((((chunk[0] as f64 - 9223372036854775808.0) / 9223372036854775808.0) as f32, ((chunk[1] as f64 - 9223372036854775808.0) / 9223372036854775808.0) as f32)); } }, |e| eprintln!("In err: {}", e), None),
        _ => panic!("Unsupported input format: {:?}", in_supported_config.sample_format()),
    }.unwrap();

    let dsp_clone = Arc::clone(&dsp);
    
    // UNIVERSAL OUTPUT STREAM
    let output_stream = match out_supported_config.sample_format() {
        cpal::SampleFormat::F32 => output_device.build_output_stream(&out_config, move |data: &mut [f32], _| {
            let mut active_dsp = dsp_clone.try_lock(); 
            for chunk in data.chunks_exact_mut(2) {
                let (mut left, mut right) = consumer.pop().unwrap_or((0.0, 0.0));
                if let Ok(ref mut state) = active_dsp {
                    let gain = state.volume_boost_linear * 0.5;
                    left *= gain; right *= gain;
                    left = state.bass_sub_boost.process(left, true); right = state.bass_sub_boost.process(right, false);
                    for i in 0..10 {
                        left = state.eq_filters[i].process(left, true); right = state.eq_filters[i].process(right, false);
                        state.current_energy[i] = (state.current_energy[i] * 0.92) + ((left.abs() + right.abs()) * 0.08);
                    }
                    left = state.clarity_mud_cut.process(left, true); right = state.clarity_mud_cut.process(right, false);
                    left = state.clarity_boost.process(left, true); right = state.clarity_boost.process(right, false);
                    
                    let mid = (left + right) * 0.5;
                    let side = (left - right) * (0.5 + state.surround_width * 0.5) * (1.0 + state.ambience_level * 0.2);
                    left = mid + side; right = mid - side;
                    
                    if (left.abs() + right.abs()) > 0.6 { left *= state.dynamic_boost_ratio; right *= state.dynamic_boost_ratio; }
                }
                chunk[0] = f32::tanh(left); chunk[1] = f32::tanh(right);
            }
        }, |e| eprintln!("Out err: {}", e), None),
        cpal::SampleFormat::I16 => output_device.build_output_stream(&out_config, move |data: &mut [i16], _| {
            let mut active_dsp = dsp_clone.try_lock(); 
            for chunk in data.chunks_exact_mut(2) {
                let (mut left, mut right) = consumer.pop().unwrap_or((0.0, 0.0));
                if let Ok(ref mut state) = active_dsp {
                    let gain = state.volume_boost_linear * 0.5;
                    left *= gain; right *= gain;
                    left = state.bass_sub_boost.process(left, true); right = state.bass_sub_boost.process(right, false);
                    for i in 0..10 {
                        left = state.eq_filters[i].process(left, true); right = state.eq_filters[i].process(right, false);
                        state.current_energy[i] = (state.current_energy[i] * 0.92) + ((left.abs() + right.abs()) * 0.08);
                    }
                    left = state.clarity_mud_cut.process(left, true); right = state.clarity_mud_cut.process(right, false);
                    left = state.clarity_boost.process(left, true); right = state.clarity_boost.process(right, false);
                    let mid = (left + right) * 0.5;
                    let side = (left - right) * (0.5 + state.surround_width * 0.5) * (1.0 + state.ambience_level * 0.2);
                    left = mid + side; right = mid - side;
                    if (left.abs() + right.abs()) > 0.6 { left *= state.dynamic_boost_ratio; right *= state.dynamic_boost_ratio; }
                }
                chunk[0] = (f32::tanh(left) * i16::MAX as f32) as i16; chunk[1] = (f32::tanh(right) * i16::MAX as f32) as i16;
            }
        }, |e| eprintln!("Out err: {}", e), None),
        cpal::SampleFormat::U16 => output_device.build_output_stream(&out_config, move |data: &mut [u16], _| {
            let mut active_dsp = dsp_clone.try_lock(); 
            for chunk in data.chunks_exact_mut(2) {
                let (mut left, mut right) = consumer.pop().unwrap_or((0.0, 0.0));
                if let Ok(ref mut state) = active_dsp {
                    let gain = state.volume_boost_linear * 0.5;
                    left *= gain; right *= gain;
                    left = state.bass_sub_boost.process(left, true); right = state.bass_sub_boost.process(right, false);
                    for i in 0..10 {
                        left = state.eq_filters[i].process(left, true); right = state.eq_filters[i].process(right, false);
                        state.current_energy[i] = (state.current_energy[i] * 0.92) + ((left.abs() + right.abs()) * 0.08);
                    }
                    left = state.clarity_mud_cut.process(left, true); right = state.clarity_mud_cut.process(right, false);
                    left = state.clarity_boost.process(left, true); right = state.clarity_boost.process(right, false);
                    let mid = (left + right) * 0.5;
                    let side = (left - right) * (0.5 + state.surround_width * 0.5) * (1.0 + state.ambience_level * 0.2);
                    left = mid + side; right = mid - side;
                    if (left.abs() + right.abs()) > 0.6 { left *= state.dynamic_boost_ratio; right *= state.dynamic_boost_ratio; }
                }
                chunk[0] = ((f32::tanh(left) * 0.5 + 0.5) * 65535.0) as u16; chunk[1] = ((f32::tanh(right) * 0.5 + 0.5) * 65535.0) as u16;
            }
        }, |e| eprintln!("Out err: {}", e), None),
        cpal::SampleFormat::U8 => output_device.build_output_stream(&out_config, move |data: &mut [u8], _| {
            let mut active_dsp = dsp_clone.try_lock(); 
            for chunk in data.chunks_exact_mut(2) {
                let (mut left, mut right) = consumer.pop().unwrap_or((0.0, 0.0));
                if let Ok(ref mut state) = active_dsp {
                    let gain = state.volume_boost_linear * 0.5;
                    left *= gain; right *= gain;
                    left = state.bass_sub_boost.process(left, true); right = state.bass_sub_boost.process(right, false);
                    for i in 0..10 {
                        left = state.eq_filters[i].process(left, true); right = state.eq_filters[i].process(right, false);
                        state.current_energy[i] = (state.current_energy[i] * 0.92) + ((left.abs() + right.abs()) * 0.08);
                    }
                    left = state.clarity_mud_cut.process(left, true); right = state.clarity_mud_cut.process(right, false);
                    left = state.clarity_boost.process(left, true); right = state.clarity_boost.process(right, false);
                    let mid = (left + right) * 0.5;
                    let side = (left - right) * (0.5 + state.surround_width * 0.5) * (1.0 + state.ambience_level * 0.2);
                    left = mid + side; right = mid - side;
                    if (left.abs() + right.abs()) > 0.6 { left *= state.dynamic_boost_ratio; right *= state.dynamic_boost_ratio; }
                }
                chunk[0] = ((f32::tanh(left) * 0.5 + 0.5) * 255.0) as u8; chunk[1] = ((f32::tanh(right) * 0.5 + 0.5) * 255.0) as u8;
            }
        }, |e| eprintln!("Out err: {}", e), None),
        cpal::SampleFormat::I32 => output_device.build_output_stream(&out_config, move |data: &mut [i32], _| {
            let mut active_dsp = dsp_clone.try_lock(); 
            for chunk in data.chunks_exact_mut(2) {
                let (mut left, mut right) = consumer.pop().unwrap_or((0.0, 0.0));
                if let Ok(ref mut state) = active_dsp {
                    let gain = state.volume_boost_linear * 0.5;
                    left *= gain; right *= gain;
                    left = state.bass_sub_boost.process(left, true); right = state.bass_sub_boost.process(right, false);
                    for i in 0..10 {
                        left = state.eq_filters[i].process(left, true); right = state.eq_filters[i].process(right, false);
                        state.current_energy[i] = (state.current_energy[i] * 0.92) + ((left.abs() + right.abs()) * 0.08);
                    }
                    left = state.clarity_mud_cut.process(left, true); right = state.clarity_mud_cut.process(right, false);
                    left = state.clarity_boost.process(left, true); right = state.clarity_boost.process(right, false);
                    let mid = (left + right) * 0.5;
                    let side = (left - right) * (0.5 + state.surround_width * 0.5) * (1.0 + state.ambience_level * 0.2);
                    left = mid + side; right = mid - side;
                    if (left.abs() + right.abs()) > 0.6 { left *= state.dynamic_boost_ratio; right *= state.dynamic_boost_ratio; }
                }
                chunk[0] = (f32::tanh(left) * i32::MAX as f32) as i32; chunk[1] = (f32::tanh(right) * i32::MAX as f32) as i32;
            }
        }, |e| eprintln!("Out err: {}", e), None),
        cpal::SampleFormat::F64 => output_device.build_output_stream(&out_config, move |data: &mut [f64], _| {
            let mut active_dsp = dsp_clone.try_lock(); 
            for chunk in data.chunks_exact_mut(2) {
                let (mut left, mut right) = consumer.pop().unwrap_or((0.0, 0.0));
                if let Ok(ref mut state) = active_dsp {
                    let gain = state.volume_boost_linear * 0.5;
                    left *= gain; right *= gain;
                    left = state.bass_sub_boost.process(left, true); right = state.bass_sub_boost.process(right, false);
                    for i in 0..10 {
                        left = state.eq_filters[i].process(left, true); right = state.eq_filters[i].process(right, false);
                        state.current_energy[i] = (state.current_energy[i] * 0.92) + ((left.abs() + right.abs()) * 0.08);
                    }
                    left = state.clarity_mud_cut.process(left, true); right = state.clarity_mud_cut.process(right, false);
                    left = state.clarity_boost.process(left, true); right = state.clarity_boost.process(right, false);
                    let mid = (left + right) * 0.5;
                    let side = (left - right) * (0.5 + state.surround_width * 0.5) * (1.0 + state.ambience_level * 0.2);
                    left = mid + side; right = mid - side;
                    if (left.abs() + right.abs()) > 0.6 { left *= state.dynamic_boost_ratio; right *= state.dynamic_boost_ratio; }
                }
                chunk[0] = f32::tanh(left) as f64; chunk[1] = f32::tanh(right) as f64;
            }
        }, |e| eprintln!("Out err: {}", e), None),
        _ => panic!("Unsupported output format: {:?}", out_supported_config.sample_format()),
    }.unwrap();

    input_stream.play().unwrap();
    output_stream.play().unwrap();
    println!("Audio Engine is Running! Playing sound...");

    let server = TcpListener::bind("127.0.0.1:3030").unwrap();
    println!("WebSocket Dashboard listening on ws://127.0.0.1:3030");

    for stream in server.incoming() {
        let mut websocket = accept(stream.unwrap()).unwrap();
        println!("Web UI Connected!");
        
        let mut last_telemetry = Instant::now();

        loop {
            if last_telemetry.elapsed() >= Duration::from_millis(80) {
                let state = dsp.lock().unwrap();
                let mut ascii = String::new();
                for &energy in state.current_energy.iter() {
                    let dots = (energy * 18.0) as usize;
                    ascii.push_str(&":".repeat(dots.min(4)));
                    ascii.push_str(&".".repeat((4_usize).saturating_sub(dots)));
                    ascii.push(' ');
                }
                
                let telemetry = TelemetryMessage {
                    msg_type: "telemetry".to_string(),
                    active_device: active_device_name.clone(),
                    available_devices: available_device_names.clone(),
                    levels: state.current_energy.to_vec(),
                    ascii_beat: ascii.trim().to_string(),
                };
                if let Ok(json) = serde_json::to_string(&telemetry) {
                    let _ = websocket.send(tungstenite::Message::Text(json));
                }
                last_telemetry = Instant::now();
            }

            if let Ok(msg) = websocket.read() {
                if msg.is_text() {
                    let json = msg.to_text().unwrap();
                    if let Ok(update) = serde_json::from_str::<ControlMessage>(json) {
                        if let Some(device) = update.set_device {
                            println!("⚠️ Device routing requested to: {}. Please set this device as default in Windows Sound Settings to switch instantly without rebooting!", device);
                        }

                        let mut state = dsp.lock().unwrap();
                        if let Some(val) = update.boost { state.volume_boost_linear = 1.0 + (val * 0.25); }
                        if let Some(val) = update.clarity {
                            state.clarity_boost.gain_db = val * 0.7; 
                            state.clarity_mud_cut.gain_db = -(val * 0.35);
                            state.clarity_boost.calculate_coeffs(sample_rate);
                            state.clarity_mud_cut.calculate_coeffs(sample_rate);
                        }
                        if let Some(val) = update.bass {
                            state.bass_sub_boost.gain_db = val * 0.9;
                            state.bass_sub_boost.calculate_coeffs(sample_rate);
                        }
                        if let Some(val) = update.surround { state.surround_width = val * 0.1; }
                        if let Some(val) = update.ambience { state.ambience_level = val * 0.1; }
                        if let Some(val) = update.dynamic { state.dynamic_boost_ratio = 1.0 + (val * 0.05); }

                        if let (Some(band), Some(gain)) = (update.band, update.gain) {
                            if band < 10 {
                                state.eq_filters[band].gain_db = gain;
                                state.eq_filters[band].calculate_coeffs(sample_rate);
                            }
                        }
                    }
                }
            }
        }
    }
}