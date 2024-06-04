use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use rodio::buffer::SamplesBuffer;
use rodio::{Decoder, OutputStream, Sink, Source};
use std::io::{self, Cursor};
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::{
    fs::File,
    io::{BufReader, BufWriter},
};

// async fn say_world() {
//     println!("world");
// }\\

const XOR_VAL: u8 = 0x5E;

async fn play_sound(sink: &Sink) {
    let sound_file = Path::new(
        r"C:\Users\mfer6\AppData\Roaming\Elgato\StreamDeck\Audio\Rain Fall.streamDeckAudio",
    );
    // Get a output stream handle to the default physical sound device
    // Load a sound from a file, using a path relative to Cargo.toml
    let file = File::open(sound_file).await.unwrap();
    let mut reader = BufReader::new(file);

    let mut buf: BufWriter<Vec<u8>> = BufWriter::new(Vec::new());

    // let mut iter = reader.read_u8();
    let mut content: Vec<u8> = Vec::new();

    let first = reader.read_u8().await;
    if let Ok(first) = first {
        // println!("First Byte {:#02x}", first);
        content.push(first ^ XOR_VAL);
    }
    while let Ok(x) = reader.read_u8().await {
        content.push(x ^ XOR_VAL);
    }

    // let mut buffer_byte: u8 = 0;
    // let single_byte_buffer = std::slice::from_mut(&mut buffer_byte);

    // while let Some(Ok(x)) = iter.next() {
    //     // single_byte_buffer[0] = x ^ XOR_VAL;
    //     writer.write_all(single_byte_buffer).unwrap();
    // }
    let buff = Cursor::new(content);

    // Get a output stream handle to the default physical sound device
    // Load a sound from a file, using a path relative to Cargo.toml
    // let file = BufReader::new(File::open(sound_file).unwrap());
    // Decode that sound file into a source

    // let data: Vec<f32> = (1..=length).map(f32::from).collect();
    // SamplesBuffer::new(1, 1, data)

    // let aslsla: SamplesBuffer<Vec<u8>> = SamplesBuffer::new(2, 1, content);
    let source = Decoder::new_looped(buff)
        .unwrap()
        .convert_samples::<f32>()
        .fade_in(Duration::from_secs(10));
    sink.append(source);
    sink.pause();
    sink.play();
    // sik
    // Play the sound directly on the device
    // let _ = stream_handle.play_raw(source.convert_samples());
}

#[tokio::main]
async fn main() {
    let mut devices = cpal::default_host().output_devices().unwrap();

    let devices_names = devices
        .map(|x| x.name().unwrap_or(String::new()).clone())
        .collect::<Vec<String>>();

    devices = cpal::default_host().output_devices().unwrap();
    let device = devices
        .find(|x| {
            x.name()
                .unwrap_or(String::new())
                .contains("Voicemeeter VAIO3 Input")
        })
        .unwrap();
    // let device = devices_filtered.next().expect("Device could not be found");

    let (_stream, stream_handle) = OutputStream::try_from_device(&device).unwrap();
    let sink = Sink::try_new(&stream_handle).unwrap();

    play_sound(&sink).await;

    // This println! comes first
    println!("hello");
    println!("{:?}", devices_names);

    // let handle = tokio::spawn(op);

    println!("world");

    // The sound plays in a separate thread. This call will block the current thread until the sink
    // has finished playing all its queued sounds.
    sink.sleep_until_end();
}
